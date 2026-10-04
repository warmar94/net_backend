//! Groups (guilds, clans): the protocol's `/v1/groups` routes (cargo feature `groups`, module
//! [`Groups`]).
//!
//! - **Create:** a player creates a group (`POST /v1/groups`: a name unique without regard to case,
//!   a description, open or closed, the game's metadata) and owns it.
//! - **Join:** by invitation (owner and admins invite: `POST /v1/groups/{group}/invites`; the player
//!   accepts or declines) or directly when the group is open (`POST /v1/groups/{group}/join`).
//!   Invitations reach the invited player as a `groups.invite` notification when the
//!   [`Notifications`](crate::notifications) module is registered (`notify = true`), and are listed by
//!   `GET /v1/groups/invites`.
//! - **Roles:** one owner (every right; `POST /v1/groups/{group}/transfer` hands the group to a member,
//!   the old owner becomes an admin), admins (edit the group, invite, revoke invitations, kick
//!   members) and members. A kicked player gets a `groups.kicked` notification. The owner leaves only
//!   as the last member (the group is then deleted); `DELETE /v1/groups/{group}` deletes it.
//! - **Upkeep:** a background task (every `upkeep_interval_secs`, 3600) gives a group whose owner's
//!   account was deleted the oldest admin, else the oldest member, as owner, and deletes a group with
//!   no member left ([`GroupService::upkeep`]).
//! - **Lists:** every group by name with a name prefix search, the caller's groups (with its role),
//!   a group's members.
//! - **Group chat:** with the [`Chat`](crate::chat) module registered (and `chat_room = true`), every
//!   group gets a chat group room on creation; joining adds the player to it, leaving and kicks
//!   remove them (at once on every instance), deleting the group deletes the room. `GroupInfo`
//!   carries the room id.
//! - **Limits:** `max_members` (100) per group, `max_groups_per_user` (10) memberships per player,
//!   `max_invites` (50) open invitations per group, `max_metadata_bytes` (2 KiB), a per-player create
//!   rate (`create_rate`: a burst of 3, then one every 20 minutes), a per-player invitation rate
//!   (`invite_rate`: a burst of 20, then one every 30 s); a player who blocked the inviter is not
//!   invited (with the friends module).
//! - **Hooks** ([`events`]): [`BeforeGroupCreate`](events::BeforeGroupCreate) and
//!   [`BeforeGroupUpdate`](events::BeforeGroupUpdate) (change or refuse: name filters),
//!   [`BeforeGroupJoin`](events::BeforeGroupJoin), [`BeforeGroupInvite`](events::BeforeGroupInvite)
//!   (refuse) and
//!   [`AfterGroupChange`](events::AfterGroupChange) (every change).
//!
//! The module needs [`Auth`](crate::auth::Auth) registered before it; the WebSocket hub is optional
//! (the chat module needs it).

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

pub use config::GroupsConfig;
pub use module::Groups;
pub use service::GroupService;
