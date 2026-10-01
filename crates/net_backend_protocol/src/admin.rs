//! Administration: list and inspect accounts, ban and unban them, revoke their sessions, grant
//! and revoke roles, read the audit log. Routes: [`routes::admin`](crate::routes::admin); every one
//! needs an access token of an account with the [`ADMIN_ROLE`] role (others get 403 `forbidden`).
//!
//! These routes are for operator tools (a dashboard, a script), not for game clients. A server
//! may leave them out of its public API description.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::Account;
use crate::error::{ApiError, ValidationDetails};
use crate::ids::UserId;
use crate::page::{Cursor, MAX_CURSOR_BYTES};
use crate::text;
use crate::time::UnixMillis;

/// The role that may use the administration routes.
pub const ADMIN_ROLE: &str = "admin";
/// The longest role name, in bytes.
pub const ROLE_MAX_BYTES: usize = 64;
/// The longest ban reason, in characters.
pub const BAN_REASON_MAX_CHARS: usize = 255;
/// The longest search text of a user listing, in characters.
pub const USER_SEARCH_MAX_CHARS: usize = 254;

/// Whether `role` is a valid role name: 1 to [`ROLE_MAX_BYTES`] bytes of `[a-z0-9_.-]`, starting
/// with a letter (`admin`, `moderator`, `beta.tester`).
pub fn is_valid_role(role: &str) -> bool {
    role.len() <= ROLE_MAX_BYTES
        && role.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && role.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}

/// A ban of an account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BanInfo {
    /// When the ban was set.
    pub banned_at: UnixMillis,
    /// When it ends by itself; `None` = until an admin lifts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<UnixMillis>,
    /// Why (shown to operators; a server may show it to the player).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl BanInfo {
    /// A ban set at `banned_at`, permanent, without a reason.
    pub fn new(banned_at: UnixMillis) -> Self {
        Self { banned_at, until: None, reason: None }
    }

    /// The same ban ending at `until`.
    pub fn with_until(mut self, until: UnixMillis) -> Self {
        self.until = Some(until);
        self
    }

    /// The same ban with a reason.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Whether the ban is in force at `now`.
    pub fn is_active(&self, now: UnixMillis) -> bool {
        self.until.is_none_or(|until| until > now)
    }
}

/// An account as an administrator sees it: `GET /v1/admin/users/{user}` (and the items of the
/// listing).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AdminUser {
    /// The account (as its owner sees it at `GET /v1/account`).
    pub account: Account,
    /// The ban, if the account is banned (also a ban that has run out, until it is lifted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ban: Option<BanInfo>,
    /// The last time one of its sessions was used or created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<UnixMillis>,
    /// How many sessions are open (not revoked, not expired).
    #[serde(default)]
    pub active_sessions: u32,
}

impl AdminUser {
    /// An entry for this account.
    pub fn new(account: Account) -> Self {
        Self { account, ban: None, last_seen_at: None, active_sessions: 0 }
    }

    /// The same entry with a ban.
    pub fn with_ban(mut self, ban: BanInfo) -> Self {
        self.ban = Some(ban);
        self
    }

    /// The same entry with the last-seen time.
    pub fn with_last_seen_at(mut self, at: UnixMillis) -> Self {
        self.last_seen_at = Some(at);
        self
    }

    /// The same entry with the number of open sessions.
    pub fn with_active_sessions(mut self, sessions: u32) -> Self {
        self.active_sessions = sessions;
        self
    }
}

/// List accounts: `GET /v1/admin/users?q=…&cursor=…&limit=…` → [`Page`](crate::Page)`<`[`AdminUser`]`>`,
/// newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UserListQuery {
    /// Only accounts whose email or display name contains this text (case-insensitive for the
    /// email), or whose id is this number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Where to continue (the previous page's `next_cursor`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// At most this many items (clamped like [`PageRequest`](crate::PageRequest)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl UserListQuery {
    /// The first page of every account.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same query with a search text.
    pub fn with_search(mut self, q: impl Into<String>) -> Self {
        self.q = Some(q.into());
        self
    }

    /// The same query continuing after `cursor`.
    pub fn with_cursor(mut self, cursor: Cursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// The same query with a limit.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The rules: search text at most [`USER_SEARCH_MAX_CHARS`] without control characters, cursor
    /// at most [`MAX_CURSOR_BYTES`].
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let Some(q) = &self.q {
            if q.chars().count() > USER_SEARCH_MAX_CHARS {
                details.add("q", format!("is longer than {USER_SEARCH_MAX_CHARS} characters"));
            }
            if q.chars().any(char::is_control) {
                details.add("q", "contains control characters");
            }
        }
        check_cursor(self.cursor.as_ref(), &mut details);
        details.into_result()
    }
}

/// Ban an account: `POST /v1/admin/users/{user}/ban` → [`Ack`](crate::Ack). Every session of the
/// account is revoked (open WebSockets close with 4003); logins answer 403 `banned` while the ban
/// is in force.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BanRequest {
    /// Why (at most [`BAN_REASON_MAX_CHARS`] characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When the ban ends by itself (must be in the future); `None` = until it is lifted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<UnixMillis>,
}

impl BanRequest {
    /// A permanent ban without a reason.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same ban with a reason.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// The same ban ending at `until`.
    pub fn with_until(mut self, until: UnixMillis) -> Self {
        self.until = Some(until);
        self
    }

    /// The shape rules (the server also checks that `until` lies in the future).
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let Some(reason) = &self.reason {
            if reason.chars().count() > BAN_REASON_MAX_CHARS {
                details.add("reason", format!("is longer than {BAN_REASON_MAX_CHARS} characters"));
            }
            if let Some(problem) = text::message_problem(reason) {
                details.add("reason", problem);
            }
        }
        details.into_result()
    }
}

/// One audit-log entry: who did what, when, from where. Never holds secrets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AuditEntry {
    /// The entry's id (grows with time).
    pub id: i64,
    /// Who acted (`None`: the server itself, the command line, or an anonymous request such as a
    /// failed login).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<UserId>,
    /// What happened (`auth.login`, `admin.ban`, a game's own action, …).
    pub action: String,
    /// The kind of thing it happened to (`user`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_type: Option<String>,
    /// Its id, as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// The client address the request came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// The id of the request (matches the server's logs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Action-specific details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// When.
    pub created_at: UnixMillis,
}

impl AuditEntry {
    /// An entry with this id, action and time.
    pub fn new(id: i64, action: impl Into<String>, created_at: UnixMillis) -> Self {
        Self { id, actor: None, action: action.into(), target_type: None, target_id: None, ip: None, request_id: None, data: None, created_at }
    }

    /// The same entry with an actor.
    pub fn with_actor(mut self, actor: UserId) -> Self {
        self.actor = Some(actor);
        self
    }

    /// The same entry with a target.
    pub fn with_target(mut self, target_type: impl Into<String>, target_id: impl Into<String>) -> Self {
        self.target_type = Some(target_type.into());
        self.target_id = Some(target_id.into());
        self
    }

    /// The same entry with the client address.
    pub fn with_ip(mut self, ip: impl Into<String>) -> Self {
        self.ip = Some(ip.into());
        self
    }

    /// The same entry with the request id.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// The same entry with details.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// Read the audit log: `GET /v1/admin/audit?user=…&action=…&cursor=…&limit=…` →
/// [`Page`](crate::Page)`<`[`AuditEntry`]`>`, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AuditQuery {
    /// Only entries where this user acted or was the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserId>,
    /// Only entries with this action, or with this prefix when it ends in `.` (`admin.`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Where to continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// At most this many items (clamped like [`PageRequest`](crate::PageRequest)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl AuditQuery {
    /// The newest entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same query for one user.
    pub fn with_user(mut self, user: UserId) -> Self {
        self.user = Some(user);
        self
    }

    /// The same query for one action (or an action prefix ending in `.`).
    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    /// The same query continuing after `cursor`.
    pub fn with_cursor(mut self, cursor: Cursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// The same query with a limit.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The rules: an action is 1–64 bytes of `[a-z0-9_.-]`; cursor at most [`MAX_CURSOR_BYTES`].
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let Some(action) = &self.action {
            let ok =
                (1..=64).contains(&action.len()) && action.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'));
            if !ok {
                details.add("action", "is not an action name ([a-z0-9_.-], at most 64 bytes)");
            }
        }
        check_cursor(self.cursor.as_ref(), &mut details);
        details.into_result()
    }
}

fn check_cursor(cursor: Option<&Cursor>, details: &mut ValidationDetails) {
    if cursor.is_some_and(|c| c.as_str().len() > MAX_CURSOR_BYTES) {
        details.add("cursor", format!("is longer than {MAX_CURSOR_BYTES} bytes"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles() {
        for ok in ["admin", "moderator", "beta.tester", "a", "x-1_2"] {
            assert!(is_valid_role(ok), "{ok}");
        }
        for bad in ["", "Admin", "1admin", "ad min", "admin!", &"a".repeat(65)] {
            assert!(!is_valid_role(bad), "{bad}");
        }
        assert!(is_valid_role(ADMIN_ROLE));
    }

    #[test]
    fn validation() {
        assert!(BanRequest::new().with_reason("cheating").validate().is_ok());
        assert!(BanRequest::new().with_reason("x".repeat(256)).validate().is_err());
        assert!(BanRequest::new().with_reason("a\u{202E}b").validate().is_err());
        assert!(UserListQuery::new().with_search("ada").validate().is_ok());
        assert!(UserListQuery::new().with_search("a\0").validate().is_err());
        assert!(AuditQuery::new().with_action("admin.").validate().is_ok());
        assert!(AuditQuery::new().with_action("Admin").validate().is_err());
        assert!(AuditQuery::new().with_cursor(Cursor::new("c".repeat(513))).validate().is_err());
    }

    #[test]
    fn ban_activity() {
        let ban = BanInfo::new(UnixMillis(10));
        assert!(ban.is_active(UnixMillis(1_000)));
        let ban = ban.with_until(UnixMillis(100)).with_reason("x");
        assert!(ban.is_active(UnixMillis(99)));
        assert!(!ban.is_active(UnixMillis(100)));
    }
}
