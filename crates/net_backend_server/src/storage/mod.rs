//! Storage: per-user JSON objects (save slots, settings, inventory snapshots), the protocol's
//! `/v1/storage` routes (cargo feature `storage`, module [`Storage`]).
//!
//! - **Routes** (all need a Bearer token; every route addresses the CALLER's own objects):
//!   `GET /v1/storage/{collection}` (a page of listings without values, ordered by key),
//!   `GET` / `PUT` / `DELETE /v1/storage/{collection}/{key}`, `POST /v1/storage/_batch/get` and
//!   `/_batch/put` (16 objects and 4 MiB of values at most; a batch put is one transaction).
//! - **Versions:** every write bumps the object's version (1 for a new one). Last write wins
//!   unless the write names the version it expects (`if_version` in the body, or `If-Match: "N"`;
//!   `If-None-Match: *` = only if new): then a different stored version is refused with 409
//!   `version_conflict` and the stored version in `details` (+ the failing `index` in a batch).
//!   A GET or PUT answer carries the object's version as an `ETag` header (`"3"`).
//! - **Concurrency:** every write and delete takes the owner's account lock, reads the object,
//!   then writes the row that exists or inserts one ([`StorageService`] docs): exact per player,
//!   no waiting and no deadlocks between players; a deadlock the database reports anyway is retried.
//! - **Write lock:** objects written by server code (or an admin) with
//!   [`WriteAccess::Server`](net_backend_protocol::storage::WriteAccess::Server) refuse client
//!   writes and deletes with 403; clients still read them. Collections in `server_collections`
//!   (default `server`, `server.*`) refuse every client write, also of new keys.
//! - **Limits:** `max_object_bytes` per value (default 256 KiB), `max_objects_per_user` (default
//!   1000) and `max_bytes_per_user` (default 4 MiB) — 403 `quota_exceeded`, for the owner's writes
//!   only —, `write_rate` owner writes per user (default 60 per 60 s; 429), names 1-128 bytes of
//!   `[A-Za-z0-9_.-]`.
//! - **Hooks** ([`events`]): [`BeforeStorageWrite`](events::BeforeStorageWrite) (validate,
//!   rewrite or refuse a value), [`InStorageWriteTx`](events::InStorageWriteTx) (inside the
//!   write's transaction: write your own rows atomically, or refuse), [`AfterStorageWrite`](events::AfterStorageWrite),
//!   [`BeforeStorageDelete`](events::BeforeStorageDelete), [`AfterStorageDelete`](events::AfterStorageDelete).
//! - **Administration:** `/v1/admin/users/{user}/storage/...` (role `admin`, every access in the
//!   audit log), and [`StorageService`] for server code (`Ext<StorageService>`,
//!   `state.get::<StorageService>()`): read, write (with the write lock), delete, list any user's
//!   objects.
//!
//! The module needs [`Auth`](crate::auth::Auth) registered before it (its table refers to the
//! accounts; deleting an account deletes its objects).

// Without any database backend `Db` has no variants: code after a query is unreachable.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

mod admin_routes;
pub mod config;
pub mod events;
mod migrations;
mod module;
mod openapi;
mod routes;
mod service;
mod store;

pub use config::StorageConfig;
pub use module::Storage;
pub use service::StorageService;
