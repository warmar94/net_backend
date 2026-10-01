//! What games can hook into on the WebSocket hub ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeWsConnect`] | before | a socket is about to become authenticated (handshake or first `auth`); refuse to keep a player out (HTTP 403 at the handshake, else `auth.failed` + 4001) |
//! | [`AfterWsConnect`] | after | a socket is authenticated and registered (pushes to it work) |
//! | [`AfterWsDisconnect`] | after | an authenticated socket closed; its rooms are already left. Not for sockets still open when the shutdown grace ran out (they are dropped) |
//! | [`BeforeWsFrame`] | before | a request from an authenticated socket is about to be dispatched; change its `data` or refuse it (the client gets the error) |
//!
//! `BeforeWsConnect` refusals: a 4xx error is final (HTTP status at the handshake; `auth.failed` +
//! 4001 for first-message auth), a 5xx / 429 is temporary (HTTP status, or close 1013 without
//! `auth.failed`: the client retries). `AfterWsConnect` runs in its own task (it never holds the
//! socket); `BeforeWsFrame` runs inside the request, while the socket keeps writing.
//!
//! ```
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::ws::events::BeforeWsFrame;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeWsFrame, _, _>(|_ctx, frame| async move {
//!     if frame.kind.starts_with("admin.") {
//!         return Ok(Decision::Reject(AppError::forbidden("not here")));
//!     }
//!     Ok(Decision::Continue(frame))
//! });
//! # let _ = server;
//! ```

use std::net::IpAddr;
use std::sync::Arc;

use net_backend_protocol::{CloseCode, UserId};
use serde_json::Value;

use super::hub::ConnectionId;
use crate::hooks::Event;

/// A socket is about to become authenticated. Refuse to keep the player out.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeWsConnect {
    /// The socket (not yet registered as the user's when the handshake authenticated it: `None`).
    pub connection: Option<ConnectionId>,
    /// The user.
    pub user_id: UserId,
    /// The session of the token.
    pub session_id: Option<i64>,
    /// The client address.
    pub ip: Option<IpAddr>,
    /// The handshake's `Origin` header (browsers send it). The built-in Bearer / `?token=` / `auth`
    /// credentials are not sent by a browser on its own, so no check is needed for them; an app
    /// authenticating with a cookie must refuse foreign origins here (cross-site WebSocket hijacking).
    pub origin: Option<String>,
}

impl Event for BeforeWsConnect {
    const NAME: &'static str = "ws.before_connect";
}

/// A socket is authenticated and registered.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterWsConnect {
    /// The socket.
    pub connection: ConnectionId,
    /// The user.
    pub user_id: UserId,
    /// The session of the token.
    pub session_id: Option<i64>,
    /// The client address.
    pub ip: Option<IpAddr>,
}

impl Event for AfterWsConnect {
    const NAME: &'static str = "ws.after_connect";
}

/// An authenticated socket closed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterWsDisconnect {
    /// The socket.
    pub connection: ConnectionId,
    /// The user.
    pub user_id: UserId,
    /// The close code the server sent, if it closed the socket (`None`: the client closed it or
    /// the connection dropped).
    pub code: Option<CloseCode>,
    /// The rooms the socket was in (already left).
    pub rooms: Vec<Arc<str>>,
}

impl Event for AfterWsDisconnect {
    const NAME: &'static str = "ws.after_disconnect";
}

/// A request is about to be dispatched. Hooks may change `data` or refuse.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeWsFrame {
    /// The socket.
    pub connection: ConnectionId,
    /// The user.
    pub user_id: UserId,
    /// The request's kind (read-only: a changed kind is ignored, the request goes to its own handler).
    pub kind: String,
    /// The request's `data` (hooks may change it).
    pub data: Value,
}

impl Event for BeforeWsFrame {
    const NAME: &'static str = "ws.before_frame";
}
