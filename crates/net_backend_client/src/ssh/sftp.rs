//! SFTP on an [`SshSession`] (feature `sftp`): upload, download, list, mkdir, remove, rename. One
//! SFTP channel per connection, opened on first use. Every operation has its own deadline
//! ([`SshTarget::with_sftp_timeout`](super::SshTarget::with_sftp_timeout)), and so has each SFTP
//! request inside it; transfers are capped
//! ([`SshTarget::with_max_transfer_bytes`](super::SshTarget::with_max_transfer_bytes)). Downloads
//! keep 16 reads of 64 KiB in flight (at most 1 MiB asked for or waiting to be written, whatever
//! the file size) and write them in file order; uploads keep 16 writes of 32 KiB in flight. A
//! download whose remote file ends before the size the server reported when it was opened fails
//! with [`Error::Ssh`] ("SFTP: the remote file was cut short during the download (expected N
//! bytes, its size when it was opened; received M)"), and a download to a file then leaves no file; a file that reports a size of 0, or
//! none, is read to its end. Transfers report their progress ([`SftpTask`]). The remote file handle is closed after every
//! transfer, also one that was cancelled or timed out. Local file I/O runs on tokio's blocking
//! pool. Remote paths are the server's (relative paths start in the login directory).

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::RawSftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;

use super::session::{current_link, deadline_after, Link, Race};
use super::{file_name, SshSession};
use crate::{Error, Reply};

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
/// A download keeps at most this many bytes asked for or received and waiting to be written: 16 reads of
/// 64 KiB in flight (the memory a download to a file uses, whatever the file size).
const READ_WINDOW_BYTES: u64 = 16 * READ_CHUNK as u64;
/// At most one progress report per this interval (plus the last one).
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// How long removing a part file may be retried (a local write may still hold it open).
const PART_REMOVE_TRIES: u32 = 40;

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

/// How far a transfer is ([`SftpTask`]): bytes moved so far and the size, when known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SftpProgress {
    /// Bytes moved so far (an upload: written and confirmed by the server; a download: written to
    /// the local file or memory).
    pub done: u64,
    /// The size: the local file's or the data's for an upload, the server's `fstat` size for a
    /// download (`None` when the server did not say).
    pub total: Option<u64>,
}

/// A running SFTP transfer ([`SshSession::start_download`], …): its progress, then exactly one
/// result. Progress reports come at most about 10 times a second, `done` only grows, and the last
/// report (`done` = the bytes moved) comes before the result. **Dropping it cancels the transfer**:
/// the remote file handle is closed and a download's part file removed. Works from a game loop
/// too ([`try_progress`](Self::try_progress), [`try_finish`](Self::try_finish)).
pub struct SftpTask<T> {
    progress: watch::Receiver<SftpProgress>,
    seen: SftpProgress,
    result: Reply<T>,
    _cancel: oneshot::Sender<()>,
}

impl<T> fmt::Debug for SftpTask<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpTask").field("progress", &*self.progress.borrow()).field("finished", &self.result.is_taken()).finish()
    }
}

impl<T> SftpTask<T> {
    fn failed(error: Error) -> Self {
        let (_, progress) = watch::channel(SftpProgress::default());
        let (cancel, _) = oneshot::channel();
        Self { progress, seen: SftpProgress::default(), result: Reply::ready(Err(error)), _cancel: cancel }
    }

    /// The latest progress (never blocks; no runtime needed).
    pub fn progress(&self) -> SftpProgress {
        *self.progress.borrow()
    }

    /// The next progress report; `None` once the transfer ended and its last report was taken.
    pub async fn next_progress(&mut self) -> Option<SftpProgress> {
        loop {
            let current = *self.progress.borrow_and_update();
            if current != self.seen {
                self.seen = current;
                return Some(current);
            }
            self.progress.changed().await.ok()?;
        }
    }

    /// A progress report that arrived since the last call, if any (never blocks; no runtime
    /// needed).
    pub fn try_progress(&mut self) -> Option<SftpProgress> {
        let current = *self.progress.borrow();
        (current != self.seen).then(|| {
            self.seen = current;
            current
        })
    }

    /// Wait for the result.
    pub async fn finish(self) -> Result<T, Error> {
        let SftpTask { result, _cancel, .. } = self;
        let result = result.await;
        drop(_cancel);
        result
    }

    /// The result, if it came (never blocks; no runtime needed).
    pub fn try_finish(&mut self) -> Option<Result<T, Error>> {
        self.result.try_take()
    }
}

/// Reports a transfer's progress: throttled, plus the last report.
trait Report {
    fn progress(&mut self, done: u64, total: Option<u64>);
}

struct Reporter {
    sender: watch::Sender<SftpProgress>,
    last: Option<std::time::Instant>,
    /// Set once any byte moved (reported or throttled): the server answered part of the transfer.
    moved: Arc<AtomicBool>,
}

impl Reporter {
    fn new(sender: watch::Sender<SftpProgress>) -> Self {
        Self { sender, last: None, moved: Arc::new(AtomicBool::new(false)) }
    }

    /// The last report (always sent).
    fn last(&mut self, done: u64, total: Option<u64>) {
        if done > 0 {
            self.moved.store(true, Ordering::Relaxed);
        }
        self.sender.send_replace(SftpProgress { done, total });
    }
}

impl Report for Reporter {
    fn progress(&mut self, done: u64, total: Option<u64>) {
        if done > 0 {
            self.moved.store(true, Ordering::Relaxed);
        }
        let now = std::time::Instant::now();
        if self.last.is_none_or(|last| now.saturating_duration_since(last) >= PROGRESS_EVERY) {
            self.last = Some(now);
            self.sender.send_replace(SftpProgress { done, total });
        }
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

/// The reason of an SFTP operation whose channel went away (its connection was lost or closed).
const CHANNEL_LOST: &str = "the SFTP channel closed (the connection was lost)";

/// The server's words for an SFTP error. The SFTP channel going away (its connection was lost)
/// is [`Error::Disconnected`]: `sent: None` here, made exact by the caller.
fn map_error(error: SftpError) -> Error {
    match error {
        // russh-sftp's words (pinned version) for a request whose channel is gone: the answer
        // channel was dropped, the session's sender is closed, or a send / receive failed.
        SftpError::UnexpectedBehavior(why)
            if why == "sender dropped" || why == "session closed" || why.starts_with("SendError") || why.starts_with("RecvError") =>
        {
            Error::disconnected(CHANNEL_LOST, None)
        }
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

/// Removes a download's part file when the download did not complete: it failed, timed out or was
/// cancelled (its future dropped). Removed on the blocking pool, retried briefly while a local
/// write that was still running holds the file open.
struct PartGuard {
    path: Option<PathBuf>,
}

impl PartGuard {
    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else { return };
        let remove = move || {
            for _ in 0..PART_REMOVE_TRIES {
                match std::fs::remove_file(&path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => std::thread::sleep(Duration::from_millis(25)),
                    _ => return,
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}

/// The SFTP subsystem of a connection, opened on first use (a channel + the `sftp` subsystem).
/// Each SFTP request waits at most `timeout` (at least 1 s) for its answer.
async fn sftp_session(link: &Arc<Link>, timeout: Duration) -> Result<Arc<RawSftpSession>, Error> {
    let mut slot = link.sftp.lock().await;
    if let Some(session) = slot.as_ref() {
        return Ok(Arc::clone(session));
    }
    let mut channel = link.handle.channel_open_session().await.map_err(|e| {
        if link.is_gone() {
            Error::disconnected(format!("could not open a channel for SFTP: {e}"), Some(false))
        } else {
            Error::Ssh(format!("could not open a channel for SFTP: {e}"))
        }
    })?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| Error::Ssh(format!("could not request the SFTP subsystem: {e}")))?;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => break,
            Some(ChannelMsg::Failure) => return Err(Error::Ssh("the server refused the SFTP subsystem".into())),
            Some(ChannelMsg::Close | ChannelMsg::Eof) | None => return Err(Error::Ssh("the SFTP channel closed before it started".into())),
            Some(_) => {}
        }
    }
    let seconds = timeout.as_secs().saturating_add(u64::from(timeout.subsec_nanos() > 0)).max(1);
    let config = russh_sftp::client::Config { request_timeout_secs: seconds, ..russh_sftp::client::Config::default() };
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
        Fut: Future<Output = Result<T, Error>>,
    {
        crate::runtime::current()?;
        let inner = &self.inner;
        let timeout = inner.target.sftp_timeout;
        let deadline = deadline_after(timeout);
        let max = inner.target.max_transfer_bytes;
        let run = async {
            let _permit = Arc::clone(&inner.channels).acquire_owned().await.map_err(|_| Error::disconnected("the session is closing", Some(false)))?;
            // Never fires: the operation is cancelled by dropping this future.
            let (_never, mut not_cancelled) = oneshot::channel::<()>();
            let link = match current_link(inner, deadline, &mut not_cancelled).await {
                Race::Done(Ok(link)) => link,
                Race::Done(Err(error)) => return Err(error),
                Race::TimedOut | Race::Cancelled => return Err(Error::timeout(format!("not sent: not connected within {timeout:?}"), Some(false))),
            };
            // A lost connection ends the operation at once. Requests in flight on a dead SFTP
            // channel are not always answered (russh-sftp can keep a request sent while its
            // channel was closing until the request timeout), so the link is watched too.
            let working = AtomicBool::new(false);
            let operation = async {
                match sftp_session(&link, timeout).await {
                    Ok(session) => {
                        working.store(true, Ordering::Relaxed);
                        work(session, max).await
                    }
                    Err(error) => Err(error),
                }
            };
            let result = tokio::select! {
                biased;
                result = operation => result,
                // Lost before the operation's first request: never sent.
                () = link.gone() => Err(Error::disconnected(CHANNEL_LOST, (!working.load(Ordering::Relaxed)).then_some(false))),
            };
            match result {
                Err(Error::Disconnected { sent, .. }) => {
                    // The SFTP channel died: the next operation opens a new one. When the whole
                    // connection is gone, say why.
                    *link.sftp.lock().await = None;
                    let reason = if link.is_gone() { link.reason() } else { CHANNEL_LOST.to_string() };
                    Err(Error::disconnected(reason, sent))
                }
                other => other,
            }
        };
        match tokio::time::timeout_at(deadline, run).await {
            Ok(result) => result,
            Err(_) => Err(Error::timeout(format!("the SFTP operation took longer than {timeout:?} (an interrupted upload may leave a partial file)"), None)),
        }
    }

    /// Run a transfer as its own task; dropping the [`SftpTask`] cancels it.
    fn start_transfer<T, F, Fut>(&self, work: F) -> SftpTask<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<RawSftpSession>, u64, Reporter) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, Error>> + Send + 'static,
    {
        let (sender, progress) = watch::channel(SftpProgress::default());
        let (answer, result) = Reply::channel();
        let (cancel, mut cancelled) = oneshot::channel::<()>();
        match crate::runtime::current() {
            Ok(runtime) => {
                let session = self.clone();
                runtime.spawn(async move {
                    let reporter = Reporter::new(sender);
                    let moved = Arc::clone(&reporter.moved);
                    let operation = session.sftp_op(move |sftp, max| work(sftp, max, reporter));
                    tokio::select! {
                        biased;
                        _ = &mut cancelled => {}
                        result = operation => {
                            // A lost connection after the server answered part of the transfer:
                            // it was sent (an upload may have left a partial file).
                            let result = match result {
                                Err(Error::Disconnected { reason, sent: None }) if moved.load(Ordering::Relaxed) => Err(Error::disconnected(reason, Some(true))),
                                other => other,
                            };
                            let _ = answer.send(result);
                        }
                    }
                });
            }
            Err(error) => {
                let _ = answer.send(Err(error));
            }
        }
        SftpTask { progress, seen: SftpProgress::default(), result, _cancel: cancel }
    }

    /// Write `data` to the remote file `remote` (created or truncated), with progress. Over the
    /// transfer limit: [`Error::RequestTooLarge`], nothing sent. The result is the bytes written.
    /// Must be called inside a tokio runtime (otherwise the task ends at once with `InvalidRequest`).
    pub fn start_upload(&self, remote: &str, data: impl Into<Vec<u8>>) -> SftpTask<u64> {
        if let Err(error) = check_remote(remote) {
            return SftpTask::failed(error);
        }
        let data = data.into();
        let size = data.len() as u64;
        if size > self.inner.target.max_transfer_bytes {
            return SftpTask::failed(Error::RequestTooLarge { limit: self.inner.target.max_transfer_bytes, size });
        }
        let remote = remote.to_string();
        self.start_transfer(
            move |session, _, mut report| async move { upload(&session, &remote, size, &mut report, ChunkSource::Memory { data, at: 0 }).await },
        )
    }

    /// Copy the local file `local` to `remote` (created or truncated), with progress. The result is
    /// the bytes written.
    pub fn start_upload_file(&self, local: impl AsRef<Path>, remote: &str) -> SftpTask<u64> {
        if let Err(error) = check_remote(remote) {
            return SftpTask::failed(error);
        }
        let local = local.as_ref().to_path_buf();
        if local.as_os_str().is_empty() {
            return SftpTask::failed(Error::invalid("the local path is empty"));
        }
        let remote = remote.to_string();
        self.start_transfer(move |session, max, mut report| async move {
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
            upload(&session, &remote, total, &mut report, ChunkSource::File { file: Some(file), path: local }).await
        })
    }

    /// Read the remote file `remote` into memory (at most the transfer limit, else
    /// `BodyTooLarge`), with progress. A file that ends before the size the server reported when it
    /// was opened is an [`Error::Ssh`] ("… changed size during the download").
    pub fn start_download(&self, remote: &str) -> SftpTask<Vec<u8>> {
        if let Err(error) = check_remote(remote) {
            return SftpTask::failed(error);
        }
        let remote = remote.to_string();
        self.start_transfer(move |session, max, mut report| async move {
            let mut sink = Sink::Memory(Vec::new());
            download(&session, &remote, max, &mut report, &mut sink).await?;
            match sink {
                Sink::Memory(data) => Ok(data),
                Sink::File { .. } => Err(Error::Ssh("internal: wrong download sink".into())),
            }
        })
    }

    /// Copy the remote file `remote` to the local file `local`, with progress: written to a part
    /// file next to it (`<name>.<process>-<n>.part`), renamed over `local` only when the whole
    /// file arrived; the part file is removed when the download fails, times out or is cancelled.
    /// A remote file that ends before the size the server reported when it was opened is an
    /// [`Error::Ssh`] ("… changed size during the download") and leaves no file. The result is the
    /// bytes written.
    pub fn start_download_file(&self, remote: &str, local: impl AsRef<Path>) -> SftpTask<u64> {
        if let Err(error) = check_remote(remote) {
            return SftpTask::failed(error);
        }
        let local = local.as_ref().to_path_buf();
        if local.as_os_str().is_empty() {
            return SftpTask::failed(Error::invalid("the local path is empty"));
        }
        let remote = remote.to_string();
        self.start_transfer(move |session, max, mut report| async move { download_to_file(&session, &remote, &local, max, &mut report).await })
    }

    /// Write `data` to the remote file `remote` (created or truncated). Over the transfer limit:
    /// [`Error::RequestTooLarge`], nothing sent. Returns the bytes written. Dropping the future
    /// cancels the upload ([`start_upload`](Self::start_upload) reports progress).
    pub async fn upload(&self, remote: &str, data: impl Into<Vec<u8>>) -> Result<u64, Error> {
        self.start_upload(remote, data).finish().await
    }

    /// Copy the local file `local` to `remote` (created or truncated). Returns the bytes written.
    pub async fn upload_file(&self, local: impl AsRef<Path>, remote: &str) -> Result<u64, Error> {
        self.start_upload_file(local, remote).finish().await
    }

    /// Read the remote file `remote` into memory (at most the transfer limit, else `BodyTooLarge`;
    /// a file whose size the server reports over the limit is refused before any data is read).
    pub async fn download(&self, remote: &str) -> Result<Vec<u8>, Error> {
        self.start_download(remote).finish().await
    }

    /// Copy the remote file `remote` to the local file `local`: written to a part file next to it,
    /// renamed over `local` only when the whole file arrived (the part file is removed on failure,
    /// timeout or cancel). Returns the bytes written.
    pub async fn download_file(&self, remote: &str, local: impl AsRef<Path>) -> Result<u64, Error> {
        self.start_download_file(remote, local).finish().await
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

/// An open remote handle that is closed when the operation ends, also when the operation is
/// cancelled or times out (its future is dropped): then the close is sent from a task of its own,
/// so the server never keeps handles of abandoned transfers.
struct HandleGuard {
    session: Arc<RawSftpSession>,
    handle: Option<String>,
}

impl HandleGuard {
    fn new(session: &Arc<RawSftpSession>, handle: &str) -> Self {
        Self { session: Arc::clone(session), handle: Some(handle.to_string()) }
    }

    async fn close(mut self) -> Result<(), SftpError> {
        match self.handle.take() {
            Some(handle) => self.session.close(handle).await.map(|_| ()),
            None => Ok(()),
        }
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // Dropped inside the runtime that ran the transfer. Without a runtime (it is shutting
        // down) there is nothing to send the close with: the SFTP session closes anyway.
        if let (Some(handle), Ok(runtime)) = (self.handle.take(), tokio::runtime::Handle::try_current()) {
            let session = Arc::clone(&self.session);
            drop(runtime.spawn(async move {
                let _ = session.close(handle).await;
            }));
        }
    }
}

async fn upload(session: &Arc<RawSftpSession>, remote: &str, total: u64, report: &mut Reporter, mut source: ChunkSource) -> Result<u64, Error> {
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let handle = session.open(remote, flags, FileAttributes::empty()).await.map_err(map_error)?.handle;
    let guard = HandleGuard::new(session, &handle);
    let result = write_all(session, &handle, total, report, &mut source).await;
    let closed = guard.close().await.map_err(map_error);
    let written = check_written(result?, total)?;
    closed?;
    report.last(written, Some(total));
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
async fn write_all(session: &Arc<RawSftpSession>, handle: &str, total: u64, report: &mut impl Report, source: &mut ChunkSource) -> Result<u64, Error> {
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
            Some(Ok(Ok(len))) => {
                acked = acked.saturating_add(len);
                report.progress(acked, Some(total));
            }
            Some(Ok(Err(error))) => return Err(map_error(error)),
            Some(Err(join)) => return Err(Error::Ssh(format!("an SFTP write task failed: {join}"))),
        }
    }
}

/// What one read at an offset gave.
enum ReadOutcome {
    /// Bytes (at most the length asked for; fewer is a short read, not the end).
    Data(Vec<u8>),
    /// Nothing at this offset: the end of the file.
    Eof,
}

/// Reads a remote file at an offset: an open SFTP handle (tests: an in-memory file).
trait ReadAt: Clone + Send + Sync + 'static {
    fn read_at(&self, offset: u64, len: u32) -> impl Future<Output = Result<ReadOutcome, Error>> + Send + 'static;
}

/// An open SFTP handle.
#[derive(Clone)]
struct SftpFile {
    session: Arc<RawSftpSession>,
    handle: Arc<str>,
}

impl ReadAt for SftpFile {
    fn read_at(&self, offset: u64, len: u32) -> impl Future<Output = Result<ReadOutcome, Error>> + Send + 'static {
        let (session, handle) = (Arc::clone(&self.session), self.handle.to_string());
        async move {
            match session.read(handle, offset, len).await {
                Ok(data) if data.data.is_empty() => Ok(ReadOutcome::Eof),
                Ok(data) => Ok(ReadOutcome::Data(data.data)),
                Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => Ok(ReadOutcome::Eof),
                Err(error) => Err(map_error(error)),
            }
        }
    }
}

/// Download `remote` into `sink`. Returns the bytes written.
async fn download(session: &Arc<RawSftpSession>, remote: &str, max_bytes: u64, report: &mut Reporter, sink: &mut Sink) -> Result<u64, Error> {
    let handle = session.open(remote, OpenFlags::READ, FileAttributes::empty()).await.map_err(map_error)?.handle;
    let guard = HandleGuard::new(session, &handle);
    let total = session.fstat(handle.as_str()).await.ok().and_then(|attrs| attrs.attrs.size);
    let result = if total.is_some_and(|total| total > max_bytes) {
        // The server says it is too large: refused before any data is read.
        Err(Error::BodyTooLarge { limit: max_bytes })
    } else {
        let file = SftpFile { session: Arc::clone(session), handle: Arc::from(handle.as_str()) };
        read_pipelined(&file, max_bytes, total, report, sink, READ_WINDOW_BYTES).await.map(|(written, _)| written)
    };
    let _ = guard.close().await;
    if let Ok(done) = result {
        report.last(done, total);
    }
    result
}

/// Pipelined reads into `sink`, in file order: reads of [`READ_CHUNK`] bytes go out ahead of the
/// answers, as long as the bytes asked for plus the bytes received and waiting to be written stay within
/// `window` (bounded memory). Answers may arrive in any order; they are written to the sink in
/// order. A short read (fewer bytes than asked, not at the end) asks again for the rest of its
/// range. The file ends at the lowest offset the server answered with end of file; once that is
/// known, no new reads go out. At most `max_bytes + 1` bytes are asked for (one byte more proves
/// the file is too large). The first error stops everything (reads still in flight are dropped and
/// their answers ignored).
///
/// Returns the bytes written and the largest number of bytes that were asked for or waiting at once.
async fn read_pipelined<R: ReadAt>(
    reader: &R,
    max_bytes: u64,
    total: Option<u64>,
    report: &mut impl Report,
    sink: &mut Sink,
    window: u64,
) -> Result<(u64, u64), Error> {
    type Answer = (u64, u64, Result<ReadOutcome, Error>);
    let chunk = u64::from(READ_CHUNK).min(window.max(1));
    let limit = max_bytes.saturating_add(1);
    let mut in_flight: JoinSet<Answer> = JoinSet::new();
    let spawn = |in_flight: &mut JoinSet<Answer>, offset: u64, len: u64| {
        let read = reader.read_at(offset, u32::try_from(len).unwrap_or(READ_CHUNK));
        in_flight.spawn(async move { (offset, len, read.await) });
    };
    // Received and waiting to be written, by offset.
    let mut waiting: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let (mut next, mut written, mut outstanding, mut peak) = (0u64, 0u64, 0u64, 0u64);
    let mut end: Option<u64> = None;
    loop {
        while end.is_none() && next < limit && outstanding.saturating_add(chunk) <= window {
            let len = chunk.min(limit - next);
            spawn(&mut in_flight, next, len);
            next = next.saturating_add(len);
            outstanding = outstanding.saturating_add(len);
        }
        peak = peak.max(outstanding);
        let Some(joined) = in_flight.join_next().await else { break };
        let (offset, asked, result) = joined.map_err(|e| Error::Ssh(format!("an SFTP read task failed: {e}")))?;
        outstanding = outstanding.saturating_sub(asked);
        match result? {
            ReadOutcome::Eof => end = Some(end.map_or(offset, |end| end.min(offset))),
            ReadOutcome::Data(data) => {
                let got = u64::try_from(data.len()).unwrap_or(u64::MAX);
                if got > asked {
                    return Err(Error::Ssh("SFTP: the server sent more bytes than were asked for".into()));
                }
                if got < asked {
                    // A short read: the rest of the range.
                    spawn(&mut in_flight, offset.saturating_add(got), asked - got);
                    outstanding = outstanding.saturating_add(asked - got);
                }
                outstanding = outstanding.saturating_add(got);
                waiting.insert(offset, data);
                while let Some(data) = waiting.remove(&written) {
                    let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
                    outstanding = outstanding.saturating_sub(len);
                    let after = written.saturating_add(len);
                    if after > max_bytes {
                        return Err(Error::BodyTooLarge { limit: max_bytes });
                    }
                    sink.put(data).await?;
                    written = after;
                    report.progress(written, total);
                }
            }
        }
    }
    // Every read is answered. Bytes the server sent beyond the end it reported (the file grew
    // meanwhile) are dropped; everything before the end must have been written. A file that ends
    // before the size the server reported when it was opened was cut short meanwhile (a size of 0,
    // or none, is read to its end: some files report no real size).
    match end {
        Some(end) if end == written => match total {
            Some(total) if total > 0 && written < total => Err(Error::Ssh(format!(
                "SFTP: the remote file was cut short during the download (expected {total} bytes, its size when it was opened; received {written})"
            ))),
            _ => Ok((written, peak)),
        },
        Some(_) => Err(Error::Ssh("SFTP: the remote file changed size during the download".into())),
        None => Err(Error::Ssh("SFTP: the download ended without reaching the end of the file".into())),
    }
}

/// Download `remote` to `local` through a part file (see [`SshSession::start_download_file`]).
async fn download_to_file(session: &Arc<RawSftpSession>, remote: &str, local: &Path, max_bytes: u64, report: &mut Reporter) -> Result<u64, Error> {
    let part = part_path(local);
    let path = part.clone();
    // `create_new`: never truncate a file that happens to have that name.
    let file = blocking(move || {
        std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| local_error("could not create the local file", &path, &e))
    })
    .await?;
    // Declared before the sink: on a cancel the local file is closed first, then removed.
    let mut guard = PartGuard { path: Some(part.clone()) };
    let mut sink = Sink::File { file: Some(file), path: part.clone() };
    let result = download(session, remote, max_bytes, report, &mut sink).await;
    let file = match sink {
        Sink::File { file, .. } => file,
        Sink::Memory(_) => None,
    };
    let local = local.to_path_buf();
    let bytes = blocking(move || {
        let flushed = match file {
            Some(mut file) => file.flush().map_err(|e| local_error("could not write the local file", &part, &e)),
            None => Ok(()),
        };
        let bytes = result.and_then(|bytes| flushed.map(|()| bytes))?;
        std::fs::rename(&part, &local).map_err(|e| local_error("could not move the download into place", &local, &e))?;
        Ok(bytes)
    })
    .await?;
    guard.disarm();
    Ok(bytes)
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

    #[test]
    fn progress_reports_are_new_values_only_and_the_last_one_survives_the_end() {
        let (sender, progress) = watch::channel(SftpProgress::default());
        let (cancel, _) = oneshot::channel();
        let (_answer, result) = Reply::<u64>::channel();
        let mut task = SftpTask { progress, seen: SftpProgress::default(), result, _cancel: cancel };
        assert_eq!(task.try_progress(), None, "nothing reported so far");
        let mut reporter = Reporter::new(sender);
        reporter.progress(10, Some(100));
        reporter.progress(20, Some(100)); // throttled: within 100 ms of the first
        assert_eq!(task.try_progress(), Some(SftpProgress { done: 10, total: Some(100) }));
        assert_eq!(task.try_progress(), None);
        reporter.last(100, Some(100));
        drop(reporter);
        assert_eq!(task.try_progress(), Some(SftpProgress { done: 100, total: Some(100) }), "the last report after the sender is gone");
        assert_eq!(task.progress().done, 100);
    }

    #[test]
    fn a_moved_byte_is_remembered_even_when_its_report_was_throttled() {
        let (sender, _progress) = watch::channel(SftpProgress::default());
        let mut reporter = Reporter::new(sender);
        reporter.progress(0, Some(100));
        assert!(!reporter.moved.load(Ordering::Relaxed));
        reporter.progress(10, Some(100)); // throttled: within 100 ms of the first
        assert!(reporter.moved.load(Ordering::Relaxed));
    }

    #[test]
    fn a_lost_sftp_channel_is_a_disconnect_and_server_errors_stay_ssh_errors() {
        for gone in ["sender dropped", "session closed", "SendError: channel closed", "RecvError: channel closed"] {
            let error = map_error(SftpError::UnexpectedBehavior(gone.into()));
            assert!(matches!(&error, Error::Disconnected { sent: None, .. }), "{gone}: {error:?}");
        }
        let error = map_error(SftpError::UnexpectedBehavior("Duplicate version".into()));
        assert!(matches!(&error, Error::Ssh(why) if why.starts_with("SFTP:")), "{error:?}");
        assert!(matches!(map_error(SftpError::Timeout), Error::Timeout { sent: None, .. }));
    }

    /// The download pipeline against an in-memory file that answers out of order, with short
    /// reads, slowly, or with an error.
    mod pipeline {
        use std::sync::Mutex;

        use super::*;

        #[derive(Default)]
        struct Seen {
            running: usize,
            peak_running: usize,
            asked: Vec<(u64, u32)>,
        }

        #[derive(Clone)]
        struct FakeFile {
            data: Arc<Mutex<Vec<u8>>>,
            short_reads: bool,
            fail_at: Option<u64>,
            /// Grow the file by this many bytes on the first read past its end.
            grow_once: Arc<Mutex<Option<usize>>>,
            seen: Arc<Mutex<Seen>>,
        }

        impl FakeFile {
            fn new(data: Vec<u8>) -> Self {
                Self { data: Arc::new(Mutex::new(data)), short_reads: false, fail_at: None, grow_once: Arc::default(), seen: Arc::default() }
            }
        }

        /// A deterministic scramble of an offset (answer order, short-read lengths).
        fn mix(offset: u64) -> u64 {
            offset.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17)
        }

        fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
            mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        impl ReadAt for FakeFile {
            fn read_at(&self, offset: u64, len: u32) -> impl Future<Output = Result<ReadOutcome, Error>> + Send + 'static {
                let me = self.clone();
                async move {
                    {
                        let mut seen = lock(&me.seen);
                        seen.running += 1;
                        seen.peak_running = seen.peak_running.max(seen.running);
                        seen.asked.push((offset, len));
                    }
                    // Answers come back in a scrambled order.
                    tokio::time::sleep(Duration::from_micros(mix(offset) % 3000)).await;
                    let outcome = (|| {
                        if me.fail_at.is_some_and(|at| offset >= at) {
                            return Err(Error::Ssh("SFTP: Failure".into()));
                        }
                        let mut data = lock(&me.data);
                        let start = usize::try_from(offset).unwrap_or(usize::MAX);
                        if start >= data.len() {
                            if let Some(grow) = lock(&me.grow_once).take() {
                                let len = data.len();
                                data.resize(len + grow, 0xEE);
                            }
                            return Ok(ReadOutcome::Eof);
                        }
                        let mut want = usize::try_from(len).unwrap_or(0);
                        if me.short_reads {
                            want = 1 + usize::try_from(mix(offset) % u64::from(len)).unwrap_or(0);
                        }
                        let end = start.saturating_add(want).min(data.len());
                        Ok(ReadOutcome::Data(data[start..end].to_vec()))
                    })();
                    lock(&me.seen).running -= 1;
                    outcome
                }
            }
        }

        #[derive(Default)]
        struct Collect {
            progress: Vec<(u64, Option<u64>)>,
        }

        impl Report for Collect {
            fn progress(&mut self, done: u64, total: Option<u64>) {
                self.progress.push((done, total));
            }
        }

        fn content(len: usize) -> Vec<u8> {
            (0..len).map(|i| u8::try_from(mix(i as u64) >> 56).unwrap_or(0)).collect()
        }

        fn run(file: &FakeFile, max_bytes: u64, window: u64) -> (Result<(u64, u64), Error>, Vec<u8>, Collect) {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap_or_else(|e| panic!("{e}"));
            let mut sink = Sink::Memory(Vec::new());
            let mut report = Collect::default();
            let total = u64::try_from(lock(&file.data).len()).ok();
            let result = runtime.block_on(read_pipelined(file, max_bytes, total, &mut report, &mut sink, window));
            let Sink::Memory(bytes) = sink else { panic!("wrong sink") };
            (result, bytes, report)
        }

        #[test]
        fn every_size_arrives_whole_and_in_order() {
            let chunk = usize::try_from(READ_CHUNK).unwrap_or(0);
            for len in [0, 1, chunk - 1, chunk, chunk + 1, 3 * chunk, 16 * chunk, 16 * chunk + 7, 40 * chunk + 12_345] {
                for short_reads in [false, true] {
                    let data = content(len);
                    let mut file = FakeFile::new(data.clone());
                    file.short_reads = short_reads;
                    let (result, bytes, _) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
                    let (written, peak) = result.unwrap_or_else(|e| panic!("{len} bytes, short reads {short_reads}: {e}"));
                    assert_eq!(written, len as u64);
                    assert!(bytes == data, "{len} bytes, short reads {short_reads}: content differs");
                    assert!(peak <= READ_WINDOW_BYTES, "memory bound: {peak}");
                }
            }
        }

        #[test]
        fn sixteen_reads_of_64_kib_run_at_once() {
            let file = FakeFile::new(content(64 * usize::try_from(READ_CHUNK).unwrap_or(0)));
            let (result, _, _) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
            let (_, peak) = result.unwrap_or_else(|e| panic!("{e}"));
            let seen = lock(&file.seen);
            assert_eq!(seen.peak_running, 16, "16 reads in flight");
            assert!(seen.asked.iter().all(|(_, len)| *len == READ_CHUNK), "64 KiB each");
            assert_eq!(peak, READ_WINDOW_BYTES);
            // Never more than the file + the one read that finds its end per slot of the window.
            assert!(seen.asked.len() <= 64 + 16, "{} reads", seen.asked.len());
        }

        #[test]
        fn a_smaller_window_bounds_memory_with_short_reads() {
            let mut file = FakeFile::new(content(5_000_000));
            file.short_reads = true;
            let window = 3 * u64::from(READ_CHUNK);
            let (result, bytes, _) = run(&file, u64::MAX - 1, window);
            let (written, peak) = result.unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(written, 5_000_000);
            assert!(bytes == content(5_000_000));
            assert!(peak <= window, "{peak} > {window}");
        }

        #[test]
        fn the_size_limit_holds_exactly() {
            let file = FakeFile::new(content(300_000));
            let (result, _, _) = run(&file, 300_000, READ_WINDOW_BYTES);
            assert_eq!(result.map(|(w, _)| w).ok(), Some(300_000), "exactly the limit is fine");
            let (result, _, _) = run(&file, 299_999, READ_WINDOW_BYTES);
            assert!(matches!(result, Err(Error::BodyTooLarge { limit: 299_999 })), "{result:?}");
            assert!(lock(&file.seen).asked.iter().all(|(offset, len)| offset + u64::from(*len) <= 600_000), "never asks far past the limit");
        }

        #[test]
        fn an_error_midway_stops_the_download() {
            let mut file = FakeFile::new(content(2_000_000));
            file.fail_at = Some(1_000_000);
            let (result, bytes, _) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
            assert!(matches!(&result, Err(Error::Ssh(why)) if why.contains("Failure")), "{result:?}");
            assert!(bytes.len() <= 1_000_000 + usize::try_from(READ_CHUNK).unwrap_or(0), "nothing past the read that failed is written");
            assert!(bytes == content(2_000_000)[..bytes.len()], "what was written is the file's start");
        }

        #[test]
        fn progress_only_grows_and_ends_at_the_written_size() {
            let file = FakeFile::new(content(3_000_000));
            let (result, _, report) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
            assert!(result.is_ok());
            assert!(report.progress.windows(2).all(|w| w[0].0 <= w[1].0), "{:?}", report.progress);
            assert!(report.progress.iter().all(|(done, total)| *done <= 3_000_000 && *total == Some(3_000_000)));
            assert_eq!(report.progress.last().map(|p| p.0), Some(3_000_000));
        }

        #[test]
        fn a_file_shorter_than_its_size_at_open_is_an_error_and_size_0_or_none_reads_to_the_end() {
            let file = FakeFile::new(content(300_000));
            let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap_or_else(|e| panic!("{e}"));
            for (total, ok) in [(Some(500_000), false), (Some(300_001), false), (Some(300_000), true), (Some(0), true), (None, true), (Some(100), true)] {
                let mut sink = Sink::Memory(Vec::new());
                let result = runtime.block_on(read_pipelined(&file, u64::MAX - 1, total, &mut Collect::default(), &mut sink, READ_WINDOW_BYTES));
                if ok {
                    assert_eq!(result.map(|(w, _)| w).ok(), Some(300_000), "reported size {total:?}");
                } else {
                    let expected = format!("expected {} bytes, its size when it was opened; received 300000", total.unwrap_or(0));
                    assert!(
                        matches!(&result, Err(Error::Ssh(why)) if why.starts_with("SFTP: the remote file was cut short during the download (") && why.contains(&expected)),
                        "{result:?}"
                    );
                }
            }
        }

        #[test]
        fn a_file_that_grows_while_it_is_read_gives_a_consistent_prefix_or_an_honest_error() {
            let data = content(200_000);
            let file = FakeFile::new(data.clone());
            *lock(&file.grow_once) = Some(100_000);
            let (result, bytes, _) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
            match result {
                Ok((written, _)) => {
                    assert_eq!(written, bytes.len() as u64);
                    assert!(bytes[..200_000] == data[..], "the original bytes first");
                }
                Err(error) => assert!(matches!(&error, Error::Ssh(why) if why.contains("changed size")), "{error:?}"),
            }
        }
    }
}
