//! Chat: public rooms, direct messages, group rooms, history, presence and moderation over the
//! WebSocket hub (cargo feature `chat`, module [`Chat`]).
//!
//! - **WebSocket kinds** (the protocol's `chat.*`): `chat.join` / `chat.leave` (membership lasts
//!   as long as the connection; at most `ws.max_rooms_per_connection` rooms, 16 by default),
//!   `chat.send` (answered with the stored id BEFORE the sender's own `chat.message` echo, which
//!   carries the `nonce`), `chat.history` (cursor pages, newest first), `chat.members` (who is
//!   online; a DM room lists only the caller: a DM never reveals the peer's online state),
//!   `chat.edit`, `chat.mark_read`, `chat.receipts`, `chat.unread`, `chat.set_typing`; pushes
//!   `chat.message`, `chat.deleted`, `chat.presence`, `chat.edited`, `chat.read`, `chat.typing`,
//!   `chat.room`.
//! - **HTTP** (`/v1/chat/*`): the public rooms, a room's history, delete or edit a message, open a
//!   direct-message room, the caller's DM rooms, read markers and unread counts, player rooms.
//! - **Rooms:** public rooms come from `[modules.chat] rooms` (created / updated at start) or
//!   [`ChatService::create_room`]; group rooms from [`ChatService::create_group`] (members only);
//!   DM rooms from `POST /v1/chat/dm` (no join: a DM's `chat.message` goes to every connection
//!   of both users); player rooms from `POST /v1/chat/rooms` (see below). Hub room names are
//!   `chat:<id>`: leave them to this module.
//! - **Limits:** member caps in connections (public rooms `max_room_members` 200 unless the room
//!   has its own, groups `max_group_members`; `RoomInfo` counts users), `max_text_chars` (500)
//!   with the protocol's text rules, a per-user send rate (a token bucket: a burst of 5, then one
//!   every 2 s), a per-user DM-open rate (a burst of 20, then one every 30 s), the history
//!   retention (30 days; a background purge in batches).
//! - **DMs are open to everyone** (the peer only has to exist); blocking is the game's:
//!   [`events::BeforeDirectOpen`] and [`events::BeforeChatSend`].
//! - **Presence** (the chat module's, not the hub's): a user's first connection in a room
//!   pushes `chat.presence` "joined", its last one "left" (also on disconnect), with the online
//!   count; rooms over `presence_max_members` (100) get none and each room has a push rate
//!   (`presence_per_second`, 10), so a big room never floods; `chat.members` lists who is online.
//! - **Moderation:** [`events::BeforeChatSend`] filters / rewrites / refuses (new messages and
//!   edits), [`events::AfterChatSend`] observes; senders delete their own messages
//!   (`allow_self_delete`), `moderator_roles` and the permission [`MODERATE`] (`chat.moderate`)
//!   delete any (audited as `chat.message_deleted`); every member gets `chat.deleted`.
//! - **Editing:** a sender edits its message within `edit_window_secs` (900 s; `allow_edit`),
//!   counted on the send rate; [`MODERATE`] holders edit any (audited as `chat.message_edited`).
//!   The text rules and the `BeforeChatSend` hooks apply again; the history shows the latest text
//!   with `edited_at`; the room gets `chat.edited`.
//! - **Read markers:** "read up to message X" per user and room (stored, forward only; a
//!   `read_rate` per user), unread counts from them (capped at 1000); DM, group and player rooms
//!   push `chat.read` (at most one per user, room and `read_push_interval_ms`, the newest marker
//!   wins). **Typing** is never stored: `chat.typing` at most once per user, room and
//!   `typing_interval_ms`, with `typing_ttl_ms` for the clients, none in rooms over
//!   `typing_max_members` online users (details in `extras.rs`).
//! - **Player rooms:** created by players (`player_rooms`, `room_create_rate`), owned by their
//!   creator (`max_rooms_per_player`), public (listed, anyone not banned joins) or private
//!   (invited players only), at most `max_player_room_members` members and open invitations. The
//!   owner names moderators, renames, changes the visibility, hands the room on, deletes it;
//!   moderators rename, invite, kick (a ban until invited again) and delete messages in the room.
//!   Membership is stored: `chat.join` / `POST …/join` makes the caller a member (accepting an
//!   invitation), `POST …/leave` ends it; the owner's room goes to the oldest moderator (else
//!   member) and a room without members is deleted. Every change is a `chat.room` push to the
//!   members; invitations are `chat.invite` notifications with the notifications module. Staff
//!   with [`MODERATE`] act as the owner of every player room (details in `rooms.rs`).
//! - **Several instances:** messages, deletions and group removals travel through the hub's
//!   [`Broadcaster`](crate::ws::Broadcaster) (room and user pushes; a removal is a
//!   [`Control`](crate::ws::Control) delivery); joined rooms, member caps, presence and the send
//!   rate are per instance (a user's sockets on another instance are not counted). Group
//!   membership is checked in the table on every group send, join and history read, so a removed
//!   member is cut off at once on every instance. The read-push coalescing and the typing
//!   throttle are per instance too.
//!
//! The module needs [`Auth`](crate::auth::Auth) registered before it and `ws.enabled = true`.

// Without any database backend `Db` has no variants: code after a query is unreachable.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

pub mod config;
pub mod events;
mod extras;
mod handlers;
mod migrations;
mod module;
mod openapi;
mod presence;
mod rooms;
mod service;
mod store;

pub use config::{ChatConfig, RoomSpec};
pub use module::Chat;
pub use service::ChatService;

use crate::permissions::Permission;

/// The permission to moderate every chat room: edit and delete any message, and act as the owner
/// of every player room (rename, change, delete, invite, kick, set roles, hand on). Held by
/// `admin` and `moderator` unless `[permissions]` says otherwise (see [`crate::permissions`]).
/// Deleting messages is also open to `moderator_roles`.
pub const MODERATE: Permission =
    Permission::new("chat.moderate", "Edit and delete any chat message; act as the owner of every player room").granted_to(&["moderator"]);
