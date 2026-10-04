//! Lobbies: a player creates a lobby and hosts it; others join by id (public lobbies, or a
//! friend's friends-only lobby) or by the lobby's join code; members set a ready flag; the host
//! sets the lobby's metadata, size, visibility and state, kicks members and hands the lobby over.
//! The server only coordinates: the game's own connection between the players stays the game's.
//!
//! | Route / kind | Request → answer |
//! |---|---|
//! | `POST /v1/lobbies` | [`CreateLobby`] → [`LobbyInfo`] (the caller hosts it) |
//! | `GET /v1/lobbies/mine` | [`MyLobbies`] → [`LobbyList`] |
//! | `POST /v1/lobbies/search` | [`LobbySearch`] → [`Page`]`<`[`LobbyInfo`]`>` (open lobbies by metadata filters, newest first) |
//! | `POST /v1/lobbies/join` | [`JoinLobbyByCode`] → [`LobbyInfo`] (join with a join code) |
//! | `GET /v1/lobbies/{lobby}` | [`GetLobby`] → [`LobbyInfo`] (with its members) |
//! | `PATCH /v1/lobbies/{lobby}` | [`EditLobby`] ([`UpdateLobby`]) → [`LobbyInfo`] (host: metadata, size, visibility, state) |
//! | `POST /v1/lobbies/{lobby}/join` | [`JoinLobby`] → [`LobbyInfo`] |
//! | `POST /v1/lobbies/{lobby}/leave` | [`LeaveLobby`] → [`Ack`] |
//! | `PUT /v1/lobbies/{lobby}/ready` | [`SetLobbyReady`] ([`SetReady`]) → [`Ack`] |
//! | `POST /v1/lobbies/{lobby}/code` | [`NewLobbyCode`] → [`LobbyInfo`] (host: a new join code; the old one stops working) |
//! | `POST /v1/lobbies/{lobby}/host` | [`TransferLobby`] ([`LobbyPlayer`]) → [`Ack`] (host: another member hosts) |
//! | `DELETE /v1/lobbies/{lobby}/members/{user}` | [`KickFromLobby`] → [`Ack`] (host) |
//! | push `lobby.member` | [`LobbyMemberUpdate`]: a member joined, left, was kicked or changed its ready flag |
//! | push `lobby.changed` | [`LobbyUpdate`]: the host, metadata, settings, state or join code changed |
//!
//! **Visibility** ([`LobbyVisibility`]): `public` lobbies are listed by [`LobbySearch`] and joined
//! by id or code; `private` ones only with the join code; `friends` ones (when the server runs the
//! friends module) with the code, or by id by a friend of the host.
//!
//! **State** ([`LobbyState`]): `open` (players join), `in_game` (no one joins; the members stay),
//! `closed` (final: the lobby is removed, its members get a last `lobby.changed`).
//!
//! **Join codes** ([`LobbyCode`]): 8 characters of the friend-code alphabet
//! ([`FRIEND_CODE_ALPHABET`]: no `0`, `O`, `1`, `I`), unique among the server's lobbies, valid
//! as long as the lobby exists (the host replaces it with [`NewLobbyCode`]). A code is also a
//! number below 2^40 ([`LobbyCode::to_u64`] / [`LobbyCode::from_u64`]): it fits a 64-bit integer
//! wherever a platform carries a number instead of a text, and [`LobbyInfo`] carries both forms.
//!
//! **Metadata:** text keys and values the game chooses (`"mode":"ranked"`), set by the host and
//! filtered on by [`LobbySearch`]. Keys are 1 to [`MAX_META_KEY_BYTES`] bytes of ASCII letters,
//! digits, `_`, `.`, `:` and `-`; values at most [`MAX_META_VALUE_CHARS`] characters.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub use crate::friends::FRIEND_CODE_ALPHABET;

use crate::envelope::{Ack, ServerPush};
use crate::error::{ApiError, ValidationDetails};
use crate::ids::{LobbyId, RoomId, UserId};
use crate::kinds;
use crate::page::{Cursor, Page, PageRequest};
use crate::time::UnixMillis;

/// The length of a join code.
pub const LOBBY_CODE_LEN: usize = 8;
/// The longest metadata key, in bytes.
pub const MAX_META_KEY_BYTES: usize = 64;
/// The longest metadata value, in characters.
pub const MAX_META_VALUE_CHARS: usize = 256;
/// The most metadata filters one [`LobbySearch`] may hold.
pub const MAX_FILTERS: usize = 8;

/// Whether `key` is a valid metadata key: 1 to [`MAX_META_KEY_BYTES`] bytes of ASCII letters,
/// digits, `_`, `.`, `:` and `-` (`"mode"`, `"map.name"`).
pub fn is_valid_meta_key(key: &str) -> bool {
    (1..=MAX_META_KEY_BYTES).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// What is wrong with a metadata value: `None` if fine (at most [`MAX_META_VALUE_CHARS`]
/// characters, the name rules of [`crate::text::name_problem`]: no control or invisible
/// characters; empty is fine).
pub fn meta_value_problem(value: &str) -> Option<String> {
    if value.chars().count() > MAX_META_VALUE_CHARS {
        return Some(format!("is longer than {MAX_META_VALUE_CHARS} characters"));
    }
    crate::text::name_problem(value).map(str::to_string)
}

fn check_metadata<'a>(details: &mut ValidationDetails, entries: impl Iterator<Item = (&'a String, Option<&'a String>)>) {
    for (key, value) in entries {
        if !is_valid_meta_key(key) {
            details.add("metadata", format!("`{key}` is not a valid key (1 to {MAX_META_KEY_BYTES} bytes of letters, digits, _ . : -)"));
        } else if let Some(problem) = value.and_then(|v| meta_value_problem(v)) {
            details.add("metadata", format!("`{key}` {problem}"));
        }
    }
}

// ---- join codes ---------------------------------------------------------------------------------

/// A lobby's join code: [`LOBBY_CODE_LEN`] characters of [`FRIEND_CODE_ALPHABET`], in upper case.
/// Each character is 5 bits, so a code is also a number below 2^40.
///
/// JSON: the text, `"K7M2Q9XD"`.
///
/// ```
/// use net_backend_protocol::lobbies::LobbyCode;
///
/// let code = LobbyCode::parse("k7m2-q9xd").expect("a code");
/// assert_eq!(code.as_str(), "K7M2Q9XD");
/// assert_eq!(code.grouped(), "K7M2-Q9XD");
/// let number = code.to_u64();
/// assert!(number < 1 << 40);
/// assert_eq!(LobbyCode::from_u64(number), Some(code));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LobbyCode(String);

impl LobbyCode {
    /// A typed-in code: spaces and dashes removed, upper case, then exactly [`LOBBY_CODE_LEN`]
    /// characters of [`FRIEND_CODE_ALPHABET`]; `None` if it cannot be a code.
    pub fn parse(input: &str) -> Option<Self> {
        crate::friends::normalize_friend_code(input).map(Self)
    }

    /// The code from its number ([`to_u64`](Self::to_u64)); `None` for 2^40 and above.
    pub fn from_u64(number: u64) -> Option<Self> {
        if number >= 1 << 40 {
            return None;
        }
        let code = (0..LOBBY_CODE_LEN)
            .rev()
            .map(|place| {
                let digit = (number >> (5 * place)) & 31;
                char::from(FRIEND_CODE_ALPHABET[digit as usize])
            })
            .collect();
        Some(Self(code))
    }

    /// The code as a number below 2^40: each character's place in [`FRIEND_CODE_ALPHABET`] is 5
    /// bits, the first character the highest. Servers never hand out the code of the number 0
    /// (`22222222`).
    pub fn to_u64(&self) -> u64 {
        self.0.bytes().fold(0u64, |acc, b| (acc << 5) | FRIEND_CODE_ALPHABET.iter().position(|a| *a == b).unwrap_or(0) as u64)
    }

    /// The code: `K7M2Q9XD`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The code in two groups of four for people to read: `K7M2-Q9XD` ([`parse`](Self::parse)
    /// reads it back).
    pub fn grouped(&self) -> String {
        format!("{}-{}", &self.0[..4], &self.0[4..])
    }
}

impl fmt::Display for LobbyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for LobbyCode {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value).ok_or_else(|| format!("not a lobby code: {value:?}"))
    }
}

impl From<LobbyCode> for String {
    fn from(code: LobbyCode) -> Self {
        code.0
    }
}

// ---- lobbies ------------------------------------------------------------------------------------

/// Who finds and joins a lobby.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LobbyVisibility {
    /// Listed by [`LobbySearch`]; joined by id or code.
    #[default]
    Public,
    /// Not listed; joined with the join code only.
    Private,
    /// Listed to the host's friends ([`LobbySearch::friends`]); joined with the code, or by id by
    /// a friend of the host. Needs the friends module on the server.
    Friends,
    /// A visibility from a newer server or client this version does not know (refused).
    #[serde(other)]
    Unknown,
}

/// Where a lobby is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LobbyState {
    /// Players may join.
    #[default]
    Open,
    /// The match runs: no one joins, the members stay (back to `open` resets every ready flag).
    InGame,
    /// Final: the lobby is removed (only ever seen in the last `lobby.changed`).
    Closed,
    /// A state from a newer server or client this version does not know (refused).
    #[serde(other)]
    Unknown,
}

/// One member of a lobby.
///
/// JSON: `{"user":42,"name":"Ada","ready":true,"joined_at":1790000000000}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LobbyMember {
    /// The member.
    pub user: UserId,
    /// Their display name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Their ready flag.
    #[serde(default)]
    pub ready: bool,
    /// When they joined.
    pub joined_at: UnixMillis,
}

impl LobbyMember {
    /// A member, not ready, without a name.
    pub fn new(user: UserId, joined_at: UnixMillis) -> Self {
        Self { user, name: None, ready: false, joined_at }
    }

    /// The same member with a display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same member, ready or not.
    pub fn with_ready(mut self, ready: bool) -> Self {
        self.ready = ready;
        self
    }
}

/// A lobby.
///
/// JSON: `{"id":7,"visibility":"public","state":"open","host":42,"max_players":4,"members":2,"metadata":{"mode":"ranked"},"created_at":1790000000000,"code":"K7M2Q9XD","code_number":590122524587,"chat_room":31,"players":[LobbyMember]}`.
/// `code`, `code_number` and `chat_room` are shown to members only; `players` is filled in the
/// answers about one lobby and empty (absent) in search results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LobbyInfo {
    /// The id.
    pub id: LobbyId,
    /// Who finds and joins it.
    pub visibility: LobbyVisibility,
    /// Where it is in its life.
    pub state: LobbyState,
    /// The host (absent only while the server hands the lobby on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<UserId>,
    /// The most members.
    pub max_players: u32,
    /// How many members it has.
    pub members: u32,
    /// The game's metadata.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// When it was created.
    pub created_at: UnixMillis,
    /// The join code (members only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<LobbyCode>,
    /// The join code as a number below 2^40 ([`LobbyCode::to_u64`]; members only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_number: Option<u64>,
    /// The lobby's chat room (members only), when the server runs the chat module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_room: Option<RoomId>,
    /// The members, in the order they joined (in answers about one lobby).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub players: Vec<LobbyMember>,
}

impl LobbyInfo {
    /// A lobby without host, metadata, code, chat room or member list.
    pub fn new(id: LobbyId, visibility: LobbyVisibility, state: LobbyState, max_players: u32, members: u32, created_at: UnixMillis) -> Self {
        Self {
            id,
            visibility,
            state,
            host: None,
            max_players,
            members,
            metadata: BTreeMap::new(),
            created_at,
            code: None,
            code_number: None,
            chat_room: None,
            players: Vec::new(),
        }
    }

    /// The same lobby with its host.
    pub fn with_host(mut self, host: UserId) -> Self {
        self.host = Some(host);
        self
    }

    /// The same lobby with metadata.
    pub fn with_metadata(mut self, metadata: BTreeMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    /// The same lobby with its join code (both forms).
    pub fn with_code(mut self, code: LobbyCode) -> Self {
        self.code_number = Some(code.to_u64());
        self.code = Some(code);
        self
    }

    /// The same lobby with its chat room.
    pub fn with_chat_room(mut self, room: RoomId) -> Self {
        self.chat_room = Some(room);
        self
    }

    /// The same lobby with its member list.
    pub fn with_players(mut self, players: Vec<LobbyMember>) -> Self {
        self.players = players;
        self
    }
}

/// The caller's lobbies ([`MyLobbies`]).
///
/// JSON: `{"lobbies":[LobbyInfo]}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LobbyList {
    /// The lobbies, with their members, oldest membership first.
    pub lobbies: Vec<LobbyInfo>,
}

impl LobbyList {
    /// A list.
    pub fn new(lobbies: Vec<LobbyInfo>) -> Self {
        Self { lobbies }
    }
}

/// Create a lobby: `POST /v1/lobbies` → [`LobbyInfo`]. The caller hosts it.
///
/// JSON: `{"visibility":"public","max_players":4,"metadata":{"mode":"ranked","map":"dust"}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CreateLobby {
    /// Who finds and joins it (default `public`).
    #[serde(default)]
    pub visibility: LobbyVisibility,
    /// The most members (at least 1; the server sets the highest allowed).
    pub max_players: u32,
    /// The game's metadata.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl CreateLobby {
    /// A public lobby for at most `max_players`.
    pub fn new(max_players: u32) -> Self {
        Self { visibility: LobbyVisibility::Public, max_players, metadata: BTreeMap::new() }
    }

    /// The same request with this visibility.
    pub fn with_visibility(mut self, visibility: LobbyVisibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// The same request with one more metadata entry.
    pub fn with_meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// The shape rules (the size and metadata limits are the server's).
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if self.visibility == LobbyVisibility::Unknown {
            details.add("visibility", "must be public, private or friends");
        }
        if self.max_players == 0 {
            details.add("max_players", "must be at least 1");
        }
        check_metadata(&mut details, self.metadata.iter().map(|(k, v)| (k, Some(v))));
        details.into_result()
    }
}

/// Change a lobby (host): the body of `PATCH /v1/lobbies/{lobby}`. Absent fields stay; in
/// `metadata` a text sets the key and `null` removes it (other keys stay).
///
/// JSON: `{"state":"in_game","metadata":{"map":"dust","password":null}}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UpdateLobby {
    /// A new visibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<LobbyVisibility>,
    /// A new size (not below the current member count).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_players: Option<u32>,
    /// A new state (`closed` removes the lobby).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<LobbyState>,
    /// Metadata changes: a text sets, `null` removes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Option<String>>,
}

impl UpdateLobby {
    /// A change of nothing (add fields).
    pub fn new() -> Self {
        Self::default()
    }

    /// The same change with a new visibility.
    pub fn with_visibility(mut self, visibility: LobbyVisibility) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// The same change with a new size.
    pub fn with_max_players(mut self, max_players: u32) -> Self {
        self.max_players = Some(max_players);
        self
    }

    /// The same change with a new state.
    pub fn with_state(mut self, state: LobbyState) -> Self {
        self.state = Some(state);
        self
    }

    /// The same change setting a metadata key.
    pub fn set_meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), Some(value.into()));
        self
    }

    /// The same change removing a metadata key.
    pub fn remove_meta(mut self, key: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), None);
        self
    }

    /// Whether it changes nothing.
    pub fn is_empty(&self) -> bool {
        self.visibility.is_none() && self.max_players.is_none() && self.state.is_none() && self.metadata.is_empty()
    }

    /// The shape rules.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if self.visibility == Some(LobbyVisibility::Unknown) {
            details.add("visibility", "must be public, private or friends");
        }
        if self.max_players == Some(0) {
            details.add("max_players", "must be at least 1");
        }
        if self.state == Some(LobbyState::Unknown) {
            details.add("state", "must be open, in_game or closed");
        }
        check_metadata(&mut details, self.metadata.iter().map(|(k, v)| (k, v.as_ref())));
        details.into_result()
    }
}

/// A ready flag, the body of `PUT /v1/lobbies/{lobby}/ready`.
///
/// JSON: `{"ready":true}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetReady {
    /// Ready or not.
    pub ready: bool,
}

impl SetReady {
    /// Ready (`true`) or not.
    pub fn new(ready: bool) -> Self {
        Self { ready }
    }
}

/// A player, the body of `POST /v1/lobbies/{lobby}/host`.
///
/// JSON: `{"user":42}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LobbyPlayer {
    /// The player.
    pub user: UserId,
}

impl LobbyPlayer {
    /// The player `user`.
    pub fn new(user: UserId) -> Self {
        Self { user }
    }
}

/// Join a lobby with its join code: `POST /v1/lobbies/join` → [`LobbyInfo`] (case, spaces and
/// dashes do not matter).
///
/// JSON: `{"code":"K7M2-Q9XD"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JoinLobbyByCode {
    /// The code ([`LobbyCode::parse`]).
    pub code: String,
}

impl JoinLobbyByCode {
    /// Join with `code`.
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }

    /// Join with the code of this number ([`LobbyCode::from_u64`]); `None` for 2^40 and above.
    pub fn from_number(number: u64) -> Option<Self> {
        LobbyCode::from_u64(number).map(|code| Self { code: code.0 })
    }

    /// The shape rule: a code [`LobbyCode::parse`] accepts.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if LobbyCode::parse(&self.code).is_none() {
            details.add("code", format!("must be {LOBBY_CODE_LEN} characters of {}", String::from_utf8_lossy(FRIEND_CODE_ALPHABET)));
        }
        details.into_result()
    }
}

/// One metadata filter: the lobby's `key` has exactly `value`.
///
/// JSON: `{"key":"mode","value":"ranked"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LobbyFilter {
    /// The metadata key.
    pub key: String,
    /// The value it must have.
    pub value: String,
}

impl LobbyFilter {
    /// `key` = `value`.
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self { key: key.into(), value: value.into() }
    }
}

/// Search open lobbies: `POST /v1/lobbies/search` → [`Page`]`<`[`LobbyInfo`]`>` (newest first).
/// Every filter must match; full lobbies are left out unless `include_full`.
///
/// JSON: `{"filters":[{"key":"mode","value":"ranked"}],"limit":20}` (every field optional).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LobbySearch {
    /// Metadata filters (at most [`MAX_FILTERS`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<LobbyFilter>,
    /// The lobbies the caller's friends host (public and friends-only) instead of every public
    /// lobby. Needs the friends module on the server.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub friends: bool,
    /// Include full lobbies.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_full: bool,
    /// Where to continue (`next_cursor` of the previous page).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// At most this many (default 50, at most 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl LobbySearch {
    /// Every open public lobby with room, newest first.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same search with one more filter.
    pub fn with_filter(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.filters.push(LobbyFilter::new(key, value));
        self
    }

    /// The same search among the lobbies of the caller's friends.
    pub fn of_friends(mut self) -> Self {
        self.friends = true;
        self
    }

    /// The same search including full lobbies.
    pub fn including_full(mut self) -> Self {
        self.include_full = true;
        self
    }

    /// The page after `cursor`.
    pub fn after(mut self, cursor: Cursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// The same search with this limit.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The page part.
    pub fn page(&self) -> PageRequest {
        PageRequest { cursor: self.cursor.clone(), limit: self.limit }
    }

    /// The shape rules: at most [`MAX_FILTERS`] filters with valid keys and values; the page.
    pub fn validate(&self) -> Result<(), ApiError> {
        self.page().validate()?;
        let mut details = ValidationDetails::new();
        if self.filters.len() > MAX_FILTERS {
            details.add("filters", format!("must hold at most {MAX_FILTERS} filters"));
        }
        for filter in &self.filters {
            if !is_valid_meta_key(&filter.key) {
                details.add("filters", format!("`{}` is not a valid key", filter.key));
            } else if let Some(problem) = meta_value_problem(&filter.value) {
                details.add("filters", format!("`{}` {problem}", filter.key));
            }
        }
        details.into_result()
    }
}

// ---- pushes -------------------------------------------------------------------------------------

/// What happened to a member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MemberChange {
    /// The member joined.
    Joined,
    /// The member left (or its account or connection went).
    Left,
    /// The host removed the member.
    Kicked,
    /// The member's ready flag changed.
    Ready,
    /// A change from a newer server this version does not know.
    #[serde(other)]
    Unknown,
}

/// Push `lobby.member`: a member joined, left, was kicked or changed its ready flag. To every
/// member of the lobby (a leaving or kicked member gets it too).
///
/// JSON: `{"lobby":7,"change":"ready","member":{"user":42,"name":"Ada","ready":true,"joined_at":1790000000000}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LobbyMemberUpdate {
    /// The lobby.
    pub lobby: LobbyId,
    /// What happened.
    pub change: MemberChange,
    /// The member (as it is now).
    pub member: LobbyMember,
}

impl LobbyMemberUpdate {
    /// A change.
    pub fn new(lobby: LobbyId, change: MemberChange, member: LobbyMember) -> Self {
        Self { lobby, change, member }
    }
}

impl ServerPush for LobbyMemberUpdate {
    const KIND: &'static str = kinds::LOBBY_MEMBER;
}

/// What changed in a lobby.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LobbyChange {
    /// Another member hosts.
    Host,
    /// The metadata.
    Metadata,
    /// The visibility or the size.
    Settings,
    /// The state (`closed`: the lobby is gone).
    State,
    /// A new join code.
    Code,
    /// A change from a newer server this version does not know.
    #[serde(other)]
    Unknown,
}

/// Push `lobby.changed`: the host, metadata, settings, state or join code changed. To every member,
/// with the lobby as it is now (the member view, without the member list).
///
/// JSON: `{"changes":["state"],"lobby":LobbyInfo}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LobbyUpdate {
    /// What changed.
    pub changes: Vec<LobbyChange>,
    /// The lobby now.
    pub lobby: LobbyInfo,
}

impl LobbyUpdate {
    /// A change.
    pub fn new(changes: Vec<LobbyChange>, lobby: LobbyInfo) -> Self {
        Self { changes, lobby }
    }
}

impl ServerPush for LobbyUpdate {
    const KIND: &'static str = kinds::LOBBY_CHANGED;
}

// ---- typed HTTP calls (see `http_call`) ---------------------------------------------------------

/// The typed HTTP calls of this module (in their own scope: their imports stay out of the
/// module's doc-link scope).
mod calls {
    use super::*;

    use crate::http_call::{payload_call, HttpCall, NoPayload, PathParams, PayloadKind, NO_PAYLOAD};
    use crate::routes::{self, HttpMethod, Route};

    payload_call!(CreateLobby, Post, routes::lobbies::LIST, true, Json, LobbyInfo);
    payload_call!(LobbySearch, Post, routes::lobbies::SEARCH, true, Json, Page<LobbyInfo>);
    payload_call!(JoinLobbyByCode, Post, routes::lobbies::JOIN_CODE, true, Json, LobbyInfo);

    /// The caller's lobbies: `GET /v1/lobbies/mine` → [`LobbyList`].
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct MyLobbies {}

    impl MyLobbies {
        /// The call.
        pub fn new() -> Self {
            Self {}
        }
    }

    impl HttpCall for MyLobbies {
        type Payload = NoPayload;
        type Response = LobbyList;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::lobbies::MINE, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn from_parts(_params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            Ok(Self::new())
        }
    }

    /// A call naming one lobby in the path, without a payload.
    macro_rules! lobby_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {
                /// The lobby.
                pub lobby: LobbyId,
            }

            impl $name {
                /// The call for `lobby`.
                pub fn new(lobby: LobbyId) -> Self {
                    Self { lobby }
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
                    PathParams::new().with("lobby", self.lobby)
                }

                fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
                    Ok(Self::new(params.id("lobby")?))
                }
            }
        };
    }

    lobby_call!(
        /// One lobby with its members: `GET /v1/lobbies/{lobby}` → [`LobbyInfo`] (404 for a lobby the
        /// caller may not see: a private one it is not in, a friends-only one of a stranger).
        GetLobby,
        Get,
        routes::lobbies::ONE,
        LobbyInfo
    );
    lobby_call!(
        /// Join a lobby by id: `POST /v1/lobbies/{lobby}/join` → [`LobbyInfo`] (a public lobby, or a
        /// friends-only one of a friend; a member already: the lobby again).
        JoinLobby,
        Post,
        routes::lobbies::JOIN,
        LobbyInfo
    );
    lobby_call!(
        /// Leave a lobby: `POST /v1/lobbies/{lobby}/leave` → [`Ack`] (also when not a member). A
        /// leaving host hands the lobby to the member who joined first; the last member's leaving
        /// removes it.
        LeaveLobby,
        Post,
        routes::lobbies::LEAVE,
        Ack
    );
    lobby_call!(
        /// A new join code (host): `POST /v1/lobbies/{lobby}/code` → [`LobbyInfo`]; the old code stops
        /// working.
        NewLobbyCode,
        Post,
        routes::lobbies::CODE,
        LobbyInfo
    );

    /// A call naming one lobby in the path, with a JSON body.
    macro_rules! lobby_body_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $body:ident, $field:ident, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Debug, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {
                /// The lobby.
                pub lobby: LobbyId,
                /// The body.
                pub $field: $body,
            }

            impl $name {
                /// The call for `lobby` with this body.
                pub fn new(lobby: LobbyId, $field: $body) -> Self {
                    Self { lobby, $field }
                }
            }

            impl HttpCall for $name {
                type Payload = $body;
                type Response = $response;
                const ROUTE: Route = Route::new(HttpMethod::$method, $path, true);
                const PAYLOAD: PayloadKind = PayloadKind::Json;

                fn payload(&self) -> &$body {
                    &self.$field
                }

                fn path_params(&self) -> PathParams {
                    PathParams::new().with("lobby", self.lobby)
                }

                fn from_parts(params: &PathParams, $field: $body) -> Result<Self, ApiError> {
                    Ok(Self::new(params.id("lobby")?, $field))
                }
            }
        };
    }

    lobby_body_call!(
        /// Change a lobby (host): `PATCH /v1/lobbies/{lobby}` with an [`UpdateLobby`] → [`LobbyInfo`].
        EditLobby,
        Patch,
        routes::lobbies::ONE,
        UpdateLobby,
        update,
        LobbyInfo
    );
    lobby_body_call!(
        /// Set the caller's ready flag: `PUT /v1/lobbies/{lobby}/ready` with a [`SetReady`] → [`Ack`].
        SetLobbyReady,
        Put,
        routes::lobbies::READY,
        SetReady,
        ready,
        Ack
    );
    lobby_body_call!(
        /// Hand the lobby to another member (host): `POST /v1/lobbies/{lobby}/host` with a
        /// [`LobbyPlayer`] → [`Ack`].
        TransferLobby,
        Post,
        routes::lobbies::HOST,
        LobbyPlayer,
        to,
        Ack
    );

    /// Remove a member (host): `DELETE /v1/lobbies/{lobby}/members/{user}` → [`Ack`] (also when the
    /// player is no member).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct KickFromLobby {
        /// The lobby.
        pub lobby: LobbyId,
        /// The member.
        pub user: UserId,
    }

    impl KickFromLobby {
        /// Remove `user` from `lobby`.
        pub fn new(lobby: LobbyId, user: UserId) -> Self {
            Self { lobby, user }
        }
    }

    impl HttpCall for KickFromLobby {
        type Payload = NoPayload;
        type Response = Ack;
        const ROUTE: Route = Route::new(HttpMethod::Delete, routes::lobbies::MEMBER, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("lobby", self.lobby).with("user", self.user)
        }

        fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("lobby")?, params.id("user")?))
        }
    }
}

pub use calls::{EditLobby, GetLobby, JoinLobby, KickFromLobby, LeaveLobby, MyLobbies, NewLobbyCode, SetLobbyReady, TransferLobby};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_numbers() {
        assert_eq!(LobbyCode::from_u64(0).map(|c| c.0).as_deref(), Some("22222222"));
        assert_eq!(LobbyCode::from_u64((1 << 40) - 1).map(|c| c.0).as_deref(), Some("ZZZZZZZZ"));
        assert_eq!(LobbyCode::from_u64(1 << 40), None);
        assert_eq!(LobbyCode::parse("ZZZZ-ZZZZ").map(|c| c.to_u64()), Some((1 << 40) - 1));
        assert_eq!(LobbyCode::parse("22222223").map(|c| c.to_u64()), Some(1));
        assert_eq!(LobbyCode::parse("K7M2Q9XD").map(|c| c.to_u64()), Some(590_122_524_587), "the documented example");
        for number in [1u64, 31, 32, 683_102_947_653, (1 << 40) - 2] {
            let code = LobbyCode::from_u64(number).expect("code");
            assert_eq!(code.to_u64(), number);
            assert_eq!(LobbyCode::parse(&code.grouped()), Some(code));
        }
        assert_eq!(LobbyCode::parse("K7M2Q9X0"), None);
        assert_eq!(serde_json::to_string(&LobbyCode::parse("k7m2q9xd")).ok().as_deref(), Some(r#""K7M2Q9XD""#));
        assert!(serde_json::from_str::<LobbyCode>(r#""nope""#).is_err());
        assert_eq!(JoinLobbyByCode::from_number(1).map(|j| j.code).as_deref(), Some("22222223"));
        assert!(JoinLobbyByCode::new("k7m2-q9xd").validate().is_ok() && JoinLobbyByCode::new("k7m2").validate().is_err());
    }

    #[test]
    fn json_and_rules() {
        assert!(is_valid_meta_key("map.name") && is_valid_meta_key("Mode:2") && !is_valid_meta_key("") && !is_valid_meta_key("a b"));
        assert!(!is_valid_meta_key(&"k".repeat(MAX_META_KEY_BYTES + 1)));
        assert!(meta_value_problem("").is_none() && meta_value_problem(&"é".repeat(MAX_META_VALUE_CHARS + 1)).is_some());
        assert!(meta_value_problem("a\u{202E}b").is_some());
        let create = CreateLobby::new(4).with_meta("mode", "ranked");
        assert_eq!(serde_json::to_string(&create).ok().as_deref(), Some(r#"{"visibility":"public","max_players":4,"metadata":{"mode":"ranked"}}"#));
        assert!(create.validate().is_ok() && CreateLobby::new(0).validate().is_err());
        assert!(CreateLobby::new(2).with_meta("bad key", "x").validate().is_err());
        assert!(serde_json::from_str::<CreateLobby>(r#"{"max_players":2,"visibility":"secret"}"#).map(|c| c.validate().is_err()).unwrap_or(false));
        let update: UpdateLobby = serde_json::from_str(r#"{"state":"in_game","metadata":{"map":"dust","password":null}}"#).expect("update");
        assert_eq!(update.state, Some(LobbyState::InGame));
        assert_eq!(update.metadata.get("password"), Some(&None));
        assert!(update.validate().is_ok() && !update.is_empty() && UpdateLobby::new().is_empty());
        assert_eq!(serde_json::to_string(&UpdateLobby::new().remove_meta("x")).ok().as_deref(), Some(r#"{"metadata":{"x":null}}"#));
        let info = LobbyInfo::new(LobbyId(7), LobbyVisibility::Friends, LobbyState::InGame, 4, 1, UnixMillis(1))
            .with_host(UserId(42))
            .with_code(LobbyCode::from_u64(1).expect("code"));
        assert_eq!(
            serde_json::to_string(&info).ok().as_deref(),
            Some(r#"{"id":7,"visibility":"friends","state":"in_game","host":42,"max_players":4,"members":1,"created_at":1,"code":"22222223","code_number":1}"#)
        );
        let search = LobbySearch::new().with_filter("mode", "ranked").including_full();
        assert_eq!(serde_json::to_string(&search).ok().as_deref(), Some(r#"{"filters":[{"key":"mode","value":"ranked"}],"include_full":true}"#));
        assert!(search.validate().is_ok());
        let too_many = (0..=MAX_FILTERS).fold(LobbySearch::new(), |s, n| s.with_filter(format!("k{n}"), "v"));
        assert!(too_many.validate().is_err());
        assert_eq!(serde_json::from_str::<MemberChange>(r#""promoted""#).ok(), Some(MemberChange::Unknown));
        let push = LobbyMemberUpdate::new(LobbyId(7), MemberChange::Ready, LobbyMember::new(UserId(42), UnixMillis(1)).with_ready(true));
        assert_eq!(serde_json::to_string(&push).ok().as_deref(), Some(r#"{"lobby":7,"change":"ready","member":{"user":42,"ready":true,"joined_at":1}}"#));
    }
}
