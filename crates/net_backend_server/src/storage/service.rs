//! [`StorageService`]: reads and writes of storage objects, for the routes and for server code.
//!
//! **Locking (every backend):** every write and delete of a user's objects first takes the
//! account lock (the `auth_users` row: MySQL `FOR UPDATE`, PostgreSQL `FOR NO KEY UPDATE`, SQLite's
//! write transaction), then reads the object's state with a plain read, then runs exactly one
//! UPDATE of the existing row, one INSERT or one DELETE. So the writes of ONE user run one at a time
//! (exact quotas, exact conditions), writes of different users never wait for each other, and no
//! statement ever touches a row that is not there: MySQL takes no gap locks, which is what made
//! first saves of different players deadlock. Lock order: the account row, then the object rows
//! (then whatever an `in_tx` hook writes). A transaction the database still aborts as a deadlock is
//! run again from the start ([`Retry`]).

use std::sync::Arc;
use std::time::Duration;

use net_backend_protocol::admin::AdminPutObject;
use net_backend_protocol::storage::{
    is_valid_name, value_bytes, BatchAcks, BatchGet, BatchObjects, BatchPut, ObjectAck, ObjectVersion, ObjectVisibility, StorageObject, StorageObjectInfo,
    VersionConflict, WriteAccess, MAX_BATCH_BYTES,
};
use net_backend_protocol::{codes, Cursor, Page, PageRequest, UnixMillis, UserId};
use serde_json::Value;

use super::config::StorageConfig;
use super::events::{AfterStorageDelete, AfterStorageWrite, BeforeStorageDelete, BeforeStorageWrite, InStorageWriteTx, Writer};
use super::store::{self, InfoRow, ObjectRow, StateRow, WRITE_OWNER, WRITE_SERVER};
use crate::auth::audit::{self, AuditRecord};
use crate::db::{DbTx, Retry};
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::rate_limit::{KeyedBuckets, RateDecision};
use crate::state::AppState;

/// Reads and writes storage objects. A state value (`Ext<StorageService>` in handlers,
/// `state.get::<StorageService>()` elsewhere) once the [`Storage`](super::Storage) module is
/// registered. Server code reads and writes ANY user's objects through it (writes are
/// [`Writer::Server`]: they may set and ignore the write lock, write in server collections, and
/// are never refused by the quotas or the write rate; the hooks run as for clients).
#[derive(Clone)]
pub struct StorageService(Arc<Inner>);

struct Inner {
    config: StorageConfig,
    /// Owner writes per user (`write_rate`; `None` = no limit).
    rate: Option<KeyedBuckets<UserId>>,
}

impl std::fmt::Debug for StorageService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StorageService").field(&self.0.config).finish()
    }
}

/// One write, after the hooks.
pub(crate) struct WriteRequest {
    pub(crate) user: UserId,
    pub(crate) collection: String,
    pub(crate) key: String,
    pub(crate) value: Value,
    pub(crate) if_version: Option<ObjectVersion>,
    /// Set the lock (server / admin writes); `None` keeps it (a new object: owner, or server in a
    /// server collection).
    pub(crate) write: Option<WriteAccess>,
    /// Set who may read it; `None` keeps it (a new object: private).
    pub(crate) visibility: Option<ObjectVisibility>,
    pub(crate) writer: Writer,
}

/// Why a write inside a transaction was refused.
enum Refusal {
    Conflict(Option<ObjectVersion>),
    Locked,
    /// Too many objects.
    Quota,
    /// Too many bytes.
    Bytes,
    NoAccount,
}

/// The transaction's view of the user's objects: the account lock (taken once) and the usage
/// (read once under the lock, then kept up to date by every write of the transaction).
#[derive(Default)]
struct TxState {
    locked: bool,
    usage: Option<(i64, i64)>,
}

fn access_of(text: &str) -> WriteAccess {
    if text == WRITE_SERVER {
        WriteAccess::Server
    } else {
        WriteAccess::Owner
    }
}

fn access_text(access: WriteAccess) -> Result<&'static str, AppError> {
    match access {
        WriteAccess::Owner => Ok(WRITE_OWNER),
        WriteAccess::Server => Ok(WRITE_SERVER),
        _ => Err(AppError::bad_request("write must be `owner` or `server`")),
    }
}

fn check_names(collection: &str, key: Option<&str>) -> Result<(), AppError> {
    if !is_valid_name(collection) || key.is_some_and(|k| !is_valid_name(k)) {
        return Err(AppError::bad_request("a collection or key is not a valid storage name"));
    }
    Ok(())
}

/// A stored visibility (an unknown text reads as private: never more open than written).
fn visibility_of(text: &str) -> ObjectVisibility {
    ObjectVisibility::parse(text).unwrap_or(ObjectVisibility::Private)
}

fn decode(row: ObjectRow, owner: UserId) -> Result<StorageObject, AppError> {
    let value: Value = serde_json::from_slice(&row.value).map_err(AppError::internal)?;
    Ok(StorageObject::new(row.collection, row.object_key, owner, value, ObjectVersion(row.version), UnixMillis(row.updated_at))
        .with_write(access_of(&row.write_access))
        .with_visibility(visibility_of(&row.visibility)))
}

fn info(collection: &str, row: InfoRow) -> StorageObjectInfo {
    let mut info =
        StorageObjectInfo::new(collection, row.object_key, ObjectVersion(row.version), u64::try_from(row.size_bytes).unwrap_or(0), UnixMillis(row.updated_at));
    info.write = access_of(&row.write_access);
    info.visibility = visibility_of(&row.visibility);
    info
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

/// The stored version, lock and size of an object, inside the transaction (a plain read; exact
/// under the account lock).
async fn current(tx: &mut DbTx, user: i64, collection: &str, key: &str) -> Result<Option<StateRow>, crate::db::DbError> {
    tx.fetch_optional::<StateRow, _>(&store::state(user, collection, key)).await
}

/// The error with `{"index": i}` added to its details (a batch item's failure).
fn with_index(error: AppError, index: Option<u32>) -> AppError {
    let Some(index) = index else { return error };
    let mut details = error.api_error().details.clone().unwrap_or_else(|| serde_json::json!({}));
    match details.as_object_mut() {
        Some(map) => {
            map.insert("index".into(), serde_json::json!(index));
        }
        None => details = serde_json::json!({ "index": index }),
    }
    error.with_details(details)
}

/// A known visibility; `friends` only with the friends module (422 `validation_failed`).
fn check_visibility(state: &AppState, visibility: Option<ObjectVisibility>) -> Result<(), AppError> {
    let problem = match visibility {
        Some(ObjectVisibility::Unknown) => "is not private, public or friends",
        Some(ObjectVisibility::Friends) if !friends_known(state) => "`friends` needs the friends module on this server",
        _ => return Ok(()),
    };
    let mut details = net_backend_protocol::ValidationDetails::new();
    details.add("visibility", problem);
    Err(AppError::validation(details))
}

fn server_collection() -> AppError {
    AppError::forbidden("this collection is written by the server only")
}

fn refusal_error(refusal: Refusal, index: Option<u32>) -> AppError {
    match refusal {
        Refusal::Conflict(current) => {
            let conflict = VersionConflict::new(current);
            AppError::from(index.map_or(conflict, |i| conflict.at_index(i)).into_error())
        }
        Refusal::Locked => with_index(AppError::forbidden("this object is written by the server only"), index),
        Refusal::Quota => with_index(AppError::new(codes::QUOTA_EXCEEDED, "the account owns as many storage objects as allowed"), index),
        Refusal::Bytes => with_index(AppError::new(codes::QUOTA_EXCEEDED, "the account's storage objects hold as many bytes as allowed"), index),
        Refusal::NoAccount => AppError::not_found("no such account"),
    }
}

impl StorageService {
    pub(crate) fn new(config: StorageConfig) -> Self {
        let rate =
            (config.write_rate > 0).then(|| KeyedBuckets::new(config.write_rate, Duration::from_secs(u64::from(config.write_rate_window_secs)), 100_000));
        Self(Arc::new(Inner { config, rate }))
    }

    /// The settings.
    pub fn config(&self) -> &StorageConfig {
        &self.0.config
    }

    // ---- reads ----------------------------------------------------------------------------------

    /// One of `user`'s objects, if it exists.
    pub async fn get(&self, state: &AppState, user: UserId, collection: &str, key: &str) -> Result<Option<StorageObject>, AppError> {
        check_names(collection, Some(key))?;
        let row = state.db().fetch_optional::<ObjectRow, _>(&store::object(user.get(), collection, key)).await?;
        row.map(|row| decode(row, user)).transpose()
    }

    /// A page of `user`'s `collection` (no values), ordered by key.
    pub async fn list(&self, state: &AppState, user: UserId, collection: &str, page: &PageRequest) -> Result<Page<StorageObjectInfo>, AppError> {
        self.list_where(state, user, collection, page, None).await
    }

    async fn list_where(
        &self,
        state: &AppState,
        user: UserId,
        collection: &str,
        page: &PageRequest,
        shown: Option<&[&str]>,
    ) -> Result<Page<StorageObjectInfo>, AppError> {
        check_names(collection, None)?;
        page.validate()?;
        let after = match &page.cursor {
            Some(cursor) if is_valid_name(cursor.as_str()) => Some(cursor.as_str()),
            Some(_) => return Err(AppError::bad_request("the cursor is not valid")),
            None => None,
        };
        let limit = u64::from(page.limit_or_default());
        let mut rows = state.db().fetch_all::<InfoRow, _>(&store::list(user.get(), collection, after, limit + 1, shown)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.object_key.clone())) } else { None };
        Ok(Page::new(rows.into_iter().map(|r| info(collection, r)).collect(), next))
    }

    /// The visibilities of `owner`'s objects that `reader` may read (`None`: all of them, the
    /// owner itself).
    async fn readable(&self, state: &AppState, reader: UserId, owner: UserId) -> Result<Option<Vec<&'static str>>, AppError> {
        if reader == owner {
            return Ok(None);
        }
        let mut shown = vec![ObjectVisibility::Public.as_str()];
        if are_friends(state, owner, reader).await? {
            shown.push(ObjectVisibility::Friends.as_str());
        }
        Ok(Some(shown))
    }

    /// One of `owner`'s objects as `reader` sees it: `None` when it does not exist or `reader` may
    /// not read it (public objects, `friends` objects for the owner's friends, everything for the
    /// owner).
    pub async fn get_visible(&self, state: &AppState, reader: UserId, owner: UserId, collection: &str, key: &str) -> Result<Option<StorageObject>, AppError> {
        let Some(object) = self.get(state, owner, collection, key).await? else { return Ok(None) };
        match self.readable(state, reader, owner).await? {
            None => Ok(Some(object)),
            Some(shown) if shown.contains(&object.visibility.as_str()) => Ok(Some(object)),
            Some(_) => Ok(None),
        }
    }

    /// A page of `owner`'s `collection` as `reader` sees it (no values), ordered by key.
    pub async fn list_visible(
        &self,
        state: &AppState,
        reader: UserId,
        owner: UserId,
        collection: &str,
        page: &PageRequest,
    ) -> Result<Page<StorageObjectInfo>, AppError> {
        let shown = self.readable(state, reader, owner).await?;
        self.list_where(state, owner, collection, page, shown.as_deref()).await
    }

    /// Several of `user`'s objects (those that exist, in request order). Over
    /// [`MAX_BATCH_BYTES`] of values: 413 `payload_too_large`.
    pub async fn get_many(&self, state: &AppState, user: UserId, batch: &BatchGet) -> Result<BatchObjects, AppError> {
        batch.validate()?;
        let names: Vec<(&str, &str)> = batch.objects.iter().map(|o| (o.collection.as_str(), o.key.as_str())).collect();
        let rows = state.db().fetch_all::<ObjectRow, _>(&store::objects(user.get(), &names)).await?;
        let total = rows.iter().fold(0usize, |sum, row| sum.saturating_add(row.value.len()));
        if total > MAX_BATCH_BYTES {
            return Err(AppError::payload_too_large(format!("the objects hold more than {MAX_BATCH_BYTES} bytes of values: read fewer per batch")));
        }
        let mut objects = Vec::with_capacity(rows.len());
        for (collection, key) in names {
            if let Some(index) = rows.iter().position(|r| r.collection == collection && r.object_key == key) {
                objects.push(index);
            }
        }
        let mut rows: Vec<Option<ObjectRow>> = rows.into_iter().map(Some).collect();
        let mut found = Vec::with_capacity(objects.len());
        for index in objects {
            if let Some(row) = rows.get_mut(index).and_then(Option::take) {
                found.push(decode(row, user)?);
            }
        }
        Ok(BatchObjects::new(found))
    }

    // ---- writes for server code -----------------------------------------------------------------

    /// Write one of `user`'s objects as the server: may set the lock (`put.write`), ignores it
    /// otherwise, honours `put.if_version`, may write in server collections, is never refused by
    /// the quotas (the object still counts). The hooks run with [`Writer::Server`].
    pub async fn put(&self, state: &AppState, user: UserId, collection: &str, key: &str, put: AdminPutObject) -> Result<ObjectAck, AppError> {
        let request = WriteRequest {
            user,
            collection: collection.into(),
            key: key.into(),
            value: put.value,
            if_version: put.if_version,
            write: put.write,
            visibility: put.visibility,
            writer: Writer::Server,
        };
        self.write(state, &HookCtx::new(state.clone(), None), request, None).await
    }

    /// Delete one of `user`'s objects as the server (also a locked one). Absent: `Ok` unless
    /// `if_version` names a version.
    pub async fn delete(&self, state: &AppState, user: UserId, collection: &str, key: &str, if_version: Option<ObjectVersion>) -> Result<(), AppError> {
        self.remove(state, &HookCtx::new(state.clone(), None), user, collection, key, if_version, Writer::Server, None).await
    }

    // ---- the write path -------------------------------------------------------------------------

    /// Count one owner write of `user` against `write_rate` (429 `rate_limited` over it).
    pub(crate) fn check_rate(&self, user: UserId) -> Result<(), AppError> {
        match self.0.rate.as_ref().map(|rate| rate.check(user)) {
            Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
            _ => Ok(()),
        }
    }

    /// The before hooks and the size rule of one write; the request to apply.
    async fn prepare(&self, state: &AppState, ctx: &HookCtx, request: WriteRequest) -> Result<WriteRequest, AppError> {
        check_names(&request.collection, Some(&request.key))?;
        if request.if_version.is_some_and(|v| v.get() < 0) {
            return Err(AppError::bad_request("if_version must not be negative"));
        }
        if let Some(write) = request.write {
            access_text(write)?;
        }
        check_visibility(state, request.visibility)?;
        if request.writer == Writer::Owner && self.0.config.is_server_collection(&request.collection) {
            return Err(server_collection());
        }
        let event = BeforeStorageWrite {
            user_id: request.user,
            collection: request.collection.clone(),
            key: request.key.clone(),
            value: request.value,
            if_version: request.if_version,
            visibility: request.visibility,
            writer: request.writer,
        };
        // Only the value and the visibility may change; the object and the condition stay what
        // was asked for.
        let event = state.hooks().run_before(ctx, event).await?;
        check_visibility(state, event.visibility)?;
        let max = self.0.config.max_object_bytes;
        if value_bytes(&event.value) > max {
            let mut details = net_backend_protocol::ValidationDetails::new();
            details.add("value", format!("is larger than {max} bytes"));
            return Err(AppError::validation(details));
        }
        Ok(WriteRequest {
            user: request.user,
            collection: request.collection,
            key: request.key,
            value: event.value,
            if_version: request.if_version,
            write: request.write,
            visibility: event.visibility,
            writer: request.writer,
        })
    }

    /// Take the account lock (once per transaction). `false`: no such account.
    async fn lock(tx: &mut DbTx, user: UserId, tx_state: &mut TxState) -> Result<bool, AppError> {
        if !tx_state.locked {
            let dialect = tx.dialect();
            if tx.fetch_optional::<store::IdRow, _>(&store::lock_user(user.get(), dialect)).await?.is_none() {
                return Ok(false);
            }
            tx_state.locked = true;
        }
        Ok(true)
    }

    /// The user's (objects, bytes), read once per transaction (under the account lock).
    async fn usage(tx: &mut DbTx, user: UserId, tx_state: &mut TxState) -> Result<(i64, i64), AppError> {
        if let Some(usage) = tx_state.usage {
            return Ok(usage);
        }
        let dialect = tx.dialect();
        let row = tx.fetch_one::<store::UsageRow, _>(&store::usage(user.get(), dialect)).await?;
        tx_state.usage = Some((row.n, row.bytes));
        Ok((row.n, row.bytes))
    }

    /// Whether the write may add `objects` objects and `bytes` bytes: the owner's writes are refused
    /// over a quota (a write that does not grow is always fine), server code and admins never;
    /// the change is counted either way.
    async fn take(&self, tx: &mut DbTx, request: &WriteRequest, tx_state: &mut TxState, objects: i64, bytes: i64) -> Result<Result<(), Refusal>, AppError> {
        let (count, used) = Self::usage(tx, request.user, tx_state).await?;
        let (count, used) = (count.saturating_add(objects), used.saturating_add(bytes));
        if request.writer == Writer::Owner {
            if objects > 0 && count > i64::from(self.0.config.max_objects_per_user) {
                return Ok(Err(Refusal::Quota));
            }
            if bytes > 0 && used > i64::try_from(self.0.config.max_bytes_per_user).unwrap_or(i64::MAX) {
                return Ok(Err(Refusal::Bytes));
            }
        }
        tx_state.usage = Some((count, used));
        Ok(Ok(()))
    }

    /// One write inside the transaction: the new version, or why not. The account lock, a plain
    /// read of the object's state, then one UPDATE of the existing row or one INSERT (module docs).
    async fn apply(&self, tx: &mut DbTx, request: &WriteRequest, now: i64, tx_state: &mut TxState) -> Result<Result<ObjectVersion, Refusal>, AppError> {
        let user = request.user.get();
        let bytes = serde_json::to_vec(&request.value).map_err(AppError::internal)?;
        let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        let owner = request.writer == Writer::Owner;
        let write = request.write.map(access_text).transpose()?;
        let visibility = request.visibility.map(ObjectVisibility::as_str);
        let (collection, key) = (request.collection.as_str(), request.key.as_str());
        if !Self::lock(tx, request.user, tx_state).await? {
            return Ok(Err(Refusal::NoAccount));
        }
        let condition = request.if_version.map(ObjectVersion::get);
        match current(tx, user, collection, key).await? {
            None => {
                // Only if the stored version is v: there is none.
                if condition.is_some_and(|v| v > 0) {
                    return Ok(Err(Refusal::Conflict(None)));
                }
                if let Err(refusal) = self.take(tx, request, tx_state, 1, size).await? {
                    return Ok(Err(refusal));
                }
                let default_write = if self.0.config.is_server_collection(collection) { WRITE_SERVER } else { WRITE_OWNER };
                tx.execute(&store::insert(
                    user,
                    collection,
                    key,
                    bytes,
                    write.unwrap_or(default_write),
                    visibility.unwrap_or(ObjectVisibility::Private.as_str()),
                    now,
                )?)
                .await?;
                Ok(Ok(ObjectVersion(1)))
            }
            Some(row) => {
                // Only if new: it exists.
                if condition == Some(0) {
                    return Ok(Err(Refusal::Conflict(Some(ObjectVersion(row.version)))));
                }
                if owner && row.write_access == WRITE_SERVER {
                    return Ok(Err(Refusal::Locked));
                }
                if condition.is_some_and(|v| v != row.version) {
                    return Ok(Err(Refusal::Conflict(Some(ObjectVersion(row.version)))));
                }
                if let Err(refusal) = self.take(tx, request, tx_state, 0, size.saturating_sub(row.size_bytes)).await? {
                    return Ok(Err(refusal));
                }
                // The version read under the lock is the condition: nothing else changed it.
                let update = store::Update { user, collection, key, value: bytes, now, if_version: Some(row.version), owner_only: owner, write, visibility };
                if tx.execute(&store::update(update)).await? != 1 {
                    // Only a writer that bypasses the account lock (raw SQL) gets here.
                    let stored = current(tx, user, collection, key).await?.map(|r| ObjectVersion(r.version));
                    return Ok(Err(Refusal::Conflict(stored)));
                }
                Ok(Ok(ObjectVersion(row.version.saturating_add(1))))
            }
        }
    }

    /// Write one object: hooks, one transaction (+ the `in_tx` hooks and an audit entry; run again
    /// when the database aborts it as a deadlock), the after hooks.
    pub(crate) async fn write(&self, state: &AppState, ctx: &HookCtx, request: WriteRequest, audit: Option<AuditRecord>) -> Result<ObjectAck, AppError> {
        let request = self.prepare(state, ctx, request).await?;
        let now = state.now().get();
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        let version = loop {
            let mut tx = state.db().begin_write().await?;
            let result = async {
                let version = match self.apply(&mut tx, &request, now, &mut TxState::default()).await? {
                    Ok(version) => version,
                    Err(refusal) => return Err(refusal_error(refusal, None)),
                };
                let event = InStorageWriteTx {
                    user_id: request.user,
                    collection: request.collection.clone(),
                    key: request.key.clone(),
                    value: request.value.clone(),
                    version,
                    writer: request.writer,
                };
                state.hooks().run_in_tx(&mut tx, ctx, &event).await?;
                if let Some(record) = &audit {
                    let record = record.clone().data(serde_json::json!({ "collection": request.collection, "key": request.key, "version": version.get() }));
                    audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
                }
                Ok(version)
            }
            .await;
            match tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => break other?,
            }
        };
        let event = AfterStorageWrite {
            user_id: request.user,
            collection: request.collection.clone(),
            key: request.key.clone(),
            value: request.value,
            version,
            updated_at: UnixMillis(now),
            writer: request.writer,
        };
        state.hooks().run_after(ctx, Arc::new(event)).await;
        Ok(ObjectAck::new(request.collection, request.key, version, UnixMillis(now)))
    }

    /// Write a batch of the owner's objects in one transaction (all or nothing). The failing item
    /// is named by `{"index": i}` in the error's details (also for a hook's refusal and a quota).
    pub(crate) async fn write_batch(&self, state: &AppState, ctx: &HookCtx, user: UserId, batch: BatchPut) -> Result<BatchAcks, AppError> {
        batch.validate(self.0.config.max_object_bytes)?;
        let mut requests = Vec::with_capacity(batch.objects.len());
        for (index, item) in batch.objects.into_iter().enumerate() {
            let request = WriteRequest {
                user,
                collection: item.collection,
                key: item.key,
                value: item.put.value,
                if_version: item.put.if_version,
                write: None,
                visibility: item.put.visibility,
                writer: Writer::Owner,
            };
            let index = Some(u32::try_from(index).unwrap_or(u32::MAX));
            requests.push(self.prepare(state, ctx, request).await.map_err(|error| with_index(error, index))?);
        }
        let total = requests.iter().fold(0usize, |sum, r| sum.saturating_add(value_bytes(&r.value)));
        if total > MAX_BATCH_BYTES {
            let mut details = net_backend_protocol::ValidationDetails::new();
            details.add("objects", format!("carry more than {MAX_BATCH_BYTES} bytes of values"));
            return Err(AppError::validation(details));
        }
        let now = state.now().get();
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        let versions = loop {
            let mut tx = state.db().begin_write().await?;
            let result = async {
                let mut tx_state = TxState::default();
                // The account lock first, always: batches of one user never interleave.
                if !Self::lock(&mut tx, user, &mut tx_state).await? {
                    return Err(refusal_error(Refusal::NoAccount, None));
                }
                let mut versions = Vec::with_capacity(requests.len());
                for (index, request) in requests.iter().enumerate() {
                    let index = Some(u32::try_from(index).unwrap_or(u32::MAX));
                    let version = match self.apply(&mut tx, request, now, &mut tx_state).await? {
                        Ok(version) => version,
                        Err(refusal) => return Err(refusal_error(refusal, index)),
                    };
                    let event = InStorageWriteTx {
                        user_id: user,
                        collection: request.collection.clone(),
                        key: request.key.clone(),
                        value: request.value.clone(),
                        version,
                        writer: Writer::Owner,
                    };
                    state.hooks().run_in_tx(&mut tx, ctx, &event).await.map_err(|error| with_index(error, index))?;
                    versions.push(version);
                }
                Ok(versions)
            }
            .await;
            match tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => break other?,
            }
        };
        let mut acks = Vec::with_capacity(requests.len());
        for (request, version) in requests.into_iter().zip(versions) {
            acks.push(ObjectAck::new(request.collection.clone(), request.key.clone(), version, UnixMillis(now)));
            let event = AfterStorageWrite {
                user_id: user,
                collection: request.collection,
                key: request.key,
                value: request.value,
                version,
                updated_at: UnixMillis(now),
                writer: Writer::Owner,
            };
            state.hooks().run_after(ctx, Arc::new(event)).await;
        }
        Ok(BatchAcks::new(acks))
    }

    /// Delete one object: hooks, the delete under the account lock (+ an audit entry; run again
    /// when the database aborts it as a deadlock), the after hooks. An absent object is never
    /// touched by a DELETE (no gap locks).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn remove(
        &self,
        state: &AppState,
        ctx: &HookCtx,
        user: UserId,
        collection: &str,
        key: &str,
        if_version: Option<ObjectVersion>,
        writer: Writer,
        audit: Option<AuditRecord>,
    ) -> Result<(), AppError> {
        check_names(collection, Some(key))?;
        let owner = writer == Writer::Owner;
        if owner && self.0.config.is_server_collection(collection) {
            return Err(server_collection());
        }
        let event = BeforeStorageDelete { user_id: user, collection: collection.into(), key: key.into(), writer };
        state.hooks().run_before(ctx, event).await?;
        let now = state.now().get();
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        let existed = loop {
            let mut tx = state.db().begin_write().await?;
            let result = async {
                let mut tx_state = TxState::default();
                // No account: nothing of it can exist.
                let stored = if Self::lock(&mut tx, user, &mut tx_state).await? { current(&mut tx, user.get(), collection, key).await? } else { None };
                let existed = match stored {
                    None if if_version.is_some() => return Err(refusal_error(Refusal::Conflict(None), None)),
                    None => false,
                    Some(row) if owner && row.write_access == WRITE_SERVER => return Err(refusal_error(Refusal::Locked, None)),
                    Some(row) if if_version.is_some_and(|v| v.get() != row.version) => {
                        return Err(refusal_error(Refusal::Conflict(Some(ObjectVersion(row.version))), None))
                    }
                    Some(row) => tx.execute(&store::delete(user.get(), collection, key, Some(row.version), owner)).await? > 0,
                };
                if let Some(record) = &audit {
                    let record = record.clone().data(serde_json::json!({ "collection": collection, "key": key, "existed": existed }));
                    audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
                }
                Ok(existed)
            }
            .await;
            match tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => break other?,
            }
        };
        let after = AfterStorageDelete { user_id: user, collection: collection.into(), key: key.into(), existed, writer };
        state.hooks().run_after(ctx, Arc::new(after)).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_become_protocol_errors() {
        let conflict = refusal_error(Refusal::Conflict(Some(ObjectVersion(3))), Some(2));
        assert_eq!(conflict.code(), codes::VERSION_CONFLICT);
        assert_eq!(conflict.api_error().details, Some(serde_json::json!({"index": 2, "current_version": 3})));
        assert_eq!(refusal_error(Refusal::Locked, None).status().as_u16(), 403);
        assert_eq!(refusal_error(Refusal::Locked, Some(1)).api_error().details, Some(serde_json::json!({"index": 1})));
        assert_eq!(refusal_error(Refusal::Quota, None).code(), codes::QUOTA_EXCEEDED);
        assert_eq!(refusal_error(Refusal::Bytes, Some(4)).api_error().details, Some(serde_json::json!({"index": 4})));
        assert_eq!(refusal_error(Refusal::NoAccount, None).status().as_u16(), 404);
        let detailed = with_index(AppError::bad_request("x").with_details(serde_json::json!({"a": 1})), Some(2));
        assert_eq!(detailed.api_error().details, Some(serde_json::json!({"a": 1, "index": 2})));
        assert!(access_text(WriteAccess::Unknown).is_err());
        assert_eq!(access_of("server"), WriteAccess::Server);
        assert_eq!(access_of("owner"), WriteAccess::Owner);
        assert!(check_names("saves", Some("../x")).is_err() && check_names("saves", Some("a")).is_ok());
    }
}
