//! The chat module's settings: `[modules.chat]` in the configuration (or
//! [`Chat::with_config`](crate::chat::Chat::with_config)).
//!
//! ```toml
//! [modules.chat]
//! max_text_chars = 500
//! rate_messages = 5              # per user: a burst of 5, then one every 10 / 5 = 2 s
//! rate_window_secs = 10          # (token bucket: rate_messages per rate_window_secs)
//! dm_open_rate = 20              # POST /v1/chat/dm per user: a burst of 20, then one every
//! dm_open_window_secs = 600      # 600 / 20 = 30 s (0 = no limit)
//! history_retention_days = 30    # 0 = keep forever
//! max_room_members = 200         # public rooms without their own cap
//! presence = true
//! presence_max_members = 100     # bigger rooms get no chat.presence pushes
//! presence_per_second = 10       # per room
//! moderator_roles = ["admin", "moderator"]
//!
//! [[modules.chat.rooms]]
//! key = "world"
//! name = "World"
//! max_members = 500
//! ```

use net_backend_protocol::chat::{
    is_valid_room_key, DEFAULT_HISTORY_RETENTION_DAYS, DEFAULT_MAX_ROOM_MEMBERS, DEFAULT_MAX_TEXT_CHARS, DEFAULT_PRESENCE_MAX_MEMBERS,
    DEFAULT_PRESENCE_PER_SECOND, DEFAULT_RATE_MESSAGES, DEFAULT_RATE_WINDOW_SECS,
};
use serde::Deserialize;

use super::migrations::BODY_CHARS;
use crate::error::Error;

/// A public room the module creates (or updates) when the server starts.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RoomSpec {
    /// The key clients join with (`world`): 1-64 bytes of `[a-z0-9_.-]`.
    pub key: String,
    /// A display name (at most 64 characters).
    #[serde(default)]
    pub name: Option<String>,
    /// This room's member cap (connections); default `max_room_members`.
    #[serde(default)]
    pub max_members: Option<u32>,
}

impl RoomSpec {
    /// A room with this key.
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into(), name: None, max_members: None }
    }

    /// The same room with a display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same room with its own member cap.
    pub fn with_max_members(mut self, max: u32) -> Self {
        self.max_members = Some(max);
        self
    }
}

/// The chat module's settings. Build in code from [`ChatConfig::default`] by changing fields, or
/// let the module read `[modules.chat]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct ChatConfig {
    /// The public rooms (created or updated at start; rooms removed here stay in the database).
    pub rooms: Vec<RoomSpec>,
    /// The longest message, in characters. Default 500; at most 4000.
    pub max_text_chars: u32,
    /// Messages per user within `rate_window_secs`, as a token bucket: a burst of this many, then
    /// one every `rate_window_secs / rate_messages` seconds (over it: `rate_limited` with
    /// `retry_after_ms`). Default 5 (per 10 s: a burst of 5, then one every 2 s).
    pub rate_messages: u32,
    /// The rate window in seconds. Default 10.
    pub rate_window_secs: u32,
    /// Direct-message rooms a user may open (`POST /v1/chat/dm`, also finding an existing one)
    /// within `dm_open_window_secs`, as a token bucket (over it: 429 `rate_limited`). It bounds DM
    /// spam and probing which account ids exist. Default 20 (per 600 s: a burst of 20, then one
    /// every 30 s); 0 = no limit.
    pub dm_open_rate: u32,
    /// The window of `dm_open_rate`, seconds. Default 600.
    pub dm_open_window_secs: u32,
    /// Messages older than this many days leave the history (and are deleted by a background
    /// task). Default 30; 0 keeps them forever.
    pub history_retention_days: u32,
    /// How often the retention purge runs, seconds (0: never). Default 3600.
    pub purge_interval_secs: u64,
    /// The member cap (connections) of public rooms without their own. Default 200.
    pub max_room_members: u32,
    /// The member cap (connections) of group rooms. Default 200.
    pub max_group_members: u32,
    /// Send `chat.presence` pushes. Default true (`chat.members` answers either way).
    pub presence: bool,
    /// Rooms with more online users than this get no `chat.presence` pushes. Default 100.
    pub presence_max_members: u32,
    /// At most this many `chat.presence` pushes per room and second. Default 10.
    pub presence_per_second: u32,
    /// Senders may delete their own messages. Default true.
    pub allow_self_delete: bool,
    /// Roles that may delete any message (audited). Default `["admin", "moderator"]`.
    pub moderator_roles: Vec<String>,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            rooms: Vec::new(),
            max_text_chars: DEFAULT_MAX_TEXT_CHARS as u32,
            rate_messages: DEFAULT_RATE_MESSAGES,
            rate_window_secs: DEFAULT_RATE_WINDOW_SECS,
            dm_open_rate: 20,
            dm_open_window_secs: 600,
            history_retention_days: DEFAULT_HISTORY_RETENTION_DAYS,
            purge_interval_secs: 3600,
            max_room_members: DEFAULT_MAX_ROOM_MEMBERS,
            max_group_members: DEFAULT_MAX_ROOM_MEMBERS,
            presence: true,
            presence_max_members: DEFAULT_PRESENCE_MAX_MEMBERS,
            presence_per_second: DEFAULT_PRESENCE_PER_SECOND,
            allow_self_delete: true,
            moderator_roles: vec!["admin".into(), "moderator".into()],
        }
    }
}

impl ChatConfig {
    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        let mut need = |ok: bool, problem: String| {
            if !ok {
                problems.push(format!("modules.chat.{problem}"));
            }
        };
        need((1..=BODY_CHARS).contains(&self.max_text_chars), format!("max_text_chars must be between 1 and {BODY_CHARS}"));
        need((1..=10_000).contains(&self.rate_messages), "rate_messages must be between 1 and 10000".into());
        need((1..=86_400).contains(&self.rate_window_secs), "rate_window_secs must be between 1 and 86400".into());
        need(self.dm_open_rate <= 100_000, "dm_open_rate must be at most 100000 (0 = no limit)".into());
        need((1..=86_400).contains(&self.dm_open_window_secs), "dm_open_window_secs must be between 1 and 86400".into());
        need(self.history_retention_days <= 36_500, "history_retention_days must be at most 36500".into());
        need(self.purge_interval_secs <= 7 * 86_400, "purge_interval_secs must be at most 604800".into());
        need((1..=10_000_000).contains(&self.max_room_members), "max_room_members must be between 1 and 10000000".into());
        need((1..=10_000_000).contains(&self.max_group_members), "max_group_members must be between 1 and 10000000".into());
        need((1..=100_000).contains(&self.presence_max_members), "presence_max_members must be between 1 and 100000".into());
        need((1..=10_000).contains(&self.presence_per_second), "presence_per_second must be between 1 and 10000".into());
        for role in &self.moderator_roles {
            need(net_backend_protocol::admin::is_valid_role(role), format!("moderator_roles: `{role}` is not a role name"));
        }
        let mut keys = std::collections::HashSet::new();
        for room in &self.rooms {
            need(is_valid_room_key(&room.key), format!("rooms: `{}` is not a room key (1-64 bytes of a-z, 0-9, _ - .)", room.key));
            need(keys.insert(room.key.as_str()), format!("rooms: `{}` is listed twice", room.key));
            if let Some(name) = &room.name {
                need(is_valid_room_name(name), format!("rooms: the name of `{}` must be 1-64 characters", room.key));
            }
            if let Some(max) = room.max_members {
                need((1..=10_000_000).contains(&max), format!("rooms: max_members of `{}` must be between 1 and 10000000", room.key));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }

    /// The history cutoff at `now` (unix ms), if there is a retention.
    pub(crate) fn cutoff(&self, now: i64) -> Option<i64> {
        (self.history_retention_days > 0).then(|| now.saturating_sub(i64::from(self.history_retention_days).saturating_mul(86_400_000)))
    }
}

/// Whether `name` is a room display name: 1-64 characters (the column's size), not blank, no
/// control or invisible characters.
pub(crate) fn is_valid_room_name(name: &str) -> bool {
    !name.trim().is_empty() && name.chars().count() <= 64 && net_backend_protocol::text::name_problem(name).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_problems() {
        assert!(ChatConfig::default().validate().is_ok());
        let bad = ChatConfig {
            max_text_chars: 5000,
            rooms: vec![RoomSpec::new("World"), RoomSpec::new("a"), RoomSpec::new("a").with_max_members(0)],
            moderator_roles: vec!["Mod".into()],
            ..ChatConfig::default()
        };
        let error = bad.validate().err().map(|e| e.to_string()).unwrap_or_default();
        for part in ["max_text_chars", "`World` is not a room key", "`a` is listed twice", "max_members of `a`", "`Mod` is not a role name"] {
            assert!(error.contains(part), "{part}: {error}");
        }
        let config = ChatConfig::default();
        assert_eq!(config.cutoff(31 * 86_400_000), Some(86_400_000));
        assert!(is_valid_room_name("Guild Hall") && !is_valid_room_name(" ") && !is_valid_room_name(&"x".repeat(65)) && !is_valid_room_name("a\u{202E}b"));
        let bad = ChatConfig { dm_open_window_secs: 0, ..ChatConfig::default() };
        assert!(bad.validate().err().map(|e| e.to_string()).unwrap_or_default().contains("dm_open_window_secs"));
        let forever = ChatConfig { history_retention_days: 0, ..ChatConfig::default() };
        assert_eq!(forever.cutoff(5), None);
    }
}
