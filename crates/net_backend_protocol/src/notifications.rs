//! Notifications: messages the server stores for one player (a reward, an invitation, a system
//! notice), pushed live to the player's open connections and kept until read, deleted or past the
//! server's retention. Players read, mark and delete their own; only the server creates them.
//!
//! | Kind / route | Request → answer |
//! |---|---|
//! | push `notify.new` | [`Notification`], to every open connection of its player |
//! | `notify.list`, `GET /v1/notifications` | [`NotificationQuery`] → [`Page`]`<`[`Notification`]`>` (newest first) |
//! | `notify.count`, `GET /v1/notifications/count` | [`CountNotifications`] → [`NotificationCount`] |
//! | `notify.mark`, `POST /v1/notifications/mark` | [`MarkNotifications`] → [`MarkAck`] (read or unread) |
//! | `notify.delete`, `DELETE /v1/notifications/{id}` | [`DeleteNotification`] → [`Ack`] |
//!
//! Every request has the same answer over the WebSocket and HTTP. A client that was offline reads
//! what it missed with a list (`unread_only` for the unread ones); while connected it gets each new
//! one as a `notify.new` push.
//!
//! **A notification** has a `kind` the game chooses (`"reward"`, `"invite.match"`: 1–64 bytes of
//! `a-z 0-9 _ . : -`, starting with a letter), an optional `text` (at most [`MAX_TEXT_CHARS`]
//! characters, the chat text rules) and optional `data` (a small JSON value, at most the server's
//! limit, [`DEFAULT_MAX_DATA_BYTES`] by default), the account that caused it (`sender`, if any) and
//! whether it was read.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::{Ack, ServerPush, WsCall};
use crate::error::{ApiError, ValidationDetails};
use crate::ids::{NotificationId, UserId};
use crate::kinds;
use crate::page::{Cursor, Page};
use crate::time::UnixMillis;

/// The longest notification kind, in bytes.
pub const MAX_KIND_BYTES: usize = 64;
/// The longest notification text, in characters.
pub const MAX_TEXT_CHARS: usize = 1000;
/// The default largest `data`, in bytes of its JSON (a server may configure another).
pub const DEFAULT_MAX_DATA_BYTES: usize = 4096;
/// The most ids one [`MarkNotifications`] may name.
pub const MAX_MARK_IDS: usize = 100;

/// Whether `kind` is a valid notification kind: 1 to [`MAX_KIND_BYTES`] bytes of ASCII lower-case
/// letters, digits, `_`, `.`, `:` and `-`, starting with a letter (`"reward"`, `"invite.match"`).
pub fn is_valid_kind(kind: &str) -> bool {
    let bytes = kind.as_bytes();
    bytes.len() <= MAX_KIND_BYTES
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// What is wrong with a notification text: `None` if fine (not blank, at most [`MAX_TEXT_CHARS`]
/// characters, the chat text rules of [`crate::text::message_problem`]).
pub fn text_problem(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return Some("is empty".into());
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Some(format!("is longer than {MAX_TEXT_CHARS} characters"));
    }
    crate::text::message_problem(text).map(str::to_string)
}

/// A stored notification: the `notify.new` push and the items of a list.
///
/// JSON: `{"id":31,"kind":"reward","text":"You won 50 gold","data":{"gold":50},"created_at":1790000000000,"read":false}`
/// (+ `"sender":42` when an account caused it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Notification {
    /// The id (newer notifications have larger ids).
    pub id: NotificationId,
    /// What it is about (the game's own kinds).
    pub kind: String,
    /// A text to show, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The game's data, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// The account that caused it, if any (absent for system notices, or once that account is
    /// deleted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<UserId>,
    /// When it was created.
    pub created_at: UnixMillis,
    /// Whether the player marked it as read.
    #[serde(default)]
    pub read: bool,
}

impl Notification {
    /// A notification without text, data or sender, unread.
    pub fn new(id: NotificationId, kind: impl Into<String>, created_at: UnixMillis) -> Self {
        Self { id, kind: kind.into(), text: None, data: None, sender: None, created_at, read: false }
    }

    /// The same notification with a text.
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// The same notification with data.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// The same notification with a sender.
    pub fn with_sender(mut self, sender: UserId) -> Self {
        self.sender = Some(sender);
        self
    }

    /// The same notification marked read (or unread).
    pub fn with_read(mut self, read: bool) -> Self {
        self.read = read;
        self
    }
}

impl ServerPush for Notification {
    const KIND: &'static str = kinds::NOTIFY_NEW;
}

/// A page of the caller's notifications, newest first: `notify.list`, and the query of
/// `GET /v1/notifications` (`?cursor=…&limit=…&unread_only=true`).
///
/// JSON: `{"cursor":"…","limit":20,"unread_only":true}` (every field optional).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct NotificationQuery {
    /// Where to continue (`next_cursor` of the previous page); absent for the newest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// At most this many (default 50, at most 100: [`PageRequest`](crate::PageRequest)'s limits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Only the unread ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread_only: Option<bool>,
}

impl NotificationQuery {
    /// The newest page, read and unread.
    pub fn new() -> Self {
        Self::default()
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

    /// The same query for the unread ones only.
    pub fn unread_only(mut self) -> Self {
        self.unread_only = Some(true);
        self
    }

    /// The limit to apply (like [`PageRequest::limit_or_default`](crate::PageRequest::limit_or_default)).
    pub fn limit_or_default(&self) -> u32 {
        self.limit.map_or(crate::page::DEFAULT_PAGE_LIMIT, |limit| limit.clamp(1, crate::page::MAX_PAGE_LIMIT))
    }
}

impl WsCall for NotificationQuery {
    type Response = Page<Notification>;
    const KIND: &'static str = kinds::NOTIFY_LIST;
}

/// How many notifications the caller has: `notify.count` and `GET /v1/notifications/count`.
///
/// JSON: `{}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CountNotifications {}

impl CountNotifications {
    /// The request.
    pub fn new() -> Self {
        Self {}
    }
}

impl WsCall for CountNotifications {
    type Response = NotificationCount;
    const KIND: &'static str = kinds::NOTIFY_COUNT;
}

/// The answer to [`CountNotifications`].
///
/// JSON: `{"unread":3,"total":12}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct NotificationCount {
    /// Unread notifications.
    pub unread: u64,
    /// All of the caller's notifications.
    pub total: u64,
}

impl NotificationCount {
    /// A count.
    pub fn new(unread: u64, total: u64) -> Self {
        Self { unread, total }
    }
}

/// Mark some (or all) of the caller's notifications read or unread: `notify.mark` and
/// `POST /v1/notifications/mark` → [`MarkAck`]. Ids that are not the caller's, or already in
/// that state, are skipped.
///
/// JSON: `{"ids":[31,32],"read":true}` or `{"all":true,"read":true}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MarkNotifications {
    /// The notifications (1 to [`MAX_MARK_IDS`] distinct ids), unless `all`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<NotificationId>,
    /// Every notification of the caller (then `ids` stays empty).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    /// Read (`true`) or unread (`false`).
    pub read: bool,
}

impl MarkNotifications {
    /// Mark these notifications read.
    pub fn read(ids: Vec<NotificationId>) -> Self {
        Self { ids, all: false, read: true }
    }

    /// Mark these notifications unread.
    pub fn unread(ids: Vec<NotificationId>) -> Self {
        Self { ids, all: false, read: false }
    }

    /// Mark every notification of the caller read.
    pub fn all_read() -> Self {
        Self { ids: Vec::new(), all: true, read: true }
    }

    /// The shape rules: either `all` or 1 to [`MAX_MARK_IDS`] distinct ids.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if self.all && !self.ids.is_empty() {
            details.add("ids", "must be empty with all");
        }
        if !self.all && !(1..=MAX_MARK_IDS).contains(&self.ids.len()) {
            details.add("ids", format!("must hold 1 to {MAX_MARK_IDS} ids (or set all)"));
        }
        let distinct: HashSet<NotificationId> = self.ids.iter().copied().collect();
        if distinct.len() != self.ids.len() {
            details.add("ids", "must be distinct");
        }
        details.into_result()
    }
}

impl WsCall for MarkNotifications {
    type Response = MarkAck;
    const KIND: &'static str = kinds::NOTIFY_MARK;
}

/// The answer to [`MarkNotifications`].
///
/// JSON: `{"changed":2,"unread":1}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MarkAck {
    /// How many notifications changed state.
    pub changed: u64,
    /// The caller's unread notifications now.
    pub unread: u64,
}

impl MarkAck {
    /// An answer.
    pub fn new(changed: u64, unread: u64) -> Self {
        Self { changed, unread }
    }
}

/// Delete one of the caller's notifications: `notify.delete` and
/// `DELETE /v1/notifications/{id}` → [`Ack`] (also when it does not exist).
///
/// JSON (WebSocket): `{"id":31}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeleteNotification {
    /// The notification.
    pub id: NotificationId,
}

impl DeleteNotification {
    /// Delete `id`.
    pub fn new(id: NotificationId) -> Self {
        Self { id }
    }
}

impl WsCall for DeleteNotification {
    type Response = Ack;
    const KIND: &'static str = kinds::NOTIFY_DELETE;
}

// ---- typed HTTP calls (see `http_call`) ---------------------------------------------------------

/// The typed HTTP calls of this module (in their own scope: their imports stay out of the
/// module's doc-link scope).
mod calls {
    use super::*;

    use crate::http_call::{payload_call, HttpCall, NoPayload, PathParams, PayloadKind, NO_PAYLOAD};
    use crate::routes::{self, HttpMethod, Route};

    payload_call!(NotificationQuery, Get, routes::notifications::LIST, true, Query, Page<Notification>);
    payload_call!(CountNotifications, Get, routes::notifications::COUNT, true, Query, NotificationCount);
    payload_call!(MarkNotifications, Post, routes::notifications::MARK, true, Json, MarkAck);

    impl HttpCall for DeleteNotification {
        type Payload = NoPayload;
        type Response = Ack;
        const ROUTE: Route = Route::new(HttpMethod::Delete, routes::notifications::ONE, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("id", self.id)
        }

        fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("id")?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_texts() {
        for good in ["reward", "invite.match", "a:b-c_d.9", &"k".repeat(MAX_KIND_BYTES)] {
            assert!(is_valid_kind(good), "{good}");
        }
        for bad in ["", "Reward", "9x", "-x", "a b", "a/b", &"k".repeat(MAX_KIND_BYTES + 1)] {
            assert!(!is_valid_kind(bad), "{bad}");
        }
        assert!(text_problem("You won!").is_none());
        assert!(text_problem("  ").is_some() && text_problem(&"é".repeat(MAX_TEXT_CHARS + 1)).is_some());
        assert!(text_problem("a\u{202E}b").is_some());
    }

    #[test]
    fn json_and_rules() {
        let n = Notification::new(NotificationId(31), "reward", UnixMillis(5)).with_data(serde_json::json!({"gold": 50}));
        assert_eq!(serde_json::to_string(&n).ok().as_deref(), Some(r#"{"id":31,"kind":"reward","data":{"gold":50},"created_at":5,"read":false}"#));
        assert_eq!(serde_json::to_string(&MarkNotifications::all_read()).ok().as_deref(), Some(r#"{"all":true,"read":true}"#));
        assert_eq!(serde_json::to_string(&MarkNotifications::read(vec![NotificationId(1)])).ok().as_deref(), Some(r#"{"ids":[1],"read":true}"#));
        assert!(MarkNotifications::all_read().validate().is_ok());
        assert!(MarkNotifications::read(vec![]).validate().is_err());
        assert!(MarkNotifications::read(vec![NotificationId(1), NotificationId(1)]).validate().is_err());
        assert!(MarkNotifications::read((0..=MAX_MARK_IDS as i64).map(NotificationId).collect()).validate().is_err());
        let mut both = MarkNotifications::all_read();
        both.ids.push(NotificationId(1));
        assert!(both.validate().is_err());
        assert_eq!(serde_json::to_string(&CountNotifications::new()).ok().as_deref(), Some("{}"));
        assert_eq!(serde_json::from_str::<CountNotifications>("{}").ok(), Some(CountNotifications::new()));
        assert_eq!(NotificationQuery::new().with_limit(0).limit_or_default(), 1);
    }
}
