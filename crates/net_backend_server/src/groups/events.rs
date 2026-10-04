//! What games can hook into in the groups module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeGroupCreate`] | before | a player is about to create a group; change the request (checked again) or refuse (a name filter, a level requirement) |
//! | [`BeforeGroupUpdate`] | before | an owner or admin is about to change a group; change the update (checked again) or refuse |
//! | [`BeforeGroupJoin`] | before | a player is about to join (an open group or by an invitation); refuse with any error |
//! | [`BeforeGroupInvite`] | before | an owner or admin (rights checked) is about to invite a player; refuse with any error |
//! | [`AfterGroupChange`] | after | a group was created, changed or deleted; a member joined, left, was kicked, got a role or the ownership (also from the upkeep after the owner's account was deleted); an invitation was sent, revoked or declined |
//!
//! ```
//! use net_backend_server::groups::events::BeforeGroupCreate;
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeGroupCreate, _, _>(|_ctx, create| async move {
//!     // The game's name filter.
//!     if create.request.name.to_lowercase().contains("admin") {
//!         return Ok(Decision::Reject(AppError::forbidden("this name is not allowed")));
//!     }
//!     Ok(Decision::Continue(create))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::groups::{CreateGroup, GroupRole, UpdateGroup};
use net_backend_protocol::{GroupId, UserId};

use crate::hooks::Event;

/// A player is about to create a group. Hooks may change `request` (checked again afterwards) or
/// refuse.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeGroupCreate {
    /// The player creating it (its owner).
    pub user: UserId,
    /// The request.
    pub request: CreateGroup,
}

impl Event for BeforeGroupCreate {
    const NAME: &'static str = "groups.before_create";
}

/// An owner or admin is about to change a group. Hooks may change `update` (checked again
/// afterwards) or refuse.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeGroupUpdate {
    /// The group.
    pub group: GroupId,
    /// The player changing it.
    pub user: UserId,
    /// The change.
    pub update: UpdateGroup,
}

impl Event for BeforeGroupUpdate {
    const NAME: &'static str = "groups.before_update";
}

/// A player is about to join a group. Refuse with any error; changed fields are ignored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeGroupJoin {
    /// The group.
    pub group: GroupId,
    /// The player.
    pub user: UserId,
    /// Whether the player was invited (else the group is open).
    pub invited: bool,
}

impl Event for BeforeGroupJoin {
    const NAME: &'static str = "groups.before_join";
}

/// An owner or admin is about to invite a player (its rights are checked, the invitee has not
/// blocked it). Refuse with any error; changed fields are ignored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeGroupInvite {
    /// The group.
    pub group: GroupId,
    /// The player inviting.
    pub user: UserId,
    /// The player invited.
    pub invitee: UserId,
}

impl Event for BeforeGroupInvite {
    const NAME: &'static str = "groups.before_invite";
}

/// What changed in a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroupChange {
    /// The group was created (`user`: the owner).
    Created,
    /// Its name, description, openness or metadata changed.
    Updated,
    /// It was deleted (by its owner, as its last member left, or by the upkeep when the owner's
    /// account was deleted and no member was left).
    Deleted,
    /// `user` joined.
    Joined,
    /// `user` left.
    Left,
    /// `user` was kicked by `actor`.
    Kicked,
    /// `user` got the role.
    RoleChanged(GroupRole),
    /// `user` is the owner now: from the old owner (`actor`, an admin now), or from the upkeep
    /// (`actor` `None`) when the owner's account was deleted.
    Transferred,
    /// `user` was invited by `actor`.
    Invited,
    /// `user`'s invitation was withdrawn by `actor`.
    InviteRevoked,
    /// `user` declined its invitation.
    InviteDeclined,
}

/// A change is stored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterGroupChange {
    /// The group.
    pub group: GroupId,
    /// The player who acted (`None`: the server: the upkeep after an owner's account was deleted).
    pub actor: Option<UserId>,
    /// The player the change is about (the actor itself for create, update, delete, join, leave;
    /// `None` for a deletion by the upkeep).
    pub user: Option<UserId>,
    /// What changed.
    pub change: GroupChange,
}

impl Event for AfterGroupChange {
    const NAME: &'static str = "groups.after_change";
}
