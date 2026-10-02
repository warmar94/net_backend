//! Shared message types for `net_backend_server`: plain Rust + serde, usable from any Rust client.
//!
//! The server and its clients use the same types, so both sides agree on every request, answer
//! and push, and on the JSON they become. The crate contains only data types and pure helpers:
//! no networking, no async runtime, no game engine.
//!
//! - [`envelope`]: the WebSocket frames (request, answer, push, first-message auth), close codes,
//!   the [`WsCall`] / [`ServerPush`] traits.
//! - [`error`]: the error body of HTTP and WebSocket answers ([`ApiError`]) and the stable [`codes`].
//! - [`ids`], [`time`], [`page`]: id newtypes on `i64`, [`UnixMillis`] timestamps, cursor pagination.
//! - [`auth`]: accounts and sessions (email + password, Steam, refresh, logout, verify, reset),
//!   with secrets that never show in `Debug`.
//! - [`admin`]: account administration (list, ban, sessions, roles) and the audit log.
//! - [`storage`]: per-user key-value objects (saves) with optimistic versions.
//! - [`chat`]: rooms, direct messages, sending, history and the `chat.message` push.
//! - [`kinds`] and [`routes`]: every WebSocket `type` and HTTP path as constants.
//! - [`http_call`]: the [`HttpCall`] trait pairing every HTTP route with its payload and answer
//!   type (the HTTP twin of [`WsCall`]).
//! - [`version`]: [`PROTOCOL_VERSION`] and how it is exchanged.
//!
//! Feature `bevy_net_backend` (off by default) implements that client crate's `WsRequest` /
//! `WsPushMessage` for the WebSocket messages and `Credentials` for [`AccessToken`]. The README
//! is the full manual.
#![warn(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod admin;
pub mod auth;
pub mod chat;
pub mod envelope;
pub mod error;
pub mod http_call;
pub mod ids;
pub mod kinds;
pub mod page;
pub mod routes;
pub mod storage;
pub mod text;
pub mod time;
pub mod version;

#[cfg(feature = "bevy_net_backend")]
mod integration;

pub use auth::{AccessToken, Password, RefreshToken, Secret};
pub use envelope::{
    Ack, CloseCode, FrameError, ServerPush, WsAuth, WsAuthOk, WsCall, WsClientFrame, WsPushFrame, WsRequestFrame, WsResponseFrame, WsServerFrame,
};
pub use error::{codes, ApiError, ErrorBody, ValidationDetails};
pub use http_call::{HttpCall, NoPayload, PathParams, PayloadKind};
pub use ids::{MessageId, RoomId, UserId};
pub use page::{Cursor, Page, PageRequest};
pub use time::UnixMillis;
pub use version::{GetServerInfo, ServerInfo, PROTOCOL_HEADER, PROTOCOL_VERSION};

/// The README's Rust blocks, compiled as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
