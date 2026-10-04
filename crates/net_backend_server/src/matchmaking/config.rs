//! The matchmaking module's settings: `[modules.matchmaking]` in the configuration (or
//! [`Matchmaking::with_config`](crate::matchmaking::Matchmaking::with_config)).
//!
//! ```toml
//! [modules.matchmaking]
//! interval_ms = 1000            # how often each queue is matched; 0 = only when server code calls run_round
//! max_attributes_bytes = 1024   # a ticket's attributes
//! max_tickets = 100000          # tickets the server holds at once
//! ticket_rate = 10              # tickets a player makes: a burst of 10, then one every
//! ticket_rate_window_secs = 60  # 60 / 10 = 6 seconds (token bucket); 0 = no limit
//! matched_keep_secs = 60        # how long a matched ticket answers GET /v1/matchmaking/ticket
//! cancel_on_disconnect = true   # a player whose last WebSocket closes leaves its queue
//!
//! [[modules.matchmaking.queues]]
//! key = "duel"                  # a-z 0-9 _ - .
//! players = 2                   # players per match (the default rule: first come, first matched)
//! timeout_secs = 120            # a waiting ticket runs out after this (match.expired)
//! ```

use std::collections::HashSet;

use net_backend_protocol::matchmaking::{is_valid_queue_key, DEFAULT_MAX_ATTRIBUTES_BYTES};
use serde::Deserialize;

use crate::error::Error;

/// The largest `max_attributes_bytes` a server may allow.
pub const MAX_ATTRIBUTES_BYTES: usize = 16 * 1024;

/// One queue.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct QueueSpec {
    /// The key tickets name (`a-z 0-9 _ - .`, at most 64 bytes).
    pub key: String,
    /// Players per match of the default rule (the game's rules may form other sizes). 1 to 100.
    pub players: u32,
    /// How long a ticket waits before it runs out, seconds. Default 120; 1 to 86400.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u32,
}

fn default_timeout() -> u32 {
    120
}

impl QueueSpec {
    /// A queue of `players`-player matches whose tickets wait 120 seconds.
    pub fn new(key: impl Into<String>, players: u32) -> Self {
        Self { key: key.into(), players, timeout_secs: default_timeout() }
    }

    /// The same queue with another ticket timeout.
    pub fn with_timeout_secs(mut self, timeout_secs: u32) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }
}

/// The matchmaking module's settings. Build in code from [`MatchmakingConfig::default`] by
/// changing fields, or let the module read `[modules.matchmaking]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct MatchmakingConfig {
    /// The queues (none by default: a server names its own).
    pub queues: Vec<QueueSpec>,
    /// How often every queue is matched, milliseconds. Default 1000; 0 = only when server code
    /// calls [`MatchmakingService::run_round`](crate::matchmaking::MatchmakingService::run_round);
    /// else 50 to 60000.
    pub interval_ms: u64,
    /// The largest ticket `attributes`, in bytes of their JSON. Default 1024; at most 16 KiB.
    pub max_attributes_bytes: usize,
    /// Tickets the server holds at once (waiting and recently matched). Default 100000.
    pub max_tickets: usize,
    /// Tickets a player makes in `ticket_rate_window_secs` (a token bucket). Default 10; 0 = no
    /// limit.
    pub ticket_rate: u32,
    /// The window of `ticket_rate`, seconds. Default 60.
    pub ticket_rate_window_secs: u32,
    /// How long a matched ticket stays readable, seconds. Default 60; 1 to 3600.
    pub matched_keep_secs: u32,
    /// A player whose last WebSocket connection on this instance closes leaves its queue. Default
    /// true.
    pub cancel_on_disconnect: bool,
}

impl Default for MatchmakingConfig {
    fn default() -> Self {
        Self {
            queues: Vec::new(),
            interval_ms: 1000,
            max_attributes_bytes: DEFAULT_MAX_ATTRIBUTES_BYTES,
            max_tickets: 100_000,
            ticket_rate: 10,
            ticket_rate_window_secs: 60,
            matched_keep_secs: 60,
            cancel_on_disconnect: true,
        }
    }
}

impl MatchmakingConfig {
    /// The same settings with one more queue.
    pub fn with_queue(mut self, queue: QueueSpec) -> Self {
        self.queues.push(queue);
        self
    }

    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        let mut seen = HashSet::new();
        for queue in &self.queues {
            if !is_valid_queue_key(&queue.key) {
                problems.push(format!("modules.matchmaking.queues: `{}` is not a valid queue key (a-z 0-9 _ - ., at most 64 bytes)", queue.key));
            }
            if !seen.insert(queue.key.as_str()) {
                problems.push(format!("modules.matchmaking.queues: `{}` is listed twice", queue.key));
            }
            if !(1..=100).contains(&queue.players) {
                problems.push(format!("modules.matchmaking.queues: `{}` players must be between 1 and 100", queue.key));
            }
            if !(1..=86_400).contains(&queue.timeout_secs) {
                problems.push(format!("modules.matchmaking.queues: `{}` timeout_secs must be between 1 and 86400", queue.key));
            }
        }
        if self.interval_ms != 0 && !(50..=60_000).contains(&self.interval_ms) {
            problems.push("modules.matchmaking.interval_ms must be 0 or between 50 and 60000".to_string());
        }
        if !(1..=MAX_ATTRIBUTES_BYTES).contains(&self.max_attributes_bytes) {
            problems.push(format!("modules.matchmaking.max_attributes_bytes must be between 1 and {MAX_ATTRIBUTES_BYTES}"));
        }
        if !(1..=10_000_000).contains(&self.max_tickets) {
            problems.push("modules.matchmaking.max_tickets must be between 1 and 10000000".to_string());
        }
        if self.ticket_rate > 0 && !(1..=7 * 86_400).contains(&self.ticket_rate_window_secs) {
            problems.push("modules.matchmaking.ticket_rate_window_secs must be between 1 and 604800".to_string());
        }
        if !(1..=3600).contains(&self.matched_keep_secs) {
            problems.push("modules.matchmaking.matched_keep_secs must be between 1 and 3600".to_string());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_problems() {
        assert!(MatchmakingConfig::default().validate().is_ok());
        assert!(MatchmakingConfig::default().with_queue(QueueSpec::new("duel", 2)).validate().is_ok());
        let bad = MatchmakingConfig {
            queues: vec![QueueSpec::new("Duel", 0), QueueSpec::new("x", 2).with_timeout_secs(0), QueueSpec::new("x", 2)],
            interval_ms: 10,
            max_attributes_bytes: 0,
            matched_keep_secs: 0,
            ..MatchmakingConfig::default()
        };
        let error = bad.validate().err().map(|e| e.to_string()).unwrap_or_default();
        for part in
            ["`Duel` is not a valid", "`Duel` players", "`x` timeout_secs", "`x` is listed twice", "interval_ms", "max_attributes_bytes", "matched_keep_secs"]
        {
            assert!(error.contains(part), "{part}: {error}");
        }
        let file: MatchmakingConfig = toml::from_str("[[queues]]\nkey = \"duel\"\nplayers = 2\n").expect("toml");
        assert_eq!(file.queues, vec![QueueSpec::new("duel", 2)]);
        assert!(toml::from_str::<MatchmakingConfig>("interval = 3\n").is_err(), "unknown keys are refused");
    }
}
