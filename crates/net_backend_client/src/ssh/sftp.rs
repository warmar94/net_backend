//! SFTP on an [`SshSession`] (feature `sftp`): upload, download, list, mkdir, remove, rename. One
//! SFTP channel per session, opened on first use. Every operation has its own deadline
//! ([`SshTarget::with_sftp_timeout`](super::SshTarget::with_sftp_timeout)) and transfers are capped
//! ([`SshTarget::with_max_transfer_bytes`](super::SshTarget::with_max_transfer_bytes)). Local file
//! I/O runs on tokio's blocking pool. Remote paths are the server's (relative paths start in the
//! login directory).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use russh::ChannelMsg;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::RawSftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
use tokio::task::JoinSet;

use super::session::{deadline_after, Inner};
use super::{file_name, SshSession};
use crate::Error;

/// The longest remote path accepted (bytes).
const MAX_REMOTE_PATH: usize = 4096;
/// The most entries a directory listing returns; a longer one is an error.
const MAX_LIST_ENTRIES: usize = 10_000;
/// The longest entry name accepted in a listing (bytes).
const MAX_ENTRY_NAME: usize = 4096;
/// The most name bytes one listing may hold in total.
const MAX_LIST_NAME_BYTES: usize = 4 * 1024 * 1024;
/// Bytes per read / write request (OpenSSH serves up to 255 KiB per read; 32 KiB writes fit every
/// server's packet limit).
const READ_CHUNK: u32 = 64 * 1024;
const WRITE_CHUNK: usize = 32 * 1024;
/// Writes in flight at once for an upload.
const WRITE_WINDOW: usize = 16;

/// The kind of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SftpEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Symlink,
    /// Something else, or unknown.
    Other,
}

/// One entry of a directory listing.
///
/// **`name` is untrusted input from the server.** A hostile or broken server can list
/// `../../.bashrc`, `C:\Windows\evil.dll`, `a/b` or names with control characters. Never join
/// `name` into a local path yourself: use [`safe_file_name`](Self::safe_file_name), which returns
/// `None` for anything that is not one plain file name.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SftpEntry {
    /// The file name as the server sent it (not the full path; UNTRUSTED, see above).
    pub name: String,
    /// File, directory, link, …
    pub kind: SftpEntryKind,
    /// The size in bytes, when the server sent it.
    pub size: Option<u64>,
    /// The modification time (Unix seconds), when the server sent it.
    pub modified: Option<u32>,
    /// The Unix permission bits, when the server sent them.
    pub permissions: Option<u32>,
}

impl SftpEntry {
    /// The name if it is safe to use as ONE local file name, else `None`: not empty, not `.` /
    /// `..`, no `/`, `\`, `:`, NUL or other control characters, no leading or trailing space and no
    /// trailing dot, not a Windows device name (`CON`, `NUL`, `COM1`, … with any extension), at
    /// most 255 bytes.
    pub fn safe_file_name(&self) -> Option<&str> {
        safe_file_name(&self.name)
    }
}

fn safe_file_name(name: &str) -> Option<&str> {
    const DEVICES: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6",
        "lpt7", "lpt8", "lpt9",
    ];
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_ascii_lowercase();
    let bad = name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.chars().any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
        || name.starts_with(' ')
        || name.ends_with(' ')
        || name.ends_with('.')
        || DEVICES.contains(&stem.as_str());
    (!bad).then_some(name)
}

fn check_remote(path: &str) -> Result<(), Error> {
    if path.is_empty() || path.len() > MAX_REMOTE_PATH || path.contains('\0') {
        return Err(Error::invalid(format!("a remote SFTP path must be 1..={MAX_REMOTE_PATH} bytes without NUL")));
    }
    Ok(())
}

/// The server's words for an SFTP error.
fn map_error(error: SftpError) -> Error {
    match error {
        SftpError::Status(status) => {
            let mut message: String = status.error_message.chars().filter(|c| !c.is_control()).take(200).collect();
            if message.is_empty() {
                message = status.status_code.to_string();
            }
            Error::Ssh(format!("SFTP: {message}"))
        }
        SftpError::Timeout => Error::timeout("the SFTP server did not answer a request in time", None),
        other => Error::Ssh(format!("SFTP: {other}")),
    }
}

fn local_error(what: &str, path: &Path, error: &std::io::Error) -> Error {
    Error::invalid(format!("{what} `{}`: {error}", file_name(path)))
}

/// Run blocking local file work on the blocking pool.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, Error> + Send + 'static) -> Result<T, Error> {
    tokio::task::spawn_blocking(work).await.map_err(|e| Error::network(format!("local file work failed ({e})"), Some(false)))?
}

/// A part file next to `local` with a name no other download uses: `<name>.<process>-<n>.part`.
fn part_path(local: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut name = local.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(format!(".{}-{}.part", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    local.with_file_name(name)
}

/// The session's SFTP subsystem, opened on first use (a channel + the `sftp` subsystem).
async fn sftp_session(inner: &Arc<Inner>) -> Result<Arc<RawSftpSession>, Error> {
    let mut slot = inner.sftp.lock().await;
    if let Some(session) = slot.as_ref() {
        return Ok(Arc::clone(session));
    }
    let mut channel = inner.handle.channel_open_session().await.map_err(|e| Error::Ssh(format!("could not open a channel for SFTP: {e}")))?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| Error::Ssh(format!("could not request the SFTP subsystem: {e}")))?;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => break,
            Some(ChannelMsg::Failure) => return Err(Error::Ssh("the server refused the SFTP subsystem".into())),
            Some(ChannelMsg::Close | ChannelMsg::Eof) | None => return Err(Error::Ssh("the SFTP channel closed before it started".into())),
            Some(_) => {}
        }
    }
    let config = russh_sftp::client::Config { request_timeout_secs: 30, ..russh_sftp::client::Config::default() };
    let session = RawSftpSession::new_with_config(channel.into_stream(), config);
    session.init().await.map_err(map_error)?;
    let session = Arc::new(session);
    *slot = Some(Arc::clone(&session));
    Ok(session)
}

/// What an operation needs, and the operation itself under the session's SFTP deadline.
impl SshSession {
    async fn sftp_op<T, F, Fut>(&self, work: F) -> Result<T, Error>
    where
        F: FnOnce(Arc<RawSftpSession>, u64) -> Fut,
        Fut: std::future::Future<Output = Result<T, Error>>,
    {
        crate::runtime::current()?;
        let inner = &self.inner;
        let timeout = inner.target.sftp_timeout;
        let deadline = deadline_after(timeout);
        let max = inner.target.max_transfer_bytes;
        let run = async {
            let _permit = Arc::clone(&inner.channels).acquire_owned().await.map_err(|_| Error::disconnected("the session is closing", Some(false)))?;
            let session = sftp_session(inner).await?;
            let result = work(session, max).await;
            if matches!(&result, Err(Error::Ssh(why)) if why.contains("session closed")) {
                // The SFTP channel died: the next operation opens a new one.
                *inner.sftp.lock().await = None;
            }
            result
        };
        match tokio::time::timeout_at(deadline, run).await {
            Ok(result) => result,
            Err(_) => Err(Error::timeout(format!("the SFTP operation took longer than {timeout:?} (an interrupted upload may leave a partial file)"), None)),
        }
    }

    /// Write `data` to the remote file `remote` (created or truncated). Over the transfer limit:
    /// [`Error::RequestTooLarge`], nothing sent. Returns the bytes written.
    pub async fn upload(&self, remote: &str, data: impl Into<Vec<u8>>) -> Result<u64, Error> {
        check_remote(remote)?;
        let data = data.into();
        let size = data.len() as u64;
        if size > self.inner.target.max_transfer_bytes {
            return Err(Error::RequestTooLarge { limit: self.inner.target.max_transfer_bytes, size });
        }
        let remote = remote.to_string();
        self.sftp_op(|session, _| async move { upload(&session, &remote, size, ChunkSource::Memory { data, at: 0 }).await }).await
    }

    /// Copy the local file `local` to `remote` (created or truncated). Returns the bytes written.
    pub async fn upload_file(&self, local: impl AsRef<Path>, remote: &str) -> Result<u64, Error> {
        check_remote(remote)?;
        let local = local.as_ref().to_path_buf();
        if local.as_os_str().is_empty() {
            return Err(Error::invalid("the local path is empty"));
        }
        let remote = remote.to_string();
        self.sftp_op(|session, max| async move {
            let path = local.clone();
            let (file, total) = blocking(move || {
                let file = std::fs::File::open(&path).map_err(|e| local_error("could not open the local file", &path, &e))?;
                let total = file.metadata().map_err(|e| local_error("could not read the local file", &path, &e))?.len();
                Ok((file, total))
            })
            .await?;
            if total > max {
                return Err(Error::RequestTooLarge { limit: max, size: total });
            }
            upload(&session, &remote, total, ChunkSource::File { file: Some(file), path: local }).await
        })
        .await
    }

    /// Read the remote file `remote` into memory (at most the transfer limit, else `BodyTooLarge`).
    pub async fn download(&self, remote: &str) -> Result<Vec<u8>, Error> {
        check_remote(remote)?;
        let remote = remote.to_string();
        self.sftp_op(|session, max| async move {
            let mut sink = Sink::Memory(Vec::new());
            download(&session, &remote, max, &mut sink).await?;
            match sink {
                Sink::Memory(data) => Ok(data),
                Sink::File { .. } => Err(Error::Ssh("internal: wrong download sink".into())),
            }
        })
        .await
    }

    /// Copy the remote file `remote` to the local file `local`: written to a part file next to it,
    /// renamed over `local` only when the whole file arrived (the part file is removed on failure).
    /// Returns the bytes written.
    pub async fn download_file(&self, remote: &str, local: impl AsRef<Path>) -> Result<u64, Error> {
        check_remote(remote)?;
        let local = local.as_ref().to_path_buf();
        if local.as_os_str().is_empty() {
            return Err(Error::invalid("the local path is empty"));
        }
        let remote = remote.to_string();
        let part = part_path(&local);
        let cleanup = part.clone();
        let result = self
            .sftp_op(|session, max| async move {
                let path = part.clone();
                // `create_new`: never truncate a file that happens to have that name.
                let file = blocking(move || {
                    std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| local_error("could not create the local file", &path, &e))
                })
                .await?;
                let mut sink = Sink::File { file: Some(file), path: part.clone() };
                let result = download(&session, &remote, max, &mut sink).await;
                let file = match sink {
                    Sink::File { file, .. } => file,
                    Sink::Memory(_) => None,
                };
                blocking(move || {
                    let flushed = match file {
                        Some(mut file) => file.flush().map_err(|e| local_error("could not write the local file", &part, &e)),
                        None => Ok(()),
                    };
                    let bytes = result.and_then(|bytes| flushed.map(|()| bytes))?;
                    std::fs::rename(&part, &local).map_err(|e| local_error("could not move the download into place", &local, &e))?;
                    Ok(bytes)
                })
                .await
            })
            .await;
        if result.is_err() {
            drop(tokio::task::spawn_blocking(move || std::fs::remove_file(cleanup)));
        }
        result
    }

    /// List the remote directory `path` (without `.` and `..`, sorted by name, at most 10 000 entries).
    pub async fn list_dir(&self, path: &str) -> Result<Vec<SftpEntry>, Error> {
        check_remote(path)?;
        let path = path.to_string();
        self.sftp_op(|session, _| async move { list(&session, &path).await }).await
    }

    /// Create the remote directory `path` (its parent must exist).
    pub async fn create_dir(&self, path: &str) -> Result<(), Error> {
        check_remote(path)?;
        let path = path.to_string();
        self.sftp_op(|session, _| async move { session.mkdir(path, FileAttributes::empty()).await.map(|_| ()).map_err(map_error) }).await
    }

    /// Remove the remote file `path`.
    pub async fn remove_file(&self, path: &str) -> Result<(), Error> {
        check_remote(path)?;
        let path = path.to_string();
        self.sftp_op(|session, _| async move { session.remove(path).await.map(|_| ()).map_err(map_error) }).await
    }

    /// Remove the empty remote directory `path`.
    pub async fn remove_dir(&self, path: &str) -> Result<(), Error> {
        check_remote(path)?;
        let path = path.to_string();
        self.sftp_op(|session, _| async move { session.rmdir(path).await.map(|_| ()).map_err(map_error) }).await
    }

    /// Rename or move `from` to `to` (the server decides whether an existing `to` is replaced;
    /// OpenSSH refuses).
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), Error> {
        check_remote(from)?;
        check_remote(to)?;
        let (from, to) = (from.to_string(), to.to_string());
        self.sftp_op(|session, _| async move { session.rename(from, to).await.map(|_| ()).map_err(map_error) }).await
    }
}

enum ChunkSource {
    Memory { data: Vec<u8>, at: usize },
    File { file: Option<std::fs::File>, path: PathBuf },
}

impl ChunkSource {
    /// The next chunk (empty at the end). A local file is read on the blocking pool.
    async fn next(&mut self) -> Result<Vec<u8>, Error> {
        match self {
            ChunkSource::Memory { data, at } => {
                let end = at.saturating_add(WRITE_CHUNK).min(data.len());
                let chunk = data.get(*at..end).map(<[u8]>::to_vec).unwrap_or_default();
                *at = end;
                Ok(chunk)
            }
            ChunkSource::File { file, path } => {
                let Some(mut handle) = file.take() else { return Ok(Vec::new()) };
                let path = path.clone();
                let (handle, chunk) = blocking(move || {
                    let mut chunk = vec![0; WRITE_CHUNK];
                    let mut filled = 0;
                    while filled < chunk.len() {
                        let Some(rest) = chunk.get_mut(filled..) else { break };
                        match handle.read(rest) {
                            Ok(0) => break,
                            Ok(n) => filled = filled.saturating_add(n),
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) => return Err(local_error("could not read the local file", &path, &e)),
                        }
                    }
                    chunk.truncate(filled);
                    Ok((handle, chunk))
                })
                .await?;
                *file = Some(handle);
                Ok(chunk)
            }
        }
    }
}

enum Sink {
    Memory(Vec<u8>),
    File { file: Option<std::fs::File>, path: PathBuf },
}

impl Sink {
    /// Keep a chunk. A local file is written on the blocking pool.
    async fn put(&mut self, chunk: Vec<u8>) -> Result<(), Error> {
        match self {
            Sink::Memory(data) => {
                data.extend_from_slice(&chunk);
                Ok(())
            }
            Sink::File { file, path } => {
                let Some(mut handle) = file.take() else { return Err(Error::Ssh("internal: the local file is gone".into())) };
                let path = path.clone();
                let handle = blocking(move || {
                    handle.write_all(&chunk).map_err(|e| local_error("could not write the local file", &path, &e))?;
                    Ok(handle)
                })
                .await?;
                *file = Some(handle);
                Ok(())
            }
        }
    }
}

async fn upload(session: &Arc<RawSftpSession>, remote: &str, total: u64, mut source: ChunkSource) -> Result<u64, Error> {
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let handle = session.open(remote, flags, FileAttributes::empty()).await.map_err(map_error)?.handle;
    let result = write_all(session, &handle, total, &mut source).await;
    let closed = session.close(handle).await.map_err(map_error);
    let written = check_written(result?, total)?;
    closed?;
    Ok(written)
}

/// A local file that changed size while it was read: the remote file is incomplete, so that is an
/// error (bytes were written: `Ssh`, never `InvalidRequest`), not a short success.
fn check_written(written: u64, total: u64) -> Result<u64, Error> {
    if written == total {
        Ok(written)
    } else {
        Err(changed_size())
    }
}

fn changed_size() -> Error {
    Error::Ssh("the local file changed size during the upload; the remote file is incomplete".into())
}

/// Pipelined writes: up to `WRITE_WINDOW` in flight; the first failure stops the upload.
async fn write_all(session: &Arc<RawSftpSession>, handle: &str, total: u64, source: &mut ChunkSource) -> Result<u64, Error> {
    let mut in_flight: JoinSet<Result<u64, SftpError>> = JoinSet::new();
    let mut offset: u64 = 0;
    let mut acked: u64 = 0;
    let mut finished_reading = false;
    loop {
        while !finished_reading && in_flight.len() < WRITE_WINDOW {
            let chunk = source.next().await?;
            if chunk.is_empty() {
                finished_reading = true;
                break;
            }
            let len = chunk.len() as u64;
            if offset.saturating_add(len) > total {
                return Err(changed_size());
            }
            let (session, handle, at) = (Arc::clone(session), handle.to_string(), offset);
            in_flight.spawn(async move { session.write(handle, at, chunk).await.map(|_| len) });
            offset = offset.saturating_add(len);
        }
        match in_flight.join_next().await {
            None => return Ok(acked),
            Some(Ok(Ok(len))) => acked = acked.saturating_add(len),
            Some(Ok(Err(error))) => return Err(map_error(error)),
            Some(Err(join)) => return Err(Error::Ssh(format!("an SFTP write task failed: {join}"))),
        }
    }
}

/// Sequential reads until end of file into `sink`. Returns the bytes read.
async fn download(session: &RawSftpSession, remote: &str, max_bytes: u64, sink: &mut Sink) -> Result<u64, Error> {
    let handle = session.open(remote, OpenFlags::READ, FileAttributes::empty()).await.map_err(map_error)?.handle;
    let mut offset: u64 = 0;
    let result = loop {
        match session.read(handle.as_str(), offset, READ_CHUNK).await {
            Ok(data) if data.data.is_empty() => break Ok(offset),
            Ok(data) => {
                offset = offset.saturating_add(data.data.len() as u64);
                if offset > max_bytes {
                    break Err(Error::BodyTooLarge { limit: max_bytes });
                }
                if let Err(error) = sink.put(data.data).await {
                    break Err(error);
                }
            }
            Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break Ok(offset),
            Err(error) => break Err(map_error(error)),
        }
    };
    let _ = session.close(handle).await;
    result
}

async fn list(session: &RawSftpSession, path: &str) -> Result<Vec<SftpEntry>, Error> {
    let handle = session.opendir(path).await.map_err(map_error)?.handle;
    let mut entries = Vec::new();
    let mut name_bytes = 0usize;
    let result = loop {
        match session.readdir(handle.as_str()).await {
            Ok(name) => {
                let mut problem = None;
                for file in name.files {
                    if file.filename == "." || file.filename == ".." {
                        continue;
                    }
                    if entries.len() >= MAX_LIST_ENTRIES {
                        problem = Some(format!("the directory has more than {MAX_LIST_ENTRIES} entries"));
                        break;
                    }
                    if file.filename.len() > MAX_ENTRY_NAME {
                        problem = Some(format!("the server listed a name longer than {MAX_ENTRY_NAME} bytes"));
                        break;
                    }
                    name_bytes = name_bytes.saturating_add(file.filename.len());
                    if name_bytes > MAX_LIST_NAME_BYTES {
                        problem = Some(format!("the listing's names are longer than {MAX_LIST_NAME_BYTES} bytes together"));
                        break;
                    }
                    let kind = match file.attrs.permissions.map(|_| file.attrs.file_type()) {
                        Some(FileType::File) => SftpEntryKind::File,
                        Some(FileType::Dir) => SftpEntryKind::Dir,
                        Some(FileType::Symlink) => SftpEntryKind::Symlink,
                        _ => SftpEntryKind::Other,
                    };
                    entries.push(SftpEntry {
                        name: file.filename,
                        kind,
                        size: file.attrs.size,
                        modified: file.attrs.mtime,
                        permissions: file.attrs.permissions.map(|p| p & 0o7777),
                    });
                }
                if let Some(problem) = problem {
                    break Err(Error::Ssh(problem));
                }
            }
            Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break Ok(()),
            Err(error) => break Err(map_error(error)),
        }
    };
    let _ = session.close(handle).await;
    result?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_file_that_changed_size_is_an_honest_error_not_a_short_success() {
        assert_eq!(check_written(4096, 4096).ok(), Some(4096));
        for (written, total) in [(1024, 4096), (0, 4096), (5000, 4096)] {
            let error = check_written(written, total).err();
            assert!(matches!(&error, Some(Error::Ssh(why)) if why.contains("changed size")), "{error:?}");
            assert_eq!(error.and_then(|e| e.was_sent()), None);
        }
    }

    #[test]
    fn listed_names_that_are_not_one_plain_file_name_are_unsafe() {
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "C:\\Windows\\evil.dll",
            "c:evil",
            "/etc/passwd",
            "x\u{0}y",
            "tab\there",
            " lead",
            "trail ",
            "dot.",
            "CON",
            "nul.txt",
            "Com1.log",
        ] {
            assert_eq!(safe_file_name(bad), None, "{bad:?} passed");
        }
        for good in ["notes.txt", ".bashrc", "a b.tar.gz", "console.log", "über.txt", "CONFIG"] {
            assert_eq!(safe_file_name(good), Some(good));
        }
        assert!(check_remote("").is_err() && check_remote("a\0b").is_err() && check_remote("logs/today.txt").is_ok());
    }
}
