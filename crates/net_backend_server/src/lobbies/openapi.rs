//! OpenAPI schemas of the protocol's lobby types (mirror structs: the protocol crate has no
//! OpenAPI dependency). A test serializes the real types and compares the field names with these
//! schemas, so the document cannot drift from the wire format.

#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::Serialize;
use utoipa::ToSchema;

/// One member of a lobby.
#[derive(Serialize, ToSchema)]
pub(crate) struct LobbyMember {
    /// The member.
    user: i64,
    /// Their display name.
    name: Option<String>,
    /// Their ready flag.
    ready: bool,
    /// When they joined (unix ms).
    joined_at: i64,
}

/// A lobby.
#[derive(Serialize, ToSchema)]
pub(crate) struct LobbyInfo {
    /// The id.
    id: i64,
    /// `public`, `private` or `friends`.
    visibility: String,
    /// `open`, `in_game` or `closed`.
    state: String,
    /// The host.
    host: Option<i64>,
    /// The most members.
    max_players: u32,
    /// How many members it has.
    members: u32,
    /// The game's metadata (text keys and values).
    metadata: BTreeMap<String, String>,
    /// When it was created (unix ms).
    created_at: i64,
    /// The join code (members only): 8 characters of `23456789ABCDEFGHJKLMNPQRSTUVWXYZ`.
    code: Option<String>,
    /// The join code as a number below 2^40 (members only): each character's place in the alphabet is 5 bits, the first character the highest.
    code_number: Option<u64>,
    /// The lobby's chat room (members only), with the chat module.
    chat_room: Option<i64>,
    /// The members, in the order they joined (in answers about one lobby; absent in search results).
    players: Vec<LobbyMember>,
}

/// A page of lobbies, newest first.
#[derive(Serialize, ToSchema)]
pub(crate) struct LobbyPage {
    /// The lobbies (without member lists and join codes).
    items: Vec<LobbyInfo>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// The caller's lobbies.
#[derive(Serialize, ToSchema)]
pub(crate) struct LobbyList {
    /// The lobbies with their members, oldest membership first.
    lobbies: Vec<LobbyInfo>,
}

/// `POST /v1/lobbies` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct CreateLobby {
    /// `public` (default), `private` or `friends` (with the friends module).
    visibility: String,
    /// The most members (1 to the server's limit, 64 by default).
    max_players: u32,
    /// The game's metadata: keys of 1-64 bytes (letters, digits, `_ . : -`), values of at most 256 characters.
    metadata: BTreeMap<String, String>,
}

/// `PATCH /v1/lobbies/{lobby}` body: absent fields stay.
#[derive(Serialize, ToSchema)]
pub(crate) struct UpdateLobby {
    /// A new visibility.
    visibility: Option<String>,
    /// A new size (not below the member count).
    max_players: Option<u32>,
    /// A new state: `open`, `in_game` or `closed` (removes the lobby).
    state: Option<String>,
    /// Metadata changes: a text sets the key, `null` removes it.
    metadata: BTreeMap<String, Option<String>>,
}

/// One metadata filter: the key has exactly this value.
#[derive(Serialize, ToSchema)]
pub(crate) struct LobbyFilter {
    /// The metadata key.
    key: String,
    /// The value.
    value: String,
}

/// `POST /v1/lobbies/search` body (every field optional).
#[derive(Serialize, ToSchema)]
pub(crate) struct LobbySearch {
    /// At most 8 filters; every one must match.
    filters: Vec<LobbyFilter>,
    /// The lobbies the caller's friends host (public and friends-only), with the friends module.
    friends: bool,
    /// Include full lobbies.
    include_full: bool,
    /// The previous page's next_cursor.
    cursor: Option<String>,
    /// 1-100, default 50.
    limit: Option<u32>,
}

/// `POST /v1/lobbies/join` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct JoinLobbyByCode {
    /// The join code (case, spaces and dashes do not matter).
    code: String,
}

/// `PUT /v1/lobbies/{lobby}/ready` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct SetReady {
    /// Ready or not.
    ready: bool,
}

/// A player: the body of `POST /v1/lobbies/{lobby}/host`.
#[derive(Serialize, ToSchema)]
pub(crate) struct LobbyPlayer {
    /// The player.
    user: i64,
}

/// The `lobby.member` push.
#[derive(Serialize, ToSchema)]
pub(crate) struct LobbyMemberUpdate {
    /// The lobby.
    lobby: i64,
    /// `joined`, `left`, `kicked` or `ready`.
    change: String,
    /// The member as it is now.
    member: LobbyMember,
}

/// The `lobby.changed` push.
#[derive(Serialize, ToSchema)]
pub(crate) struct LobbyUpdate {
    /// What changed: `host`, `metadata`, `settings`, `state`, `code`.
    changes: Vec<String>,
    /// The lobby now (without its member list).
    lobby: LobbyInfo,
}

/// An empty success answer: `{}`.
#[derive(Serialize, ToSchema)]
pub(crate) struct Ack {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::lobbies as p;
    use net_backend_protocol::{Cursor, LobbyId, Page, RoomId, UnixMillis, UserId};
    use serde_json::Value;
    use utoipa::openapi::schema::Schema;
    use utoipa::openapi::RefOr;
    use utoipa::PartialSchema;

    use super::*;

    fn properties<T: PartialSchema>() -> BTreeSet<String> {
        match T::schema() {
            RefOr::T(Schema::Object(object)) => object.properties.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    fn keys(value: impl serde::Serialize) -> BTreeSet<String> {
        match serde_json::to_value(value) {
            Ok(Value::Object(map)) => map.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    /// Every mirror has exactly the fields of the real type with all optional fields set.
    #[test]
    fn mirrors_match_the_protocol() {
        let member = p::LobbyMember::new(UserId(1), UnixMillis(1)).with_name("Ada").with_ready(true);
        assert_eq!(properties::<LobbyMember>(), keys(&member));
        let mut metadata = std::collections::BTreeMap::new();
        metadata.insert("mode".to_string(), "ranked".to_string());
        let info = p::LobbyInfo::new(LobbyId(7), p::LobbyVisibility::Public, p::LobbyState::Open, 4, 1, UnixMillis(1))
            .with_host(UserId(1))
            .with_metadata(metadata)
            .with_code(p::LobbyCode::from_u64(5).expect("code"))
            .with_chat_room(RoomId(9))
            .with_players(vec![member.clone()]);
        assert_eq!(properties::<LobbyInfo>(), keys(&info));
        assert_eq!(properties::<LobbyPage>(), keys(Page::new(vec![info.clone()], Some(Cursor::new("1")))));
        assert_eq!(properties::<LobbyList>(), keys(p::LobbyList::new(vec![info.clone()])));
        assert_eq!(properties::<CreateLobby>(), keys(p::CreateLobby::new(4).with_meta("mode", "ranked")));
        let update =
            p::UpdateLobby::new().with_visibility(p::LobbyVisibility::Private).with_max_players(3).with_state(p::LobbyState::InGame).set_meta("a", "b");
        assert_eq!(properties::<UpdateLobby>(), keys(&update));
        assert_eq!(properties::<LobbyFilter>(), keys(p::LobbyFilter::new("a", "b")));
        let search = p::LobbySearch::new().with_filter("a", "b").of_friends().including_full().after(Cursor::new("1")).with_limit(5);
        assert_eq!(properties::<LobbySearch>(), keys(&search));
        assert_eq!(properties::<JoinLobbyByCode>(), keys(p::JoinLobbyByCode::new("K7M2Q9XD")));
        assert_eq!(properties::<SetReady>(), keys(p::SetReady::new(true)));
        assert_eq!(properties::<LobbyPlayer>(), keys(p::LobbyPlayer::new(UserId(1))));
        assert_eq!(properties::<LobbyMemberUpdate>(), keys(p::LobbyMemberUpdate::new(LobbyId(7), p::MemberChange::Joined, member)));
        assert_eq!(properties::<LobbyUpdate>(), keys(p::LobbyUpdate::new(vec![p::LobbyChange::State], info)));
    }
}
