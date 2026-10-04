//! The files module's settings: `[modules.files]` in the configuration (or
//! [`Files::with_config`](crate::files::Files::with_config)).
//!
//! ```toml
//! [modules.files]
//! dir = "/var/lib/my-game/files"   # where the local disk store keeps the bytes
//! max_file_bytes = 16777216        # 16 MiB per file (http.max_body_bytes must allow it)
//! max_files_per_user = 100
//! max_bytes_per_user = 268435456   # 256 MiB per player
//! upload_rate = 10                 # uploads per player: a burst of 10, then one every 6 s
//! upload_rate_window_secs = 60
//! allowed_content_types = []       # e.g. ["image/png", "application/x-replay", "image/*"]; empty: any
//! max_metadata_bytes = 4096
//! max_shared_with = 50             # accounts per shared file
//! purge_interval_secs = 3600       # remove stored bytes no file names (0 = never)
//! ```

use std::path::PathBuf;

use serde::Deserialize;

use crate::error::Error;

/// The files module's settings. Build in code from [`FilesConfig::default`] by changing fields,
/// or let the module read `[modules.files]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct FilesConfig {
    /// The folder of the built-in local disk store (created when missing). Default `files` (in
    /// the working directory). Ignored when the module is given its own store
    /// ([`Files::store`](crate::files::Files::store)).
    pub dir: PathBuf,
    /// The largest file, bytes. Default 16 MiB; at most `http.max_body_bytes` minus 64 KiB (the
    /// upload's other parts).
    pub max_file_bytes: u64,
    /// The most files one player owns. Default 100; 1 to 1 000 000.
    pub max_files_per_user: u32,
    /// The most bytes one player's files hold. Default 256 MiB.
    pub max_bytes_per_user: u64,
    /// Uploads per player in `upload_rate_window_secs` (a token bucket). Default 10; 0 = no limit.
    pub upload_rate: u32,
    /// The window of `upload_rate`, seconds. Default 60.
    pub upload_rate_window_secs: u32,
    /// The content types accepted (`image/png`, or `image/*` for a whole type); empty = any.
    /// Default empty.
    pub allowed_content_types: Vec<String>,
    /// The largest metadata (bytes of its JSON). Default 4096; at most 65 536.
    pub max_metadata_bytes: usize,
    /// The most accounts one file is shared with. Default 50; 1 to 100.
    pub max_shared_with: u32,
    /// How often stored bytes that no file row names (older than an hour) are deleted, in seconds
    /// ([`FileService::purge_orphans`](crate::files::FileService::purge_orphans)). Default 3600; 0 =
    /// never.
    pub purge_interval_secs: u64,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("files"),
            max_file_bytes: 16 * 1024 * 1024,
            max_files_per_user: 100,
            max_bytes_per_user: 256 * 1024 * 1024,
            upload_rate: 10,
            upload_rate_window_secs: 60,
            allowed_content_types: Vec::new(),
            max_metadata_bytes: net_backend_protocol::files::DEFAULT_MAX_METADATA_BYTES,
            max_shared_with: 50,
            purge_interval_secs: 3600,
        }
    }
}

/// The room an upload's other parts (boundaries, headers, the meta part) get on top of the file.
pub(crate) const UPLOAD_OVERHEAD_BYTES: u64 = 64 * 1024;

impl FilesConfig {
    /// Check every setting against the server's hard body cap (`http.max_body_bytes`).
    pub fn validate(&self, max_body_bytes: usize) -> Result<(), Error> {
        let mut problems = Vec::new();
        let cap = u64::try_from(max_body_bytes).unwrap_or(u64::MAX).saturating_sub(UPLOAD_OVERHEAD_BYTES);
        if self.max_file_bytes == 0 || self.max_file_bytes > cap {
            problems.push(format!(
                "modules.files.max_file_bytes must be between 1 and {cap} (http.max_body_bytes minus 64 KiB; raise http.max_body_bytes for larger files)"
            ));
        }
        if !(1..=1_000_000).contains(&self.max_files_per_user) {
            problems.push("modules.files.max_files_per_user must be between 1 and 1000000".to_string());
        }
        if self.max_bytes_per_user < self.max_file_bytes {
            problems.push("modules.files.max_bytes_per_user must be at least max_file_bytes".to_string());
        }
        if self.upload_rate > 0 && !(1..=86_400).contains(&self.upload_rate_window_secs) {
            problems.push("modules.files.upload_rate_window_secs must be between 1 and 86400".to_string());
        }
        if self.max_metadata_bytes > 65_536 {
            problems.push("modules.files.max_metadata_bytes must be at most 65536".to_string());
        }
        if !(1..=100).contains(&self.max_shared_with) {
            problems.push("modules.files.max_shared_with must be between 1 and 100".to_string());
        }
        for pattern in &self.allowed_content_types {
            let plain = net_backend_protocol::files::is_valid_content_type(pattern);
            let whole = pattern.strip_suffix("/*").is_some_and(|kind| net_backend_protocol::files::is_valid_content_type(&format!("{kind}/x")));
            if !plain && !whole {
                problems.push(format!("modules.files.allowed_content_types: `{pattern}` is not type/subtype or type/*"));
            }
        }
        if self.dir.as_os_str().is_empty() {
            problems.push("modules.files.dir must not be empty".to_string());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }

    /// Whether `content_type` is accepted.
    pub(crate) fn allows(&self, content_type: &str) -> bool {
        self.allowed_content_types.is_empty()
            || self.allowed_content_types.iter().any(|pattern| match pattern.strip_suffix("/*") {
                Some(kind) => content_type.split('/').next() == Some(kind),
                None => pattern == content_type,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_problems() {
        let cap = 32 * 1024 * 1024;
        assert!(FilesConfig::default().validate(cap).is_ok());
        let too_big = FilesConfig { max_file_bytes: 32 * 1024 * 1024, ..FilesConfig::default() };
        assert!(too_big.validate(cap).err().map(|e| e.to_string()).unwrap_or_default().contains("http.max_body_bytes"));
        let bad = FilesConfig {
            max_files_per_user: 0,
            max_bytes_per_user: 1,
            upload_rate_window_secs: 0,
            max_metadata_bytes: 100_000,
            max_shared_with: 0,
            allowed_content_types: vec!["image/*".into(), "nonsense".into()],
            ..FilesConfig::default()
        };
        let text = bad.validate(cap).err().map(|e| e.to_string()).unwrap_or_default();
        for part in ["max_files_per_user", "max_bytes_per_user", "upload_rate_window_secs", "max_metadata_bytes", "max_shared_with", "`nonsense`"] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(!text.contains("`image/*`"));
        let types = FilesConfig { allowed_content_types: vec!["image/*".into(), "application/x-replay".into()], ..FilesConfig::default() };
        assert!(types.allows("image/png") && types.allows("application/x-replay") && !types.allows("application/zip") && !types.allows("imagex/png"));
        assert!(FilesConfig::default().allows("anything/at-all"));
        assert!(toml::from_str::<FilesConfig>("max_file = 3\n").is_err(), "unknown keys are refused");
    }
}
