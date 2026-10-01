//! Chat: public rooms, direct messages, group rooms, history, presence and moderation over the
//! WebSocket hub (cargo feature `chat`, module [`Chat`]).
//!
//! - **WebSocket kinds** (the protocol's `chat.*`): `chat.join` / `chat.leave` (membership lasts
//!   as long as the connection; at most `ws.max_rooms_per_connection` rooms, 16 by default),
//!   `chat.send` (answered with the stored id BEFORE the sender's own `chat.message` echo, which
//!   carries the `nonce`), `chat.history` (cursor pages, newest first), `chat.members` (who is
//!   online; a DM room lists only the caller: a DM never reveals the peer's online state);
//!   pushes `chat.message`, `chat.deleted`, `chat.presence`.
//! - **HTTP** (`/v1/chat/*`): the public rooms, a room's history, delete a message, open a
//!   direct-message room, the caller's DM rooms.
//! - **Rooms:** public rooms come from `[modules.chat] rooms` (created / updated at start) or
//!   [`ChatService::create_room`]; group rooms from [`ChatService::create_group`] (members only);
//!   DM rooms from `POST /v1/chat/dm` (no join: a DM's `chat.message` goes to every connection
//!   of both users). Hub room names are `chat:<id>`: leave them to this module.
//! - **Limits:** member caps in connections (public rooms `max_room_members` 200 unless the room
//!   has its own, groups `max_group_members`; `RoomInfo` counts users), `max_text_chars` (500)
//!   with the protocol's text rules, a per-user send rate (a token bucket: a burst of 5, then one
//!   every 2 s), a per-user DM-open rate (a burst of 20, then one every 30 s), the history
//!   retention (30 days; a background purge in batches).
//! - **DMs are open to everyone** (the peer only has to exist); blocking is the game's:
//!   [`events::BeforeDirectOpen`] and [`events::BeforeChatSend`].
//! - **Presence** (owner decision: chat's, not the hub's): a user's first connection in a room
//!   pushes `chat.presence` "joined", its last one "left" (also on disconnect), with the online
//!   count; rooms over `presence_max_members` (100) get none and each room has a push rate
//!   (`presence_per_second`, 10), so a big room never floods; `chat.members` lists who is online.
//! - **Moderation:** [`events::BeforeChatSend`] filters / rewrites / refuses, [`events::AfterChatSend`]
//!   observes; senders delete their own messages (`allow_self_delete`), `moderator_roles` delete
//!   any (audited as `chat.message_deleted`); every member gets `chat.deleted`.
//! - **Several instances:** messages, deletions and group removals travel through the hub's
//!   [`Broadcaster`](crate::ws::Broadcaster) (room and user pushes; a removal is a
//!   [`Control`](crate::ws::Control) delivery); joined rooms, member caps, presence and the send
//!   rate are per instance (a user's sockets on another instance are not counted). Group
//!   membership is checked in the table on every group send, join and history read, so a removed
//!   member is cut off at once on every instance.
//!
//! The module needs [`Auth`](crate::auth::Auth) registered before it and `ws.enabled = true`.

// Without any database backend `Db` has no variants: code after a query is unreachable.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

pub mod config;
pub mod events;
mod handlers;
mod migrations;
mod module;
mod openapi;
mod presence;
mod service;
mod store;

pub use config::{ChatConfig, RoomSpec};
pub use module::Chat;
pub use service::ChatService;
