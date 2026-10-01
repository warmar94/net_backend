//! What games can hook into in the storage module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeStorageWrite`] | before | an object is about to be written (PUT, each batch item, server / admin writes); validate the game's data, change `value` (checked again against the size limit) or refuse |
//! | [`InStorageWriteTx`] | in_tx | the object row is written, the transaction is still open: write your own rows with it (atomic with the save), or refuse (everything rolls back). Use only the given transaction (never `StorageService` inside it); it may run again if the database aborts the transaction as a deadlock |
//! | [`AfterStorageWrite`] | after | the write is committed |
//! | [`BeforeStorageDelete`] | before | an object is about to be deleted; refuse to keep it |
//! | [`AfterStorageDelete`] | after | the delete is committed (also when nothing was there) |
//!
//! Every event says who writes ([`Writer`]): the owner through the API, server code, or an admin.
//!
//! **Server-owned objects:** list the collections only the server writes in
//! `StorageConfig::server_collections` (default `["server"]`, which covers `server` and
//! `server.*`): the owner can never create an object there, so a player cannot pre-create a key
//! the server will own (`wallet/gold`). For finer rules (some keys of a collection), refuse
//! `Writer::Owner` writes in a [`BeforeStorageWrite`] hook.
//!
//! ```
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::storage::events::BeforeStorageWrite;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeStorageWrite, _, _>(|_ctx, write| async move {
//!     // A save must say which level it is at.
//!     if write.collection == "saves" && write.value.get("level").and_then(|l| l.as_u64()).is_none() {
//!         return Ok(Decision::Reject(AppError::bad_request("a save needs a level")));
//!     }
//!     Ok(Decision::Continue(write))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::storage::ObjectVersion;
use net_backend_protocol::{UnixMillis, UserId};
use serde_json::Value;

use crate::hooks::Event;

/// Who writes or deletes an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Writer {
    /// The owner, through the storage routes (refused for server-locked objects).
    Owner,
    /// Server code, through [`StorageService`](super::StorageService).
    Server,
    /// An administrator, through `/v1/admin/users/{user}/storage` (this account).
    Admin(UserId),
}

/// An object is about to be written. Hooks may change `value` or refuse; the other fields are
/// for reading (changes to them are ignored).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeStorageWrite {
    /// The owner.
    pub user_id: UserId,
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
    /// The value (hooks may change it; its size is checked again afterwards).
    pub value: Value,
    /// The version the writer expects, if any.
    pub if_version: Option<ObjectVersion>,
    /// Who writes.
    pub writer: Writer,
}

impl Event for BeforeStorageWrite {
    const NAME: &'static str = "storage.before_write";
}

/// The object row is written; the transaction is still open (an `in_tx` event: register with
/// [`Hooks::in_tx`](crate::hooks::Hooks::in_tx)).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct InStorageWriteTx {
    /// The owner.
    pub user_id: UserId,
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
    /// The value written.
    pub value: Value,
    /// Its new version (1: created).
    pub version: ObjectVersion,
    /// Who writes.
    pub writer: Writer,
}

impl Event for InStorageWriteTx {
    const NAME: &'static str = "storage.in_tx_write";
}

/// A write is committed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterStorageWrite {
    /// The owner.
    pub user_id: UserId,
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
    /// The value written.
    pub value: Value,
    /// Its new version (1: created).
    pub version: ObjectVersion,
    /// When.
    pub updated_at: UnixMillis,
    /// Who wrote.
    pub writer: Writer,
}

impl Event for AfterStorageWrite {
    const NAME: &'static str = "storage.after_write";
}

/// An object is about to be deleted. Refuse to keep it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeStorageDelete {
    /// The owner.
    pub user_id: UserId,
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
    /// Who deletes.
    pub writer: Writer,
}

impl Event for BeforeStorageDelete {
    const NAME: &'static str = "storage.before_delete";
}

/// A delete is committed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterStorageDelete {
    /// The owner.
    pub user_id: UserId,
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
    /// Whether an object was there (deleting an absent object is not an error).
    pub existed: bool,
    /// Who deleted.
    pub writer: Writer,
}

impl Event for AfterStorageDelete {
    const NAME: &'static str = "storage.after_delete";
}
