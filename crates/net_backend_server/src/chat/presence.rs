//! Who is online in which chat room, on this server instance: users (each once, however many
//! connections), with the display name they had when they joined. Transitions are decided under
//! one lock, so a user's first connection in a room yields exactly one "joined" and its last one
//! exactly one "left", whatever the interleaving of joins, leaves and disconnects.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use net_backend_protocol::chat::{RoomMember, MAX_LISTED_MEMBERS};
use net_backend_protocol::{RoomId, UserId};

use crate::rate_limit::KeyedBuckets;
use crate::ws::ConnectionId;

#[derive(Debug)]
struct Member {
    connections: Vec<ConnectionId>,
    name: Option<String>,
}

/// The online users of the chat rooms.
#[derive(Debug)]
pub(crate) struct Presence {
    rooms: Mutex<HashMap<RoomId, HashMap<UserId, Member>>>,
    rate: KeyedBuckets<RoomId>,
    enabled: bool,
    max_members: u32,
}

/// What a change did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    /// The user's first connection joined / last one left.
    pub(crate) transition: bool,
    /// The user's name (as of joining).
    pub(crate) name: Option<String>,
    /// The room's online users after the change.
    pub(crate) count: u32,
}

fn count_of(room: Option<&HashMap<UserId, Member>>) -> u32 {
    room.map_or(0, |r| u32::try_from(r.len()).unwrap_or(u32::MAX))
}

impl Presence {
    pub(crate) fn new(enabled: bool, max_members: u32, per_second: u32) -> Self {
        Self { rooms: Mutex::new(HashMap::new()), rate: KeyedBuckets::new(per_second, Duration::from_secs(1), 100_000), enabled, max_members }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RoomId, HashMap<UserId, Member>>> {
        self.rooms.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `connection` of `user` is in `room` now.
    pub(crate) fn add(&self, room: RoomId, user: UserId, connection: ConnectionId, name: Option<String>) -> Change {
        let mut rooms = self.lock();
        let members = rooms.entry(room).or_default();
        let member = members.entry(user).or_insert_with(|| Member { connections: Vec::new(), name: name.clone() });
        let transition = member.connections.is_empty();
        if !member.connections.contains(&connection) {
            member.connections.push(connection);
        }
        let name = member.name.clone();
        Change { transition, name, count: count_of(Some(members)) }
    }

    /// `connection` of `user` left `room` (idempotent).
    pub(crate) fn remove(&self, room: RoomId, user: UserId, connection: ConnectionId) -> Change {
        let mut rooms = self.lock();
        let Some(members) = rooms.get_mut(&room) else { return Change { transition: false, name: None, count: 0 } };
        let Some(member) = members.get_mut(&user) else { return Change { transition: false, name: None, count: count_of(Some(members)) } };
        let before = member.connections.len();
        member.connections.retain(|c| *c != connection);
        let transition = before > 0 && member.connections.is_empty();
        let name = member.name.clone();
        if member.connections.is_empty() {
            members.remove(&user);
        }
        let count = count_of(Some(members));
        if members.is_empty() {
            rooms.remove(&room);
        }
        Change { transition, name, count }
    }

    /// How many users are online in `room`.
    pub(crate) fn count(&self, room: RoomId) -> u32 {
        count_of(self.lock().get(&room))
    }

    /// Whether `user` is online in `room`.
    #[cfg(test)]
    pub(crate) fn contains(&self, room: RoomId, user: UserId) -> bool {
        self.lock().get(&room).is_some_and(|m| m.contains_key(&user))
    }

    /// Up to [`MAX_LISTED_MEMBERS`] online users of `room` (by id), the total, and whether the list
    /// is cut.
    pub(crate) fn members(&self, room: RoomId) -> (Vec<RoomMember>, u32, bool) {
        let rooms = self.lock();
        let Some(members) = rooms.get(&room) else { return (Vec::new(), 0, false) };
        let mut list: Vec<(&UserId, &Member)> = members.iter().collect();
        list.sort_by_key(|(user, _)| **user);
        let limit = MAX_LISTED_MEMBERS as usize;
        let truncated = list.len() > limit;
        let listed = list
            .into_iter()
            .take(limit)
            .map(|(user, member)| match &member.name {
                Some(name) => RoomMember::new(*user).with_name(name.clone()),
                None => RoomMember::new(*user),
            })
            .collect();
        (listed, count_of(Some(members)), truncated)
    }

    /// Whether a change in a room of `count` online users (the larger of before / after) is
    /// pushed: presence on, the room within the cap, and the room's rate not used up.
    pub(crate) fn should_push(&self, room: RoomId, count: u32) -> bool {
        self.enabled && count <= self.max_members && self.rate.check(room).is_allow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_per_user() {
        let presence = Presence::new(true, 2, 100);
        let (room, ada, bo) = (RoomId(1), UserId(1), UserId(2));
        let (c1, c2, c3) = (ConnectionId::for_tests(1), ConnectionId::for_tests(2), ConnectionId::for_tests(3));
        assert_eq!(presence.add(room, ada, c1, Some("Ada".into())), Change { transition: true, name: Some("Ada".into()), count: 1 });
        assert!(!presence.add(room, ada, c2, None).transition, "a second connection is no new presence");
        assert!(!presence.add(room, ada, c2, None).transition, "idempotent");
        assert!(presence.add(room, bo, c3, None).transition);
        assert_eq!(presence.count(room), 2);
        let (members, count, truncated) = presence.members(room);
        assert_eq!((members.len(), count, truncated), (2, 2, false));
        assert_eq!(members[0].name.as_deref(), Some("Ada"));
        assert!(!presence.remove(room, ada, c1).transition);
        let left = presence.remove(room, ada, c2);
        assert_eq!((left.transition, left.name.as_deref(), left.count), (true, Some("Ada"), 1));
        assert!(!presence.remove(room, ada, c2).transition, "a second leave is nothing");
        assert!(presence.contains(room, bo) && !presence.contains(room, ada));
        assert!(presence.remove(room, bo, c3).transition);
        assert_eq!(presence.count(room), 0);
        assert!(!presence.remove(RoomId(9), bo, c3).transition);
    }

    #[test]
    fn caps_and_rate() {
        let presence = Presence::new(true, 3, 2);
        assert!(presence.should_push(RoomId(1), 3));
        assert!(!presence.should_push(RoomId(2), 4), "over the cap");
        assert!(presence.should_push(RoomId(1), 1));
        assert!(!presence.should_push(RoomId(1), 1), "the room's rate is used up");
        assert!(!Presence::new(false, 3, 2).should_push(RoomId(1), 1));
    }
}
