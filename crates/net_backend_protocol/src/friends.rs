//! Friends: in-game friends by account. A player adds another by account id, by display name or
//! by friend code; the other accepts or declines; either removes the friendship; a player blocks
//! anyone (no requests from them, and a block ends a friendship). Friends see each other's online
//! state.
//!
//! | Route | Request → answer |
//! |---|---|
//! | `GET /v1/friends` | [`ListFriends`] → [`Page`]`<`[`FriendEntry`]`>` (state `friend`, with `online` / `last_seen`) |
//! | `DELETE /v1/friends/{user}` | [`RemoveFriend`] → [`Ack`] |
//! | `GET /v1/friends/requests` | [`ListFriendRequests`] → [`Page`]`<`[`FriendEntry`]`>` (`received` or `sent`) |
//! | `POST /v1/friends/requests` | [`AddFriend`] → [`FriendEntry`] (`sent`, or `friend` when that player had asked first) |
//! | `DELETE /v1/friends/requests/{user}` | [`CancelFriendRequest`] → [`Ack`] |
//! | `POST /v1/friends/requests/{user}/accept` | [`AcceptFriend`] → [`FriendEntry`] (`friend`) |
//! | `POST /v1/friends/requests/{user}/decline` | [`DeclineFriend`] → [`Ack`] |
//! | `GET /v1/friends/blocks` | [`ListBlocks`] → [`Page`]`<`[`FriendEntry`]`>` (state `blocked`) |
//! | `PUT /v1/friends/blocks/{user}` | [`BlockUser`] → [`Ack`] |
//! | `DELETE /v1/friends/blocks/{user}` | [`UnblockUser`] → [`Ack`] |
//! | `GET /v1/friends/code` | [`GetFriendCode`] → [`FriendCode`] |
//! | `POST /v1/friends/code` | [`ResetFriendCode`] → [`FriendCode`] (a new code; the old one stops working) |
//! | `POST /v1/friends/presence` | [`FriendsHeartbeat`] → [`Ack`] (online for the server's online window, for clients without a WebSocket) |
//! | `POST /v1/friends/steam` | [`SteamMatch`] → [`SteamMatchResult`] (which of these Steam IDs belong to accounts here) |
//! | `GET /v1/friends/settings` | [`GetFriendSettings`] → [`FriendSettings`] |
//! | `PUT /v1/friends/settings` | [`UpdateFriendSettings`] → [`FriendSettings`] |
//! | push `friends.presence` | [`FriendPresence`], to the player's friends when the player comes online or goes offline |
//!
//! **Online state:** a friend is online while it has an open WebSocket connection, or for the
//! server's online window (90 s by default) after its last [`FriendsHeartbeat`]. Lists show
//! `online` and `last_seen` for friends only; requests and blocks never show them.
//!
//! **Friend codes** ([`normalize_friend_code`]): 8 characters of [`FRIEND_CODE_ALPHABET`] (no `0`,
//! `O`, `1`, `I`), shown as the server sends them; when typed in, case, spaces and dashes do not
//! matter (`k7m2-q9xd` finds `K7M2Q9XD`).
//!
//! **Names:** [`AddFriend::by_name`] finds the account whose display name is exactly that name
//! (after trimming); display names are not unique, so several matches answer 409 `conflict` and
//! the player uses the friend code instead.
//!
//! **Steam IDs** ([`SteamMatch`]): a player who linked a Steam account sends a list of Steam IDs
//! (e.g. its Steam friends list) and gets back the ones that belong to accounts on this server,
//! with the account id and display name. Only accounts with a linked Steam account are found;
//! players who turned [`FriendSettings::steam_findable`] off are never found. Steam IDs travel as
//! decimal strings ([`parse_steam_id`]): a SteamID64 is larger than the integers JSON numbers
//! carry exactly.

use serde::{Deserialize, Serialize};

use crate::envelope::{Ack, ServerPush};
use crate::error::{ApiError, ValidationDetails};
use crate::ids::UserId;
use crate::kinds;
use crate::page::{Cursor, Page, PageRequest};
use crate::time::UnixMillis;

/// The characters of a friend code: digits and upper-case letters without `0`, `O`, `1` and `I`.
pub const FRIEND_CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
/// The length of a friend code.
pub const FRIEND_CODE_LEN: usize = 8;
/// The longest display name [`AddFriend::by_name`] accepts, in characters (the account rules).
pub const MAX_NAME_CHARS: usize = 64;

/// A typed-in friend code in its stored form: spaces and dashes removed, upper case, then exactly
/// [`FRIEND_CODE_LEN`] characters of [`FRIEND_CODE_ALPHABET`]; `None` if it cannot be a code.
///
/// ```
/// use net_backend_protocol::friends::normalize_friend_code;
///
/// assert_eq!(normalize_friend_code(" k7m2-q9xd ").as_deref(), Some("K7M2Q9XD"));
/// assert_eq!(normalize_friend_code("K7M2Q9X0"), None);
/// ```
pub fn normalize_friend_code(input: &str) -> Option<String> {
    let code: String = input.chars().filter(|c| !matches!(c, ' ' | '-')).map(|c| c.to_ascii_uppercase()).collect();
    (code.len() == FRIEND_CODE_LEN && code.bytes().all(|b| FRIEND_CODE_ALPHABET.contains(&b))).then_some(code)
}

/// How the caller relates to another player.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FriendState {
    /// Friends (both accepted).
    Friend,
    /// The caller sent a friend request that is still open.
    Sent,
    /// The caller received a friend request that is still open.
    Received,
    /// The caller blocked that player.
    Blocked,
    /// A state from a newer server this version does not know (never sent by this one).
    #[serde(other)]
    Unknown,
}

/// One player in the caller's friends, requests or blocks.
///
/// JSON: `{"user":7,"name":"Ada","state":"friend","since":1790000000000,"online":true,"last_seen":1790000500000}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FriendEntry {
    /// The other player.
    pub user: UserId,
    /// Their display name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How the caller relates to them.
    pub state: FriendState,
    /// When this state began (the request, the friendship, the block).
    pub since: UnixMillis,
    /// Whether they are online (friends only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online: Option<bool>,
    /// When they were last online, if ever (friends only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<UnixMillis>,
}

impl FriendEntry {
    /// An entry without a name or online state.
    pub fn new(user: UserId, state: FriendState, since: UnixMillis) -> Self {
        Self { user, name: None, state, since, online: None, last_seen: None }
    }

    /// The same entry with a display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same entry with the online state.
    pub fn with_online(mut self, online: bool, last_seen: Option<UnixMillis>) -> Self {
        self.online = Some(online);
        self.last_seen = last_seen;
        self
    }
}

/// Send a friend request: `POST /v1/friends/requests` → [`FriendEntry`]. Exactly one of `user`,
/// `name` and `code`. When that player already sent the caller a request, the two become friends
/// at once (state `friend`); a request already sent answers its entry again.
///
/// JSON: `{"user":7}`, `{"name":"Ada"}` or `{"code":"K7M2Q9XD"}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AddFriend {
    /// By account id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserId>,
    /// By display name (exact, after trimming).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// By friend code ([`normalize_friend_code`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl AddFriend {
    /// By account id.
    pub fn by_id(user: UserId) -> Self {
        Self { user: Some(user), ..Self::default() }
    }

    /// By display name.
    pub fn by_name(name: impl Into<String>) -> Self {
        Self { name: Some(name.into()), ..Self::default() }
    }

    /// By friend code.
    pub fn by_code(code: impl Into<String>) -> Self {
        Self { code: Some(code.into()), ..Self::default() }
    }

    /// The shape rules: exactly one of `user`, `name`, `code`; a name of 1 to [`MAX_NAME_CHARS`]
    /// characters after trimming; a code that [`normalize_friend_code`] accepts.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        let given = usize::from(self.user.is_some()) + usize::from(self.name.is_some()) + usize::from(self.code.is_some());
        if given != 1 {
            details.add("user", "give exactly one of user, name and code");
        }
        if let Some(name) = &self.name {
            let count = name.trim().chars().count();
            if count == 0 || count > MAX_NAME_CHARS {
                details.add("name", format!("must be 1 to {MAX_NAME_CHARS} characters"));
            }
        }
        if let Some(code) = &self.code {
            if normalize_friend_code(code).is_none() {
                details.add("code", format!("must be {FRIEND_CODE_LEN} characters of {}", String::from_utf8_lossy(FRIEND_CODE_ALPHABET)));
            }
        }
        details.into_result()
    }
}

/// Which friend requests to list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RequestDirection {
    /// Requests other players sent the caller.
    #[default]
    Received,
    /// Requests the caller sent.
    Sent,
    /// A direction from a newer client this server does not know (refused).
    #[serde(other)]
    Unknown,
}

/// The query of `GET /v1/friends/requests`: `?direction=sent&cursor=…&limit=…`.
///
/// JSON (as a query): `{"direction":"sent","limit":20}` (every field optional; `received` by default).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RequestQuery {
    /// Received (default) or sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<RequestDirection>,
    /// Where to continue (`next_cursor` of the previous page).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// At most this many (default 50, at most 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl RequestQuery {
    /// The newest received requests.
    pub fn received() -> Self {
        Self::default()
    }

    /// The newest sent requests.
    pub fn sent() -> Self {
        Self { direction: Some(RequestDirection::Sent), ..Self::default() }
    }

    /// The page after `cursor`.
    pub fn after(mut self, cursor: Cursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// The same query with this limit.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The page part.
    pub fn page(&self) -> PageRequest {
        PageRequest { cursor: self.cursor.clone(), limit: self.limit }
    }
}

/// The caller's friend code ([`GetFriendCode`], [`ResetFriendCode`]).
///
/// JSON: `{"code":"K7M2Q9XD"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FriendCode {
    /// The code other players add the caller with.
    pub code: String,
}

impl FriendCode {
    /// A code.
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }
}

/// A friend came online or went offline: the `friends.presence` push, to every open connection of
/// each of that player's friends.
///
/// JSON: `{"user":7,"online":false,"last_seen":1790000500000}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FriendPresence {
    /// The friend.
    pub user: UserId,
    /// Online now.
    pub online: bool,
    /// When the friend was last online (with `online: false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<UnixMillis>,
}

impl FriendPresence {
    /// A change.
    pub fn new(user: UserId, online: bool) -> Self {
        Self { user, online, last_seen: None }
    }

    /// The same change with the last-seen time.
    pub fn with_last_seen(mut self, at: UnixMillis) -> Self {
        self.last_seen = Some(at);
        self
    }
}

impl ServerPush for FriendPresence {
    const KIND: &'static str = kinds::FRIENDS_PRESENCE;
}

// ---- Steam IDs ----------------------------------------------------------------------------------

/// The most Steam IDs one [`SteamMatch`] may carry under any server setting (a server's own cap is
/// lower: 500 by default).
pub const MAX_STEAM_IDS: usize = 2000;

/// The upper 32 bits of an individual public SteamID64: universe 1 (public), account type 1
/// (individual), instance 1 (desktop).
const STEAM_INDIVIDUAL_HIGH: u64 = 0x0110_0001;

/// Whether `id` is an individual public SteamID64: universe 1, account type 1, instance 1, and an
/// account number other than 0.
///
/// ```
/// use net_backend_protocol::friends::is_individual_steam_id;
///
/// assert!(is_individual_steam_id(76_561_201_960_265_729));
/// assert!(!is_individual_steam_id(76_561_197_960_265_728)); // account number 0
/// assert!(!is_individual_steam_id(103_582_791_429_521_412)); // a Steam group
/// ```
pub fn is_individual_steam_id(id: u64) -> bool {
    id >> 32 == STEAM_INDIVIDUAL_HIGH && id & 0xFFFF_FFFF != 0
}

/// A Steam ID as it travels in JSON: the decimal SteamID64 without signs, spaces or leading zeros,
/// of an individual public account ([`is_individual_steam_id`]); `None` otherwise.
///
/// ```
/// use net_backend_protocol::friends::parse_steam_id;
///
/// assert_eq!(parse_steam_id("76561201960265729"), Some(76_561_201_960_265_729));
/// assert_eq!(parse_steam_id(" 76561201960265729"), None);
/// assert_eq!(parse_steam_id("076561201960265729"), None);
/// ```
pub fn parse_steam_id(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id: u64 = text.parse().ok()?;
    (id.to_string() == text && is_individual_steam_id(id)).then_some(id)
}

/// Which of these Steam IDs belong to accounts here: `POST /v1/friends/steam` →
/// [`SteamMatchResult`]. The caller must have a Steam account linked to its account, and the
/// server must have Steam login. At most the server's cap (500 by default, never more than
/// [`MAX_STEAM_IDS`]); a Steam ID sent twice is answered once. A per-player rate applies (429
/// `rate_limited`).
///
/// JSON: `{"steam_ids":["76561201960265729","76561201960265730"]}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SteamMatch {
    /// SteamID64s as decimal strings ([`parse_steam_id`]).
    pub steam_ids: Vec<String>,
}

impl SteamMatch {
    /// A lookup of these Steam IDs.
    ///
    /// ```
    /// use net_backend_protocol::friends::SteamMatch;
    ///
    /// let call = SteamMatch::new([76_561_201_960_265_729, 76_561_201_960_265_730]);
    /// assert_eq!(call.steam_ids, ["76561201960265729", "76561201960265730"]);
    /// ```
    pub fn new(steam_ids: impl IntoIterator<Item = u64>) -> Self {
        Self { steam_ids: steam_ids.into_iter().map(|id| id.to_string()).collect() }
    }

    /// The Steam IDs as numbers, in order, each once (entries [`parse_steam_id`] refuses are left
    /// out; [`validate`](Self::validate) refuses them first).
    pub fn ids(&self) -> Vec<u64> {
        let mut seen = std::collections::HashSet::new();
        self.steam_ids.iter().filter_map(|text| parse_steam_id(text)).filter(|id| seen.insert(*id)).collect()
    }

    /// The shape rules: at most [`MAX_STEAM_IDS`] entries, each the decimal SteamID64 of an
    /// individual Steam account ([`parse_steam_id`]). The details name the first ten bad entries
    /// (`steam_ids[3]`).
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if self.steam_ids.len() > MAX_STEAM_IDS {
            details.add("steam_ids", format!("at most {MAX_STEAM_IDS} Steam IDs"));
        }
        for (index, _) in self.steam_ids.iter().enumerate().filter(|(_, text)| parse_steam_id(text).is_none()).take(10) {
            details.add(format!("steam_ids[{index}]"), "is not the SteamID64 of an individual Steam account (decimal digits)");
        }
        details.into_result()
    }
}

/// One account that a Steam ID of a [`SteamMatch`] belongs to.
///
/// JSON: `{"steam_id":"76561201960265729","user":7,"name":"Ada","state":"friend"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SteamPlayer {
    /// The Steam ID as the caller sent it.
    pub steam_id: String,
    /// The account here.
    pub user: UserId,
    /// Its display name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How the caller already relates to it: `friend`, `sent` or `received` (absent: no relation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<FriendState>,
}

impl SteamPlayer {
    /// A found account without a name or relation.
    pub fn new(steam_id: impl Into<String>, user: UserId) -> Self {
        Self { steam_id: steam_id.into(), user, name: None, state: None }
    }

    /// The same entry with a display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same entry with the caller's relation to it.
    pub fn with_state(mut self, state: FriendState) -> Self {
        self.state = Some(state);
        self
    }
}

/// The answer to [`SteamMatch`]: the accounts found, in the order of the request.
///
/// JSON: `{"players":[{"steam_id":"76561201960265729","user":7,"name":"Ada"}]}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SteamMatchResult {
    /// The accounts found (none found: an empty list).
    pub players: Vec<SteamPlayer>,
}

impl SteamMatchResult {
    /// An answer with these players.
    pub fn new(players: Vec<SteamPlayer>) -> Self {
        Self { players }
    }
}

/// The caller's friends settings ([`GetFriendSettings`], [`UpdateFriendSettings`]).
///
/// JSON: `{"steam_findable":true}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FriendSettings {
    /// Whether other players find this account by its linked Steam account ([`SteamMatch`]).
    /// On by default.
    pub steam_findable: bool,
}

impl FriendSettings {
    /// Settings with this findable flag.
    pub fn new(steam_findable: bool) -> Self {
        Self { steam_findable }
    }
}

impl Default for FriendSettings {
    fn default() -> Self {
        Self::new(true)
    }
}

/// Change the caller's friends settings: `PUT /v1/friends/settings` → [`FriendSettings`] (all of
/// them, after the change). Fields left out keep their value.
///
/// JSON: `{"steam_findable":false}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UpdateFriendSettings {
    /// Whether other players find this account by its linked Steam account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_findable: Option<bool>,
}

impl UpdateFriendSettings {
    /// No change.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same change with the findable flag.
    pub fn steam_findable(mut self, findable: bool) -> Self {
        self.steam_findable = Some(findable);
        self
    }
}

// ---- typed HTTP calls (see `http_call`) ---------------------------------------------------------

/// The typed HTTP calls of this module (in their own scope: their imports stay out of the
/// module's doc-link scope).
mod calls {
    use super::*;

    use crate::http_call::{payload_call, HttpCall, NoPayload, PathParams, PayloadKind, NO_PAYLOAD};
    use crate::routes::{self, HttpMethod, Route};

    payload_call!(AddFriend, Post, routes::friends::REQUESTS, true, Json, FriendEntry);
    payload_call!(SteamMatch, Post, routes::friends::STEAM, true, Json, SteamMatchResult);
    payload_call!(UpdateFriendSettings, Put, routes::friends::SETTINGS, true, Json, FriendSettings);

    /// A call with a page query and no path parameters.
    macro_rules! page_call {
        ($(#[$meta:meta])* $name:ident, $path:expr, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Debug, Default, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {
                /// Which page.
                pub page: PageRequest,
            }

            impl $name {
                /// The first page.
                pub fn new() -> Self {
                    Self::default()
                }

                /// The same call for this page.
                pub fn with_page(mut self, page: PageRequest) -> Self {
                    self.page = page;
                    self
                }
            }

            impl HttpCall for $name {
                type Payload = PageRequest;
                type Response = $response;
                const ROUTE: Route = Route::new(HttpMethod::Get, $path, true);
                const PAYLOAD: PayloadKind = PayloadKind::Query;

                fn payload(&self) -> &PageRequest {
                    &self.page
                }

                fn from_parts(_params: &PathParams, page: PageRequest) -> Result<Self, ApiError> {
                    Ok(Self::new().with_page(page))
                }
            }
        };
    }

    page_call!(
        /// The caller's friends: `GET /v1/friends?cursor=…&limit=…` → [`Page`]`<`[`FriendEntry`]`>`
        /// (newest friendship first, with `online` and `last_seen`).
        ListFriends,
        routes::friends::LIST,
        Page<FriendEntry>
    );
    page_call!(
        /// The players the caller blocked: `GET /v1/friends/blocks?cursor=…&limit=…` →
        /// [`Page`]`<`[`FriendEntry`]`>` (newest first).
        ListBlocks,
        routes::friends::BLOCKS,
        Page<FriendEntry>
    );

    /// The caller's friend requests: `GET /v1/friends/requests?direction=…` →
    /// [`Page`]`<`[`FriendEntry`]`>` (newest first).
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListFriendRequests {
        /// Which requests and which page.
        pub query: RequestQuery,
    }

    impl ListFriendRequests {
        /// The newest received requests.
        pub fn received() -> Self {
            Self::default()
        }

        /// The newest sent requests.
        pub fn sent() -> Self {
            Self { query: RequestQuery::sent() }
        }

        /// The same call with this query.
        pub fn with_query(mut self, query: RequestQuery) -> Self {
            self.query = query;
            self
        }
    }

    impl HttpCall for ListFriendRequests {
        type Payload = RequestQuery;
        type Response = Page<FriendEntry>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::friends::REQUESTS, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &RequestQuery {
            &self.query
        }

        fn from_parts(_params: &PathParams, query: RequestQuery) -> Result<Self, ApiError> {
            Ok(Self::received().with_query(query))
        }
    }

    /// A call naming one other player in the path, without a payload.
    macro_rules! user_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {
                /// The other player.
                pub user: UserId,
            }

            impl $name {
                /// The call for `user`.
                pub fn new(user: UserId) -> Self {
                    Self { user }
                }
            }

            impl HttpCall for $name {
                type Payload = NoPayload;
                type Response = $response;
                const ROUTE: Route = Route::new(HttpMethod::$method, $path, true);
                const PAYLOAD: PayloadKind = PayloadKind::Empty;

                fn payload(&self) -> &NoPayload {
                    &NO_PAYLOAD
                }

                fn path_params(&self) -> PathParams {
                    PathParams::new().with("user", self.user)
                }

                fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
                    Ok(Self::new(params.id("user")?))
                }
            }
        };
    }

    user_call!(
        /// End a friendship: `DELETE /v1/friends/{user}` → [`Ack`] (also when there was none).
        RemoveFriend,
        Delete,
        routes::friends::ONE,
        Ack
    );
    user_call!(
        /// Withdraw a friend request the caller sent: `DELETE /v1/friends/requests/{user}` →
        /// [`Ack`] (also when there was none).
        CancelFriendRequest,
        Delete,
        routes::friends::REQUEST,
        Ack
    );
    user_call!(
        /// Accept a friend request the caller received: `POST /v1/friends/requests/{user}/accept`
        /// → [`FriendEntry`] (404 `not_found` when there is none).
        AcceptFriend,
        Post,
        routes::friends::ACCEPT,
        FriendEntry
    );
    user_call!(
        /// Decline a friend request the caller received: `POST /v1/friends/requests/{user}/decline`
        /// → [`Ack`] (also when there was none).
        DeclineFriend,
        Post,
        routes::friends::DECLINE,
        Ack
    );
    user_call!(
        /// Block a player: `PUT /v1/friends/blocks/{user}` → [`Ack`]. Ends a friendship and every
        /// open request between the two; they cannot send the caller requests.
        BlockUser,
        Put,
        routes::friends::BLOCK,
        Ack
    );
    user_call!(
        /// Lift a block: `DELETE /v1/friends/blocks/{user}` → [`Ack`] (also when there was none).
        UnblockUser,
        Delete,
        routes::friends::BLOCK,
        Ack
    );

    /// A call without path parameters or payload.
    macro_rules! empty_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {}

            impl $name {
                /// The call.
                pub fn new() -> Self {
                    Self {}
                }
            }

            impl HttpCall for $name {
                type Payload = NoPayload;
                type Response = $response;
                const ROUTE: Route = Route::new(HttpMethod::$method, $path, true);
                const PAYLOAD: PayloadKind = PayloadKind::Empty;

                fn payload(&self) -> &NoPayload {
                    &NO_PAYLOAD
                }

                fn from_parts(_params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
                    Ok(Self::new())
                }
            }
        };
    }

    empty_call!(
        /// The caller's friend code: `GET /v1/friends/code` → [`FriendCode`] (made on first use).
        GetFriendCode,
        Get,
        routes::friends::CODE,
        FriendCode
    );
    empty_call!(
        /// A new friend code for the caller: `POST /v1/friends/code` → [`FriendCode`]; the old code
        /// stops working.
        ResetFriendCode,
        Post,
        routes::friends::CODE,
        FriendCode
    );
    empty_call!(
        /// The caller is online: `POST /v1/friends/presence` → [`Ack`]. Friends see the caller online
        /// for the server's online window (90 s by default) after it; a client with an open
        /// WebSocket connection is online anyway and needs no heartbeat.
        FriendsHeartbeat,
        Post,
        routes::friends::PRESENCE,
        Ack
    );
    empty_call!(
        /// The caller's friends settings: `GET /v1/friends/settings` → [`FriendSettings`].
        GetFriendSettings,
        Get,
        routes::friends::SETTINGS,
        FriendSettings
    );
}

pub use calls::{
    AcceptFriend, BlockUser, CancelFriendRequest, DeclineFriend, FriendsHeartbeat, GetFriendCode, GetFriendSettings, ListBlocks, ListFriendRequests,
    ListFriends, RemoveFriend, ResetFriendCode, UnblockUser,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(normalize_friend_code("k7m2-q9xd").as_deref(), Some("K7M2Q9XD"));
        assert_eq!(normalize_friend_code("K7M2 Q9XD").as_deref(), Some("K7M2Q9XD"));
        for bad in ["", "K7M2Q9X", "K7M2Q9XDD", "K7M2Q9XO", "K7M2Q9X1", "K7M2Q9XÄ", "K7M2_Q9XD"] {
            assert_eq!(normalize_friend_code(bad), None, "{bad}");
        }
        assert_eq!(FRIEND_CODE_ALPHABET.len(), 32);
    }

    #[test]
    fn json_and_rules() {
        assert_eq!(serde_json::to_string(&AddFriend::by_id(UserId(7))).ok().as_deref(), Some(r#"{"user":7}"#));
        assert!(AddFriend::by_id(UserId(7)).validate().is_ok());
        assert!(AddFriend::by_name(" Ada ").validate().is_ok());
        assert!(AddFriend::by_code("k7m2-q9xd").validate().is_ok());
        assert!(AddFriend::default().validate().is_err());
        assert!(AddFriend { user: Some(UserId(1)), name: Some("Ada".into()), code: None }.validate().is_err());
        assert!(AddFriend::by_name("  ").validate().is_err());
        assert!(AddFriend::by_name("x".repeat(MAX_NAME_CHARS + 1)).validate().is_err());
        assert!(AddFriend::by_code("nope").validate().is_err());
        let entry = FriendEntry::new(UserId(7), FriendState::Friend, UnixMillis(5)).with_name("Ada").with_online(false, Some(UnixMillis(4)));
        assert_eq!(serde_json::to_string(&entry).ok().as_deref(), Some(r#"{"user":7,"name":"Ada","state":"friend","since":5,"online":false,"last_seen":4}"#));
        assert_eq!(serde_json::from_str::<FriendState>(r#""muted""#).ok(), Some(FriendState::Unknown));
        assert_eq!(serde_json::to_string(&RequestQuery::sent().with_limit(5)).ok().as_deref(), Some(r#"{"direction":"sent","limit":5}"#));
        assert_eq!(RequestQuery::sent().with_limit(5).page().limit, Some(5));
        let push = FriendPresence::new(UserId(7), false).with_last_seen(UnixMillis(9));
        assert_eq!(serde_json::to_string(&push).ok().as_deref(), Some(r#"{"user":7,"online":false,"last_seen":9}"#));
        assert_eq!(<FriendPresence as ServerPush>::KIND, "friends.presence");
    }

    #[test]
    fn steam_ids() {
        let base: u64 = 76_561_197_960_265_728;
        assert!(is_individual_steam_id(base + 1) && is_individual_steam_id(base + u64::from(u32::MAX)));
        assert!(!is_individual_steam_id(base), "account number 0");
        assert!(!is_individual_steam_id(base + (1 << 32)), "instance 2");
        assert!(!is_individual_steam_id(0) && !is_individual_steam_id(u64::MAX));
        for bad in
            ["", "-76561201960265729", "+76561201960265729", "7656120196026572x", "76561201960265729 ", "076561201960265729", "1", "18446744073709551616"]
        {
            assert_eq!(parse_steam_id(bad), None, "{bad}");
        }
        let call = SteamMatch::new([base + 1, base + 2, base + 1]);
        assert!(call.validate().is_ok());
        assert_eq!(call.ids(), [base + 1, base + 2], "in order, once each");
        assert_eq!(serde_json::to_string(&call).ok().as_deref(), Some(r#"{"steam_ids":["76561197960265729","76561197960265730","76561197960265729"]}"#));
        let bad = SteamMatch { steam_ids: vec!["76561197960265729".into(), "x".into(), "76561197960265728".into()] };
        let error = bad.validate().err().map(|e| format!("{e:?}")).unwrap_or_default();
        assert!(error.contains("steam_ids[1]") && error.contains("steam_ids[2]") && !error.contains("steam_ids[0]"), "{error}");
        let many = SteamMatch::new((1..=MAX_STEAM_IDS as u64 + 1).map(|n| base + n));
        assert!(many.validate().is_err());
        assert!(SteamMatch::default().validate().is_ok(), "an empty list is fine");
        let player = SteamPlayer::new("76561197960265729", UserId(7)).with_name("Ada").with_state(FriendState::Sent);
        assert_eq!(serde_json::to_string(&player).ok().as_deref(), Some(r#"{"steam_id":"76561197960265729","user":7,"name":"Ada","state":"sent"}"#));
        assert_eq!(
            serde_json::to_string(&SteamPlayer::new("76561197960265729", UserId(7))).ok().as_deref(),
            Some(r#"{"steam_id":"76561197960265729","user":7}"#)
        );
        assert_eq!(serde_json::to_string(&FriendSettings::default()).ok().as_deref(), Some(r#"{"steam_findable":true}"#));
        assert_eq!(serde_json::to_string(&UpdateFriendSettings::new().steam_findable(false)).ok().as_deref(), Some(r#"{"steam_findable":false}"#));
        assert_eq!(serde_json::to_string(&UpdateFriendSettings::new()).ok().as_deref(), Some("{}"));
    }
}
