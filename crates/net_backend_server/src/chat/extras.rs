//! Read markers (stored per user and room; `chat.read` pushes coalesced) and typing indicators
//! (never stored; `chat.typing` pushes throttled) of the chat module.
//!
//! **Read markers:** one row per user and room in `chat_reads`, moved forward only (an `UPDATE …
//! WHERE message_id < new`, else an `INSERT`; a unique violation means another request stored one
//! first). Unread counts are counted per room from the marker, capped at
//! [`MAX_UNREAD_COUNT`] (a `COUNT` over a `LIMIT`ed subquery: a huge public room costs at most that
//! many index entries). A moved marker in a DM, group or player room is pushed to the room: at once
//! when the user's last push of that room is older than `read_push_interval_ms`, else once at the
//! end of the interval with the newest marker (in memory, per instance).
//!
//! **Typing:** per user and room, `typing: true` is pushed at most once per `typing_interval_ms`;
//! `typing: false` only after a pushed `true` that has not expired. The push carries
//! `typing_ttl_ms`: clients drop the indicator after it. A sent message clears the user's state (the
//! next `true` pushes at once). Rooms with more online users than `typing_max_members` (this
//! instance) get none; DMs always do.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use net_backend_protocol::chat::{
    MarkRead, ReadReceipt, ReadReceipts, SetTyping, TypingUpdate, UnreadCount, UnreadCounts, UnreadQuery, MAX_LISTED_RECEIPTS, MAX_UNREAD_COUNT,
};
use net_backend_protocol::{MessageId, RoomId, UnixMillis, UserId};

use super::events::{AfterChatRead, BeforeChatTyping};
use super::service::{chat_room, kind_of, not_a_member, ChatService};
use super::store::{self, CountRow, MessageRow, ReadRow, RoomRow, KIND_DM, KIND_ROOM};
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::rate_limit::RateDecision;
use crate::state::AppState;
use crate::ws::ConnectionId;

/// Entries kept per table before the old ones are dropped.
const SLOTS: usize = 10_000;

/// What to do with a moved marker.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Offer {
    /// Push it now.
    Now,
    /// A push is held for the end of the interval: it takes this marker.
    Merged,
    /// Plan a push after this wait (it takes the newest marker by then).
    Later(Duration),
}

#[derive(Debug)]
struct ReadSlot {
    last: Instant,
    pending: Option<ReadReceipt>,
    scheduled: bool,
}

/// The coalesced `chat.read` pushes (per instance).
#[derive(Debug)]
pub(crate) struct ReadPushes {
    interval: Duration,
    slots: Mutex<HashMap<(RoomId, UserId), ReadSlot>>,
}

impl ReadPushes {
    pub(crate) fn new(interval: Duration) -> Self {
        Self { interval, slots: Mutex::new(HashMap::new()) }
    }

    /// A marker moved at `now`: push it now, merge it into a held push, or hold one.
    pub(crate) fn offer(&self, receipt: ReadReceipt, now: Instant) -> Offer {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots.len() >= SLOTS {
            let interval = self.interval;
            slots.retain(|_, s| s.scheduled || now.saturating_duration_since(s.last) < interval);
        }
        let key = (receipt.room, receipt.user);
        match slots.get_mut(&key) {
            Some(slot) if slot.scheduled => {
                slot.pending = Some(receipt);
                Offer::Merged
            }
            Some(slot) if now.saturating_duration_since(slot.last) < self.interval => {
                let wait = self.interval.saturating_sub(now.saturating_duration_since(slot.last));
                slot.pending = Some(receipt);
                slot.scheduled = true;
                Offer::Later(wait)
            }
            Some(slot) => {
                slot.last = now;
                Offer::Now
            }
            None => {
                slots.insert(key, ReadSlot { last: now, pending: None, scheduled: false });
                Offer::Now
            }
        }
    }

    /// A held push is due at `now`: the newest marker.
    pub(crate) fn take(&self, room: RoomId, user: UserId, now: Instant) -> Option<ReadReceipt> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot = slots.get_mut(&(room, user))?;
        slot.scheduled = false;
        slot.last = now;
        slot.pending.take()
    }
}

/// The typing throttle (per instance): per user and room, when the last push went out and whether
/// it said "typing".
#[derive(Debug)]
pub(crate) struct Typing {
    interval: Duration,
    ttl: Duration,
    slots: Mutex<HashMap<(RoomId, UserId), (Instant, bool)>>,
}

impl Typing {
    pub(crate) fn new(interval: Duration, ttl: Duration) -> Self {
        Self { interval, ttl, slots: Mutex::new(HashMap::new()) }
    }

    /// Whether this change is pushed (and remember it).
    pub(crate) fn decide(&self, room: RoomId, user: UserId, typing: bool, now: Instant) -> bool {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots.len() >= SLOTS {
            let ttl = self.ttl;
            slots.retain(|_, (last, _)| now.saturating_duration_since(*last) < ttl);
        }
        let key = (room, user);
        let last = slots.get(&key).copied();
        let push = match (last, typing) {
            (Some((at, true)), true) => now.saturating_duration_since(at) >= self.interval,
            (_, true) => true,
            (Some((at, true)), false) => now.saturating_duration_since(at) < self.ttl,
            (_, false) => false,
        };
        if push {
            slots.insert(key, (now, typing));
        }
        push
    }

    /// The user sent a message in the room: the indicator ends on the clients.
    pub(crate) fn clear(&self, room: RoomId, user: UserId) {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).remove(&(room, user));
    }

    fn ttl_ms(&self) -> u32 {
        u32::try_from(self.ttl.as_millis()).unwrap_or(u32::MAX)
    }
}

impl ChatService {
    /// Store `user`'s read marker of a room (it only moves forward); push it to DM, group and
    /// player rooms (coalesced). The message must belong to the room.
    pub(crate) async fn mark_read(&self, state: &AppState, ctx: &HookCtx, user: UserId, request: MarkRead) -> Result<(), AppError> {
        let row = self.room_row(state, request.room).await?;
        if !Self::may_read(state, &row, user).await? {
            return Err(not_a_member());
        }
        if let RateDecision::Deny { retry_after_ms } = self.0.read_rate.check(user) {
            return Err(AppError::rate_limited(retry_after_ms));
        }
        let message = request.read.message;
        let db = state.db();
        if db.fetch_optional::<MessageRow, _>(&store::message(row.id, message.get())).await?.is_none() {
            return Err(AppError::not_found("no such message in this room"));
        }
        let now = state.now().get();
        let advance = store::advance_read(row.id, user.get(), message.get(), now);
        let moved = if db.execute(&advance).await? > 0 {
            true
        } else {
            match db.execute(&store::insert_read(row.id, user.get(), message.get(), now)?).await {
                Ok(_) => true,
                // A marker exists (at this message or beyond, or stored by a request racing this one).
                Err(error) if error.is_unique_violation() => db.execute(&advance).await? > 0,
                Err(error) => return Err(error.into()),
            }
        };
        if !moved {
            return Ok(());
        }
        if self.0.config.read_receipts && row.kind != KIND_ROOM {
            self.push_receipt(state, &row, ReadReceipt::new(RoomId(row.id), user, message, UnixMillis(now)));
        }
        let after = AfterChatRead { room: RoomId(row.id), kind: kind_of(&row), user_id: user, message };
        let (hooks, ctx) = (state.hooks().clone(), ctx.clone());
        tokio::spawn(async move { hooks.run_after(&ctx, Arc::new(after)).await });
        Ok(())
    }

    fn push_receipt(&self, state: &AppState, row: &RoomRow, receipt: ReadReceipt) {
        match self.0.reads.offer(receipt, Instant::now()) {
            Offer::Now => {
                if let Err(error) = self.push_message(state.ws(), row, &receipt) {
                    tracing::warn!(%error, "chat: a read receipt push failed");
                }
            }
            Offer::Merged => {}
            Offer::Later(wait) => {
                let (service, state, row) = (self.clone(), state.clone(), row.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    if let Some(latest) = service.0.reads.take(receipt.room, receipt.user, Instant::now()) {
                        if let Err(error) = service.push_message(state.ws(), &row, &latest) {
                            tracing::warn!(%error, "chat: a read receipt push failed");
                        }
                    }
                });
            }
        }
    }

    /// The read markers of a DM, group or player room (its members), the newest first.
    pub async fn receipts(&self, state: &AppState, user: UserId, room: RoomId) -> Result<ReadReceipts, AppError> {
        let row = self.room_row(state, room).await?;
        if row.kind == KIND_ROOM {
            return Err(AppError::bad_request("read markers are shared in direct-message, group and player rooms only"));
        }
        if !Self::may_read(state, &row, user).await? {
            return Err(not_a_member());
        }
        let rows = state.db().fetch_all::<ReadRow, _>(&store::receipts(row.id, u64::from(MAX_LISTED_RECEIPTS))).await?;
        let receipts =
            rows.into_iter().map(|r| ReadReceipt::new(RoomId(r.room_id), UserId(r.user_id), MessageId(r.message_id), UnixMillis(r.read_at))).collect();
        Ok(ReadReceipts::new(room, receipts))
    }

    /// `user`'s unread counts of these rooms (rooms it cannot read, or that do not exist, are left
    /// out), in the order asked.
    pub async fn unread(&self, state: &AppState, user: UserId, query: &UnreadQuery) -> Result<UnreadCounts, AppError> {
        query.validate()?;
        let mut seen = HashSet::new();
        let rooms: Vec<RoomId> = query.rooms.iter().copied().filter(|r| seen.insert(*r)).collect();
        let ids: Vec<i64> = rooms.iter().map(|r| r.get()).collect();
        let markers: HashMap<i64, i64> =
            state.db().fetch_all::<ReadRow, _>(&store::reads_of(user.get(), &ids)).await?.into_iter().map(|r| (r.room_id, r.message_id)).collect();
        let since = self.0.config.cutoff(state.now().get());
        let mut counts = Vec::with_capacity(rooms.len());
        for room in rooms {
            let row = match self.room_row(state, room).await {
                Ok(row) => row,
                Err(error) if error.status().as_u16() == 404 => continue,
                Err(error) => return Err(error),
            };
            if !Self::may_read(state, &row, user).await? {
                continue;
            }
            let last = markers.get(&row.id).copied();
            let statement = store::unread_count(row.id, user.get(), last, since, u64::from(MAX_UNREAD_COUNT));
            let n = state.db().fetch_one::<CountRow, _>(&statement).await?.n;
            counts.push(UnreadCount::new(room, u32::try_from(n).unwrap_or(MAX_UNREAD_COUNT).min(MAX_UNREAD_COUNT), last.map(MessageId)));
        }
        Ok(UnreadCounts::new(counts))
    }

    /// `user` types in a room (or stopped): the room joined on `connection` (a DM: a member);
    /// pushed after the throttle and the [`BeforeChatTyping`] hooks.
    pub(crate) async fn set_typing(&self, state: &AppState, ctx: &HookCtx, connection: ConnectionId, user: UserId, request: SetTyping) -> Result<(), AppError> {
        let row = self.room_row(state, request.room).await?;
        let room = RoomId(row.id);
        let allowed =
            if row.kind == KIND_DM { Self::is_participant(&row, user) } else { state.ws().rooms_of(connection).iter().any(|r| chat_room(r) == Some(room)) };
        if !allowed {
            return Err(not_a_member());
        }
        if row.kind != KIND_DM && self.0.presence.count(room) > self.0.config.typing_max_members {
            return Ok(());
        }
        if !self.0.typing.decide(room, user, request.typing, Instant::now()) {
            return Ok(());
        }
        let event = BeforeChatTyping { room, kind: kind_of(&row), user_id: user, typing: request.typing };
        state.hooks().run_before(ctx, event).await?;
        let ttl = if request.typing { self.0.typing.ttl_ms() } else { 0 };
        self.push_message(state.ws(), &row, &TypingUpdate::new(room, user, request.typing, ttl))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pushes_coalesce() {
        let pushes = ReadPushes::new(Duration::from_millis(1000));
        let t0 = Instant::now();
        let r = |m: i64| ReadReceipt::new(RoomId(1), UserId(2), MessageId(m), UnixMillis(m));
        assert_eq!(pushes.offer(r(1), t0), Offer::Now);
        assert_eq!(pushes.offer(r(2), t0 + Duration::from_millis(300)), Offer::Later(Duration::from_millis(700)));
        assert_eq!(pushes.offer(r(3), t0 + Duration::from_millis(400)), Offer::Merged);
        assert_eq!(pushes.take(RoomId(1), UserId(2), t0 + Duration::from_millis(1000)).map(|x| x.message), Some(MessageId(3)), "the newest wins");
        assert_eq!(pushes.offer(r(4), t0 + Duration::from_millis(2500)), Offer::Now);
        assert_eq!(pushes.offer(ReadReceipt::new(RoomId(1), UserId(3), MessageId(4), UnixMillis(4)), t0), Offer::Now, "per user");
    }

    #[test]
    fn typing_throttles_and_expires() {
        let typing = Typing::new(Duration::from_millis(3000), Duration::from_millis(6000));
        let t0 = Instant::now();
        let (room, user) = (RoomId(1), UserId(2));
        assert!(!typing.decide(room, user, false, t0), "a stop without a start is quiet");
        assert!(typing.decide(room, user, true, t0));
        assert!(!typing.decide(room, user, true, t0 + Duration::from_millis(1000)), "throttled");
        assert!(typing.decide(room, user, true, t0 + Duration::from_millis(3000)), "refreshed");
        assert!(typing.decide(room, user, false, t0 + Duration::from_millis(4000)), "stop after a start");
        assert!(!typing.decide(room, user, false, t0 + Duration::from_millis(4100)), "stopped once");
        assert!(typing.decide(room, user, true, t0 + Duration::from_millis(4200)), "a start after a stop pushes at once");
        assert!(!typing.decide(room, user, false, t0 + Duration::from_millis(20_000)), "expired: no stop");
        assert!(typing.decide(room, user, true, t0 + Duration::from_millis(20_100)));
        typing.clear(room, user);
        assert!(typing.decide(room, user, true, t0 + Duration::from_millis(20_200)), "a message clears the throttle");
        assert_eq!(typing.ttl_ms(), 6000);
    }
}
