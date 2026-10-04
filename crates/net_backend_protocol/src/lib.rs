//! Shared message types for `net_backend_server`: plain Rust + serde, usable from any Rust client.
//!
//! The server and its clients use the same types, so both sides agree on every request, answer
//! and push, and on the JSON they become. The crate contains only data types and pure helpers;
//! a client library or the server sends them.
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
//! - [`leaderboards`]: boards, score submission, the top, the caller's rank and the ranks around it.
//! - [`notifications`]: stored per-player notifications, read / unread, deletes and the `notify.new` push.
//! - [`friends`]: friends by account (requests by id, name or friend code; accept, decline, remove,
//!   block), online state and the `friends.presence` push.
//! - [`groups`]: groups (guilds, clans): create, invite, join, leave, kick, roles, metadata, a name
//!   search.
//! - [`lobbies`]: lobbies (create, join by id or join code, leave, kick, host, ready flags,
//!   metadata, a search by metadata) and the `lobby.member` / `lobby.changed` pushes.
//! - [`matchmaking`]: queues, tickets, the `match.found` / `match.expired` pushes.
//! - [`oauth`]: OpenID Connect logins (an identity provider's ID token, e.g. Google's), linking.
//! - [`files`]: binary files with a content type, a SHA-256, quotas and a visibility (private,
//!   public, friends, shared with accounts); the upload and download routes in [`routes::BINARY`].
//! - [`kinds`] and [`routes`]: every WebSocket `type` and HTTP path as constants.
//! - [`http_call`]: the [`HttpCall`] trait pairing every HTTP route with its payload and answer
//!   type (the HTTP twin of [`WsCall`]).
//! - [`version`]: [`PROTOCOL_VERSION`] and how it is exchanged.
//!
//! Feature `bevy_net_backend` (off by default) implements that client crate's `WsRequest` /
//! `WsPushMessage` for the WebSocket messages and `Credentials` for [`AccessToken`], and adds
//! the `bevy` module: every [`HttpCall`] as a typed request of that client. The README is the full
//! manual.
#![warn(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod admin;
pub mod auth;
#[cfg(feature = "bevy_net_backend")]
#[cfg_attr(docsrs, doc(cfg(feature = "bevy_net_backend")))]
pub mod bevy;
pub mod chat;
pub mod envelope;
pub mod error;
pub mod files;
pub mod friends;
pub mod groups;
pub mod http_call;
pub mod ids;
pub mod kinds;
pub mod leaderboards;
pub mod lobbies;
pub mod matchmaking;
pub mod notifications;
pub mod oauth;
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
pub use ids::{FileId, GroupId, LobbyId, MessageId, NotificationId, RoomId, TicketId, UserId};
pub use page::{Cursor, Page, PageRequest};
pub use time::UnixMillis;
pub use version::{GetServerInfo, ServerInfo, PROTOCOL_HEADER, PROTOCOL_VERSION};

/// The README's Rust blocks, compiled as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
