//! What games can hook into in the lobbies module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeLobbyCreate`] | before | a player is about to create a lobby; change the request (checked again) or refuse |
//! | [`BeforeLobbyUpdate`] | before | a host is about to change a lobby; change the update (checked again) or refuse (a metadata filter) |
//! | [`BeforeLobbyJoin`] | before | a player is about to join; refuse with any error (a level requirement, a ban list) |
//! | [`AfterLobbyChange`] | after | a lobby was created, changed or closed; a member joined, left, was kicked or changed its ready flag; the host or the join code changed |
//!
//! ```
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::lobbies::events::BeforeLobbyJoin;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeLobbyJoin, _, _>(|_ctx, join| async move {
//!     // The game's rule: lobby 1 is the tutorial, for new players only.
//!     if join.lobby.get() == 1 && join.user.get() < 100 {
//!         return Ok(Decision::Reject(AppError::forbidden("the tutorial lobby is for new players")));
//!     }
//!     Ok(Decision::Continue(join))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::lobbies::{CreateLobby, LobbyChange, UpdateLobby};
use net_backend_protocol::{LobbyId, UserId};

use crate::hooks::Event;

/// A player is about to create a lobby. Hooks may change `request` (checked again afterwards) or
/// refuse.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeLobbyCreate {
    /// The player creating it (its host).
    pub user: UserId,
    /// The request.
    pub request: CreateLobby,
}

impl Event for BeforeLobbyCreate {
    const NAME: &'static str = "lobbies.before_create";
}

/// A host (or server code) is about to change a lobby. Hooks may change `update` (checked again
/// afterwards) or refuse.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeLobbyUpdate {
    /// The lobby.
    pub lobby: LobbyId,
    /// The player changing it (`None`: server code).
    pub user: Option<UserId>,
    /// The change.
    pub update: UpdateLobby,
}

impl Event for BeforeLobbyUpdate {
    const NAME: &'static str = "lobbies.before_update";
}

/// A player is about to join a lobby. Refuse with any error; changed fields are ignored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeLobbyJoin {
    /// The lobby.
    pub lobby: LobbyId,
    /// The player.
    pub user: UserId,
    /// Whether the player used the join code (else the lobby's id).
    pub by_code: bool,
}

impl Event for BeforeLobbyJoin {
    const NAME: &'static str = "lobbies.before_join";
}

/// What happened in a lobby.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LobbyEvent {
    /// The lobby was created (`user`: the host).
    Created,
    /// `user` joined.
    Joined,
    /// `user` left (its own request, its account, or its connection).
    Left,
    /// `user` was removed by `actor`.
    Kicked,
    /// `user`'s ready flag is now this.
    Ready(bool),
    /// The host changed the lobby (what changed; a `closed` state is [`Closed`](Self::Closed)).
    Updated(Vec<LobbyChange>),
    /// `user` hosts the lobby now.
    HostChanged,
    /// The lobby has a new join code.
    CodeChanged,
    /// The lobby was closed and removed (by its host, by server code, or as its last member left).
    Closed,
}

/// A change is stored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterLobbyChange {
    /// The lobby.
    pub lobby: LobbyId,
    /// The player who acted (`None`: the server: a disconnect, a purge, server code).
    pub actor: Option<UserId>,
    /// The player the change is about (`None` for changes of the lobby itself).
    pub user: Option<UserId>,
    /// What happened.
    pub event: LobbyEvent,
}

impl Event for AfterLobbyChange {
    const NAME: &'static str = "lobbies.after_change";
}
