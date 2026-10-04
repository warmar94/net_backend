//! Files: binary uploads owned by players (screenshots, replays, mods, levels) with a content type,
//! a SHA-256, per-player quotas and a visibility (cargo feature `files`, module [`Files`]).
//!
//! - **Upload:** `POST /v1/files`, `multipart/form-data`: an optional `meta` part first (JSON
//!   `FileMeta`: name, content type, visibility, share list, metadata, the expected SHA-256), then
//!   the `file` part (the bytes; its `filename` and `Content-Type` are the defaults). The bytes
//!   stream to the store while their size and SHA-256 are counted: over `max_file_bytes` answers 413,
//!   over the player's byte quota 403 `quota_exceeded`, a SHA-256 other than the expected one 422.
//!   Nothing is kept of a refused or broken upload. The answer is the `FileInfo`.
//! - **Download:** `GET /v1/files/{file}/content` streams the bytes as an attachment
//!   (`Content-Type` as stored, `Content-Length`, `ETag` = the SHA-256, `If-None-Match` → 304,
//!   `X-Content-Type-Options: nosniff`, a `sandbox` Content-Security-Policy).
//! - **Settings and lists:** `GET` / `PATCH` / `DELETE /v1/files/{file}`, `GET /v1/files` (the
//!   caller's files, or `?owner=` another player's readable ones, newest first), `GET
//!   /v1/files/usage`.
//! - **Visibility:** `private` (the owner), `public` (every logged-in player), `friends` (the
//!   owner's friends; needs the friends module), `shared` (the accounts in the share list, at most
//!   `max_shared_with`). Players who may not read a file get 404; only the owner changes or deletes
//!   it (403 for a reader).
//! - **Store:** [`FileStore`] (put all or nothing / get / delete by a random key); the built-in
//!   [`LocalFileStore`] keeps one file per key in `dir`. The database holds the file's row
//!   (`stored_files`) and its shares (`stored_file_shares`).
//! - **Limits:** `max_file_bytes` (16 MiB, within `http.max_body_bytes`), `max_files_per_user` (100),
//!   `max_bytes_per_user` (256 MiB) counted exactly under the account lock, `upload_rate` (10 per
//!   60 s per player), `allowed_content_types` (any by default), `max_metadata_bytes` (4 KiB). An
//!   upload is timed by its data: it fails when no data arrives for `http.upload_idle_timeout_secs`
//!   (30) or the body takes longer than `http.upload_timeout_secs` (3600; 0 = no overall limit).
//! - **Purge:** every `purge_interval_secs` (3600; 0 = never) the module deletes stored bytes no
//!   file row names that are older than an hour (an account deleted straight from the database, a
//!   server stopped mid-upload); [`FileService::purge_orphans`] runs it from server code.
//! - **Hooks** ([`events`]): [`BeforeFileUpload`](events::BeforeFileUpload) (the bytes arrived:
//!   refuse), [`BeforeFileUpdate`](events::BeforeFileUpdate) (a settings change: change the name,
//!   the visibility or the share list, or refuse) and [`AfterFileChange`](events::AfterFileChange).
//!
//! The module needs [`Auth`](crate::auth::Auth) registered before it.

// Without any database backend `Db` has no variants: code after a query is unreachable.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

pub mod backend;
pub mod config;
pub mod events;
mod migrations;
mod module;
mod openapi;
mod routes;
mod service;
mod store;

pub use backend::{FileStore, LocalFileStore};
pub use config::FilesConfig;
pub use module::Files;
pub use service::FileService;
