//! The groups module's settings: `[modules.groups]` in the configuration (or
//! [`Groups::with_config`](crate::groups::Groups::with_config)).
//!
//! ```toml
//! [modules.groups]
//! max_members = 100              # members per group
//! max_groups_per_user = 10       # groups one player belongs to
//! max_invites = 50               # open invitations per group
//! max_metadata_bytes = 2048      # the JSON metadata of a group
//! create_rate = 3                # groups a player creates: a burst of 3, then one every
//! create_rate_window_secs = 3600 # 3600 / 3 = 20 minutes (token bucket); 0 = no limit
//! invite_rate = 20               # invitations a player sends: a burst of 20, then one every
//! invite_rate_window_secs = 600  # 600 / 20 = 30 s (token bucket); 0 = no limit
//! chat_room = true               # a chat group room per group (with the chat module)
//! notify = true                  # groups.invite / groups.kicked notifications (with the notifications module)
//! upkeep_interval_secs = 3600    # give groups whose owner's account was deleted a new owner; 0 = never
//! ```

use net_backend_protocol::groups::DEFAULT_MAX_METADATA_BYTES;
use serde::Deserialize;

use crate::error::Error;

/// The largest `max_metadata_bytes` a server may allow.
pub const MAX_METADATA_BYTES: usize = 16 * 1024;

/// The groups module's settings. Build in code from [`GroupsConfig::default`] by changing fields,
/// or let the module read `[modules.groups]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct GroupsConfig {
    /// The most members of one group. Default 100; 2 to 100 000.
    pub max_members: u32,
    /// The most groups one player belongs to. Default 10; 1 to 1000.
    pub max_groups_per_user: u32,
    /// The most open invitations of one group. Default 50; 1 to 10 000.
    pub max_invites: u32,
    /// The largest `metadata` of a group, in bytes of its JSON. Default 2048; at most 16 KiB.
    pub max_metadata_bytes: usize,
    /// Groups a player creates in `create_rate_window_secs` (a token bucket). Default 3; 0 = no limit.
    pub create_rate: u32,
    /// The window of `create_rate`, seconds. Default 3600.
    pub create_rate_window_secs: u32,
    /// Invitations a player sends (`POST /v1/groups/{group}/invites`) in
    /// `invite_rate_window_secs` (a token bucket). It bounds invitation notifications. Default 20;
    /// 0 = no limit.
    pub invite_rate: u32,
    /// The window of `invite_rate`, seconds. Default 600.
    pub invite_rate_window_secs: u32,
    /// Give every group a chat group room when the chat module is registered. Default true.
    pub chat_room: bool,
    /// Send `groups.invite` / `groups.kicked` notifications when the notifications module is
    /// registered. Default true.
    pub notify: bool,
    /// How often the background task gives groups whose owner's account was deleted a new owner
    /// (the oldest admin, else the oldest member; a group with no member left is deleted), seconds.
    /// Default 3600; 0 = never; at most 86400.
    pub upkeep_interval_secs: u64,
}

impl Default for GroupsConfig {
    fn default() -> Self {
        Self {
            max_members: 100,
            max_groups_per_user: 10,
            max_invites: 50,
            max_metadata_bytes: DEFAULT_MAX_METADATA_BYTES,
            create_rate: 3,
            create_rate_window_secs: 3600,
            invite_rate: 20,
            invite_rate_window_secs: 600,
            chat_room: true,
            notify: true,
            upkeep_interval_secs: 3600,
        }
    }
}

impl GroupsConfig {
    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        if !(2..=100_000).contains(&self.max_members) {
            problems.push("modules.groups.max_members must be between 2 and 100000".to_string());
        }
        if !(1..=1000).contains(&self.max_groups_per_user) {
            problems.push("modules.groups.max_groups_per_user must be between 1 and 1000".to_string());
        }
        if !(1..=10_000).contains(&self.max_invites) {
            problems.push("modules.groups.max_invites must be between 1 and 10000".to_string());
        }
        if !(1..=MAX_METADATA_BYTES).contains(&self.max_metadata_bytes) {
            problems.push(format!("modules.groups.max_metadata_bytes must be between 1 and {MAX_METADATA_BYTES}"));
        }
        if self.create_rate > 0 && !(1..=7 * 86_400).contains(&self.create_rate_window_secs) {
            problems.push("modules.groups.create_rate_window_secs must be between 1 and 604800".to_string());
        }
        if self.invite_rate > 0 && !(1..=7 * 86_400).contains(&self.invite_rate_window_secs) {
            problems.push("modules.groups.invite_rate_window_secs must be between 1 and 604800".to_string());
        }
        if self.upkeep_interval_secs > 86_400 {
            problems.push("modules.groups.upkeep_interval_secs must be at most 86400 (0 = never)".to_string());
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
        assert!(GroupsConfig::default().validate().is_ok());
        let bad = GroupsConfig {
            max_members: 1,
            max_groups_per_user: 0,
            max_invites: 0,
            max_metadata_bytes: MAX_METADATA_BYTES + 1,
            create_rate: 1,
            create_rate_window_secs: 0,
            invite_rate: 1,
            invite_rate_window_secs: 0,
            upkeep_interval_secs: 86_401,
            ..GroupsConfig::default()
        };
        let error = bad.validate().err().map(|e| e.to_string()).unwrap_or_default();
        for part in [
            "max_members",
            "max_groups_per_user",
            "max_invites",
            "max_metadata_bytes",
            "create_rate_window_secs",
            "invite_rate_window_secs",
            "upkeep_interval_secs",
        ] {
            assert!(error.contains(part), "{part}: {error}");
        }
        assert!(toml::from_str::<GroupsConfig>("max_member = 3\n").is_err(), "unknown keys are refused");
    }
}
