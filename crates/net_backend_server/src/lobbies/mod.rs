//! Lobbies: the protocol's `/v1/lobbies` routes and the `lobby.member` / `lobby.changed` pushes
//! (cargo feature `lobbies`, module [`Lobbies`]). The server only coordinates: the game's own
//! connection between the players stays the game's.
//!
//! - **Create:** a player creates a lobby (`POST /v1/lobbies`: visibility, the most players, the
//!   game's metadata) and hosts it. Every lobby gets a join code (8 characters of the friend-code
//!   alphabet, also a number below 2^40), unique among the lobbies, valid while the lobby exists;
//!   the host replaces it (`POST /v1/lobbies/{lobby}/code`).
//! - **Join:** by id (`public` lobbies; `friends` lobbies by a friend of the host, with the
//!   `Friends` module) or with the join code (`POST /v1/lobbies/join`, every
//!   visibility). Only `open` lobbies with room take players; with the friends module, a player the
//!   host blocked is refused. Join attempts are rate-limited per player (`join_rate`), and join
//!   codes that match no lobby have their own, stricter limit (`bad_code_rate`).
//! - **Members:** a ready flag each (`PUT /v1/lobbies/{lobby}/ready`); leave; the host kicks and
//!   hands the lobby over. A leaving host passes the lobby to the member who joined first; the last
//!   member's leaving removes it. A player whose last WebSocket connection on this instance closes
//!   leaves its lobbies after `disconnect_grace_secs` (`leave_on_disconnect`).
//! - **The host** changes the metadata (key by key), the size, the visibility and the state:
//!   `open`, `in_game` (no one joins; back to `open` resets every ready flag), `closed` (the lobby
//!   is removed).
//! - **Search:** open lobbies by metadata filters (`POST /v1/lobbies/search`): every public one, or
//!   those the caller's friends host; full ones left out unless asked.
//! - **Pushes** to the members: `lobby.member` (joined, left, kicked, ready) and `lobby.changed`
//!   (host, metadata, settings, state, code).
//! - **Lobby chat:** with the `Chat` module registered (and `chat_room = true`),
//!   every lobby gets a chat group room; members join and leave it with the lobby.
//! - **Limits:** `max_players` (64), `max_lobbies_per_user` (1), `max_metadata_keys` (32),
//!   `max_metadata_bytes` (4 KiB), a per-player create rate.
//! - **Staff:** a caller with the `lobbies.manage` permission ([`MANAGE`]; `admin` and `moderator`
//!   by default) acts as the host of every lobby.
//! - **Hooks** ([`events`]): [`BeforeLobbyCreate`](events::BeforeLobbyCreate) and
//!   [`BeforeLobbyUpdate`](events::BeforeLobbyUpdate) (change or refuse),
//!   [`BeforeLobbyJoin`](events::BeforeLobbyJoin) (refuse) and
//!   [`AfterLobbyChange`](events::AfterLobbyChange) (every change).
//!
//! **Storage:** lobbies, members and metadata live in the database (MySQL, PostgreSQL or SQLite),
//! so every server instance sees the same lobbies, codes and searches. The disconnect rule follows
//! the player's connections on the instance it was connected to.
//!
//! The module needs [`Auth`](crate::auth::Auth) registered before it; the WebSocket hub is optional
//! (no pushes without it).

// Without any database backend `Db` has no variants: code after a query is unreachable.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

pub mod config;
pub mod events;
mod migrations;
mod module;
mod openapi;
mod routes;
mod service;
mod store;

pub use config::LobbiesConfig;
pub use module::Lobbies;
pub use service::{LobbyActor, LobbyService};

use crate::auth::AuthContext;
use crate::permissions::Permission;
use crate::state::AppState;

/// The permission to act as the host of every lobby (change, close, kick, a new code, a new host):
/// held by `admin` and `moderator` unless `[permissions]` says otherwise (see
/// [`crate::permissions`]).
pub const MANAGE: Permission =
    Permission::new("lobbies.manage", "Act as the host of every lobby: change or close it, kick members, give it a new code or a new host")
        .granted_to(&["moderator"]);

/// Whether the caller may manage every lobby ([`MANAGE`]).
pub(crate) fn may_manage(state: &AppState, who: &AuthContext) -> bool {
    who.has_permission(state, MANAGE.name())
}
