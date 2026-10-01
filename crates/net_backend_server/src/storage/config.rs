//! The storage module's settings: `[modules.storage]` in the configuration (or
//! [`Storage::with_config`](crate::storage::Storage::with_config)).
//!
//! ```toml
//! [modules.storage]
//! max_object_bytes = 262144      # per value (its JSON); at most 4 MiB
//! max_objects_per_user = 1000
//! max_bytes_per_user = 4194304   # all of a user's values together (4 MiB)
//! write_rate = 60                # owner writes / deletes / batch puts per user ...
//! write_rate_window_secs = 60    # ... within this many seconds (token bucket); 0 = no limit
//! server_collections = ["server"]  # owners never write here (also `server.*`)
//! admin_in_openapi = false       # list the /v1/admin storage routes in /v1/openapi.json
//! ```

use net_backend_protocol::storage::{DEFAULT_MAX_OBJECTS_PER_USER, DEFAULT_MAX_OBJECT_BYTES, MAX_BATCH_BYTES};
use serde::Deserialize;

use crate::error::Error;

/// The default of [`StorageConfig::max_bytes_per_user`]: 4 MiB, one full batch.
pub const DEFAULT_MAX_BYTES_PER_USER: u64 = MAX_BATCH_BYTES as u64;

/// The storage module's settings. Build in code from [`StorageConfig::default`] by changing
/// fields, or let the module read `[modules.storage]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct StorageConfig {
    /// The largest value, in bytes of its JSON. Default 256 KiB (the protocol's default); at most
    /// 4 MiB (a batch read of larger objects may then exceed the batch budget and answer 413).
    pub max_object_bytes: usize,
    /// The most objects one user may own (over it: 403 `quota_exceeded`). Default 1000. Counts
    /// every object, but only the owner's own writes are refused by it (server code and admins
    /// may always write).
    pub max_objects_per_user: u32,
    /// The most bytes (of value JSON) all of one user's objects may hold together (over it: 403
    /// `quota_exceeded`). Default 4 MiB ([`DEFAULT_MAX_BYTES_PER_USER`]): sixteen objects of the
    /// largest default size, or hundreds of typical saves, and a bounded disk cost per account
    /// (10 000 accounts at most ~40 GiB). Like the object quota it binds only the owner's writes.
    pub max_bytes_per_user: u64,
    /// Owner writes (PUT, DELETE, each batch put) per user within `write_rate_window_secs`, as a
    /// token bucket: a burst of this many, then one every `window / write_rate` (over it: 429
    /// `rate_limited` + `retry_after_ms`). Default 60 (per 60 s: a burst of 60, then one a second).
    /// 0 = no limit. Server code and admins are not limited.
    pub write_rate: u32,
    /// The window of `write_rate`, seconds. Default 60.
    pub write_rate_window_secs: u32,
    /// Server-owned collections: the owner may read objects there but never create, change or
    /// delete one (403 `forbidden`), so a player cannot pre-create an object the server will own
    /// (e.g. `wallet/gold`). An entry `x` covers the collection `x` and every collection starting
    /// with `x.` (`server` covers `server` and `server.wallet`). New objects that server code or an
    /// admin creates there are server-locked unless `write` says otherwise. Default `["server"]`.
    pub server_collections: Vec<String>,
    /// List the `/v1/admin/users/{user}/storage` routes in the OpenAPI document. Default false
    /// (like the auth module's admin routes).
    pub admin_in_openapi: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            max_objects_per_user: DEFAULT_MAX_OBJECTS_PER_USER,
            max_bytes_per_user: DEFAULT_MAX_BYTES_PER_USER,
            write_rate: 60,
            write_rate_window_secs: 60,
            server_collections: vec!["server".into()],
            admin_in_openapi: false,
        }
    }
}

impl StorageConfig {
    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        if !(1..=MAX_BATCH_BYTES).contains(&self.max_object_bytes) {
            problems.push(format!("modules.storage.max_object_bytes must be between 1 and {MAX_BATCH_BYTES}"));
        }
        if !(1..=100_000_000).contains(&self.max_objects_per_user) {
            problems.push("modules.storage.max_objects_per_user must be between 1 and 100000000".to_string());
        }
        if !(1..=(1u64 << 40)).contains(&self.max_bytes_per_user) {
            problems.push("modules.storage.max_bytes_per_user must be between 1 and 1099511627776 (1 TiB)".to_string());
        }
        if self.write_rate > 100_000 {
            problems.push("modules.storage.write_rate must be at most 100000 (0 = no limit)".to_string());
        }
        if !(1..=86_400).contains(&self.write_rate_window_secs) {
            problems.push("modules.storage.write_rate_window_secs must be between 1 and 86400".to_string());
        }
        for entry in &self.server_collections {
            if !net_backend_protocol::storage::is_valid_name(entry) {
                problems.push(format!("modules.storage.server_collections: `{entry}` is not a collection name"));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }

    /// Whether the owner may never write in `collection` (`server_collections`).
    pub fn is_server_collection(&self, collection: &str) -> bool {
        self.server_collections.iter().any(|entry| collection == entry || collection.strip_prefix(entry.as_str()).is_some_and(|rest| rest.starts_with('.')))
    }

    /// The request body limit of a single-object PUT: the value plus room for the envelope (at
    /// least the protocol's `PUT_BODY_LIMIT_BYTES`).
    pub fn put_body_limit(&self) -> usize {
        net_backend_protocol::storage::PUT_BODY_LIMIT_BYTES.max(self.max_object_bytes.saturating_add(16 * 1024))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_ranges() {
        let config = StorageConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.put_body_limit(), net_backend_protocol::storage::PUT_BODY_LIMIT_BYTES);
        let mut big = StorageConfig { max_object_bytes: MAX_BATCH_BYTES, ..StorageConfig::default() };
        assert!(big.validate().is_ok());
        assert_eq!(big.put_body_limit(), MAX_BATCH_BYTES + 16 * 1024);
        big.max_object_bytes = MAX_BATCH_BYTES + 1;
        big.max_objects_per_user = 0;
        big.max_bytes_per_user = 0;
        big.write_rate_window_secs = 0;
        big.server_collections = vec!["../x".into()];
        let error = big.validate().err().map(|e| e.to_string()).unwrap_or_default();
        for part in ["max_object_bytes", "max_objects_per_user", "max_bytes_per_user", "write_rate_window_secs", "`../x`"] {
            assert!(error.contains(part), "{part}: {error}");
        }
        let config = StorageConfig { server_collections: vec!["server".into(), "wallet".into()], ..StorageConfig::default() };
        for (collection, reserved) in
            [("server", true), ("server.gold", true), ("wallet", true), ("wallet.x.y", true), ("servers", false), ("walletx", false), ("saves", false)]
        {
            assert_eq!(config.is_server_collection(collection), reserved, "{collection}");
        }
    }
}
