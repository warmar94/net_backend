//! The WebSocket hub at `/v1/ws`: authenticated sockets speaking the protocol's JSON envelope,
//! request handlers by kind, server pushes to a connection / a user / a room / everyone, rooms,
//! caps, rate limits, backpressure, heartbeats and graceful shutdown.
//!
//! **Wire format** (exactly `net_backend_protocol::envelope`, which is what `bevy_net_backend`'s
//! `JsonEnvelope` speaks):
//!
//! | Direction | Frame |
//! |---|---|
//! | client → server | request `{"id":7,"type":"game.echo","data":{…}}` |
//! | server → client | answer `{"id":7,"ok":true,"data":{…}}` or `{"id":7,"ok":false,"error":{"code":…,"message":…}}` |
//! | server → client | push `{"type":"chat.message","data":{…}}` (never `id` / `ok`) |
//! | client → server | first-message auth `{"type":"auth","data":{"token":"…","protocol":1}}` |
//! | server → client | `{"type":"auth.ok","data":{"user_id":42,"protocol":1}}` / `{"type":"auth.failed","error":{…}}` + close 4001 |
//!
//! **Authentication.** At the handshake through the app's authenticators (`Authorization: Bearer`,
//! or `?token=` when [`WsConfig::query_token`](crate::config::WsConfig::query_token) is on): a bad
//! token is a 401 / 403 before the upgrade (the client does not retry those), an expired one 401
//! `token_expired`. Or with a first-message `auth` within `ws.auth_timeout_secs` (default 5 s, then
//! close 1008). Every `auth` frame gets exactly one `auth.ok` / `auth.failed`; a later `auth` with
//! the same user's token re-authenticates, another user's token is refused (`auth.failed` + 4001).
//! An open socket survives the expiry of its access token; a revocation closes it (4001; a ban
//! 4003), also when another process (the command line, another instance) revoked the session.
//!
//! **Handlers** are registered by kind, typed through the protocol's [`WsCall`](net_backend_protocol::WsCall) trait or raw on
//! [`serde_json::Value`]: [`NetBackendServer::ws`](crate::NetBackendServer::ws) /
//! [`ws_call`](crate::NetBackendServer::ws_call) for the game, [`Module::ws_handlers`](crate::Module::ws_handlers)
//! for modules. A handler gets a [`WsCtx`] (state, connection, [`AuthContext`](crate::AuthContext))
//! and answers `Result<Response, AppError>`. Requests of one socket run one after another, in
//! order; the answer is written before any push the handler queued (e.g. a chat echo).
//!
//! ```
//! use net_backend_server::protocol::{ServerPush, WsCall};
//! use net_backend_server::ws::WsCtx;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Serialize, Deserialize)]
//! struct Shout { text: String }
//! #[derive(Serialize, Deserialize)]
//! struct Shouted { listeners: usize }
//! impl WsCall for Shout { type Response = Shouted; const KIND: &'static str = "game.shout"; }
//!
//! #[derive(Serialize, Deserialize)]
//! struct Heard { from: i64, text: String }
//! impl ServerPush for Heard { const KIND: &'static str = "game.heard"; }
//!
//! async fn shout(ctx: WsCtx, shout: Shout) -> Result<Shouted, AppError> {
//!     let hub = ctx.hub();
//!     hub.join(ctx.connection, "lobby")?;
//!     hub.push_room("lobby", &Heard { from: ctx.auth.user_id.get(), text: shout.text })?;
//!     Ok(Shouted { listeners: hub.room_size("lobby") })
//! }
//!
//! let server = NetBackendServer::new(Config::default()).ws_call::<Shout, _, _>(shout);
//! # let _ = server;
//! ```
//!
//! **Close codes** ([`CloseCode`](net_backend_protocol::CloseCode)): 1000 normal, 1001 server
//! shutting down (reconnect), 1008 policy (no `auth` in time, flooding), 1009 message too big,
//! 1011 internal error, 1013 too slow to read its pushes / overloaded (reconnect later), 4001
//! authentication refused or revoked, 4003 banned, 4009 replaced (the user opened more than
//! `ws.max_connections_per_user` sockets), 4010 unsupported protocol version. The client never
//! reconnects after 4000–4099.
//!
//! **Multi-instance seam:** every push goes through a [`Broadcaster`]. The default
//! [`LocalBroadcaster`] delivers in this process; a pub/sub implementation (Redis / Valkey) can
//! publish to every instance, each delivering to its own sockets with [`LocalDelivery`].

pub(crate) mod asyncapi;
pub(crate) mod connection;
pub mod events;
mod handlers;
mod hub;

pub use handlers::{KindDoc, WsCtx, WsHandlers};
pub use hub::{Broadcaster, ConnectionId, ConnectionInfo, Control, Delivery, HubStats, JoinError, LocalBroadcaster, LocalDelivery, PushError, Target};
pub use hub::{Hub, MAX_ROOM_NAME_BYTES};
