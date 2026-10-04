//! What games can hook into in the friends module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeFriendRequest`] | before | a player is about to send a friend request (also one that accepts the other's request by asking back); refuse it with any error (the game's own rules) |
//! | [`AfterFriendChange`] | after | a request, an acceptance, a decline, a cancel, a removal, a block or an unblock is stored |
//! | [`BeforeSteamMatch`] | before | a player is about to look up Steam IDs (`POST /v1/friends/steam`); refuse it, or remove Steam IDs from the lookup |
//!
//! ```
//! use net_backend_server::friends::events::BeforeFriendRequest;
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeFriendRequest, _, _>(|_ctx, request| async move {
//!     // The game's rule: account 1 is the game's own bot.
//!     if request.to.get() == 1 {
//!         return Ok(Decision::Reject(AppError::forbidden("this account takes no friends")));
//!     }
//!     Ok(Decision::Continue(request))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::UserId;

use crate::hooks::Event;

/// A player is about to send a friend request. Refuse with any error (the client gets it);
/// changed fields are ignored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeFriendRequest {
    /// The player sending it.
    pub from: UserId,
    /// The player it goes to.
    pub to: UserId,
}

impl Event for BeforeFriendRequest {
    const NAME: &'static str = "friends.before_request";
}

/// What changed between two players.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FriendChange {
    /// `user` sent `other` a request.
    Requested,
    /// `user` accepted `other`'s request (or asked back): they are friends.
    Accepted,
    /// `user` declined `other`'s request.
    Declined,
    /// `user` withdrew its request to `other`.
    Cancelled,
    /// `user` ended the friendship with `other`.
    Removed,
    /// `user` blocked `other` (a friendship or open requests between them ended).
    Blocked,
    /// `user` lifted its block of `other`.
    Unblocked,
}

/// A change is stored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterFriendChange {
    /// The player who acted.
    pub user: UserId,
    /// The other player.
    pub other: UserId,
    /// What changed.
    pub change: FriendChange,
}

impl Event for AfterFriendChange {
    const NAME: &'static str = "friends.after_change";
}

/// A player is about to look up which Steam IDs belong to accounts here (`POST /v1/friends/steam`,
/// after the shape, rate and linked-Steam checks). Refuse with any error (the client gets it), or
/// remove Steam IDs from `steam_ids` (e.g. keep only the player's friends as the game's own
/// check knows them); Steam IDs a hook adds are ignored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeSteamMatch {
    /// The player looking up.
    pub user: UserId,
    /// The player's own SteamID64 (its linked Steam account).
    pub own_steam_id: u64,
    /// The SteamID64s to look up, in the request's order, each once.
    pub steam_ids: Vec<u64>,
}

impl Event for BeforeSteamMatch {
    const NAME: &'static str = "friends.before_steam_match";
}
