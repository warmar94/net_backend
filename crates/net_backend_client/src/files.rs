//! Files on the server (its `files` module): uploads streamed from disk or memory as
//! `multipart/form-data` with upload progress, downloads into memory or streamed to a file with
//! download progress. The settings and listings are typed calls of the protocol
//! ([`ListFiles`](crate::protocol::files::ListFiles), [`GetFile`](crate::protocol::files::GetFile),
//! [`EditFile`](crate::protocol::files::EditFile), [`DeleteFile`](crate::protocol::files::DeleteFile),
//! [`GetFileUsage`](crate::protocol::files::GetFileUsage)) through [`Client::call`](crate::Client::call).
//!
//! ```no_run
//! use net_backend_client::files::FileUpload;
//! use net_backend_client::protocol::files::{FileMeta, FileVisibility};
//! use net_backend_client::Client;
//!
//! # async fn run(client: Client) -> Result<(), net_backend_client::Error> {
//! let upload = FileUpload::path("replays/match-17.replay")
//!     .content_type("application/x-replay")
//!     .meta(FileMeta::new().with_visibility(FileVisibility::Public).with_metadata(serde_json::json!({"map": "caves"})));
//! let mut transfer = client.start_upload(upload);
//! while let Some(progress) = transfer.next_progress().await {
//!     println!("{} of {:?} bytes", progress.done, progress.total);
//! }
//! let file = transfer.finish().await?;
//! client.download_file_to(file.id, "downloads/match-17.replay").await?;
//! # Ok(())
//! # }
//! ```
//!
//! **The multipart body.** Two parts: `meta` (`application/json`: the [`FileMeta`], only when it
//! has a field set) and `file` (the bytes, with the file name and content type). A file from disk is
//! opened, measured and read in 64 KiB pieces while the request is sent (never loaded whole);
//! the request carries its exact `Content-Length`. The boundary is 128 random bits from the
//! operating system. A file that changes size while it is sent fails the upload.
//!
//! **Time limits.** A transfer has its own limit ([`FileUpload::timeout`],
//! [`DownloadOptions`]; 10 minutes by default) instead of the client's per-call timeout. A 401 is
//! answered with one token refresh and one more attempt (the body is built again).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use net_backend_protocol::files::{FileInfo, FileMeta, UPLOAD_FILE_PART, UPLOAD_META_PART};
use net_backend_protocol::{routes, FileId};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

use crate::http::{ReqBody, Streamed};
use crate::{Client, Error, Reply};

/// The time limit of a transfer by default (10 minutes).
pub const DEFAULT_TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);

/// The longest time limit of a transfer (24 hours); longer ones (e.g. `Duration::MAX`) are
/// clamped to it, shorter than 1 ms to 1 ms.
pub const MAX_TRANSFER_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// A transfer's deadline: `timeout` from now, clamped to 1 ms..=[`MAX_TRANSFER_TIMEOUT`].
fn transfer_deadline(timeout: Duration) -> Instant {
    let now = Instant::now();
    let timeout = timeout.clamp(Duration::from_millis(1), MAX_TRANSFER_TIMEOUT);
    now.checked_add(timeout).unwrap_or(now)
}

/// The piece size of a file read from disk.
const CHUNK: usize = 64 * 1024;

/// What to upload: the bytes (from a file or memory), their name and content type, and the
/// settings ([`FileMeta`]).
#[derive(Clone)]
pub struct FileUpload {
    source: Source,
    name: Option<String>,
    content_type: Option<String>,
    meta: FileMeta,
    timeout: Duration,
}

#[derive(Clone)]
enum Source {
    Path(PathBuf),
    Bytes(Bytes),
}

impl fmt::Debug for FileUpload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match &self.source {
            Source::Path(_) => "path".to_string(),
            Source::Bytes(bytes) => format!("{} bytes", bytes.len()),
        };
        f.debug_struct("FileUpload").field("source", &source).field("content_type", &self.content_type).field("timeout", &self.timeout).finish_non_exhaustive()
    }
}

impl FileUpload {
    /// A file from disk (read while it is sent; the name defaults to the path's file name).
    pub fn path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        Self { source: Source::Path(path), name, content_type: None, meta: FileMeta::new(), timeout: DEFAULT_TRANSFER_TIMEOUT }
    }

    /// Bytes from memory, named `name`.
    pub fn bytes(name: impl Into<String>, bytes: impl Into<Bytes>) -> Self {
        Self { source: Source::Bytes(bytes.into()), name: Some(name.into()), content_type: None, meta: FileMeta::new(), timeout: DEFAULT_TRANSFER_TIMEOUT }
    }

    /// The file part's name (what the server stores unless the meta names another).
    pub fn file_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The content type (`image/png`; default `application/octet-stream`).
    pub fn content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// The settings of the new file (visibility, share list, metadata, an expected SHA-256).
    pub fn meta(mut self, meta: FileMeta) -> Self {
        self.meta = meta;
        self
    }

    /// The time limit of the whole upload (default 10 minutes; 1 ms..=24 h, see
    /// [`MAX_TRANSFER_TIMEOUT`]).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.clamp(Duration::from_millis(1), MAX_TRANSFER_TIMEOUT);
        self
    }
}

/// How a download runs.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct DownloadOptions {
    /// The time limit of the whole download (default 10 minutes; used clamped to 1 ms..=24 h, see
    /// [`MAX_TRANSFER_TIMEOUT`]).
    pub timeout: Duration,
    /// The largest file read into memory ([`Client::download_file`]); a larger one answers
    /// [`Error::BodyTooLarge`]. A download to disk ([`Client::start_download_to`]) is not limited
    /// by it.
    pub max_bytes: u64,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self { timeout: DEFAULT_TRANSFER_TIMEOUT, max_bytes: 64 * 1024 * 1024 }
    }
}

/// How far a transfer is: bytes moved so far and the size, when known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransferProgress {
    /// Bytes moved so far (an upload: the file's bytes handed to the connection; a download:
    /// received).
    pub done: u64,
    /// The size (an upload: the file's; a download: the server's `Content-Length`).
    pub total: Option<u64>,
}

/// A running upload or download ([`Client::start_upload`], [`Client::start_download_to`]): its
/// progress, then exactly one result. `done` only grows; the last report comes before the result.
/// **Dropping it cancels the transfer** (a download's part file is removed). Works from a game loop
/// too ([`try_progress`](Self::try_progress), [`try_finish`](Self::try_finish)).
pub struct FileTransfer<T> {
    progress: watch::Receiver<TransferProgress>,
    seen: TransferProgress,
    result: Reply<T>,
    _cancel: oneshot::Sender<()>,
}

impl<T> fmt::Debug for FileTransfer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileTransfer").field("progress", &*self.progress.borrow()).field("finished", &self.result.is_taken()).finish()
    }
}

impl<T: Send + 'static> FileTransfer<T> {
    fn failed(error: Error) -> Self {
        let (_, progress) = watch::channel(TransferProgress::default());
        let (cancel, _) = oneshot::channel();
        Self { progress, seen: TransferProgress::default(), result: Reply::ready(Err(error)), _cancel: cancel }
    }

    /// Run `work` (which reports to the sender) as its own task on the current runtime.
    fn spawn<F, Fut>(work: F) -> Self
    where
        F: FnOnce(watch::Sender<TransferProgress>) -> Fut,
        Fut: std::future::Future<Output = Result<T, Error>> + Send + 'static,
    {
        let handle = match crate::runtime::current() {
            Ok(handle) => handle,
            Err(error) => return Self::failed(error),
        };
        let (sender, progress) = watch::channel(TransferProgress::default());
        let (cancel, cancelled) = oneshot::channel::<()>();
        let (answer, result) = Reply::channel();
        let future = work(sender);
        handle.spawn(async move {
            tokio::select! {
                outcome = future => { let _ = answer.send(outcome); }
                _ = cancelled => { let _ = answer.send(Err(Error::Cancelled { sent: None })); }
            }
        });
        Self { progress, seen: TransferProgress::default(), result, _cancel: cancel }
    }
}

impl<T> FileTransfer<T> {
    /// The latest progress (never blocks; no runtime needed).
    pub fn progress(&self) -> TransferProgress {
        *self.progress.borrow()
    }

    /// The next progress report; `None` once the transfer ended and its last report was taken.
    pub async fn next_progress(&mut self) -> Option<TransferProgress> {
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
    pub fn try_progress(&mut self) -> Option<TransferProgress> {
        let current = *self.progress.borrow();
        (current != self.seen).then(|| {
            self.seen = current;
            current
        })
    }

    /// Wait for the result.
    pub async fn finish(self) -> Result<T, Error> {
        let FileTransfer { result, _cancel, .. } = self;
        let result = result.await;
        drop(_cancel);
        result
    }

    /// The result, if it came (never blocks; no runtime needed).
    pub fn try_finish(&mut self) -> Option<Result<T, Error>> {
        self.result.try_take()
    }

    /// Block until the result (programs without a runtime; see [`Reply::wait`]).
    pub fn wait(self) -> Result<T, Error> {
        let FileTransfer { result, _cancel, .. } = self;
        let result = result.wait();
        drop(_cancel);
        result
    }
}

// ---- the multipart body -------------------------------------------------------------------------------

/// A field name or file name for a `Content-Disposition` quoted string, as browsers write it
/// (`"` → `%22`, CR → `%0D`, LF → `%0A`); refused: other control characters and a trailing
/// backslash (parsers cut or misread them).
fn quoted(text: &str) -> Result<String, Error> {
    if text.chars().any(|c| c.is_control() && c != '\r' && c != '\n') || text.ends_with('\\') {
        return Err(Error::invalid("the file name has a control character or ends with a backslash"));
    }
    Ok(text.replace('"', "%22").replace('\r', "%0D").replace('\n', "%0A"))
}

/// The parts around the file's bytes.
struct Frame {
    boundary: String,
    head: Bytes,
    tail: Bytes,
}

fn frame(upload: &FileUpload) -> Result<Frame, Error> {
    let mut random = [0u8; 16];
    SystemRandom::new().fill(&mut random).map_err(|_| Error::invalid("the operating system's random source failed"))?;
    let boundary = format!("nbc-{}", random.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let content_type = upload.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());
    if http::HeaderValue::from_str(&content_type).is_err() || content_type.contains(['\r', '\n']) {
        return Err(Error::invalid("the content type is not a valid header value"));
    }
    let mut head = Vec::new();
    if upload.meta != FileMeta::new() {
        let json = serde_json::to_vec(&upload.meta).map_err(|e| Error::invalid(format!("the file settings cannot be encoded: {e}")))?;
        head.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{UPLOAD_META_PART}\"\r\nContent-Type: application/json\r\n\r\n").as_bytes(),
        );
        head.extend_from_slice(&json);
        head.extend_from_slice(b"\r\n");
    }
    let file_name = match &upload.name {
        Some(name) => format!("; filename=\"{}\"", quoted(name)?),
        None => String::new(),
    };
    head.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{UPLOAD_FILE_PART}\"{file_name}\r\nContent-Type: {content_type}\r\n\r\n").as_bytes(),
    );
    let tail = Bytes::from(format!("\r\n--{boundary}--\r\n"));
    Ok(Frame { boundary, head: Bytes::from(head), tail })
}

/// The file's bytes as a body source.
enum Reader {
    File { file: tokio::fs::File, left: u64 },
    Memory { bytes: Bytes, at: usize },
}

impl Reader {
    async fn open(source: &Source) -> Result<(Self, u64), Error> {
        match source {
            Source::Path(path) => {
                let file = tokio::fs::File::open(path).await.map_err(|e| Error::invalid(format!("the file to upload cannot be opened: {e}")))?;
                let size = file.metadata().await.map_err(|e| Error::invalid(format!("the file to upload cannot be measured: {e}")))?.len();
                Ok((Reader::File { file, left: size }, size))
            }
            Source::Bytes(bytes) => Ok((Reader::Memory { bytes: bytes.clone(), at: 0 }, bytes.len() as u64)),
        }
    }

    /// The next piece, `None` at the end.
    async fn next(&mut self) -> io::Result<Option<Bytes>> {
        match self {
            Reader::File { file, left } => {
                if *left == 0 {
                    return Ok(None);
                }
                let want = usize::try_from((*left).min(CHUNK as u64)).unwrap_or(CHUNK);
                let mut buffer = vec![0u8; want];
                let n = file.read(&mut buffer).await?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the file got shorter while it was sent"));
                }
                buffer.truncate(n);
                *left -= n as u64;
                Ok(Some(Bytes::from(buffer)))
            }
            Reader::Memory { bytes, at } => {
                if *at >= bytes.len() {
                    return Ok(None);
                }
                let end = (*at + CHUNK).min(bytes.len());
                let piece = bytes.slice(*at..end);
                *at = end;
                Ok(Some(piece))
            }
        }
    }
}

/// The body: head, the file's pieces, tail. A task reads the pieces (a small queue ahead); the body
/// counts each piece of the file into the progress when the connection takes it.
struct ChannelBody {
    pieces: tokio::sync::mpsc::Receiver<(io::Result<Bytes>, bool)>,
    done: u64,
    size: u64,
    progress: watch::Sender<TransferProgress>,
}

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        match self.pieces.poll_recv(cx) {
            Poll::Ready(Some((Ok(piece), counted))) => {
                if counted {
                    self.done += piece.len() as u64;
                    let progress = TransferProgress { done: self.done, total: Some(self.size) };
                    self.progress.send_replace(progress);
                }
                Poll::Ready(Some(Ok(hyper::body::Frame::data(piece))))
            }
            Poll::Ready(Some((Err(error), _))) => Poll::Ready(Some(Err(Box::new(error)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn body(frame: &Frame, mut reader: Reader, size: u64, progress: watch::Sender<TransferProgress>) -> ReqBody {
    let (sender, pieces) = tokio::sync::mpsc::channel(4);
    let (head, tail) = (frame.head.clone(), frame.tail.clone());
    tokio::spawn(async move {
        if sender.send((Ok(head), false)).await.is_err() {
            return;
        }
        loop {
            match reader.next().await {
                Ok(Some(piece)) => {
                    if sender.send((Ok(piece), true)).await.is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = sender.send((Ok(tail), false)).await;
                    return;
                }
                Err(error) => {
                    let _ = sender.send((Err(error), false)).await;
                    return;
                }
            }
        }
    });
    ChannelBody { pieces, done: 0, size, progress }.boxed()
}

// ---- the client's calls -------------------------------------------------------------------------------

/// Whether a download's `ETag` names this SHA-256.
fn etag_sha(headers: &http::HeaderMap) -> Option<String> {
    headers.get(http::header::ETAG).and_then(|v| v.to_str().ok()).map(|v| v.trim().trim_matches('"').to_string()).filter(|v| v.len() == 64)
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl Client {
    /// Upload a file (`POST /v1/files`); see [`start_upload`](Self::start_upload) for the progress.
    pub async fn upload_file(&self, upload: FileUpload) -> Result<FileInfo, Error> {
        self.start_upload(upload).finish().await
    }

    /// Start an upload with progress reports (the file's bytes handed to the connection).
    pub fn start_upload(&self, upload: FileUpload) -> FileTransfer<FileInfo> {
        let client = self.clone();
        FileTransfer::spawn(move |progress| async move { client.run_upload(upload, progress).await })
    }

    async fn run_upload(&self, upload: FileUpload, progress: watch::Sender<TransferProgress>) -> Result<FileInfo, Error> {
        let deadline = transfer_deadline(upload.timeout);
        let frame = frame(&upload)?;
        let content_type = format!("multipart/form-data; boundary={}", frame.boundary);
        let mut refreshed = false;
        loop {
            let (token, generation) = self.access_token(deadline).await?;
            let (reader, size) = Reader::open(&upload.source).await?;
            progress.send_replace(TransferProgress { done: 0, total: Some(size) });
            let length = frame.head.len() as u64 + size + frame.tail.len() as u64;
            let body = body(&frame, reader, size, progress.clone());
            let answer = self.inner.http.send_body(routes::files::LIST, &content_type, length, body, Some(token.expose()), deadline).await?;
            if answer.status == 401 && !refreshed {
                // The upload never ran: one refresh (unless another caller brought a new token), once more.
                refreshed = true;
                if self.inner.session.access().map(|(_, g, _)| g) == Some(generation) {
                    self.refresh_shared(deadline).await?;
                }
                continue;
            }
            return answer.decode();
        }
    }

    /// Open a file's download (`GET /v1/files/{file}/content`) with the Bearer token (one refresh
    /// and one more attempt on a 401).
    async fn open_download(&self, file: FileId, deadline: Instant) -> Result<Streamed, Error> {
        let path = routes::file_content_path(file);
        let mut refreshed = false;
        loop {
            let (token, generation) = self.access_token(deadline).await?;
            let streamed = self.inner.http.open(&path, Some(token.expose()), None, deadline).await?;
            match streamed.status {
                200 => return Ok(streamed),
                401 if !refreshed => {
                    refreshed = true;
                    if self.inner.session.access().map(|(_, g, _)| g) == Some(generation) {
                        self.refresh_shared(deadline).await?;
                    }
                }
                _ => return Err(crate::http::Http::error_of(streamed, deadline).await),
            }
        }
    }

    /// Download a file into memory (at most `options.max_bytes`); the bytes are checked against
    /// the SHA-256 the server sends.
    pub async fn download_file(&self, file: FileId, options: DownloadOptions) -> Result<Vec<u8>, Error> {
        crate::runtime::current()?;
        let deadline = transfer_deadline(options.timeout);
        let streamed = self.open_download(file, deadline).await?;
        let expected = etag_sha(&streamed.headers);
        let mut body = streamed.body;
        let mut out = Vec::new();
        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
        let read = async {
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| Error::network(format!("reading the file failed: {e}"), Some(true)))?;
                if let Ok(data) = frame.into_data() {
                    if (out.len() + data.len()) as u64 > options.max_bytes {
                        return Err(Error::BodyTooLarge { limit: options.max_bytes });
                    }
                    hasher.update(&data);
                    out.extend_from_slice(&data);
                }
            }
            Ok(())
        };
        match tokio::time::timeout_at(deadline, read).await {
            Ok(result) => result?,
            Err(_) => return Err(Error::timeout("the file did not arrive completely before the time limit", Some(true))),
        }
        if expected.is_some_and(|sha| sha != hex(hasher.finish().as_ref())) {
            return Err(Error::network("the downloaded bytes do not match the file's SHA-256", Some(true)));
        }
        Ok(out)
    }

    /// Download a file to `path` (streamed to a part file next to it, `<name>.<process>-<n>.part`,
    /// renamed to `path` when complete and its SHA-256 matches); the bytes written.
    pub async fn download_file_to(&self, file: FileId, path: impl AsRef<Path>) -> Result<u64, Error> {
        self.start_download_to(file, path, DownloadOptions::default()).finish().await
    }

    /// Start a download to `path` with progress reports.
    pub fn start_download_to(&self, file: FileId, path: impl AsRef<Path>, options: DownloadOptions) -> FileTransfer<u64> {
        let client = self.clone();
        let path = path.as_ref().to_path_buf();
        FileTransfer::spawn(move |progress| async move { client.run_download_to(file, path, options, progress).await })
    }

    async fn run_download_to(&self, file: FileId, path: PathBuf, options: DownloadOptions, progress: watch::Sender<TransferProgress>) -> Result<u64, Error> {
        let deadline = transfer_deadline(options.timeout);
        let streamed = self.open_download(file, deadline).await?;
        let expected = etag_sha(&streamed.headers);
        let total = streamed.headers.get(http::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        progress.send_replace(TransferProgress { done: 0, total });
        // A part file no other download uses (two downloads to one path never write into each other).
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut part_name = path.file_name().map(|n| n.to_os_string()).ok_or_else(|| Error::invalid("the download path has no file name"))?;
        part_name.push(format!(".{}-{}.part", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let part = path.with_file_name(part_name);
        let guard = PartFile(Some(part.clone()));
        let mut out = tokio::fs::File::create(&part).await.map_err(|e| Error::invalid(format!("the download file cannot be created: {e}")))?;
        let mut body = streamed.body;
        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
        let mut done = 0u64;
        let read = async {
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| Error::network(format!("reading the file failed: {e}"), Some(true)))?;
                if let Ok(data) = frame.into_data() {
                    hasher.update(&data);
                    out.write_all(&data).await.map_err(|e| Error::network(format!("writing the download failed: {e}"), Some(true)))?;
                    done += data.len() as u64;
                    progress.send_replace(TransferProgress { done, total });
                }
            }
            out.flush().await.map_err(|e| Error::network(format!("writing the download failed: {e}"), Some(true)))?;
            out.sync_all().await.map_err(|e| Error::network(format!("writing the download failed: {e}"), Some(true)))?;
            Ok::<(), Error>(())
        };
        match tokio::time::timeout_at(deadline, read).await {
            Ok(result) => result?,
            Err(_) => return Err(Error::timeout("the file did not arrive completely before the time limit", Some(true))),
        }
        if expected.is_some_and(|sha| sha != hex(hasher.finish().as_ref())) || total.is_some_and(|t| t != done) {
            return Err(Error::network("the downloaded bytes do not match the file's size or SHA-256", Some(true)));
        }
        tokio::fs::rename(&part, &path).await.map_err(|e| Error::invalid(format!("the download could not be put in place: {e}")))?;
        guard.keep();
        Ok(done)
    }
}

/// Removes a download's part file unless it was put in place.
struct PartFile(Option<PathBuf>);

impl PartFile {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_body_is_exact_and_counted() {
        let upload = FileUpload::bytes("a \"b\".bin", vec![7u8; 200_000]).content_type("application/x-test").meta(FileMeta::new().with_name("x"));
        let framed = frame(&upload).unwrap_or_else(|_| Frame { boundary: String::new(), head: Bytes::new(), tail: Bytes::new() });
        assert_eq!(framed.boundary.len(), 36);
        let head = String::from_utf8_lossy(&framed.head).into_owned();
        assert!(head.contains("name=\"meta\"\r\nContent-Type: application/json\r\n\r\n{\"name\":\"x\"}\r\n"), "{head}");
        assert!(head.contains("name=\"file\"; filename=\"a %22b%22.bin\"\r\nContent-Type: application/x-test\r\n\r\n"), "{head}");
        let (reader, size) = Reader::open(&upload.source).await.unwrap_or((Reader::Memory { bytes: Bytes::new(), at: 0 }, 0));
        let (sender, receiver) = watch::channel(TransferProgress::default());
        let collected = body(&framed, reader, size, sender).collect().await.map(|c| c.to_bytes()).unwrap_or_default();
        assert_eq!(collected.len() as u64, framed.head.len() as u64 + 200_000 + framed.tail.len() as u64);
        assert_eq!(*receiver.borrow(), TransferProgress { done: 200_000, total: Some(200_000) });
        assert!(collected.ends_with(format!("\r\n--{}--\r\n", framed.boundary).as_bytes()));
        // No meta part when no setting is given; bad names and types are refused.
        let bare = frame_of(FileUpload::bytes("a", vec![1]));
        assert!(!bare.contains("name=\"meta\""), "{bare}");
        assert!(frame(&FileUpload::bytes("a\u{1}", vec![1])).is_err());
        assert!(frame(&FileUpload::bytes("a\\", vec![1])).is_err());
        assert!(frame(&FileUpload::bytes("a", vec![1]).content_type("a/b\r\nX: y")).is_err());
    }

    #[test]
    fn transfer_time_limits_are_clamped() {
        let before = Instant::now();
        let far = transfer_deadline(Duration::MAX);
        assert!(far >= before + MAX_TRANSFER_TIMEOUT && far <= Instant::now() + MAX_TRANSFER_TIMEOUT, "Duration::MAX: 24 h, not an instant timeout");
        assert!(transfer_deadline(Duration::ZERO) > before, "zero: 1 ms");
        assert_eq!(FileUpload::bytes("a", vec![1]).timeout(Duration::MAX).timeout, MAX_TRANSFER_TIMEOUT);
        assert_eq!(FileUpload::bytes("a", vec![1]).timeout(Duration::ZERO).timeout, Duration::from_millis(1));
    }

    fn frame_of(upload: FileUpload) -> String {
        frame(&upload).map(|f| String::from_utf8_lossy(&f.head).into_owned()).unwrap_or_default()
    }
}
