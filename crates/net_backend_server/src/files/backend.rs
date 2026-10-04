//! Where file bytes live: the [`FileStore`] trait and the built-in [`LocalFileStore`] (a folder on
//! the server's disk).
//!
//! The module names every stored file by a key it makes itself (32 lower-case hex characters from
//! the operating system's random source): no file name, account id or other client text ever
//! reaches a path. A store keeps the bytes under that key and nothing else; the database holds
//! everything about the file.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use axum::body::Bytes;
use futures_util::future::BoxFuture;
use futures_util::stream::{BoxStream, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The bytes of a file, read or written in pieces.
pub type ByteStream<'a> = BoxStream<'a, io::Result<Bytes>>;

/// A storage backend for file bytes. Implement it to keep files elsewhere (another disk, an object
/// store) and hand it to [`Files::store`](crate::files::Files::store).
///
/// Every key is 32 lower-case hex characters ([`is_key`]).
pub trait FileStore: Send + Sync + 'static {
    /// Store `data` under `key`, all or nothing: on success every byte is kept (and durable when
    /// the call returns); when `data` yields an error or the write fails, nothing stays under
    /// `key` and the error is returned. Answers how many bytes were stored.
    fn put<'a>(&'a self, key: &'a str, data: ByteStream<'a>) -> BoxFuture<'a, io::Result<u64>>;

    /// The bytes stored under `key` (`ErrorKind::NotFound` when there are none).
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<ByteStream<'static>>>;

    /// Remove what is stored under `key` (nothing there is fine).
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>>;

    /// The keys of the bytes stored before `stored_before`, for the files module's purge: it
    /// deletes the ones no file row names (bytes left by an account deleted straight from the
    /// database, or by a crash between storing the bytes and writing the row). The default lists
    /// nothing, so a store without its own `keys` is never purged.
    fn keys(&self, stored_before: SystemTime) -> BoxStream<'_, io::Result<String>> {
        let _ = stored_before;
        futures_util::stream::empty().boxed()
    }
}

/// Whether `key` is a store key (32 lower-case hex characters).
pub fn is_key(key: &str) -> bool {
    key.len() == 32 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn bad_key() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a file store key")
}

/// The built-in store: one file per key in a folder (`<dir>/<k0k1>/<k2k3>/<key>`), written to
/// `<dir>/tmp/<key>.part` first, flushed to disk, then renamed into place (a crash or a broken
/// upload never leaves a partial file under its key).
#[derive(Clone, Debug)]
pub struct LocalFileStore {
    root: PathBuf,
}

impl LocalFileStore {
    /// A store in `root` (created on the first write when missing).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_of(&self, key: &str) -> PathBuf {
        self.root.join(&key[0..2]).join(&key[2..4]).join(key)
    }

    async fn write(&self, key: &str, mut data: ByteStream<'_>) -> io::Result<u64> {
        let tmp_dir = self.root.join("tmp");
        tokio::fs::create_dir_all(&tmp_dir).await?;
        let part = tmp_dir.join(format!("{key}.part"));
        // A write cut off before it ends (the request ran out of time, the server stopped) removes
        // its part file too.
        let mut guard = PartGuard(Some(part.clone()));
        let result = async {
            let mut file = tokio::fs::File::create(&part).await?;
            let mut written: u64 = 0;
            while let Some(chunk) = data.next().await {
                let chunk = chunk?;
                file.write_all(&chunk).await?;
                written = written.saturating_add(chunk.len() as u64);
            }
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            let target = self.path_of(key);
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::rename(&part, &target).await?;
            Ok(written)
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&part).await;
        }
        guard.0 = None;
        result
    }
}

/// Removes a part file whose write did not end (on drop, unless disarmed).
struct PartGuard(Option<PathBuf>);

impl Drop for PartGuard {
    fn drop(&mut self) {
        if let Some(part) = self.0.take() {
            let _ = std::fs::remove_file(part);
        }
    }
}

/// Run blocking file system work off the async threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(work).await.map_err(io::Error::other)?
}

fn is_two_hex(name: &str) -> bool {
    name.len() == 2 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn older(entry: &std::fs::DirEntry, before: SystemTime) -> bool {
    entry.metadata().and_then(|m| m.modified()).is_ok_and(|modified| modified < before)
}

/// The store's first-level folders; part files in `tmp` older than `before` (left by a crash) are
/// removed on the way.
fn first_level(root: &Path, before: SystemTime) -> io::Result<Vec<PathBuf>> {
    if let Ok(parts) = std::fs::read_dir(root.join("tmp")) {
        for entry in parts.flatten() {
            if entry.file_name().to_str().is_some_and(|n| n.ends_with(".part")) && older(&entry, before) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut folders = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(is_two_hex) && entry.file_type()?.is_dir() {
            folders.push(entry.path());
        }
    }
    Ok(folders)
}

/// The keys under one first-level folder stored before `before`.
fn keys_under(folder: &Path, before: SystemTime) -> io::Result<Vec<String>> {
    let mut keys = Vec::new();
    for second in std::fs::read_dir(folder)? {
        let second = second?;
        if !second.file_name().to_str().is_some_and(is_two_hex) || !second.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(second.path())? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if is_key(name) && older(&entry, before) {
                    keys.push(name.to_string());
                }
            }
        }
    }
    Ok(keys)
}

impl FileStore for LocalFileStore {
    fn put<'a>(&'a self, key: &'a str, data: ByteStream<'a>) -> BoxFuture<'a, io::Result<u64>> {
        Box::pin(async move {
            if !is_key(key) {
                return Err(bad_key());
            }
            self.write(key, data).await
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<ByteStream<'static>>> {
        Box::pin(async move {
            if !is_key(key) {
                return Err(bad_key());
            }
            let file = tokio::fs::File::open(self.path_of(key)).await?;
            let stream = futures_util::stream::unfold(Some(file), |file| async move {
                let mut file = file?;
                let mut buffer = vec![0u8; 64 * 1024];
                match file.read(&mut buffer).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buffer.truncate(n);
                        Some((Ok(Bytes::from(buffer)), Some(file)))
                    }
                    Err(error) => Some((Err(error), None)),
                }
            });
            Ok(stream.boxed())
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            if !is_key(key) {
                return Err(bad_key());
            }
            match tokio::fs::remove_file(self.path_of(key)).await {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            }
        })
    }

    /// Every key whose file was last written before `stored_before`. Also removes part files in
    /// `tmp` older than that (left by a crash during an upload).
    fn keys(&self, stored_before: SystemTime) -> BoxStream<'_, io::Result<String>> {
        let root = self.root.clone();
        futures_util::stream::once(blocking(move || first_level(&root, stored_before)))
            .flat_map(move |folders| match folders {
                Ok(folders) => futures_util::stream::iter(folders)
                    .then(move |folder| blocking(move || keys_under(&folder, stored_before)))
                    .flat_map(|keys| match keys {
                        Ok(keys) => futures_util::stream::iter(keys.into_iter().map(Ok)).boxed(),
                        Err(error) => futures_util::stream::once(std::future::ready(Err(error))).boxed(),
                    })
                    .boxed(),
                Err(error) => futures_util::stream::once(std::future::ready(Err(error))).boxed(),
            })
            .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp").join(format!("files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    async fn read_all(store: &LocalFileStore, key: &str) -> io::Result<Vec<u8>> {
        let mut stream = store.get(key).await?;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn round_trip_failure_and_delete() {
        let store = LocalFileStore::new(temp("store"));
        let key = "0123456789abcdef0123456789abcdef";
        let pieces: Vec<io::Result<Bytes>> = vec![Ok(Bytes::from_static(b"hello ")), Ok(Bytes::from(vec![7u8; 100_000]))];
        assert_eq!(store.put(key, futures_util::stream::iter(pieces).boxed()).await.ok(), Some(100_006));
        let back = read_all(&store, key).await.unwrap_or_default();
        assert_eq!((back.len(), &back[..6]), (100_006, &b"hello "[..]));
        assert!(store.root().join("01").join("23").join(key).is_file());

        // A stream that fails midway leaves nothing (also not the earlier copy's temp file).
        let other = "fedcba9876543210fedcba9876543210";
        let broken: Vec<io::Result<Bytes>> = vec![Ok(Bytes::from_static(b"part")), Err(io::Error::other("cut"))];
        assert!(store.put(other, futures_util::stream::iter(broken).boxed()).await.is_err());
        assert_eq!(store.get(other).await.err().map(|e| e.kind()), Some(io::ErrorKind::NotFound));
        assert_eq!(std::fs::read_dir(store.root().join("tmp")).map(|d| d.count()).unwrap_or(0), 0);

        // Keys are checked; deleting twice is fine.
        assert!(store.get("../../etc/passwd").await.is_err());
        assert!(store.put("ABC", futures_util::stream::empty().boxed()).await.is_err());
        assert!(store.delete(key).await.is_ok() && store.delete(key).await.is_ok());
        assert_eq!(store.get(key).await.err().map(|e| e.kind()), Some(io::ErrorKind::NotFound));
        assert!(is_key(key) && !is_key("0123") && !is_key(&"G".repeat(32)));
    }

    #[tokio::test]
    async fn keys_list_old_files_and_a_cut_write_leaves_nothing() {
        let store = LocalFileStore::new(temp("keys"));
        assert!(store.keys(SystemTime::now()).collect::<Vec<_>>().await.is_empty(), "no folder yet");
        let (a, b) = ("aa11223344556677889900aabbccddee", "bb11223344556677889900aabbccddee");
        for key in [a, b] {
            let data: Vec<io::Result<Bytes>> = vec![Ok(Bytes::from_static(b"x"))];
            assert!(store.put(key, futures_util::stream::iter(data).boxed()).await.is_ok());
        }
        // Not keys: other names in the folders. An old part file (a crash) goes.
        let _ = std::fs::write(store.root().join("aa").join("11").join("notes.txt"), b"x");
        let _ = std::fs::write(store.root().join("tmp").join("cccccccccccccccccccccccccccccccc.part"), b"x");
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        let mut keys: Vec<String> = store.keys(later).filter_map(|k| async move { k.ok() }).collect().await;
        keys.sort();
        assert_eq!(keys, [a, b]);
        assert_eq!(std::fs::read_dir(store.root().join("tmp")).map(|d| d.count()).unwrap_or(9), 0, "the old part file is removed");
        let earlier = SystemTime::now() - std::time::Duration::from_secs(3600);
        assert!(store.keys(earlier).collect::<Vec<_>>().await.is_empty(), "nothing that old");

        // A write dropped midway (a request out of time) leaves no part file.
        let endless = futures_util::stream::once(async { Ok(Bytes::from_static(b"start")) }).chain(futures_util::stream::pending()).boxed();
        let cut = tokio::time::timeout(std::time::Duration::from_millis(200), store.put("dd11223344556677889900aabbccddee", endless)).await;
        assert!(cut.is_err());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::fs::read_dir(store.root().join("tmp")).map(|d| d.count()).unwrap_or(0) != 0 {
            assert!(std::time::Instant::now() < deadline, "the part file stayed");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
