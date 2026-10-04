//! The friends module's settings: `[modules.friends]` in the configuration (or
//! [`Friends::with_config`](crate::friends::Friends::with_config)).
//!
//! ```toml
//! [modules.friends]
//! max_friends = 200              # friends per player
//! max_pending = 50               # open requests per player, sent and received each
//! max_blocks = 500               # blocked players per player
//! request_rate = 10              # friend requests per player: a burst of 10, then one every
//! request_rate_window_secs = 60  # 60 / 10 = 6 s (token bucket); 0 = no limit
//! online_window_secs = 90        # how long a heartbeat keeps a player online
//! presence = true                # push friends.presence to friends (with the WebSocket hub)
//! notify = true                  # friends.request / friends.accepted notifications (with the notifications module)
//! steam_max_ids = 500            # Steam IDs per POST /v1/friends/steam (1 to 2000)
//! steam_rate = 3                 # Steam ID lookups per player: a burst of 3, then one every
//! steam_rate_window_secs = 900   # 900 / 3 = 300 s (token bucket); 0 = no limit
//! update_rate = 30               # heartbeats, friend-code resets, settings changes per player:
//! update_rate_window_secs = 60   # a burst of 30, then one every 2 s (token bucket); 0 = no limit
//! ```

use serde::Deserialize;

use crate::error::Error;

/// The friends module's settings. Build in code from [`FriendsConfig::default`] by changing
/// fields, or let the module read `[modules.friends]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct FriendsConfig {
    /// The most friends one player has. Default 200; 1 to 10 000.
    pub max_friends: u32,
    /// The most open requests one player has sent, and the most it has received. Default 50; 1 to
    /// 10 000.
    pub max_pending: u32,
    /// The most players one player blocks. Default 500; 1 to 100 000.
    pub max_blocks: u32,
    /// Friend requests per player in `request_rate_window_secs` (a token bucket). Default 10; 0 = no
    /// limit.
    pub request_rate: u32,
    /// The window of `request_rate`, seconds. Default 60.
    pub request_rate_window_secs: u32,
    /// How long a player stays online after a heartbeat (`POST /v1/friends/presence`), and how fresh
    /// the stored online time of connected players is kept. Default 90; 10 to 3600.
    pub online_window_secs: u32,
    /// Push `friends.presence` to a player's friends when the player comes online or goes offline.
    /// Default true.
    pub presence: bool,
    /// Send `friends.request` / `friends.accepted` notifications when the notifications module is
    /// registered. Default true.
    pub notify: bool,
    /// The most Steam IDs one `POST /v1/friends/steam` carries. Default 500; 1 to 2000.
    pub steam_max_ids: u32,
    /// Steam ID lookups (`POST /v1/friends/steam`) per player in `steam_rate_window_secs` (a token
    /// bucket). Default 3; 0 = no limit.
    pub steam_rate: u32,
    /// The window of `steam_rate`, seconds. Default 900 (with 3: one lookup every 5 minutes after
    /// the burst).
    pub steam_rate_window_secs: u32,
    /// Heartbeats (`POST /v1/friends/presence`), friend-code resets (`POST /v1/friends/code`) and
    /// settings changes (`PUT /v1/friends/settings`) per player in `update_rate_window_secs` (one
    /// token bucket for the three). Default 30; 0 = no limit.
    pub update_rate: u32,
    /// The window of `update_rate`, seconds. Default 60.
    pub update_rate_window_secs: u32,
}

impl Default for FriendsConfig {
    fn default() -> Self {
        Self {
            max_friends: 200,
            max_pending: 50,
            max_blocks: 500,
            request_rate: 10,
            request_rate_window_secs: 60,
            online_window_secs: 90,
            presence: true,
            notify: true,
            steam_max_ids: 500,
            steam_rate: 3,
            steam_rate_window_secs: 900,
            update_rate: 30,
            update_rate_window_secs: 60,
        }
    }
}

impl FriendsConfig {
    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        if !(1..=10_000).contains(&self.max_friends) {
            problems.push("modules.friends.max_friends must be between 1 and 10000".to_string());
        }
        if !(1..=10_000).contains(&self.max_pending) {
            problems.push("modules.friends.max_pending must be between 1 and 10000".to_string());
        }
        if !(1..=100_000).contains(&self.max_blocks) {
            problems.push("modules.friends.max_blocks must be between 1 and 100000".to_string());
        }
        if self.request_rate > 0 && !(1..=86_400).contains(&self.request_rate_window_secs) {
            problems.push("modules.friends.request_rate_window_secs must be between 1 and 86400".to_string());
        }
        if !(1..=net_backend_protocol::friends::MAX_STEAM_IDS as u32).contains(&self.steam_max_ids) {
            problems.push(format!("modules.friends.steam_max_ids must be between 1 and {}", net_backend_protocol::friends::MAX_STEAM_IDS));
        }
        if self.steam_rate > 0 && !(1..=86_400).contains(&self.steam_rate_window_secs) {
            problems.push("modules.friends.steam_rate_window_secs must be between 1 and 86400".to_string());
        }
        if self.update_rate > 0 && !(1..=86_400).contains(&self.update_rate_window_secs) {
            problems.push("modules.friends.update_rate_window_secs must be between 1 and 86400".to_string());
        }
        if !(10..=3600).contains(&self.online_window_secs) {
            problems.push("modules.friends.online_window_secs must be between 10 and 3600".to_string());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }

    /// The online window in milliseconds.
    pub(crate) fn window_ms(&self) -> i64 {
        i64::from(self.online_window_secs) * 1000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_problems() {
        assert!(FriendsConfig::default().validate().is_ok());
        let bad = FriendsConfig {
            max_friends: 0,
            max_pending: 20_000,
            max_blocks: 0,
            request_rate: 5,
            request_rate_window_secs: 0,
            online_window_secs: 5,
            steam_max_ids: 2001,
            steam_rate: 1,
            steam_rate_window_secs: 0,
            update_rate: 1,
            update_rate_window_secs: 0,
            ..FriendsConfig::default()
        };
        let error = bad.validate().err().map(|e| e.to_string()).unwrap_or_default();
        for part in [
            "max_friends",
            "max_pending",
            "max_blocks",
            "request_rate_window_secs",
            "online_window_secs",
            "steam_max_ids",
            "steam_rate_window_secs",
            "update_rate_window_secs",
        ] {
            assert!(error.contains(part), "{part}: {error}");
        }
        assert!(FriendsConfig { request_rate: 0, request_rate_window_secs: 0, ..FriendsConfig::default() }.validate().is_ok(), "no rate: no window");
        assert!(FriendsConfig { steam_rate: 0, steam_rate_window_secs: 0, ..FriendsConfig::default() }.validate().is_ok(), "no Steam rate: no window");
        assert!(FriendsConfig { steam_max_ids: 0, ..FriendsConfig::default() }.validate().is_err());
        assert!(toml::from_str::<FriendsConfig>("max_friend = 3\n").is_err(), "unknown keys are refused");
        assert_eq!(FriendsConfig::default().window_ms(), 90_000);
    }
}
