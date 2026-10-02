//! A Rust client for `net_backend_server`, built on the shared types of `net_backend_protocol`.
//!
//! For Rust apps that do not use Bevy (other engines, tools, bots, command-line programs); Bevy
//! games use `bevy_net_backend`.
//!
//! - [`Client`] (async, tokio): typed calls for every route ([`Client::call`] with any
//!   [`HttpCall`](protocol::HttpCall)), the session (login, register, Steam, automatic refresh
//!   before expiry, one refresh at a time, rotating refresh tokens reported through
//!   [`TokenUpdates`], logout), errors as the server's codes ([`Error::Api`]).
//! - [`blocking::Client`]: the same client for programs without a runtime, with [`Reply::try_take`]
//!   for game loops and [`Reply::cancel`].
//! - HTTP and the WebSocket go through an HTTP CONNECT proxy from the environment or
//!   [`ClientBuilder::proxy`]. The protocol's secret types and the client's own copies of secrets
//!   are overwritten with zeros when dropped (the README lists exactly what is and is not).
//! - Feature `ws`: the WebSocket (module `ws`): typed requests, pushes, heartbeats, reconnects that obey
//!   the close codes.
//! - Features `ssh` / `sftp` / `ssh-rsa`: SSH and SFTP to the server machine's OpenSSH (module `ssh`),
//!   for admin tools: opt-in reconnects, pipelined transfers with progress.
//!
//! ```no_run
//! use net_backend_client::protocol::auth::{GetAccount, LoginRequest};
//! use net_backend_client::protocol::storage::{GetObject, PutObject, WriteObject};
//! use net_backend_client::Client;
//!
//! # async fn run() -> Result<(), net_backend_client::Error> {
//! let client = Client::new("https://api.example.com")?;
//! client.login(LoginRequest::new("player@example.com", "a long password")).await?;
//! let me = client.call(&GetAccount::new()).await?;
//! client.call(&WriteObject::new("saves", "slot-1", PutObject::new(serde_json::json!({"level": 3})))).await?;
//! let save = client.call(&GetObject::new("saves", "slot-1")).await?;
//! println!("{:?} is on level {}", me.display_name, save.value["level"]);
//! # Ok(())
//! # }
//! ```
//!
//! The README is the full manual.
#![warn(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod blocking;
mod client;
mod error;
mod http;
mod reply;
mod runtime;
mod session;
#[cfg(feature = "ssh")]
#[cfg_attr(docsrs, doc(cfg(feature = "ssh")))]
pub mod ssh;
mod tls;
#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
pub mod ws;

pub use client::{Client, ClientBuilder, DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_TIMEOUT, MAX_TIMEOUT};
pub use error::{Error, HostKeyProblem};
/// The shared message types (`net_backend_protocol`), re-exported so the versions always match.
pub use net_backend_protocol as protocol;
pub use reply::{CancelHandle, Reply};
pub use session::TokenUpdates;

/// The README's Rust blocks, compiled as doctests.
#[cfg(all(doctest, feature = "ws", feature = "sftp"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
