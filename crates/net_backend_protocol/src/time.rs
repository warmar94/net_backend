//! Timestamps: [`UnixMillis`], milliseconds since 1970-01-01 00:00:00 UTC as a signed 64-bit
//! integer (a `BIGINT` column on every supported database, no time zones, no 2038 problem).
//! On the wire it is a plain JSON number: `{"sent_at":1790000000000}`.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A point in time: milliseconds since the Unix epoch (UTC). Negative values are before 1970.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMillis(pub i64);

impl UnixMillis {
    /// The epoch itself (0).
    pub const EPOCH: UnixMillis = UnixMillis(0);

    /// The timestamp with this many milliseconds since the epoch.
    pub const fn new(millis: i64) -> Self {
        Self(millis)
    }

    /// The milliseconds since the epoch.
    pub const fn get(self) -> i64 {
        self.0
    }

    /// The current time from the system clock (saturating at the `i64` range).
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// A `SystemTime` as milliseconds (saturating at the `i64` range; sub-millisecond parts are
    /// dropped, towards the epoch).
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self(i64::try_from(after.as_millis()).unwrap_or(i64::MAX)),
            Err(before) => Self(i64::try_from(before.duration().as_millis()).map(|m| -m).unwrap_or(i64::MIN)),
        }
    }

    /// The timestamp as a `SystemTime` (`None` if the platform cannot represent it).
    pub fn to_system_time(self) -> Option<SystemTime> {
        let magnitude = Duration::from_millis(self.0.unsigned_abs());
        if self.0 >= 0 {
            UNIX_EPOCH.checked_add(magnitude)
        } else {
            UNIX_EPOCH.checked_sub(magnitude)
        }
    }

    /// This timestamp plus `millis` (saturating).
    pub const fn saturating_add_millis(self, millis: i64) -> Self {
        Self(self.0.saturating_add(millis))
    }
}

impl fmt::Display for UnixMillis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<i64> for UnixMillis {
    fn from(millis: i64) -> Self {
        Self(millis)
    }
}

impl From<UnixMillis> for i64 {
    fn from(time: UnixMillis) -> Self {
        time.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_millis() {
        assert_eq!(serde_json::to_string(&UnixMillis(1_790_000_000_000)).ok().as_deref(), Some("1790000000000"));
        assert_eq!(UnixMillis::from_system_time(UNIX_EPOCH + Duration::from_millis(1500)), UnixMillis(1500));
        assert_eq!(UnixMillis::from_system_time(UNIX_EPOCH - Duration::from_millis(1500)), UnixMillis(-1500));
        assert_eq!(UnixMillis(1500).to_system_time(), Some(UNIX_EPOCH + Duration::from_millis(1500)));
        assert_eq!(UnixMillis(-1500).to_system_time(), Some(UNIX_EPOCH - Duration::from_millis(1500)));
        assert!(UnixMillis::now() > UnixMillis(1_700_000_000_000));
        assert_eq!(UnixMillis(i64::MAX).saturating_add_millis(5), UnixMillis(i64::MAX));
        assert_eq!(UnixMillis(7).to_string(), "7");
    }
}
