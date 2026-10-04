//! The notifications module's settings: `[modules.notifications]` in the configuration (or
//! [`Notifications::with_config`](crate::notifications::Notifications::with_config)).
//!
//! ```toml
//! [modules.notifications]
//! retention_days = 30            # older notifications are deleted; 0 = keep them
//! max_per_user = 200             # beyond it a new notification deletes the player's oldest
//! max_data_bytes = 4096          # the JSON `data` of one notification
//! purge_interval_secs = 3600     # how often the retention purge runs (0: never)
//! ```

use net_backend_protocol::notifications::DEFAULT_MAX_DATA_BYTES;
use serde::Deserialize;

use crate::error::Error;

/// The largest `max_data_bytes` a server may allow.
pub const MAX_DATA_BYTES: usize = 64 * 1024;

/// The notifications module's settings. Build in code from [`NotificationsConfig::default`] by
/// changing fields, or let the module read `[modules.notifications]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct NotificationsConfig {
    /// Notifications older than this many days are deleted by a background task (read or not).
    /// Default 30; 0 keeps them.
    pub retention_days: u32,
    /// The most notifications one player keeps: a new one beyond it deletes the player's oldest.
    /// Default 200; 1 to 10 000.
    pub max_per_user: u32,
    /// The largest `data` of one notification, in bytes of its JSON. Default 4096; at most 64 KiB.
    pub max_data_bytes: usize,
    /// How often the retention purge runs, seconds (0: never). Default 3600.
    pub purge_interval_secs: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { retention_days: 30, max_per_user: 200, max_data_bytes: DEFAULT_MAX_DATA_BYTES, purge_interval_secs: 3600 }
    }
}

impl NotificationsConfig {
    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        if self.retention_days > 36_500 {
            problems.push("modules.notifications.retention_days must be at most 36500 (0 = keep them)".to_string());
        }
        if !(1..=10_000).contains(&self.max_per_user) {
            problems.push("modules.notifications.max_per_user must be between 1 and 10000".to_string());
        }
        if !(1..=MAX_DATA_BYTES).contains(&self.max_data_bytes) {
            problems.push(format!("modules.notifications.max_data_bytes must be between 1 and {MAX_DATA_BYTES}"));
        }
        if self.purge_interval_secs > 7 * 86_400 {
            problems.push("modules.notifications.purge_interval_secs must be at most 604800".to_string());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }

    /// The retention cutoff at `now` (unix ms), if there is a retention.
    pub(crate) fn cutoff(&self, now: i64) -> Option<i64> {
        (self.retention_days > 0).then(|| now.saturating_sub(i64::from(self.retention_days).saturating_mul(86_400_000)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_problems() {
        assert!(NotificationsConfig::default().validate().is_ok());
        let bad = NotificationsConfig { max_per_user: 0, max_data_bytes: MAX_DATA_BYTES + 1, retention_days: 40_000, purge_interval_secs: 10_000_000 };
        let error = bad.validate().err().map(|e| e.to_string()).unwrap_or_default();
        for part in ["max_per_user", "max_data_bytes", "retention_days", "purge_interval_secs"] {
            assert!(error.contains(part), "{part}: {error}");
        }
        assert_eq!(NotificationsConfig::default().cutoff(31 * 86_400_000), Some(86_400_000));
        assert_eq!(NotificationsConfig { retention_days: 0, ..NotificationsConfig::default() }.cutoff(5), None);
        assert!(toml::from_str::<NotificationsConfig>("retention = 3\n").is_err(), "unknown keys are refused");
    }
}
