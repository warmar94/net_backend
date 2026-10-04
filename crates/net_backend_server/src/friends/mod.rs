//! Friends: in-game friends by account; the protocol's `/v1/friends` routes and the
//! `friends.presence` push (cargo feature `friends`, module [`Friends`]).
//!
//! - **Requests:** a player adds another by account id, by display name (exact; several accounts
//!   with that name answer 409 `conflict`) or by friend code (`POST /v1/friends/requests`); the
//!   other accepts or declines, the sender cancels; when both asked, they are friends at once.
//!   Either ends the friendship (`DELETE /v1/friends/{user}`).
//! - **Blocks:** a block ends a friendship and every open request between the two; the blocked
//!   player's requests are refused with 403 `forbidden`. [`FriendService::is_blocked`] lets a game
//!   apply blocks elsewhere (e.g. in the chat module's `BeforeDirectOpen` / `BeforeChatSend` hooks).
//! - **Friend codes:** 8 characters per account, made on first use (`GET /v1/friends/code`),
//!   replaced by `POST /v1/friends/code`.
//! - **Online state:** a player is online while it has an open WebSocket connection, and for
//!   `online_window_secs` (90) after its last `POST /v1/friends/presence` (clients without a
//!   WebSocket). Every instance refreshes the stored online time of its connected players, so the
//!   friends list is right on every instance. With the WebSocket hub on, a player's friends get
//!   `friends.presence` when the player's first connection opens and when its last one closes
//!   (with several instances: counted over every instance, `friend_presence`), and when a heartbeat
//!   brings an offline player online.
//! - **Notifications:** with the [`Notifications`](crate::notifications) module registered (and
//!   `notify = true`), a request sends the other player a `friends.request` notification and an
//!   acceptance sends `friends.accepted` (its `sender` is the acting player); without it nothing is
//!   sent and players see requests in their list.
//! - **Steam IDs:** with Steam login on (the auth module has a Steam verifier), a player who
//!   linked a Steam account sends a list of Steam IDs, e.g. its Steam friends list
//!   (`POST /v1/friends/steam`, up to `steam_max_ids` = 500), and gets back the ones that belong to
//!   accounts here: the account id, the display name and the caller's relation (friend / request).
//!   Found are accounts with that Steam account linked; never the caller, banned accounts, players
//!   who turned `steam_findable` off (`PUT /v1/friends/settings`; on by default) or players with a
//!   block between them and the caller in either direction. A hidden player and a Steam ID without
//!   an account look the same (absent). A per-player rate (`steam_rate`: a burst of 3, then one
//!   every 5 minutes); one audit entry per lookup (`friends.steam_match`: how many asked and found,
//!   never the IDs). Without Steam login the route answers 404 `not_found`.
//! - **Limits:** `max_friends` (200) per player, `max_pending` (50) open requests sent and received
//!   per player, `max_blocks` (500), a per-player request rate (`request_rate`: a burst of 10, then
//!   one every 6 s).
//! - **Hooks** ([`events`]): [`BeforeFriendRequest`](events::BeforeFriendRequest) (refuse a
//!   request), [`AfterFriendChange`](events::AfterFriendChange) (every change) and
//!   [`BeforeSteamMatch`](events::BeforeSteamMatch) (refuse a Steam ID lookup or remove Steam IDs
//!   from it).
//!
//! The module needs [`Auth`](crate::auth::Auth) registered before it; the WebSocket hub is optional.

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

pub use config::FriendsConfig;
pub use module::Friends;
pub use service::FriendService;
