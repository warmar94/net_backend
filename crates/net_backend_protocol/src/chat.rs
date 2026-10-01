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
/// The default rate limit: at most this many messages per user …
pub const DEFAULT_RATE_MESSAGES: u32 = 5;
/// … within this many seconds.
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
}

impl RoomInfo {
    /// A room with no key, name or counts.
    pub fn new(id: RoomId, kind: RoomKind) -> Self {
        Self { id, kind, key: None, name: None, member_count: None, max_members: None, peer: None }
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
    /// The sender's [`SendMessage::nonce`], if it set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

impl ChatMessage {
    /// A message.
    pub fn new(id: MessageId, room: RoomId, sender: UserId, text: impl Into<String>, sent_at: UnixMillis) -> Self {
        Self { id, room, sender, sender_name: None, text: text.into(), sent_at, nonce: None }
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
}
