//! [`TokenFile`]: the session's tokens in a file, so the next start resumes the session.
//!
//! The file is JSON: `{"format":1,"server":"<the server URL>","tokens":{<TokenPair>}}`. It is
//! written atomically (a new file next to it, flushed to disk, then renamed over the old one), so a
//! crash never leaves half a file. The text buffers that hold the tokens are overwritten with zeros
//! when dropped.
//!
//! Who can read it:
//! - Unix: the file is created with mode `0600` (owner read / write only), a missing folder with
//!   `0700`.
//! - Windows: the file gets the permissions (ACL) it inherits from its folder. Under the user's
//!   profile (e.g. `%APPDATA%\<game>\` or `%LOCALAPPDATA%\<game>\`) that is the user, `SYSTEM` and
//!   the Administrators group.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use net_backend_protocol::auth::TokenPair;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::Error;

/// The file format this crate writes and reads.
const FORMAT: u32 = 1;
/// A token file is a few hundred bytes; anything above this is not one.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// A file that holds one session's tokens (an access + refresh token pair) for one server.
///
/// Usually handed to [`ClientBuilder::token_file`](crate::ClientBuilder::token_file), which loads
/// it at `build` and keeps it up to date with every new pair (login, refresh, logout). It can also
/// be used on its own with [`load`](Self::load), [`save`](Self::save) and [`remove`](Self::remove).
///
/// Treat the file like a password: the refresh token in it logs in as the player until it expires
/// or the session is logged out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenFile {
    path: PathBuf,
}

/// Why [`TokenFile::load_checked`] found no usable tokens.
pub(crate) enum LoadError {
    /// The file is there but is not a token file (damaged, another format, too large).
    Damaged(Error),
    /// The file could not be opened or read.
    Unreadable(Error),
}

#[derive(Serialize)]
struct StoredRef<'a> {
    format: u32,
    server: &'a str,
    tokens: &'a TokenPair,
}

#[derive(Deserialize)]
struct Head {
    format: u32,
}

#[derive(Deserialize)]
struct Stored {
    server: String,
    tokens: TokenPair,
}

impl TokenFile {
    /// A token file at `path` (no I/O: the file is read and written by the other methods).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Where the file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn fail(&self, what: &str, detail: impl std::fmt::Display) -> Error {
        Error::invalid(format!("token file `{}`: {what}: {detail}", self.path.display()))
    }

    /// The stored tokens for `server` (the client's base URL, as [`Client::server_url`](crate::Client::server_url)
    /// reports it). `Ok(None)` when there is no file, or when the file belongs to another server
    /// (its tokens are never sent to a different server). A file that is not a token file
    /// (damaged, another format, too large) is `InvalidRequest`; the message never quotes its content.
    pub fn load(&self, server: &str) -> Result<Option<TokenPair>, Error> {
        self.load_checked(server).map_err(|e| match e {
            LoadError::Damaged(error) | LoadError::Unreadable(error) => error,
        })
    }

    /// [`load`](Self::load), telling a damaged file from one that cannot be read.
    pub(crate) fn load_checked(&self, server: &str) -> Result<Option<TokenPair>, LoadError> {
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LoadError::Unreadable(self.fail("could not be opened", e))),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = file.metadata() {
                if meta.permissions().mode() & 0o077 != 0 {
                    tracing::warn!(
                        "net_backend_client: token file `{}` can be read by other users; the next save writes it owner-only (0600)",
                        self.path.display()
                    );
                }
            }
        }
        // One buffer big enough for everything `take` lets through: it never grows, so no freed
        // copy of the tokens is left behind (`Zeroizing` wipes this one).
        let capacity = usize::try_from(MAX_FILE_BYTES + 1).unwrap_or(usize::MAX);
        let mut text = Zeroizing::new(Vec::with_capacity(capacity));
        let before = text.capacity();
        (&mut file).take(MAX_FILE_BYTES + 1).read_to_end(&mut text).map_err(|e| LoadError::Unreadable(self.fail("could not be read", e)))?;
        debug_assert_eq!(text.capacity(), before, "the read buffer never moved");
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(LoadError::Damaged(self.fail("not a token file", format_args!("larger than {MAX_FILE_BYTES} bytes"))));
        }
        // serde_json's message can quote a value: only its position is reported.
        let damaged = |e: serde_json::Error| {
            LoadError::Damaged(self.fail("not a valid token file", format_args!("{:?} error at line {}, column {}", e.classify(), e.line(), e.column())))
        };
        // The format number first (the rest is skipped, not copied), then the whole file.
        let head: Head = serde_json::from_slice(&text).map_err(damaged)?;
        if head.format != FORMAT {
            return Err(LoadError::Damaged(self.fail("not a valid token file", format_args!("format {} (this version reads format {FORMAT})", head.format))));
        }
        let stored: Stored = serde_json::from_slice(&text).map_err(damaged)?;
        if stored.server != server {
            tracing::debug!("net_backend_client: token file `{}` belongs to another server; not used", self.path.display());
            return Ok(None);
        }
        Ok(Some(stored.tokens))
    }

    /// Store `tokens` for `server`, atomically, owner-only (see the module notes). A missing
    /// folder is created.
    pub fn save(&self, server: &str, tokens: &TokenPair) -> Result<(), Error> {
        let text = crate::http::wiped_json(&StoredRef { format: FORMAT, server, tokens }).map_err(|e| self.fail("could not be encoded", e))?;
        let dir = match self.path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => PathBuf::from("."),
        };
        create_dir(&dir).map_err(|e| self.fail("its folder could not be created", e))?;
        let name = self.path.file_name().ok_or_else(|| self.fail("not a file path", "it has no file name"))?;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut temp_name = std::ffi::OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".{}.{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let temp = dir.join(temp_name);
        let written = (|| {
            let mut file = create_owner_only(&temp)?;
            file.write_all(&text)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &self.path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            return Err(self.fail("could not be written", e));
        }
        sync_dir(&dir);
        Ok(())
    }

    /// Delete the file (no file is fine).
    pub fn remove(&self) -> Result<(), Error> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.fail("could not be removed", e)),
        }
    }
}

/// A new file only the owner can read and write (Unix `0600`; Windows: the folder's inherited ACL).
fn create_owner_only(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// The folder and its missing parents (Unix `0700` for the new ones).
fn create_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Make the rename durable (Unix: flush the folder entry; best effort).
fn sync_dir(_dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = fs::File::open(_dir) {
        let _ = dir.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use net_backend_protocol::{AccessToken, RefreshToken, UnixMillis};

    use super::*;

    #[test]
    fn the_format_round_trips_in_memory() {
        let file = TokenFile::new("session.json");
        assert_eq!(file.path(), Path::new("session.json"));
        let pair = TokenPair::new(AccessToken::new("nbsa_fake"), UnixMillis(1), RefreshToken::new("nbsr_fake"), UnixMillis(2));
        let text = crate::http::wiped_json(&StoredRef { format: FORMAT, server: "https://api.example.com", tokens: &pair }).unwrap_or_else(|e| panic!("{e}"));
        let head: Head = serde_json::from_slice(&text).unwrap_or_else(|e| panic!("{e}"));
        let back: Stored = serde_json::from_slice(&text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((head.format, back.server.as_str(), back.tokens.refresh_token.expose()), (1, "https://api.example.com", "nbsr_fake"));
        assert!(!format!("{file:?}").contains("nbsa"));
    }
}
