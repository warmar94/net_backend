//! The lobbies module's settings: `[modules.lobbies]` in the configuration (or
//! [`Lobbies::with_config`](crate::lobbies::Lobbies::with_config)).
//!
//! ```toml
//! [modules.lobbies]
//! max_players = 64                # the largest lobby a player may create
//! max_lobbies_per_user = 1        # lobbies one player is in at a time
//! max_metadata_keys = 32          # metadata keys per lobby
//! max_metadata_bytes = 4096       # metadata keys + values per lobby, in bytes
//! create_rate = 5                 # lobbies a player creates: a burst of 5, then one every
//! create_rate_window_secs = 60    # 60 / 5 = 12 seconds (token bucket); 0 = no limit
//! join_rate = 10                  # join attempts (by id or code): a burst of 10, then one
//! join_rate_window_secs = 60      # every 6 seconds; 0 = no limit
//! bad_code_rate = 20              # join codes that match no lobby: 20 per hour, then 429
//! bad_code_rate_window_secs = 3600
//! update_rate = 30                # host changes (PATCH, a new join code): a burst of 30, then
//! update_rate_window_secs = 60    # one every 2 seconds; 0 = no limit
//! leave_on_disconnect = true      # a player whose last WebSocket closes leaves its lobbies
//! disconnect_grace_secs = 30      # ... unless it reconnects within this time
//! chat_room = true                # a chat group room per lobby (with the chat module)
//! purge_interval_secs = 300       # remove lobbies left without members, repair host-less ones; 0 = never
//! ```

use serde::Deserialize;

use crate::error::Error;

/// The largest `max_players` a server may allow.
pub const MAX_PLAYERS_LIMIT: u32 = 1000;
/// The largest `max_metadata_bytes` a server may allow.
pub const MAX_METADATA_BYTES: usize = 64 * 1024;

/// The lobbies module's settings. Build in code from [`LobbiesConfig::default`] by changing
/// fields, or let the module read `[modules.lobbies]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct LobbiesConfig {
    /// The largest `max_players` of a lobby. Default 64; 1 to 1000.
    pub max_players: u32,
    /// How many lobbies one player is in at a time. Default 1; 1 to 100.
    pub max_lobbies_per_user: u32,
    /// Metadata keys per lobby. Default 32; 1 to 256.
    pub max_metadata_keys: u32,
    /// Metadata bytes per lobby (every key and value). Default 4096; at most 64 KiB.
    pub max_metadata_bytes: usize,
    /// Lobbies a player creates in `create_rate_window_secs` (a token bucket). Default 5; 0 = no
    /// limit.
    pub create_rate: u32,
    /// The window of `create_rate`, seconds. Default 60.
    pub create_rate_window_secs: u32,
    /// Join attempts of a player (by id or code) in `join_rate_window_secs`. Default 10; 0 = no
    /// limit.
    pub join_rate: u32,
    /// The window of `join_rate`, seconds. Default 60.
    pub join_rate_window_secs: u32,
    /// Join codes of a player that match no lobby in `bad_code_rate_window_secs`; past it every
    /// code attempt answers 429 until the window frees one. Default 20; 0 = no limit.
    pub bad_code_rate: u32,
    /// The window of `bad_code_rate`, seconds. Default 3600.
    pub bad_code_rate_window_secs: u32,
    /// Lobby changes of a player (`PATCH /v1/lobbies/{lobby}` and `POST …/code`) in
    /// `update_rate_window_secs` (a token bucket). Default 30; 0 = no limit.
    pub update_rate: u32,
    /// The window of `update_rate`, seconds. Default 60.
    pub update_rate_window_secs: u32,
    /// A player whose last WebSocket connection on this instance closes leaves its lobbies (after
    /// `disconnect_grace_secs`). Default true.
    pub leave_on_disconnect: bool,
    /// How long a disconnected player keeps its places, seconds. Default 30; at most 3600.
    pub disconnect_grace_secs: u32,
    /// Give every lobby a chat group room when the chat module is registered. Default true.
    pub chat_room: bool,
    /// How often lobbies without members are removed and lobbies whose host's account was deleted
    /// get the member who joined first as host, seconds. Default 300; 0 = never.
    pub purge_interval_secs: u64,
}

impl Default for LobbiesConfig {
    fn default() -> Self {
        Self {
            max_players: 64,
            max_lobbies_per_user: 1,
            max_metadata_keys: 32,
            max_metadata_bytes: 4096,
            create_rate: 5,
            create_rate_window_secs: 60,
            join_rate: 10,
            join_rate_window_secs: 60,
            bad_code_rate: 20,
            bad_code_rate_window_secs: 3600,
            update_rate: 30,
            update_rate_window_secs: 60,
            leave_on_disconnect: true,
            disconnect_grace_secs: 30,
            chat_room: true,
            purge_interval_secs: 300,
        }
    }
}

impl LobbiesConfig {
    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        if !(1..=MAX_PLAYERS_LIMIT).contains(&self.max_players) {
            problems.push(format!("modules.lobbies.max_players must be between 1 and {MAX_PLAYERS_LIMIT}"));
        }
        if !(1..=100).contains(&self.max_lobbies_per_user) {
            problems.push("modules.lobbies.max_lobbies_per_user must be between 1 and 100".to_string());
        }
        if !(1..=256).contains(&self.max_metadata_keys) {
            problems.push("modules.lobbies.max_metadata_keys must be between 1 and 256".to_string());
        }
        if !(1..=MAX_METADATA_BYTES).contains(&self.max_metadata_bytes) {
            problems.push(format!("modules.lobbies.max_metadata_bytes must be between 1 and {MAX_METADATA_BYTES}"));
        }
        for (name, rate, window) in [
            ("create_rate", self.create_rate, self.create_rate_window_secs),
            ("join_rate", self.join_rate, self.join_rate_window_secs),
            ("bad_code_rate", self.bad_code_rate, self.bad_code_rate_window_secs),
            ("update_rate", self.update_rate, self.update_rate_window_secs),
        ] {
            if rate > 0 && !(1..=7 * 86_400).contains(&window) {
                problems.push(format!("modules.lobbies.{name}_window_secs must be between 1 and 604800"));
            }
        }
        if self.disconnect_grace_secs > 3600 {
            problems.push("modules.lobbies.disconnect_grace_secs must be at most 3600".to_string());
        }
        if self.purge_interval_secs > 86_400 {
            problems.push("modules.lobbies.purge_interval_secs must be at most 86400 (0 = never)".to_string());
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
        assert!(LobbiesConfig::default().validate().is_ok());
        let bad = LobbiesConfig {
            max_players: 0,
            max_lobbies_per_user: 0,
            max_metadata_keys: 0,
            max_metadata_bytes: MAX_METADATA_BYTES + 1,
            join_rate: 1,
            join_rate_window_secs: 0,
            update_rate: 1,
            update_rate_window_secs: 0,
            disconnect_grace_secs: 3601,
            purge_interval_secs: 86_401,
            ..LobbiesConfig::default()
        };
        let error = bad.validate().err().map(|e| e.to_string()).unwrap_or_default();
        for part in [
            "max_players",
            "max_lobbies_per_user",
            "max_metadata_keys",
            "max_metadata_bytes",
            "join_rate_window_secs",
            "update_rate_window_secs",
            "disconnect_grace_secs",
            "purge_interval_secs",
        ] {
            assert!(error.contains(part), "{part}: {error}");
        }
        assert!(toml::from_str::<LobbiesConfig>("max_player = 3\n").is_err(), "unknown keys are refused");
    }
}
