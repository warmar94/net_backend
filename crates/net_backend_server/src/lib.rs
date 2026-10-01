//! A framework for building game backend servers: tokio + axum, modules with hooks, MySQL /
//! PostgreSQL / SQLite through one database handle, plain-SQL migrations the app can own,
//! OpenAPI, a command line, graceful shutdown.
//!
//! It is a library you build your own server binary with. The message types are shared with the
//! clients through [`net_backend_protocol`] (re-exported as [`protocol`]).
//!
//! ```no_run
//! use net_backend_server::{AppError, Config, NetBackendServer};
//! use net_backend_server::axum::{routing::get, Json};
//!
//! async fn motd() -> Result<Json<&'static str>, AppError> {
//!     Ok(Json("welcome"))
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), net_backend_server::Error> {
//!     let config = Config::load()?;                 // NBS_CONFIG / config.toml + NBS__* variables
//!     NetBackendServer::new(config)
//!         .route("/v1/game/motd", get(motd))      // your own routes, plain axum handlers
//!         .run()                                  // CLI: serve, migrate, migrations publish, config check
//!         .await
//! }
//! ```
//!
//! **What is inside:** the core: the app builder and module
//! system ([`NetBackendServer`], [`Module`], [`hooks`]), configuration ([`Config`]), the error
//! model ([`AppError`]), the database layer ([`db`]) and migrations ([`migrate`]), the HTTP basics
//! ([`http`]: health, info, body limits, request ids, client addresses, timeouts, panic safety,
//! CORS), OpenAPI, metrics, the command line ([`cli`], [`command`]) and graceful shutdown; and the
//! accounts module [`Auth`] ([`auth`]: email + password and Steam logins, rotating tokens,
//! sessions, verification and reset mails ([`mail`]), roles, the audit log, `/v1/admin`, rate
//! limits ([`rate_limit`])); and the WebSocket hub ([`ws`]: `/v1/ws` with the protocol's envelope,
//! handlers by kind, pushes, rooms, caps, close codes, an AsyncAPI document at
//! [`ASYNCAPI_PATH`]); typed routes mounted from the protocol's `HttpCall` ([`http::call`]); and
//! the modules `storage` (per-user JSON objects with versions) and `chat` (rooms, direct
//! messages, history, presence, moderation), each behind its cargo feature.
//!
//! Cargo features: `mysql` (default), `postgres`, `sqlite` (additive, any combination compiles,
//! at least one is needed to run a server); `steam` (the built-in Steam ticket check) and `smtp`
//! (the SMTP mailer); `storage` and `chat` (the modules; off by default).
#![warn(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod auth;
#[cfg(feature = "chat")]
#[cfg_attr(docsrs, doc(cfg(feature = "chat")))]
pub mod chat;
pub mod cli;
pub mod command;
pub mod config;
pub mod db;
pub mod error;
pub mod hooks;
pub mod http;
pub mod mail;
pub mod migrate;
pub mod module;
pub mod rate_limit;
pub mod shutdown;
pub mod state;
#[cfg(feature = "storage")]
#[cfg_attr(docsrs, doc(cfg(feature = "storage")))]
pub mod storage;
pub mod ws;

mod app;
mod metrics;
mod openapi;
mod serve;

pub use app::{NetBackendServer, PreparedServer, ASYNCAPI_PATH};
pub use auth::{Auth, AuthContext, AuthService, Authenticator};
pub use config::{Config, SecretString};
pub use db::{Db, DbError, DbTx, Dialect};
pub use error::{AppError, Error};
pub use hooks::{Decision, Event, HookCtx, Hooks};
pub use http::{ApiJson, ClientIp, Ext, RequestId};
pub use migrate::Migration;
pub use module::Module;
pub use state::{AppState, Clock, ManualClock, SystemClock};

/// The shared message types (error body, routes, versions, ids, timestamps).
pub use net_backend_protocol as protocol;

pub use axum;
pub use sea_query;
pub use sqlx;
pub use utoipa;
pub use utoipa_axum;

/// The README's Rust blocks, compiled as doctests (they show the storage and chat modules too, so
/// they run with both features).
#[cfg(all(doctest, feature = "storage", feature = "chat"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    /// A fresh directory under `target/tmp/unit/<name>-<n>`.
    #[allow(dead_code)]
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp").join("unit").join(format!(
            "{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}
