//! [`FileService`]: uploads, reads, changes and deletes of stored files, for the routes and for
//! server code.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::body::Bytes;
use futures_util::stream::StreamExt;
use net_backend_protocol::files::{is_sha256_hex, is_valid_content_type, name_problem, FileInfo, FileMeta, FileQuery, FileUsage, FileVisibility, UpdateFile};
use net_backend_protocol::{codes, Cursor, FileId, Page, PageRequest, UnixMillis, UserId, ValidationDetails};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::backend::{is_key, ByteStream, FileStore};
use super::config::FilesConfig;
use super::events::{AfterFileChange, BeforeFileUpdate, BeforeFileUpload, FileChange};
use super::store::{self, FileRow};
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::rate_limit::{KeyedBuckets, RateDecision};
use crate::state::AppState;

/// The default content type.
const OCTET_STREAM: &str = "application/octet-stream";

/// Store keys one purge statement checks.
const PURGE_CHUNK: usize = 500;

/// Stored files. A state value (`Ext<FileService>` in handlers, `state.get::<FileService>()`
/// elsewhere) once the [`Files`](super::Files) module is registered. Server code reads any file
/// with [`read_as`](Self::read_as) / [`open`](Self::open) and the owner's view with the owner's id.
#[derive(Clone)]
pub struct FileService(Arc<Inner>);

struct Inner {
    config: FilesConfig,
    store: Arc<dyn FileStore>,
    rate: Option<KeyedBuckets<UserId>>,
}

impl std::fmt::Debug for FileService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileService").field(&self.0.config).finish()
    }
}

/// What an upload stream did, counted while it passes to the store.
#[derive(Default)]
struct Progress {
    bytes: u64,
    over: bool,
    broken: bool,
    hasher: Sha256,
}

fn invalid(field: &str, problem: impl Into<String>) -> AppError {
    let mut details = ValidationDetails::new();
    details.add(field, problem.into());
    AppError::validation(details)
}

fn not_found() -> AppError {
    AppError::not_found("no such file")
}

fn visibility_of(text: &str) -> FileVisibility {
    FileVisibility::parse(text).unwrap_or(FileVisibility::Private)
}

/// Whether the friends module is registered (the `friends` visibility needs it).
fn friends_known(state: &AppState) -> bool {
    #[cfg(feature = "friends")]
    {
        state.get::<crate::friends::FriendService>().is_some()
    }
    #[cfg(not(feature = "friends"))]
    {
        let _ = state;
        false
    }
}

/// Whether `a` and `b` are friends (false without the friends module).
async fn are_friends(state: &AppState, a: UserId, b: UserId) -> Result<bool, AppError> {
    #[cfg(feature = "friends")]
    {
        match state.get::<crate::friends::FriendService>() {
            Some(friends) => friends.are_friends(state, a, b).await,
            None => Ok(false),
        }
    }
    #[cfg(not(feature = "friends"))]
    {
        let _ = (state, a, b);
        Ok(false)
    }
}

/// The last part of a client's file name (`C:\shots\a.png` → `a.png`), trimmed.
fn base_name(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name).trim()
}

fn new_key() -> Result<String, AppError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| AppError::internal(io::Error::other(format!("the system random generator failed: {e}"))))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl FileService {
    pub(crate) fn new(config: FilesConfig, store: Arc<dyn FileStore>) -> Self {
        let rate =
            (config.upload_rate > 0).then(|| KeyedBuckets::new(config.upload_rate, Duration::from_secs(u64::from(config.upload_rate_window_secs)), 100_000));
        Self(Arc::new(Inner { config, store, rate }))
    }

    /// The settings.
    pub fn config(&self) -> &FilesConfig {
        &self.0.config
    }

    /// Count one upload of `user` against `upload_rate` (429 `rate_limited` over it).
    pub(crate) fn check_rate(&self, user: UserId) -> Result<(), AppError> {
        match self.0.rate.as_ref().map(|rate| rate.check(user)) {
            Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
            _ => Ok(()),
        }
    }

    // ---- reading ----------------------------------------------------------------------------------

    async fn row(&self, state: &AppState, file: FileId) -> Result<Option<FileRow>, AppError> {
        Ok(state.db().fetch_optional::<FileRow, _>(&store::file(file.get())).await?)
    }

    /// Whether `reader` may read the file of `row`.
    async fn may_read(&self, state: &AppState, reader: UserId, row: &FileRow) -> Result<bool, AppError> {
        if row.owner_id == reader.get() {
            return Ok(true);
        }
        Ok(match visibility_of(&row.visibility) {
            FileVisibility::Public => true,
            FileVisibility::Friends => are_friends(state, UserId(row.owner_id), reader).await?,
            FileVisibility::Shared => state.db().fetch_one::<store::CountRow, _>(&store::is_shared(row.id, reader.get())).await?.n > 0,
            _ => false,
        })
    }

    /// The protocol's view of a row; the shares only for the owner.
    async fn info(&self, state: &AppState, row: &FileRow, with_shares: bool) -> Result<FileInfo, AppError> {
        let mut info = FileInfo::new(
            FileId(row.id),
            UserId(row.owner_id),
            row.name.clone(),
            row.content_type.clone(),
            u64::try_from(row.size_bytes).unwrap_or(0),
            row.sha256.clone(),
            UnixMillis(row.created_at),
        );
        info.updated_at = UnixMillis(row.updated_at);
        info.visibility = visibility_of(&row.visibility);
        info.metadata = row.metadata.as_deref().map(serde_json::from_slice::<Value>).transpose().map_err(AppError::internal)?;
        if with_shares && info.visibility == FileVisibility::Shared {
            info.shared_with = state.db().fetch_all::<store::UserRow, _>(&store::shares_of(row.id)).await?.into_iter().map(|r| UserId(r.user_id)).collect();
        }
        Ok(info)
    }

    /// A file's settings as `reader` sees them: `None` when it does not exist or `reader` may not
    /// read it (the owner always may; others by its visibility).
    pub async fn read_as(&self, state: &AppState, reader: UserId, file: FileId) -> Result<Option<FileInfo>, AppError> {
        let Some(row) = self.row(state, file).await? else { return Ok(None) };
        if !self.may_read(state, reader, &row).await? {
            return Ok(None);
        }
        let owner = row.owner_id == reader.get();
        self.info(state, &row, owner).await.map(Some)
    }

    /// A file's settings and bytes as `reader` may read them (404 otherwise).
    pub async fn open(&self, state: &AppState, reader: UserId, file: FileId) -> Result<(FileInfo, ByteStream<'static>), AppError> {
        let row = self.row(state, file).await?.ok_or_else(not_found)?;
        if !self.may_read(state, reader, &row).await? {
            return Err(not_found());
        }
        let info = self.info(state, &row, false).await?;
        let bytes = self.0.store.get(&row.storage_key).await.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                tracing::error!(file = row.id, "files: the store has no bytes for a stored file");
            }
            AppError::internal(error)
        })?;
        Ok((info, bytes))
    }

    /// A page of files: `reader`'s own (no `owner`, or `reader`'s id), or another player's that
    /// `reader` may read; newest first.
    pub async fn list(&self, state: &AppState, reader: UserId, query: &FileQuery) -> Result<Page<FileInfo>, AppError> {
        let mut page = PageRequest::first();
        page.cursor = query.cursor.clone();
        page.limit = query.limit;
        page.validate()?;
        let before = match &query.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let owner = query.owner.unwrap_or(reader);
        let limit = u64::from(page.limit_or_default());
        let shown: Vec<&str> = if owner == reader {
            Vec::new()
        } else {
            let mut shown = vec![FileVisibility::Public.as_str()];
            if are_friends(state, owner, reader).await? {
                shown.push(FileVisibility::Friends.as_str());
            }
            shown
        };
        let reader_filter = (owner != reader).then_some((reader.get(), shown.as_slice()));
        let mut rows = state.db().fetch_all::<FileRow, _>(&store::page(owner.get(), before, limit + 1, reader_filter)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            items.push(self.info(state, row, false).await?);
        }
        Ok(Page::new(items, next))
    }

    /// What `user`'s files use, with the limits.
    pub async fn usage(&self, state: &AppState, user: UserId) -> Result<FileUsage, AppError> {
        let dialect = state.db().dialect();
        let row = state.db().fetch_one::<store::UsageRow, _>(&store::usage(user.get(), dialect)).await?;
        let config = &self.0.config;
        Ok(FileUsage::new(
            u64::try_from(row.n).unwrap_or(0),
            u64::try_from(row.bytes).unwrap_or(0),
            u64::from(config.max_files_per_user),
            config.max_bytes_per_user,
            config.max_file_bytes,
        ))
    }

    // ---- checks shared by upload and change -------------------------------------------------------

    fn check_metadata(&self, metadata: Option<&Value>) -> Result<Option<Vec<u8>>, AppError> {
        let Some(value) = metadata.filter(|v| !v.is_null()) else { return Ok(None) };
        let bytes = serde_json::to_vec(value).map_err(AppError::internal)?;
        let max = self.0.config.max_metadata_bytes;
        if bytes.len() > max {
            return Err(invalid("metadata", format!("is larger than {max} bytes")));
        }
        Ok(Some(bytes))
    }

    /// The visibility and the share list after the rules: known visibility, `friends` only with
    /// the friends module, a share list only for `shared` (deduplicated, without the owner, at
    /// most `max_shared_with`, every account existing).
    async fn check_sharing(&self, state: &AppState, owner: UserId, visibility: FileVisibility, shared_with: Option<&[UserId]>) -> Result<Vec<i64>, AppError> {
        match visibility {
            FileVisibility::Unknown => return Err(invalid("visibility", "is not private, public, friends or shared")),
            FileVisibility::Friends if !friends_known(state) => return Err(invalid("visibility", "`friends` needs the friends module on this server")),
            _ => {}
        }
        let mut users: Vec<i64> = shared_with.unwrap_or_default().iter().map(|u| u.get()).filter(|u| *u != owner.get()).collect();
        users.sort_unstable();
        users.dedup();
        if visibility != FileVisibility::Shared {
            if !users.is_empty() {
                return Err(invalid("shared_with", "is only for the `shared` visibility"));
            }
            return Ok(users);
        }
        let max = self.0.config.max_shared_with as usize;
        if users.len() > max {
            return Err(invalid("shared_with", format!("names more than {max} accounts")));
        }
        if !users.is_empty() {
            let found = state.db().fetch_one::<store::CountRow, _>(&store::count_accounts(&users)).await?.n;
            if usize::try_from(found).unwrap_or(0) != users.len() {
                return Err(invalid("shared_with", "names an account that does not exist"));
            }
        }
        Ok(users)
    }

    // ---- upload -----------------------------------------------------------------------------------

    /// Store an upload of `owner`: `meta` (the settings; validated before any byte is read), the
    /// file part's own name and content type as defaults, and the bytes. The quotas and
    /// `max_file_bytes` are enforced while the bytes stream into the store; a broken upload, a
    /// refused one (a hook, a quota, a checksum) leaves nothing behind.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn upload(
        &self,
        state: &AppState,
        ctx: &HookCtx,
        owner: UserId,
        meta: FileMeta,
        part_name: Option<&str>,
        part_type: Option<&str>,
        data: ByteStream<'_>,
    ) -> Result<FileInfo, AppError> {
        meta.validate()?;
        let config = &self.0.config;
        let name = match meta.name.as_deref().or(part_name.map(base_name).filter(|n| !n.is_empty())) {
            Some(name) => {
                if let Some(problem) = name_problem(name) {
                    return Err(invalid("name", problem));
                }
                name.trim().to_string()
            }
            None => "file".to_string(),
        };
        let content_type = meta.content_type.clone().or_else(|| part_type.map(str::to_ascii_lowercase)).unwrap_or_else(|| OCTET_STREAM.to_string());
        let content_type = content_type.split(';').next().unwrap_or(OCTET_STREAM).trim().to_ascii_lowercase();
        if !is_valid_content_type(&content_type) {
            return Err(invalid("content_type", "is not a plain type/subtype"));
        }
        if !config.allows(&content_type) {
            return Err(invalid("content_type", "is not accepted by this server"));
        }
        let metadata = self.check_metadata(meta.metadata.as_ref())?;
        let visibility = meta.visibility.unwrap_or_default();
        let shares = self.check_sharing(state, owner, visibility, meta.shared_with.as_deref()).await?;

        // The room left: the quota decides how many bytes may come (checked again under the lock).
        let usage = self.usage(state, owner).await?;
        if usage.files >= u64::from(config.max_files_per_user) {
            return Err(AppError::new(codes::QUOTA_EXCEEDED, "the account owns as many files as allowed"));
        }
        let room = config.max_bytes_per_user.saturating_sub(usage.bytes);
        if room == 0 {
            return Err(AppError::new(codes::QUOTA_EXCEEDED, "the account's files hold as many bytes as allowed"));
        }
        let limit = config.max_file_bytes.min(room);

        let key = new_key()?;
        let mut progress = Progress::default();
        let counted = {
            let progress = &mut progress;
            data.map(move |chunk: io::Result<Bytes>| {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        progress.broken = true;
                        return Err(error);
                    }
                };
                progress.bytes = progress.bytes.saturating_add(chunk.len() as u64);
                if progress.bytes > limit {
                    progress.over = true;
                    return Err(io::Error::other("over the size limit"));
                }
                progress.hasher.update(&chunk);
                Ok(chunk)
            })
            .boxed()
        };
        let stored = self.0.store.put(&key, counted).await;
        if let Err(error) = stored {
            // A store keeps nothing of a failed put; an app's own store is held to that too.
            self.discard(&key).await;
            if progress.over {
                return Err(if limit < config.max_file_bytes {
                    AppError::new(codes::QUOTA_EXCEEDED, "the account's files would hold more bytes than allowed")
                } else {
                    AppError::payload_too_large(format!("the file is larger than {} bytes", config.max_file_bytes))
                });
            }
            if progress.broken {
                return Err(AppError::bad_request("the upload ended early or is malformed"));
            }
            return Err(AppError::internal(error));
        }
        let size = progress.bytes;
        let sha256: String = progress.hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let result = self.record(state, ctx, owner, &key, name, content_type, size, sha256, visibility, metadata, shares, meta.sha256.as_deref()).await;
        if result.is_err() {
            self.discard(&key).await;
        }
        result
    }

    /// After the bytes are stored: the checksum, the hook, the row under the account lock.
    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        state: &AppState,
        ctx: &HookCtx,
        owner: UserId,
        key: &str,
        name: String,
        content_type: String,
        size: u64,
        sha256: String,
        visibility: FileVisibility,
        metadata: Option<Vec<u8>>,
        shares: Vec<i64>,
        expected: Option<&str>,
    ) -> Result<FileInfo, AppError> {
        if expected.is_some_and(|e| is_sha256_hex(e) && e != sha256) {
            return Err(invalid("sha256", "the bytes that arrived have another SHA-256"));
        }
        let event = BeforeFileUpload { owner, name: name.clone(), content_type: content_type.clone(), size, sha256: sha256.clone(), visibility };
        state.hooks().run_before(ctx, event).await?;
        let config = &self.0.config;
        let now = state.now().get();
        let size_i64 = i64::try_from(size).unwrap_or(i64::MAX);
        let mut tx = state.db().begin_write().await?;
        let result = async {
            let dialect = tx.dialect();
            if tx.fetch_optional::<store::IdRow, _>(&store::lock_user(owner.get(), dialect)).await?.is_none() {
                return Err(AppError::not_found("no such account"));
            }
            let usage = tx.fetch_one::<store::UsageRow, _>(&store::usage(owner.get(), dialect)).await?;
            if usage.n >= i64::from(config.max_files_per_user) {
                return Err(AppError::new(codes::QUOTA_EXCEEDED, "the account owns as many files as allowed"));
            }
            if usage.bytes.saturating_add(size_i64) > i64::try_from(config.max_bytes_per_user).unwrap_or(i64::MAX) {
                return Err(AppError::new(codes::QUOTA_EXCEEDED, "the account's files would hold more bytes than allowed"));
            }
            let new = store::NewFile {
                owner: owner.get(),
                name: &name,
                content_type: &content_type,
                size: size_i64,
                sha256: &sha256,
                key,
                visibility: visibility.as_str(),
                metadata: metadata.clone(),
                now,
            };
            let id = tx.insert_id(&store::insert_file(new)?, "id").await?;
            for user in &shares {
                tx.execute(&store::insert_share(id, *user, now)?).await?;
            }
            Ok(id)
        }
        .await;
        let id = tx.finish(result).await?;
        state.hooks().run_after(ctx, Arc::new(AfterFileChange { owner, file: FileId(id), change: FileChange::Uploaded })).await;
        let row = self.row(state, FileId(id)).await?.ok_or_else(not_found)?;
        self.info(state, &row, true).await
    }

    /// Remove stored bytes nobody refers to (logged when the store refuses).
    async fn discard(&self, key: &str) {
        if let Err(error) = self.0.store.delete(key).await {
            tracing::warn!(%error, "files: could not remove the bytes of a refused upload");
        }
    }

    // ---- change and delete ------------------------------------------------------------------------

    /// The row of a file `user` owns: 404 when it does not exist or `user` may not read it, 403
    /// when `user` reads it but does not own it.
    async fn owned(&self, state: &AppState, user: UserId, file: FileId) -> Result<FileRow, AppError> {
        let row = self.row(state, file).await?.ok_or_else(not_found)?;
        if row.owner_id != user.get() {
            return Err(if self.may_read(state, user, &row).await? { AppError::forbidden("only the owner changes or deletes a file") } else { not_found() });
        }
        Ok(row)
    }

    /// Change a file's settings (its owner): the [`BeforeFileUpdate`] hooks (they may change the
    /// name, the visibility and the share list, or refuse), then the change under the file row's
    /// lock (404 when the file was deleted meanwhile).
    pub async fn update(&self, state: &AppState, ctx: &HookCtx, user: UserId, file: FileId, change: UpdateFile) -> Result<FileInfo, AppError> {
        change.validate()?;
        let row = self.owned(state, user, file).await?;
        let event = BeforeFileUpdate {
            owner: user,
            file,
            current: visibility_of(&row.visibility),
            name: change.name.clone(),
            visibility: change.visibility,
            shared_with: change.shared_with.clone(),
        };
        let event = state.hooks().run_before(ctx, event).await?;
        let mut change = change;
        change.name = event.name;
        change.visibility = event.visibility;
        change.shared_with = event.shared_with;
        change.validate()?;
        let metadata = match &change.metadata {
            None => None,
            Some(value) => Some(self.check_metadata(Some(value))?),
        };
        let current = visibility_of(&row.visibility);
        let visibility = change.visibility.unwrap_or(current);
        // A new share list, or the old one when only other fields change; none away from `shared`.
        let shares = match (&change.shared_with, visibility) {
            (Some(list), _) => Some(self.check_sharing(state, user, visibility, Some(list)).await?),
            (None, FileVisibility::Shared) if current == FileVisibility::Shared => {
                self.check_sharing(state, user, visibility, None).await?;
                None
            }
            (None, _) => Some(self.check_sharing(state, user, visibility, None).await?),
        };
        let name = change.name.as_deref().map(|name| name.trim().to_string());
        let now = state.now().get();
        let mut tx = state.db().begin_write().await?;
        let result = async {
            // The row's lock: concurrent changes of the share list queue, a deleted file is 404.
            let dialect = tx.dialect();
            if tx.fetch_optional::<store::IdRow, _>(&store::lock_file(row.id, dialect)).await?.is_none() {
                return Err(not_found());
            }
            tx.execute(&store::update_file(row.id, name.as_deref(), change.visibility.map(FileVisibility::as_str), metadata, now)).await?;
            if let Some(shares) = &shares {
                tx.execute(&store::delete_shares(row.id)).await?;
                for user in shares {
                    tx.execute(&store::insert_share(row.id, *user, now)?).await?;
                }
            }
            Ok::<(), AppError>(())
        }
        .await;
        tx.finish(result).await?;
        state.hooks().run_after(ctx, Arc::new(AfterFileChange { owner: user, file, change: FileChange::Updated })).await;
        let row = self.row(state, file).await?.ok_or_else(not_found)?;
        self.info(state, &row, true).await
    }

    /// Delete a file (its owner): the row, then the bytes.
    pub async fn delete(&self, state: &AppState, ctx: &HookCtx, user: UserId, file: FileId) -> Result<(), AppError> {
        let row = self.owned(state, user, file).await?;
        let mut tx = state.db().begin_write().await?;
        let result = async {
            // The file row's lock first, as `update` takes it: the same lock order (file, then
            // shares) on every path. Shares first here deadlocked with a concurrent change on
            // MySQL (one of them failed with 500); a file deleted meanwhile is 404.
            let dialect = tx.dialect();
            if tx.fetch_optional::<store::IdRow, _>(&store::lock_file(row.id, dialect)).await?.is_none() {
                return Err(not_found());
            }
            tx.execute(&store::delete_shares(row.id)).await?;
            tx.execute(&store::delete_file(row.id)).await?;
            Ok::<(), AppError>(())
        }
        .await;
        tx.finish(result).await?;
        if let Err(error) = self.0.store.delete(&row.storage_key).await {
            tracing::warn!(%error, file = row.id, "files: the row is deleted but the store kept the bytes");
        }
        state.hooks().run_after(ctx, Arc::new(AfterFileChange { owner: user, file, change: FileChange::Deleted })).await;
        Ok(())
    }
}

impl FileService {
    // ---- purge ------------------------------------------------------------------------------------

    /// Delete the stored bytes that no file row names and that were stored at least `min_age` ago;
    /// answers how many. Such bytes are left when an account is deleted straight from the
    /// database (its file rows go with it, the bytes stay) or when the server stops between
    /// storing an upload's bytes and writing its row. An upload's row is written right after its
    /// bytes, so a `min_age` of a few minutes never touches an upload in progress; the module's
    /// background purge uses an hour. Works with stores that list their keys
    /// ([`FileStore::keys`]; the built-in [`LocalFileStore`](super::LocalFileStore) does, and also
    /// removes part files older than `min_age` left by a crash). The store's folder must belong to
    /// this database alone.
    pub async fn purge_orphans(&self, state: &AppState, min_age: Duration) -> Result<u64, AppError> {
        let before = SystemTime::now().checked_sub(min_age).unwrap_or(SystemTime::UNIX_EPOCH);
        let mut keys = self.0.store.keys(before);
        let mut batch = Vec::with_capacity(PURGE_CHUNK);
        let mut removed = 0;
        while let Some(key) = keys.next().await {
            let key = key.map_err(AppError::internal)?;
            if !is_key(&key) {
                continue;
            }
            batch.push(key);
            if batch.len() == PURGE_CHUNK {
                removed += self.purge_batch(state, &mut batch).await?;
            }
        }
        if !batch.is_empty() {
            removed += self.purge_batch(state, &mut batch).await?;
        }
        Ok(removed)
    }

    async fn purge_batch(&self, state: &AppState, batch: &mut Vec<String>) -> Result<u64, AppError> {
        let known: HashSet<String> = state.db().fetch_all::<store::KeyRow, _>(&store::known_keys(batch)).await?.into_iter().map(|r| r.storage_key).collect();
        let mut removed = 0;
        for key in batch.drain(..).filter(|key| !known.contains(key)) {
            match self.0.store.delete(&key).await {
                Ok(()) => removed += 1,
                Err(error) => tracing::warn!(%error, "files: could not remove bytes no file names"),
            }
        }
        Ok(removed)
    }
}
