//! OpenAPI / AsyncAPI schemas of the protocol's friends types (mirror structs: the protocol crate
//! has no OpenAPI dependency). A test serializes the real types and compares the field names with
//! these schemas, so the documents cannot drift from the wire format.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::ToSchema;

/// One player in the caller's friends, requests or blocks.
#[derive(Serialize, ToSchema)]
pub(crate) struct FriendEntry {
    /// The other player.
    user: i64,
    /// Their display name.
    name: Option<String>,
    /// `friend`, `sent`, `received` or `blocked`.
    state: String,
    /// When this state began (unix ms).
    since: i64,
    /// Whether they are online (friends only).
    online: Option<bool>,
    /// When they were last online (unix ms; friends only).
    last_seen: Option<i64>,
}

/// A page of entries, newest first.
#[derive(Serialize, ToSchema)]
pub(crate) struct FriendPage {
    /// The entries.
    items: Vec<FriendEntry>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// `POST /v1/friends/requests` body: exactly one of `user`, `name`, `code`.
#[derive(Serialize, ToSchema)]
pub(crate) struct AddFriend {
    /// By account id.
    user: Option<i64>,
    /// By display name (exact, after trimming).
    name: Option<String>,
    /// By friend code (case, spaces and dashes do not matter).
    code: Option<String>,
}

/// The caller's friend code.
#[derive(Serialize, ToSchema)]
pub(crate) struct FriendCode {
    /// 8 characters of 2-9 and A-Z without O and I.
    code: String,
}

/// `friends.presence` push: a friend came online or went offline.
#[derive(Serialize, ToSchema)]
pub(crate) struct FriendPresence {
    /// The friend.
    user: i64,
    /// Online now.
    online: bool,
    /// When the friend was last online (unix ms; with `online: false`).
    last_seen: Option<i64>,
}

/// `POST /v1/friends/steam` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct SteamMatch {
    /// SteamID64s of individual Steam accounts as decimal strings (at most the server's cap, 500 by default).
    steam_ids: Vec<String>,
}

/// An account a Steam ID belongs to.
#[derive(Serialize, ToSchema)]
pub(crate) struct SteamPlayer {
    /// The Steam ID as sent.
    steam_id: String,
    /// The account.
    user: i64,
    /// Its display name.
    name: Option<String>,
    /// The caller's relation to it: `friend`, `sent` or `received` (absent: none).
    state: Option<String>,
}

/// The accounts found, in the order of the request.
#[derive(Serialize, ToSchema)]
pub(crate) struct SteamMatchResult {
    /// The accounts.
    players: Vec<SteamPlayer>,
}

/// The caller's friends settings.
#[derive(Serialize, ToSchema)]
pub(crate) struct FriendSettings {
    /// Whether other players find this account by its linked Steam account (`POST /v1/friends/steam`). Default true.
    steam_findable: bool,
}

/// `PUT /v1/friends/settings` body: fields left out keep their value.
#[derive(Serialize, ToSchema)]
pub(crate) struct UpdateFriendSettings {
    /// Whether other players find this account by its linked Steam account.
    steam_findable: Option<bool>,
}

/// An empty success answer: `{}`.
#[derive(Serialize, ToSchema)]
pub(crate) struct Ack {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::friends as p;
    use net_backend_protocol::{Cursor, Page, UnixMillis, UserId};
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
        let entry = p::FriendEntry::new(UserId(7), p::FriendState::Friend, UnixMillis(1)).with_name("Ada").with_online(true, Some(UnixMillis(2)));
        assert_eq!(properties::<FriendEntry>(), keys(&entry));
        assert_eq!(properties::<FriendPage>(), keys(Page::new(vec![entry], Some(Cursor::new("1")))));
        let mut all = p::AddFriend::by_id(UserId(1));
        all.name = Some("Ada".into());
        all.code = Some("K7M2Q9XD".into());
        assert_eq!(properties::<AddFriend>(), keys(&all));
        assert_eq!(properties::<FriendCode>(), keys(p::FriendCode::new("K7M2Q9XD")));
        assert_eq!(properties::<FriendPresence>(), keys(p::FriendPresence::new(UserId(7), false).with_last_seen(UnixMillis(3))));
        assert_eq!(properties::<SteamMatch>(), keys(p::SteamMatch::new([76_561_201_960_265_729])));
        let player = p::SteamPlayer::new("76561201960265729", UserId(7)).with_name("Ada").with_state(p::FriendState::Friend);
        assert_eq!(properties::<SteamPlayer>(), keys(&player));
        assert_eq!(properties::<SteamMatchResult>(), keys(p::SteamMatchResult::new(vec![player])));
        assert_eq!(properties::<FriendSettings>(), keys(p::FriendSettings::default()));
        assert_eq!(properties::<UpdateFriendSettings>(), keys(p::UpdateFriendSettings::new().steam_findable(false)));
    }
}
