//! Chat: rooms, direct messages, sending and history. Joining, leaving, sending and live messages
//! go over the WebSocket ([`kinds`] `chat.*`); the room list, history pages and
//! opening a direct-message room are also HTTP routes ([`routes::chat`](crate::routes::chat)).
//!
//! | Kind | Request → answer |
//! |---|---|
//! | `chat.join` | [`JoinRoom`] → [`RoomInfo`] |
//! | `chat.leave` | [`LeaveRoom`] → [`Ack`] |
//! | `chat.send` | [`SendMessage`] → [`SendAck`] |
//! | `chat.history` | [`ChatHistory`] → [`Page`]`<`[`ChatMessage`]`>` (newest first) |
//! | push `chat.message` | [`ChatMessage`], to every member of the room, the sender included |
//! | push `chat.deleted` | [`MessageDeleted`] (moderation) |
//! | `chat.members` | [`ListMembers`] → [`RoomMembers`] (who is online) |
//! | push `chat.presence` | [`Presence`] (a user came online in a room or left; best effort) |
//! | `chat.edit` | [`EditMessage`] → [`ChatMessage`] (the edited message) |
//! | push `chat.edited` | [`MessageEdited`] |
//! | `chat.mark_read` | [`MarkRead`] → [`Ack`] (the caller's read marker) |
//! | push `chat.read` | [`ReadReceipt`] (a member's read marker moved; coalesced) |
//! | `chat.receipts` | [`ListReceipts`] → [`ReadReceipts`] |
//! | `chat.unread` | [`UnreadQuery`] → [`UnreadCounts`] |
//! | `chat.set_typing` | [`SetTyping`] → [`Ack`] (nothing stored) |
//! | push `chat.typing` | [`TypingUpdate`] (throttled; expires on the client) |
//! | push `chat.room` | [`RoomUpdate`] (a player room changed: name, members, roles, deletion) |
//!
//! HTTP ([`HttpCall`](crate::HttpCall) types): [`ListRooms`], [`ListMessages`], [`OpenDirect`], [`ListDirects`],
//! [`DeleteMessage`], [`EditMessage`], [`MarkRead`], [`ListReceipts`], [`UnreadQuery`]; player rooms:
//! [`CreateRoom`], [`MyRooms`], [`PublicRooms`], [`GetRoom`], [`EditRoom`], [`DeleteRoom`],
//! [`JoinChatRoom`], [`LeaveChatRoom`], [`ListRoomMembers`], [`InviteToRoom`], [`KickFromRoom`],
//! [`SetRoomRole`], [`TransferRoom`].
//!
//! **Player rooms** ([`RoomKind::Player`]): created by a player ([`CreateRoom`]), who owns it; the
//! owner names moderators ([`RoomRole`]); public rooms are listed and open to anyone not banned,
//! private rooms take invited players only. Membership is stored (it outlives connections):
//! [`JoinChatRoom`] or a `chat.join` makes the caller a member (accepting an invitation),
//! [`LeaveChatRoom`] ends it. A kicked player is banned until invited again. Every change is a
//! `chat.room` push ([`RoomUpdate`]) to the members.
//!
//! **Editing:** a sender changes its message within the server's edit window; staff with the
//! moderation permission change any. The history shows the latest text with
//! [`ChatMessage::edited_at`].
//!
//! **Read markers and typing:** [`MarkRead`] stores "read up to message X" per user and room (it
//! only moves forward) and feeds [`UnreadQuery`]; DM, group and player rooms share the markers
//! ([`ReadReceipt`] pushes, [`ListReceipts`]). [`SetTyping`] is never stored: the server throttles
//! the pushes and a client lets an indicator expire after [`TypingUpdate::expires_in_ms`].
//!
//! **Public and group rooms:** membership lasts as long as the WebSocket connection: after a
//! reconnect, join again.
//!
//! **Direct messages** need no join: a `chat.send` to a DM room the caller belongs to is accepted
//! on any connection, and its `chat.message` push goes to EVERY open connection of both members.
//! A client learns about a new DM from that push (its `room`); [`routes::chat::DMS`](crate::routes::chat::DMS)
//! lists the caller's DM rooms with their [`RoomInfo::peer`].
//!
//! **Echo and order:** the sender also receives its own `chat.message` (it carries the final text
//! after the server's hooks and reaches the sender's other devices). On the sending socket the
//! `SendAck` answer is sent BEFORE that push; games dedupe by `message_id` (and may tag a send with
//! a [`SendMessage::nonce`], echoed in the push, to match a send whose answer was lost).
//!
//! **Presence:** a room's members get `chat.presence` when a user's first connection joins the
//! room and when its last one leaves (leave or disconnect), with the room's online `count`. It is
//! best effort: rooms with more online users than the server's presence cap
//! ([`DEFAULT_PRESENCE_MAX_MEMBERS`]) get none, and a burst beyond the per-room rate
//! ([`DEFAULT_PRESENCE_PER_SECOND`]) is not pushed; `chat.members` always answers the current list.
//! Presence is per server instance.
//!
//! **Text rules** ([`SendMessage::validate`], [`crate::text`]): not blank, at most
//! [`DEFAULT_MAX_TEXT_CHARS`], line breaks and tabs allowed, other control characters and
//! invisible / direction-changing characters refused (zero-width joiners allowed for emoji).
//!
//! The limits below are the server's defaults; a server may configure others, and its hooks decide who
//! may join and what may be said. Public ("world") rooms are capped
//! ([`DEFAULT_MAX_ROOM_MEMBERS`]): one huge room multiplies every message by its member count.

use serde::{Deserialize, Serialize};

use crate::envelope::{Ack, ServerPush, WsCall};
use crate::error::{ApiError, ValidationDetails};
use crate::ids::{MessageId, RoomId, UserId};
use crate::kinds;
use crate::page::{Page, PageRequest};
use crate::text;
use crate::time::UnixMillis;

/// The default longest message, in characters (Unicode scalar values).
pub const DEFAULT_MAX_TEXT_CHARS: usize = 500;
/// The default rate limit, a token bucket: a burst of this many messages per user …
pub const DEFAULT_RATE_MESSAGES: u32 = 5;
/// … refilled over this many seconds (one message every `10 / 5 = 2` s after a burst).
pub const DEFAULT_RATE_WINDOW_SECS: u32 = 10;
/// The default history retention, in days.
pub const DEFAULT_HISTORY_RETENTION_DAYS: u32 = 30;
/// The default member cap of a public room.
pub const DEFAULT_MAX_ROOM_MEMBERS: u32 = 200;
/// The default number of rooms one connection may have joined at a time.
pub const DEFAULT_MAX_JOINED_ROOMS: u32 = 16;
/// The longest room key, in bytes.
pub const MAX_ROOM_KEY_BYTES: usize = 64;
/// The longest [`SendMessage::nonce`], in bytes.
pub const MAX_NONCE_BYTES: usize = 64;

/// Whether `key` is a valid public room key: 1 to [`MAX_ROOM_KEY_BYTES`] bytes of ASCII lower-case
/// letters, digits, `_`, `-` and `.`, starting with a letter or digit (`"world"`, `"trade-eu"`).
pub fn is_valid_room_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() <= MAX_ROOM_KEY_BYTES
        && bytes.first().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'))
}

/// Which room: by id (`12`) or, for public rooms, by key (`"world"`).
///
/// JSON: a number or a string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum RoomRef {
    /// The room with this id.
    Id(RoomId),
    /// The public room with this key.
    Key(String),
}

impl From<RoomId> for RoomRef {
    fn from(id: RoomId) -> Self {
        RoomRef::Id(id)
    }
}

impl From<&str> for RoomRef {
    fn from(key: &str) -> Self {
        RoomRef::Key(key.to_string())
    }
}

impl From<String> for RoomRef {
    fn from(key: String) -> Self {
        RoomRef::Key(key)
    }
}

/// What kind of room.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RoomKind {
    /// A public room (configured on the server, joined by key or id).
    Room,
    /// A direct-message room between two users.
    Dm,
    /// A group's room.
    Group,
    /// A room a player created: an owner, moderators and members; public (anyone may become a
    /// member) or private (by invitation). See [`CreateRoom`].
    Player,
    /// A kind from a newer server this version does not know (never sent by a server).
    #[serde(other)]
    Unknown,
}

/// A room.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoomInfo {
    /// The id.
    pub id: RoomId,
    /// What kind of room.
    pub kind: RoomKind,
    /// The key of a public room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// A display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The number of members connected right now, if the server shares it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_count: Option<u32>,
    /// The member cap, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_members: Option<u32>,
    /// For a direct-message room: the other member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<UserId>,
    /// For a player room: public or private.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<RoomVisibility>,
    /// For a player room: its owner (absent while the room has none, e.g. after the owner's
    /// account was deleted and before the server named a new one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<UserId>,
    /// For a player room: the caller's role in it (absent: no member, no invitation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<RoomRole>,
}

impl RoomInfo {
    /// A room with no key, name or counts.
    pub fn new(id: RoomId, kind: RoomKind) -> Self {
        Self { id, kind, key: None, name: None, member_count: None, max_members: None, peer: None, visibility: None, owner: None, role: None }
    }

    /// The same room with this visibility (player rooms).
    pub fn with_visibility(mut self, visibility: RoomVisibility) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// The same room with this owner (player rooms).
    pub fn with_owner(mut self, owner: UserId) -> Self {
        self.owner = Some(owner);
        self
    }

    /// The same room with the caller's role (player rooms).
    pub fn with_role(mut self, role: RoomRole) -> Self {
        self.role = Some(role);
        self
    }

    /// The same room with this direct-message peer.
    pub fn with_peer(mut self, peer: UserId) -> Self {
        self.peer = Some(peer);
        self
    }

    /// The same room with this key.
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// The same room with this name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same room with these counts.
    pub fn with_members(mut self, member_count: Option<u32>, max_members: Option<u32>) -> Self {
        self.member_count = member_count;
        self.max_members = max_members;
        self
    }
}

/// Join a room: `chat.join` → [`RoomInfo`]. Joining a room already joined is not an error.
///
/// JSON: `{"room":"world"}` or `{"room":12}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JoinRoom {
    /// The room.
    pub room: RoomRef,
}

impl JoinRoom {
    /// Join `room` (a [`RoomId`] or a public room's key).
    pub fn new(room: impl Into<RoomRef>) -> Self {
        Self { room: room.into() }
    }

    /// The shape rule: a key is a valid room key ([`is_valid_room_key`]).
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let RoomRef::Key(key) = &self.room {
            if !is_valid_room_key(key) {
                details.add("room", format!("is not a room key (1 to {MAX_ROOM_KEY_BYTES} bytes of a-z, 0-9, _ - .)"));
            }
        }
        details.into_result()
    }
}

impl WsCall for JoinRoom {
    type Response = RoomInfo;
    const KIND: &'static str = kinds::CHAT_JOIN;
}

/// Leave a room: `chat.leave` → [`Ack`]. Leaving a room not joined is not an error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LeaveRoom {
    /// The room.
    pub room: RoomId,
}

impl LeaveRoom {
    /// Leave `room`.
    pub fn new(room: RoomId) -> Self {
        Self { room }
    }
}

impl WsCall for LeaveRoom {
    type Response = Ack;
    const KIND: &'static str = kinds::CHAT_LEAVE;
}

/// Send a message to a joined room (or a DM room, no join needed): `chat.send` → [`SendAck`]. Every
/// member, the sender included, then gets the `chat.message` push (the server's hooks may have
/// changed the text); the answer comes first.
///
/// JSON: `{"room":12,"text":"hello"}` (+ `"nonce":"…"` when set).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SendMessage {
    /// The room (joined).
    pub room: RoomId,
    /// The text.
    pub text: String,
    /// A client-chosen tag (at most [`MAX_NONCE_BYTES`] of visible ASCII), echoed in the
    /// `chat.message` push, so a game can match a send whose answer was lost (e.g. a disconnect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

impl SendMessage {
    /// A message.
    pub fn new(room: RoomId, text: impl Into<String>) -> Self {
        Self { room, text: text.into(), nonce: None }
    }

    /// The same message with a nonce.
    pub fn with_nonce(mut self, nonce: impl Into<String>) -> Self {
        self.nonce = Some(nonce.into());
        self
    }

    /// The shape rules: not blank, at most `max_chars` characters
    /// ([`DEFAULT_MAX_TEXT_CHARS`] unless the server configures another), no forbidden characters
    /// ([`text::message_problem`]), a nonce of 1 to [`MAX_NONCE_BYTES`] visible ASCII characters.
    pub fn validate(&self, max_chars: usize) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if self.text.trim().is_empty() {
            details.add("text", "is empty");
        }
        if self.text.chars().count() > max_chars {
            details.add("text", format!("is longer than {max_chars} characters"));
        }
        if let Some(problem) = text::message_problem(&self.text) {
            details.add("text", problem);
        }
        if let Some(nonce) = &self.nonce {
            if nonce.is_empty() || nonce.len() > MAX_NONCE_BYTES || !nonce.bytes().all(|b| b.is_ascii_graphic()) {
                details.add("nonce", format!("must be 1 to {MAX_NONCE_BYTES} visible ASCII characters"));
            }
        }
        details.into_result()
    }
}

impl WsCall for SendMessage {
    type Response = SendAck;
    const KIND: &'static str = kinds::CHAT_SEND;
}

/// The answer to [`SendMessage`]: the stored message's id and time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SendAck {
    /// The message id.
    pub message_id: MessageId,
    /// When it was stored.
    pub sent_at: UnixMillis,
}

impl SendAck {
    /// An acknowledgement.
    pub fn new(message_id: MessageId, sent_at: UnixMillis) -> Self {
        Self { message_id, sent_at }
    }
}

/// A page of a room's history, newest first: `chat.history` → [`Page`]`<`[`ChatMessage`]`>`.
/// WebSocket only: never decode it as an HTTP query (its flattened page does not survive
/// urlencoding); the HTTP route `GET /v1/chat/rooms/{room}/messages` takes the room from the path
/// and a [`PageRequest`] as its query.
///
/// JSON: `{"room":12,"cursor":"…","limit":50}` (`cursor` and `limit` optional).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChatHistory {
    /// The room.
    pub room: RoomId,
    /// Which page.
    #[serde(flatten)]
    pub page: PageRequest,
}

impl ChatHistory {
    /// The newest page of `room`.
    pub fn new(room: RoomId) -> Self {
        Self { room, page: PageRequest::first() }
    }

    /// The same request for this page.
    pub fn with_page(mut self, page: PageRequest) -> Self {
        self.page = page;
        self
    }
}

impl WsCall for ChatHistory {
    type Response = Page<ChatMessage>;
    const KIND: &'static str = kinds::CHAT_HISTORY;
}

/// A chat message: the `chat.message` push, and the items of a history page.
///
/// JSON: `{"id":981,"room":12,"sender":42,"sender_name":"Ada","text":"hello","sent_at":1790000000000}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChatMessage {
    /// The message id.
    pub id: MessageId,
    /// The room.
    pub room: RoomId,
    /// Who sent it.
    pub sender: UserId,
    /// The sender's display name at sending time, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_name: Option<String>,
    /// The text (after the server's hooks).
    pub text: String,
    /// When it was stored.
    pub sent_at: UnixMillis,
    /// The sender's [`SendMessage::nonce`], if it set one (a public or group room's push carries it
    /// to every member; a DM's push and the history show it to the sender only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// When the text was last edited (absent: never edited); `text` is the latest text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<UnixMillis>,
}

impl ChatMessage {
    /// A message.
    pub fn new(id: MessageId, room: RoomId, sender: UserId, text: impl Into<String>, sent_at: UnixMillis) -> Self {
        Self { id, room, sender, sender_name: None, text: text.into(), sent_at, nonce: None, edited_at: None }
    }

    /// The same message marked as edited at `at`.
    pub fn with_edited_at(mut self, at: UnixMillis) -> Self {
        self.edited_at = Some(at);
        self
    }

    /// The same message with the sender's nonce.
    pub fn with_nonce(mut self, nonce: impl Into<String>) -> Self {
        self.nonce = Some(nonce.into());
        self
    }

    /// The same message with the sender's name.
    pub fn with_sender_name(mut self, name: impl Into<String>) -> Self {
        self.sender_name = Some(name.into());
        self
    }
}

impl ServerPush for ChatMessage {
    const KIND: &'static str = kinds::CHAT_MESSAGE;
}

/// A message was deleted (moderation): the `chat.deleted` push.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageDeleted {
    /// The message id.
    pub id: MessageId,
    /// The room.
    pub room: RoomId,
}

impl MessageDeleted {
    /// A deletion.
    pub fn new(id: MessageId, room: RoomId) -> Self {
        Self { id, room }
    }
}

impl ServerPush for MessageDeleted {
    const KIND: &'static str = kinds::CHAT_DELETED;
}

/// Open (or find) the direct-message room with another user: `POST /v1/chat/dm` → [`RoomInfo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OpenDirect {
    /// The other user.
    pub user: UserId,
}

impl OpenDirect {
    /// A direct-message room with `user`.
    pub fn new(user: UserId) -> Self {
        Self { user }
    }
}

// ---- presence -----------------------------------------------------------------------------------

/// The default: rooms with more online users than this get no `chat.presence` pushes (one join
/// would be a push to every member); [`ListMembers`] still answers.
pub const DEFAULT_PRESENCE_MAX_MEMBERS: u32 = 100;
/// The default: at most this many `chat.presence` pushes per room and second; the rest of a burst
/// is not pushed (a client that needs the exact list asks with [`ListMembers`]).
pub const DEFAULT_PRESENCE_PER_SECOND: u32 = 10;
/// The most members one [`RoomMembers`] answer lists ([`RoomMembers::truncated`] beyond).
pub const MAX_LISTED_MEMBERS: u32 = 200;

/// Who is online in a room the caller joined: `chat.members` → [`RoomMembers`]. For a DM room the
/// answer lists only the caller (a DM never reveals whether the peer is online).
///
/// JSON: `{"room":12}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ListMembers {
    /// The room.
    pub room: RoomId,
}

impl ListMembers {
    /// The online members of `room`.
    pub fn new(room: RoomId) -> Self {
        Self { room }
    }
}

impl WsCall for ListMembers {
    type Response = RoomMembers;
    const KIND: &'static str = kinds::CHAT_MEMBERS;
}

/// One online member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoomMember {
    /// The user.
    pub user: UserId,
    /// The display name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl RoomMember {
    /// A member.
    pub fn new(user: UserId) -> Self {
        Self { user, name: None }
    }

    /// The same member with a name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// The answer to [`ListMembers`]: the users online in the room (each once, however many
/// connections they have), on this server instance.
///
/// JSON: `{"room":12,"members":[{"user":42,"name":"Ada"}],"count":1}` (+ `"truncated":true`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoomMembers {
    /// The room.
    pub room: RoomId,
    /// Up to [`MAX_LISTED_MEMBERS`] members.
    pub members: Vec<RoomMember>,
    /// How many users are online in the room.
    pub count: u32,
    /// Whether `members` is cut at [`MAX_LISTED_MEMBERS`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl RoomMembers {
    /// An answer.
    pub fn new(room: RoomId, members: Vec<RoomMember>, count: u32) -> Self {
        Self { room, members, count, truncated: false }
    }

    /// The same answer marked as cut.
    pub fn truncated(mut self) -> Self {
        self.truncated = true;
        self
    }
}

/// What happened in a [`Presence`] push.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PresenceEvent {
    /// The user's first connection joined the room.
    Joined,
    /// The user's last connection left the room (leave, disconnect).
    Left,
    /// An event from a newer server this version does not know (never sent by a server).
    #[serde(other)]
    Unknown,
}

/// A user came online in a room or went: the `chat.presence` push, to the room's members
/// (best effort: not in rooms over the server's presence cap, not beyond its per-room rate; see
/// [`DEFAULT_PRESENCE_MAX_MEMBERS`]).
///
/// JSON: `{"room":12,"user":42,"event":"joined","name":"Ada","count":7}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Presence {
    /// The room.
    pub room: RoomId,
    /// The user.
    pub user: UserId,
    /// Joined or left.
    pub event: PresenceEvent,
    /// The user's display name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How many users are online in the room now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

impl Presence {
    /// A presence change.
    pub fn new(room: RoomId, user: UserId, event: PresenceEvent) -> Self {
        Self { room, user, event, name: None, count: None }
    }

    /// The same change with the user's name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same change with the room's online count.
    pub fn with_count(mut self, count: u32) -> Self {
        self.count = Some(count);
        self
    }
}

impl ServerPush for Presence {
    const KIND: &'static str = kinds::CHAT_PRESENCE;
}

// ---- editing, read receipts, typing -------------------------------------------------------------

/// The new text of a message: the body of `PATCH /v1/chat/rooms/{room}/messages/{message}` (inside
/// [`EditMessage`]).
///
/// JSON: `{"text":"hello again"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageEdit {
    /// The new text (the same rules as a new message).
    pub text: String,
}

impl MessageEdit {
    /// A new text.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Change the text of a message: `chat.edit` and `PATCH /v1/chat/rooms/{room}/messages/{message}`
/// → the edited [`ChatMessage`] (with [`ChatMessage::edited_at`]). Allowed for its sender within the
/// server's edit window (default [`DEFAULT_EDIT_WINDOW_SECS`]) and for staff with the server's
/// moderation permission; the room's members get a `chat.edited` push ([`MessageEdited`]).
///
/// JSON (WebSocket): `{"room":12,"message":981,"text":"hello again"}`; over HTTP the room and the
/// message are in the path and the body is a [`MessageEdit`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EditMessage {
    /// The room.
    pub room: RoomId,
    /// The message.
    pub message: MessageId,
    /// The new text.
    #[serde(flatten)]
    pub edit: MessageEdit,
}

impl EditMessage {
    /// Give `message` in `room` the text `text`.
    pub fn new(room: RoomId, message: MessageId, text: impl Into<String>) -> Self {
        Self { room, message, edit: MessageEdit::new(text) }
    }

    /// The shape rules of the text (as [`SendMessage::validate`]).
    pub fn validate(&self, max_chars: usize) -> Result<(), ApiError> {
        SendMessage::new(self.room, self.edit.text.clone()).validate(max_chars)
    }
}

impl WsCall for EditMessage {
    type Response = ChatMessage;
    const KIND: &'static str = kinds::CHAT_EDIT;
}

/// The default time a sender may edit a message after sending it, in seconds.
pub const DEFAULT_EDIT_WINDOW_SECS: u32 = 900;

/// A message's text was changed: the `chat.edited` push, to the room's members (a DM: both users).
///
/// JSON: `{"id":981,"room":12,"text":"hello again","edited_at":1790000060000,"edited_by":42}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageEdited {
    /// The message id.
    pub id: MessageId,
    /// The room.
    pub room: RoomId,
    /// The new text.
    pub text: String,
    /// When it was edited.
    pub edited_at: UnixMillis,
    /// Who edited it (the sender or a moderator; absent: server code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_by: Option<UserId>,
}

impl MessageEdited {
    /// An edit.
    pub fn new(id: MessageId, room: RoomId, text: impl Into<String>, edited_at: UnixMillis) -> Self {
        Self { id, room, text: text.into(), edited_at, edited_by: None }
    }

    /// The same edit made by `user`.
    pub fn by(mut self, user: UserId) -> Self {
        self.edited_by = Some(user);
        self
    }
}

impl ServerPush for MessageEdited {
    const KIND: &'static str = kinds::CHAT_EDITED;
}

/// "Read up to this message": the body of `PUT /v1/chat/rooms/{room}/read` (inside [`MarkRead`]).
///
/// JSON: `{"message":981}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReadUpTo {
    /// The newest message read (a message of the room).
    pub message: MessageId,
}

impl ReadUpTo {
    /// Read up to `message`.
    pub fn new(message: MessageId) -> Self {
        Self { message }
    }
}

/// Store the caller's read marker of a room ("read up to message X"): `chat.mark_read` and
/// `PUT /v1/chat/rooms/{room}/read` → [`Ack`]. The marker only moves forward (an older message
/// leaves it where it is). In direct-message, group and player rooms the members get a
/// `chat.read` push ([`ReadReceipt`]; at most one per user, room and the server's push interval,
/// the newest marker wins); public rooms store the marker for [`UnreadQuery`] without a push.
///
/// JSON (WebSocket): `{"room":12,"message":981}`; over HTTP the room is in the path and the body
/// is a [`ReadUpTo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MarkRead {
    /// The room.
    pub room: RoomId,
    /// The marker.
    #[serde(flatten)]
    pub read: ReadUpTo,
}

impl MarkRead {
    /// Mark `room` read up to `message`.
    pub fn new(room: RoomId, message: MessageId) -> Self {
        Self { room, read: ReadUpTo::new(message) }
    }
}

impl WsCall for MarkRead {
    type Response = Ack;
    const KIND: &'static str = kinds::CHAT_MARK_READ;
}

/// A member's read marker: the `chat.read` push, and the items of [`ReadReceipts`].
///
/// JSON: `{"room":12,"user":42,"message":981,"read_at":1790000000000}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReadReceipt {
    /// The room.
    pub room: RoomId,
    /// Who read.
    pub user: UserId,
    /// The newest message the user has read.
    pub message: MessageId,
    /// When the marker was stored.
    pub read_at: UnixMillis,
}

impl ReadReceipt {
    /// A marker.
    pub fn new(room: RoomId, user: UserId, message: MessageId, read_at: UnixMillis) -> Self {
        Self { room, user, message, read_at }
    }
}

impl ServerPush for ReadReceipt {
    const KIND: &'static str = kinds::CHAT_READ;
}

/// The most read markers one [`ReadReceipts`] answer lists (the newest first).
pub const MAX_LISTED_RECEIPTS: u32 = 200;

/// The read markers of a direct-message, group or player room: `chat.receipts` and
/// `GET /v1/chat/rooms/{room}/receipts` → [`ReadReceipts`] (members only).
///
/// JSON (WebSocket): `{"room":12}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ListReceipts {
    /// The room.
    pub room: RoomId,
}

impl ListReceipts {
    /// The markers of `room`.
    pub fn new(room: RoomId) -> Self {
        Self { room }
    }
}

impl WsCall for ListReceipts {
    type Response = ReadReceipts;
    const KIND: &'static str = kinds::CHAT_RECEIPTS;
}

/// The answer to [`ListReceipts`]: up to [`MAX_LISTED_RECEIPTS`] markers, the newest first.
///
/// JSON: `{"room":12,"receipts":[{"room":12,"user":42,"message":981,"read_at":1790000000000}]}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReadReceipts {
    /// The room.
    pub room: RoomId,
    /// The markers.
    pub receipts: Vec<ReadReceipt>,
}

impl ReadReceipts {
    /// An answer.
    pub fn new(room: RoomId, receipts: Vec<ReadReceipt>) -> Self {
        Self { room, receipts }
    }
}

/// The most rooms one [`UnreadQuery`] may name.
pub const MAX_UNREAD_ROOMS: usize = 100;
/// Unread counts stop at this number ("this many or more").
pub const MAX_UNREAD_COUNT: u32 = 1000;

/// The caller's unread counts of some rooms: `chat.unread` and `POST /v1/chat/unread` →
/// [`UnreadCounts`]. A message counts when it is newer than the caller's read marker, not the
/// caller's own, not deleted and inside the history retention. Rooms the caller cannot read (or
/// that do not exist) are left out of the answer.
///
/// JSON: `{"rooms":[12,13]}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnreadQuery {
    /// 1 to [`MAX_UNREAD_ROOMS`] rooms.
    pub rooms: Vec<RoomId>,
}

impl UnreadQuery {
    /// The counts of these rooms.
    pub fn new(rooms: Vec<RoomId>) -> Self {
        Self { rooms }
    }

    /// The shape rule: 1 to [`MAX_UNREAD_ROOMS`] rooms.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if self.rooms.is_empty() || self.rooms.len() > MAX_UNREAD_ROOMS {
            details.add("rooms", format!("must name 1 to {MAX_UNREAD_ROOMS} rooms"));
        }
        details.into_result()
    }
}

impl WsCall for UnreadQuery {
    type Response = UnreadCounts;
    const KIND: &'static str = kinds::CHAT_UNREAD;
}

/// One room's unread count.
///
/// JSON: `{"room":12,"unread":3,"last_read":978}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnreadCount {
    /// The room.
    pub room: RoomId,
    /// Unread messages, at most [`MAX_UNREAD_COUNT`] ("this many or more").
    pub unread: u32,
    /// The caller's read marker (absent: none stored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read: Option<MessageId>,
}

impl UnreadCount {
    /// A count.
    pub fn new(room: RoomId, unread: u32, last_read: Option<MessageId>) -> Self {
        Self { room, unread, last_read }
    }
}

/// The answer to [`UnreadQuery`], in the order asked.
///
/// JSON: `{"rooms":[{"room":12,"unread":3,"last_read":978}]}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnreadCounts {
    /// The rooms the caller can read.
    pub rooms: Vec<UnreadCount>,
}

impl UnreadCounts {
    /// An answer.
    pub fn new(rooms: Vec<UnreadCount>) -> Self {
        Self { rooms }
    }
}

/// The default time a client shows a typing indicator without a new push, in milliseconds
/// ([`TypingUpdate::expires_in_ms`] carries the server's value).
pub const DEFAULT_TYPING_TTL_MS: u32 = 6000;
/// The default: at most one `chat.typing` push per user, room and this many milliseconds (a client
/// sends `typing: true` about this often while its player types).
pub const DEFAULT_TYPING_INTERVAL_MS: u32 = 3000;

/// Say that the caller types in a room (or stopped): `chat.set_typing` → [`Ack`]. Nothing is
/// stored. The room must be joined on this connection (a direct-message room: no join). The
/// members get a `chat.typing` push ([`TypingUpdate`]), throttled by the server; the user's next
/// `chat.message` ends the indicator on the clients.
///
/// JSON: `{"room":12,"typing":true}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetTyping {
    /// The room.
    pub room: RoomId,
    /// Typing (`true`) or stopped (`false`).
    pub typing: bool,
}

impl SetTyping {
    /// The caller types in `room`.
    pub fn started(room: RoomId) -> Self {
        Self { room, typing: true }
    }

    /// The caller stopped typing in `room`.
    pub fn stopped(room: RoomId) -> Self {
        Self { room, typing: false }
    }
}

impl WsCall for SetTyping {
    type Response = Ack;
    const KIND: &'static str = kinds::CHAT_SET_TYPING;
}

/// A user types in a room or stopped: the `chat.typing` push (best effort: throttled, not in
/// rooms with more online users than the server's typing cap). A client shows the indicator for
/// `expires_in_ms` unless a new push or the user's `chat.message` comes first.
///
/// JSON: `{"room":12,"user":42,"typing":true,"expires_in_ms":6000}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TypingUpdate {
    /// The room.
    pub room: RoomId,
    /// Who types.
    pub user: UserId,
    /// Typing or stopped.
    pub typing: bool,
    /// How long the indicator lasts without a new push, in milliseconds (0 when stopped).
    pub expires_in_ms: u32,
}

impl TypingUpdate {
    /// An update.
    pub fn new(room: RoomId, user: UserId, typing: bool, expires_in_ms: u32) -> Self {
        Self { room, user, typing, expires_in_ms }
    }
}

impl ServerPush for TypingUpdate {
    const KIND: &'static str = kinds::CHAT_TYPING;
}

// ---- rooms created by players -------------------------------------------------------------------

/// The longest room name, in characters.
pub const MAX_ROOM_NAME_CHARS: usize = 64;

/// What is wrong with a room name, if anything: not blank, at most [`MAX_ROOM_NAME_CHARS`]
/// characters, no control or invisible characters ([`text::name_problem`]).
pub fn room_name_problem(name: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("is empty".into());
    }
    if name.chars().count() > MAX_ROOM_NAME_CHARS {
        return Some(format!("is longer than {MAX_ROOM_NAME_CHARS} characters"));
    }
    text::name_problem(name).map(str::to_string)
}

/// Who may become a member of a player room.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RoomVisibility {
    /// Listed (`GET /v1/chat/rooms/public`); anyone not banned may join.
    Public,
    /// Not listed; invited players only.
    #[default]
    Private,
    /// A value from a newer server this version does not know (never sent by a server).
    #[serde(other)]
    Unknown,
}

/// A role in a player room.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RoomRole {
    /// Created the room (or was handed it): changes the visibility, sets roles, hands the room on,
    /// deletes it; everything moderators do.
    Owner,
    /// Renames the room, invites players, kicks members, deletes messages in the room.
    Moderator,
    /// Reads and writes.
    Member,
    /// Invited, not a member (joining accepts, leaving declines).
    Invited,
    /// Kicked: may not join until invited again (listed to the owner and moderators only).
    Banned,
    /// A role from a newer server this version does not know (never sent by a server).
    #[serde(other)]
    Unknown,
}

/// Create a player room; the caller owns it: `POST /v1/chat/rooms` → [`RoomInfo`].
///
/// JSON: `{"name":"Night Owls","visibility":"public"}` (`visibility` optional, default private).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CreateRoom {
    /// The display name ([`room_name_problem`]).
    pub name: String,
    /// Public or private (default private).
    #[serde(default)]
    pub visibility: RoomVisibility,
}

impl CreateRoom {
    /// A private room named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), visibility: RoomVisibility::Private }
    }

    /// The same room, public.
    pub fn public(mut self) -> Self {
        self.visibility = RoomVisibility::Public;
        self
    }

    /// The shape rules: a valid name, a known visibility.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let Some(problem) = room_name_problem(&self.name) {
            details.add("name", problem);
        }
        if self.visibility == RoomVisibility::Unknown {
            details.add("visibility", "must be public or private");
        }
        details.into_result()
    }
}

/// Change a player room: the body of `PATCH /v1/chat/rooms/{room}` (inside [`EditRoom`]); absent
/// fields stay. The name: owner and moderators; the visibility: the owner.
///
/// JSON: `{"name":"Early Birds"}`, `{"visibility":"private"}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UpdateRoom {
    /// A new name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A new visibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<RoomVisibility>,
}

impl UpdateRoom {
    /// An empty change (add a name or a visibility).
    pub fn new() -> Self {
        Self::default()
    }

    /// The same change with a new name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same change with a new visibility.
    pub fn with_visibility(mut self, visibility: RoomVisibility) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// The shape rules: something to change, a valid name, a known visibility.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if self.name.is_none() && self.visibility.is_none() {
            details.add("name", "give a name or a visibility");
        }
        if let Some(problem) = self.name.as_deref().and_then(room_name_problem) {
            details.add("name", problem);
        }
        if self.visibility == Some(RoomVisibility::Unknown) {
            details.add("visibility", "must be public or private");
        }
        details.into_result()
    }
}

/// A player: the body of `POST /v1/chat/rooms/{room}/invites` and `POST /v1/chat/rooms/{room}/owner`.
///
/// JSON: `{"user":7}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoomUser {
    /// The player.
    pub user: UserId,
}

impl RoomUser {
    /// `user`.
    pub fn new(user: UserId) -> Self {
        Self { user }
    }
}

/// A new role: the body of `PUT /v1/chat/rooms/{room}/members/{user}/role` (`moderator` or
/// `member`).
///
/// JSON: `{"role":"moderator"}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoomRoleChange {
    /// The role.
    pub role: RoomRole,
}

impl RoomRoleChange {
    /// Give the role `role`.
    pub fn new(role: RoomRole) -> Self {
        Self { role }
    }
}

/// One member (or invitation, or ban) of a player room: the items of
/// `GET /v1/chat/rooms/{room}/members`.
///
/// JSON: `{"user":42,"name":"Ada","role":"owner","since":1790000000000}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoomMembership {
    /// The player.
    pub user: UserId,
    /// The display name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The role.
    pub role: RoomRole,
    /// Since when (joined, invited or banned).
    pub since: UnixMillis,
}

impl RoomMembership {
    /// An entry.
    pub fn new(user: UserId, role: RoomRole, since: UnixMillis) -> Self {
        Self { user, name: None, role, since }
    }

    /// The same entry with a name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// What changed in a [`RoomUpdate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RoomChange {
    /// The name or the visibility (`info` has the room).
    Updated,
    /// The room was deleted.
    Deleted,
    /// `user` was invited (also pushed to `user`).
    Invited,
    /// `user` became a member.
    Joined,
    /// `user` left (or declined an invitation).
    Left,
    /// `user` was kicked (also pushed to `user`).
    Kicked,
    /// `user` got the role `role`.
    Role,
    /// `user` is the new owner.
    Owner,
    /// A change from a newer server this version does not know (never sent by a server).
    #[serde(other)]
    Unknown,
}

/// A player room changed: the `chat.room` push, to its members and invited players (and to the
/// player an invitation or a kick is about).
///
/// JSON: `{"room":12,"change":"joined","user":7,"by":7}` (+ `"role"`, `"info"`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoomUpdate {
    /// The room.
    pub room: RoomId,
    /// What changed.
    pub change: RoomChange,
    /// The player the change is about, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserId>,
    /// Who made it (absent: server code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<UserId>,
    /// The new role (`role`, `owner`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<RoomRole>,
    /// The room after the change (`updated`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<RoomInfo>,
}

impl RoomUpdate {
    /// A change.
    pub fn new(room: RoomId, change: RoomChange) -> Self {
        Self { room, change, user: None, by: None, role: None, info: None }
    }

    /// The same change about `user`.
    pub fn about(mut self, user: UserId) -> Self {
        self.user = Some(user);
        self
    }

    /// The same change made by `user`.
    pub fn by(mut self, user: UserId) -> Self {
        self.by = Some(user);
        self
    }

    /// The same change with a role.
    pub fn with_role(mut self, role: RoomRole) -> Self {
        self.role = Some(role);
        self
    }

    /// The same change with the room.
    pub fn with_info(mut self, info: RoomInfo) -> Self {
        self.info = Some(info);
        self
    }
}

impl ServerPush for RoomUpdate {
    const KIND: &'static str = kinds::CHAT_ROOM;
}

// ---- typed HTTP calls (see `http_call`) ---------------------------------------------------------

/// The typed HTTP calls of this module (in their own scope: their imports stay out of the
/// module's doc-link scope).
mod calls {
    use super::*;

    use crate::http_call::{payload_call, HttpCall, NoPayload, PathParams, PayloadKind, NO_PAYLOAD};
    use crate::routes::{self, HttpMethod, Route};

    payload_call!(OpenDirect, Post, routes::chat::DM, true, Json, RoomInfo);

    /// The public rooms: `GET /v1/chat/rooms?cursor=…&limit=…` → [`Page`]`<`[`RoomInfo`]`>`.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListRooms {
        /// Which page.
        pub page: PageRequest,
    }

    impl ListRooms {
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

    impl HttpCall for ListRooms {
        type Payload = PageRequest;
        type Response = Page<RoomInfo>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::chat::ROOMS, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &PageRequest {
            &self.page
        }

        fn from_parts(_params: &PathParams, page: PageRequest) -> Result<Self, ApiError> {
            Ok(Self::new().with_page(page))
        }
    }

    /// The caller's direct-message rooms: `GET /v1/chat/dms?cursor=…&limit=…` →
    /// [`Page`]`<`[`RoomInfo`]`>` (with `peer`, newest activity first).
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListDirects {
        /// Which page.
        pub page: PageRequest,
    }

    impl ListDirects {
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

    impl HttpCall for ListDirects {
        type Payload = PageRequest;
        type Response = Page<RoomInfo>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::chat::DMS, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &PageRequest {
            &self.page
        }

        fn from_parts(_params: &PathParams, page: PageRequest) -> Result<Self, ApiError> {
            Ok(Self::new().with_page(page))
        }
    }

    /// A page of a room's history over HTTP: `GET /v1/chat/rooms/{room}/messages?cursor=…&limit=…` →
    /// [`Page`]`<`[`ChatMessage`]`>`, newest first (the WebSocket twin is [`ChatHistory`]).
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListMessages {
        /// The room.
        pub room: RoomId,
        /// Which page.
        pub page: PageRequest,
    }

    impl ListMessages {
        /// The newest page of `room`.
        pub fn new(room: RoomId) -> Self {
            Self { room, page: PageRequest::first() }
        }

        /// The same call for this page.
        pub fn with_page(mut self, page: PageRequest) -> Self {
            self.page = page;
            self
        }
    }

    impl HttpCall for ListMessages {
        type Payload = PageRequest;
        type Response = Page<ChatMessage>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::chat::HISTORY, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &PageRequest {
            &self.page
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("room", self.room)
        }

        fn from_parts(params: &PathParams, page: PageRequest) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("room")?).with_page(page))
        }
    }

    /// Delete one message: `DELETE /v1/chat/rooms/{room}/messages/{message}` → [`Ack`]. Allowed for
    /// its sender (unless the server turned that off) and for moderators; the room's members get a
    /// `chat.deleted` push ([`MessageDeleted`]) and the message leaves the history.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct DeleteMessage {
        /// The room.
        pub room: RoomId,
        /// The message.
        pub message: MessageId,
    }

    impl DeleteMessage {
        /// Delete `message` in `room`.
        pub fn new(room: RoomId, message: MessageId) -> Self {
            Self { room, message }
        }
    }

    impl HttpCall for DeleteMessage {
        type Payload = NoPayload;
        type Response = Ack;
        const ROUTE: Route = Route::new(HttpMethod::Delete, routes::chat::MESSAGE, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("room", self.room).with("message", self.message)
        }

        fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("room")?, params.id("message")?))
        }
    }

    // ---- editing, read markers, unread counts, player rooms -------------------------------------

    payload_call!(CreateRoom, Post, routes::chat::ROOMS, true, Json, RoomInfo);
    payload_call!(UnreadQuery, Post, routes::chat::UNREAD, true, Json, UnreadCounts);

    impl HttpCall for EditMessage {
        type Payload = MessageEdit;
        type Response = ChatMessage;
        const ROUTE: Route = Route::new(HttpMethod::Patch, routes::chat::MESSAGE, true);
        const PAYLOAD: PayloadKind = PayloadKind::Json;

        fn payload(&self) -> &MessageEdit {
            &self.edit
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("room", self.room).with("message", self.message)
        }

        fn from_parts(params: &PathParams, edit: MessageEdit) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("room")?, params.id("message")?, edit.text))
        }
    }

    impl HttpCall for MarkRead {
        type Payload = ReadUpTo;
        type Response = Ack;
        const ROUTE: Route = Route::new(HttpMethod::Put, routes::chat::READ, true);
        const PAYLOAD: PayloadKind = PayloadKind::Json;

        fn payload(&self) -> &ReadUpTo {
            &self.read
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("room", self.room)
        }

        fn from_parts(params: &PathParams, read: ReadUpTo) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("room")?, read.message))
        }
    }

    impl HttpCall for ListReceipts {
        type Payload = NoPayload;
        type Response = ReadReceipts;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::chat::RECEIPTS, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("room", self.room)
        }

        fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("room")?))
        }
    }

    /// A call with a page as its query string and no path parameters.
    macro_rules! page_call {
        ($(#[$meta:meta])* $name:ident, $path:expr, $item:ty) => {
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
                type Response = Page<$item>;
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
        /// The player rooms the caller belongs to or is invited to (with `role`):
        /// `GET /v1/chat/rooms/mine?cursor=…&limit=…` → [`Page`]`<`[`RoomInfo`]`>`, oldest
        /// membership first.
        MyRooms,
        routes::chat::ROOMS_MINE,
        RoomInfo
    );
    page_call!(
        /// The public player rooms: `GET /v1/chat/rooms/public?cursor=…&limit=…` →
        /// [`Page`]`<`[`RoomInfo`]`>`, oldest first (the server's configured rooms:
        /// [`ListRooms`]).
        PublicRooms,
        routes::chat::ROOMS_PUBLIC,
        RoomInfo
    );

    /// A call naming one room in the path, without a body.
    macro_rules! room_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {
                /// The room.
                pub room: RoomId,
            }

            impl $name {
                /// The call for `room`.
                pub fn new(room: RoomId) -> Self {
                    Self { room }
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
                    PathParams::new().with("room", self.room)
                }

                fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
                    Ok(Self::new(params.id("room")?))
                }
            }
        };
    }

    room_call!(
        /// One room as the caller sees it (public rooms: anyone; others: members and, for a player
        /// room, invited players): `GET /v1/chat/rooms/{room}` → [`RoomInfo`].
        GetRoom,
        Get,
        routes::chat::ROOM,
        RoomInfo
    );
    room_call!(
        /// Delete a player room (its owner, or staff with the moderation permission):
        /// `DELETE /v1/chat/rooms/{room}` → [`Ack`]; its messages go with it.
        DeleteRoom,
        Delete,
        routes::chat::ROOM,
        Ack
    );
    room_call!(
        /// Become a member of a player room (a public room, or accept an invitation):
        /// `POST /v1/chat/rooms/{room}/join` → [`RoomInfo`]. A `chat.join` of the room does the
        /// same and joins the connection too.
        JoinChatRoom,
        Post,
        routes::chat::ROOM_JOIN,
        RoomInfo
    );
    room_call!(
        /// Stop being a member of a player room (or decline an invitation):
        /// `POST /v1/chat/rooms/{room}/leave` → [`Ack`]. The owner's room goes to the oldest
        /// moderator (else the oldest member); the last member's room is deleted.
        LeaveChatRoom,
        Post,
        routes::chat::ROOM_LEAVE,
        Ack
    );

    /// A call naming one room in the path, with a JSON body.
    macro_rules! room_body_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $body:ident, $field:ident, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Debug, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {
                /// The room.
                pub room: RoomId,
                /// The body.
                pub $field: $body,
            }

            impl $name {
                /// The call for `room` with this body.
                pub fn new(room: RoomId, $field: $body) -> Self {
                    Self { room, $field }
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
                    PathParams::new().with("room", self.room)
                }

                fn from_parts(params: &PathParams, $field: $body) -> Result<Self, ApiError> {
                    Ok(Self::new(params.id("room")?, $field))
                }
            }
        };
    }

    room_body_call!(
        /// Rename a player room (owner, moderators) or change its visibility (owner):
        /// `PATCH /v1/chat/rooms/{room}` with an [`UpdateRoom`] → [`RoomInfo`].
        EditRoom,
        Patch,
        routes::chat::ROOM,
        UpdateRoom,
        update,
        RoomInfo
    );
    room_body_call!(
        /// Invite a player into a player room (owner, moderators; also lifts a ban):
        /// `POST /v1/chat/rooms/{room}/invites` with a [`RoomUser`] → [`Ack`].
        InviteToRoom,
        Post,
        routes::chat::ROOM_INVITES,
        RoomUser,
        invitee,
        Ack
    );
    room_body_call!(
        /// Hand a player room to a member (owner): `POST /v1/chat/rooms/{room}/owner` with a
        /// [`RoomUser`] → [`Ack`]; the old owner becomes a moderator.
        TransferRoom,
        Post,
        routes::chat::ROOM_OWNER,
        RoomUser,
        to,
        Ack
    );

    /// The members of a player room with their roles, oldest first (members and invited players;
    /// the owner and moderators also see bans): `GET /v1/chat/rooms/{room}/members?cursor=…&limit=…`
    /// → [`Page`]`<`[`RoomMembership`]`>`.
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListRoomMembers {
        /// The room.
        pub room: RoomId,
        /// Which page.
        pub page: PageRequest,
    }

    impl ListRoomMembers {
        /// The first page of `room`.
        pub fn new(room: RoomId) -> Self {
            Self { room, page: PageRequest::first() }
        }

        /// The same call for this page.
        pub fn with_page(mut self, page: PageRequest) -> Self {
            self.page = page;
            self
        }
    }

    impl HttpCall for ListRoomMembers {
        type Payload = PageRequest;
        type Response = Page<RoomMembership>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::chat::ROOM_MEMBERS, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &PageRequest {
            &self.page
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("room", self.room)
        }

        fn from_parts(params: &PathParams, page: PageRequest) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("room")?).with_page(page))
        }
    }

    /// Kick a player from a player room (owner: anyone; moderators: members and invited players):
    /// `DELETE /v1/chat/rooms/{room}/members/{user}` → [`Ack`]. The player is banned from the room
    /// until invited again; an invitation is withdrawn the same way.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct KickFromRoom {
        /// The room.
        pub room: RoomId,
        /// The player.
        pub user: UserId,
    }

    impl KickFromRoom {
        /// Kick `user` from `room`.
        pub fn new(room: RoomId, user: UserId) -> Self {
            Self { room, user }
        }
    }

    impl HttpCall for KickFromRoom {
        type Payload = NoPayload;
        type Response = Ack;
        const ROUTE: Route = Route::new(HttpMethod::Delete, routes::chat::ROOM_MEMBER, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("room", self.room).with("user", self.user)
        }

        fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("room")?, params.id("user")?))
        }
    }

    /// Make a member a moderator or a member again (owner):
    /// `PUT /v1/chat/rooms/{room}/members/{user}/role` with a [`RoomRoleChange`] → [`Ack`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct SetRoomRole {
        /// The room.
        pub room: RoomId,
        /// The member.
        pub user: UserId,
        /// The new role.
        pub change: RoomRoleChange,
    }

    impl SetRoomRole {
        /// Give `user` in `room` the role `role`.
        pub fn new(room: RoomId, user: UserId, role: RoomRole) -> Self {
            Self { room, user, change: RoomRoleChange::new(role) }
        }
    }

    impl HttpCall for SetRoomRole {
        type Payload = RoomRoleChange;
        type Response = Ack;
        const ROUTE: Route = Route::new(HttpMethod::Put, routes::chat::ROOM_ROLE, true);
        const PAYLOAD: PayloadKind = PayloadKind::Json;

        fn payload(&self) -> &RoomRoleChange {
            &self.change
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("room", self.room).with("user", self.user)
        }

        fn from_parts(params: &PathParams, change: RoomRoleChange) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("room")?, params.id("user")?, change.role))
        }
    }
}

pub use calls::{
    DeleteMessage, DeleteRoom, EditRoom, GetRoom, InviteToRoom, JoinChatRoom, KickFromRoom, LeaveChatRoom, ListDirects, ListMessages, ListRoomMembers,
    ListRooms, MyRooms, PublicRooms, SetRoomRole, TransferRoom,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_ref_json() {
        assert_eq!(serde_json::to_string(&JoinRoom::new("world")).ok().as_deref(), Some(r#"{"room":"world"}"#));
        assert_eq!(serde_json::to_string(&JoinRoom::new(RoomId(12))).ok().as_deref(), Some(r#"{"room":12}"#));
        assert_eq!(serde_json::from_str::<JoinRoom>(r#"{"room":12}"#).ok(), Some(JoinRoom::new(RoomId(12))));
        assert_eq!(serde_json::from_str::<JoinRoom>(r#"{"room":"trade"}"#).ok(), Some(JoinRoom::new("trade")));
        assert!(serde_json::from_str::<JoinRoom>(r#"{"room":true}"#).is_err());
        assert!(JoinRoom::new("").validate().is_err());
        assert!(JoinRoom::new("k".repeat(MAX_ROOM_KEY_BYTES + 1)).validate().is_err());
        assert!(JoinRoom::new("world").validate().is_ok());
        for bad in ["a b/c", "\0", "World", "-x", "w\u{200B}"] {
            assert!(JoinRoom::new(bad).validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn send_rules() {
        assert!(SendMessage::new(RoomId(1), "hi").validate(DEFAULT_MAX_TEXT_CHARS).is_ok());
        assert!(SendMessage::new(RoomId(1), "  ").validate(DEFAULT_MAX_TEXT_CHARS).is_err());
        assert!(SendMessage::new(RoomId(1), "é".repeat(DEFAULT_MAX_TEXT_CHARS)).validate(DEFAULT_MAX_TEXT_CHARS).is_ok());
        assert!(SendMessage::new(RoomId(1), "é".repeat(DEFAULT_MAX_TEXT_CHARS + 1)).validate(DEFAULT_MAX_TEXT_CHARS).is_err());
        assert!(SendMessage::new(RoomId(1), "two\nlines").validate(DEFAULT_MAX_TEXT_CHARS).is_ok());
        assert!(SendMessage::new(RoomId(1), "x").with_nonce("n-1").validate(DEFAULT_MAX_TEXT_CHARS).is_ok());
        for nonce in ["", "has space", &"n".repeat(MAX_NONCE_BYTES + 1)] {
            assert!(SendMessage::new(RoomId(1), "x").with_nonce(nonce).validate(DEFAULT_MAX_TEXT_CHARS).is_err(), "{nonce:?}");
        }
    }

    #[test]
    fn history_flattens_the_page() {
        let request = ChatHistory::new(RoomId(4)).with_page(PageRequest::after(crate::Cursor::new("c1")).with_limit(20));
        assert_eq!(serde_json::to_string(&request).ok().as_deref(), Some(r#"{"room":4,"cursor":"c1","limit":20}"#));
        assert_eq!(serde_json::to_string(&ChatHistory::new(RoomId(4))).ok().as_deref(), Some(r#"{"room":4}"#));
        assert_eq!(serde_json::from_str::<RoomKind>("\"guild_hall\"").ok(), Some(RoomKind::Unknown));
    }

    #[test]
    fn extras_json_and_rules() {
        let edit = EditMessage::new(RoomId(12), MessageId(981), "hello again");
        assert_eq!(serde_json::to_string(&edit).ok().as_deref(), Some(r#"{"room":12,"message":981,"text":"hello again"}"#));
        assert_eq!(serde_json::from_str::<EditMessage>(r#"{"room":12,"message":981,"text":"hello again"}"#).ok(), Some(edit.clone()));
        assert!(edit.validate(DEFAULT_MAX_TEXT_CHARS).is_ok() && EditMessage::new(RoomId(1), MessageId(1), " ").validate(DEFAULT_MAX_TEXT_CHARS).is_err());
        assert_eq!(serde_json::to_string(&MarkRead::new(RoomId(12), MessageId(9))).ok().as_deref(), Some(r#"{"room":12,"message":9}"#));
        assert!(UnreadQuery::new(vec![]).validate().is_err());
        assert!(UnreadQuery::new(vec![RoomId(1); MAX_UNREAD_ROOMS + 1]).validate().is_err());
        assert!(UnreadQuery::new(vec![RoomId(1)]).validate().is_ok());
        assert_eq!(serde_json::to_string(&UnreadCount::new(RoomId(1), 3, None)).ok().as_deref(), Some(r#"{"room":1,"unread":3}"#));
        assert_eq!(serde_json::to_string(&SetTyping::stopped(RoomId(4))).ok().as_deref(), Some(r#"{"room":4,"typing":false}"#));
        let message = ChatMessage::new(MessageId(1), RoomId(2), UserId(3), "x", UnixMillis(5));
        assert!(!serde_json::to_string(&message).unwrap_or_default().contains("edited_at"));
        assert!(serde_json::to_string(&message.with_edited_at(UnixMillis(6))).unwrap_or_default().contains(r#""edited_at":6"#));
        // Player rooms.
        assert_eq!(serde_json::from_str::<CreateRoom>(r#"{"name":"Den"}"#).ok(), Some(CreateRoom::new("Den")));
        assert_eq!(serde_json::to_string(&CreateRoom::new("Den").public()).ok().as_deref(), Some(r#"{"name":"Den","visibility":"public"}"#));
        for bad in ["", "  ", &"x".repeat(MAX_ROOM_NAME_CHARS + 1), "a\u{202E}b"] {
            assert!(CreateRoom::new(bad).validate().is_err(), "{bad:?}");
        }
        let unknown: CreateRoom = serde_json::from_str(r#"{"name":"Den","visibility":"secret"}"#).unwrap_or_else(|_| CreateRoom::new("x"));
        assert_eq!(unknown.visibility, RoomVisibility::Unknown);
        assert!(unknown.validate().is_err());
        assert!(UpdateRoom::new().validate().is_err());
        assert!(UpdateRoom::new().with_visibility(RoomVisibility::Public).validate().is_ok());
        assert_eq!(serde_json::from_str::<RoomKind>("\"player\"").ok(), Some(RoomKind::Player));
        assert_eq!(serde_json::from_str::<RoomRole>("\"chief\"").ok(), Some(RoomRole::Unknown));
        assert_eq!(serde_json::from_str::<RoomChange>("\"renamed\"").ok(), Some(RoomChange::Unknown));
        let update = RoomUpdate::new(RoomId(12), RoomChange::Joined).about(UserId(7)).by(UserId(7));
        assert_eq!(serde_json::to_string(&update).ok().as_deref(), Some(r#"{"room":12,"change":"joined","user":7,"by":7}"#));
        let info = RoomInfo::new(RoomId(12), RoomKind::Player).with_visibility(RoomVisibility::Private).with_owner(UserId(1)).with_role(RoomRole::Invited);
        assert_eq!(serde_json::to_string(&info).ok().as_deref(), Some(r#"{"id":12,"kind":"player","visibility":"private","owner":1,"role":"invited"}"#));
    }
}
