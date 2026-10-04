//! The HTTP routes. Everything versioned lives under [`PREFIX`] (`/v1`), which is public API from
//! 0.1.0: a route is never renamed or removed within `/v1`, new routes may be added. Path
//! parameters use the `{name}` syntax; the `*_path` helpers fill them in.
//!
//! Authentication: routes marked "auth" need `Authorization: Bearer <access token>`; "admin" routes
//! need the token of an account with the [`ADMIN_ROLE`](crate::admin::ADMIN_ROLE) role.
//!
//! | Method | Path | Auth | Body → answer |
//! |---|---|---|---|
//! | GET | [`INFO`] | no | → [`ServerInfo`](crate::ServerInfo) |
//! | POST | [`auth::REGISTER`] | no | [`RegisterRequest`](crate::auth::RegisterRequest) → [`AuthSession`](crate::auth::AuthSession) |
//! | POST | [`auth::LOGIN`] | no | [`LoginRequest`](crate::auth::LoginRequest) → [`AuthSession`](crate::auth::AuthSession) |
//! | POST | [`auth::STEAM`] | no | [`SteamLoginRequest`](crate::auth::SteamLoginRequest) → [`AuthSession`](crate::auth::AuthSession) |
//! | POST | [`auth::OAUTH`] | no (a Bearer token links) | [`OAuthToken`](crate::oauth::OAuthToken) → [`AuthSession`](crate::auth::AuthSession) |
//! | POST | [`auth::REFRESH`] | no | [`RefreshRequest`](crate::auth::RefreshRequest) → [`TokenPair`](crate::auth::TokenPair) |
//! | POST | [`auth::LOGOUT`] | auth or `refresh_token` | [`LogoutRequest`](crate::auth::LogoutRequest) → [`Ack`](crate::Ack) |
//! | POST | [`auth::VERIFY_EMAIL`] | no | [`VerifyEmailRequest`](crate::auth::VerifyEmailRequest) → [`Ack`](crate::Ack) |
//! | POST | [`auth::RESEND_VERIFICATION`] | auth | (none) → [`Ack`](crate::Ack) |
//! | POST | [`auth::FORGOT_PASSWORD`] | no | [`ForgotPasswordRequest`](crate::auth::ForgotPasswordRequest) → [`Ack`](crate::Ack) (always, no account enumeration) |
//! | POST | [`auth::RESET_PASSWORD`] | no | [`ResetPasswordRequest`](crate::auth::ResetPasswordRequest) → [`Ack`](crate::Ack) |
//! | GET | [`account::ME`] | auth | → [`Account`](crate::auth::Account) |
//! | PATCH | [`account::ME`] | auth | [`UpdateAccountRequest`](crate::auth::UpdateAccountRequest) → [`Account`](crate::auth::Account) |
//! | POST | [`account::PASSWORD`] | auth | [`ChangePasswordRequest`](crate::auth::ChangePasswordRequest) → [`Ack`](crate::Ack) |
//! | DELETE | [`account::IDENTITY`] | auth (recent login) | → [`Ack`](crate::Ack) (unlink a login provider) |
//! | GET | [`storage::COLLECTION`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`StorageObjectInfo`](crate::storage::StorageObjectInfo)`>` (no values) |
//! | GET | [`storage::OBJECT`] | auth | → [`StorageObject`](crate::storage::StorageObject) |
//! | PUT | [`storage::OBJECT`] | auth | [`PutObject`](crate::storage::PutObject) → [`ObjectAck`](crate::storage::ObjectAck) |
//! | DELETE | [`storage::OBJECT`] | auth | query [`DeleteObject`](crate::storage::DeleteObject) → [`Ack`](crate::Ack) |
//! | POST | [`storage::BATCH_GET`] | auth | [`BatchGet`](crate::storage::BatchGet) → [`BatchObjects`](crate::storage::BatchObjects) |
//! | POST | [`storage::BATCH_PUT`] | auth | [`BatchPut`](crate::storage::BatchPut) → [`BatchAcks`](crate::storage::BatchAcks) |
//! | GET | [`storage::PLAYER_COLLECTION`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`StorageObjectInfo`](crate::storage::StorageObjectInfo)`>` (another player's objects the caller may read) |
//! | GET | [`storage::PLAYER_OBJECT`] | auth | → [`StorageObject`](crate::storage::StorageObject) (public, or for the owner's friends) |
//! | GET | [`chat::ROOMS`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`RoomInfo`](crate::chat::RoomInfo)`>` |
//! | GET | [`chat::HISTORY`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`ChatMessage`](crate::chat::ChatMessage)`>` |
//! | DELETE | [`chat::MESSAGE`] | auth (sender or moderator) | → [`Ack`](crate::Ack) (pushes `chat.deleted`) |
//! | POST | [`chat::DM`] | auth | [`OpenDirect`](crate::chat::OpenDirect) → [`RoomInfo`](crate::chat::RoomInfo) |
//! | GET | [`chat::DMS`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`RoomInfo`](crate::chat::RoomInfo)`>` (the caller's DMs, with `peer`) |
//! | PATCH | [`chat::MESSAGE`] | auth (sender in the edit window, or the moderation permission) | [`MessageEdit`](crate::chat::MessageEdit) → [`ChatMessage`](crate::chat::ChatMessage) (pushes `chat.edited`) |
//! | PUT | [`chat::READ`] | auth | [`ReadUpTo`](crate::chat::ReadUpTo) → [`Ack`](crate::Ack) (the read marker; pushes `chat.read`) |
//! | GET | [`chat::RECEIPTS`] | auth (members) | → [`ReadReceipts`](crate::chat::ReadReceipts) |
//! | POST | [`chat::UNREAD`] | auth | [`UnreadQuery`](crate::chat::UnreadQuery) → [`UnreadCounts`](crate::chat::UnreadCounts) |
//! | POST | [`chat::ROOMS`] | auth | [`CreateRoom`](crate::chat::CreateRoom) → [`RoomInfo`](crate::chat::RoomInfo) (a player room) |
//! | GET | [`chat::ROOMS_MINE`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`RoomInfo`](crate::chat::RoomInfo)`>` (the caller's player rooms and invitations) |
//! | GET | [`chat::ROOMS_PUBLIC`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`RoomInfo`](crate::chat::RoomInfo)`>` (public player rooms) |
//! | GET | [`chat::ROOM`] | auth | → [`RoomInfo`](crate::chat::RoomInfo) |
//! | PATCH | [`chat::ROOM`] | auth (owner, moderators) | [`UpdateRoom`](crate::chat::UpdateRoom) → [`RoomInfo`](crate::chat::RoomInfo) |
//! | DELETE | [`chat::ROOM`] | auth (owner) | → [`Ack`](crate::Ack) |
//! | POST | [`chat::ROOM_JOIN`] | auth | (none) → [`RoomInfo`](crate::chat::RoomInfo) (become a member / accept an invitation) |
//! | POST | [`chat::ROOM_LEAVE`] | auth | (none) → [`Ack`](crate::Ack) (stop being a member / decline) |
//! | GET | [`chat::ROOM_MEMBERS`] | auth (members) | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`RoomMembership`](crate::chat::RoomMembership)`>` |
//! | POST | [`chat::ROOM_INVITES`] | auth (owner, moderators) | [`RoomUser`](crate::chat::RoomUser) → [`Ack`](crate::Ack) |
//! | DELETE | [`chat::ROOM_MEMBER`] | auth (owner, moderators) | → [`Ack`](crate::Ack) (kick: banned until invited again) |
//! | PUT | [`chat::ROOM_ROLE`] | auth (owner) | [`RoomRoleChange`](crate::chat::RoomRoleChange) → [`Ack`](crate::Ack) |
//! | POST | [`chat::ROOM_OWNER`] | auth (owner) | [`RoomUser`](crate::chat::RoomUser) → [`Ack`](crate::Ack) |
//! | GET | [`leaderboards::BOARDS`] | auth | → [`Boards`](crate::leaderboards::Boards) |
//! | GET | [`leaderboards::BOARD`] | auth | query [`TopQuery`](crate::leaderboards::TopQuery) → [`LeaderboardPage`](crate::leaderboards::LeaderboardPage) (best first) |
//! | POST | [`leaderboards::SCORES`] | auth | [`SubmitScore`](crate::leaderboards::SubmitScore) → [`ScoreAck`](crate::leaderboards::ScoreAck) |
//! | GET | [`leaderboards::ME`] | auth | query [`RankQuery`](crate::leaderboards::RankQuery) → [`MyRank`](crate::leaderboards::MyRank) |
//! | GET | [`leaderboards::AROUND`] | auth | query [`AroundQuery`](crate::leaderboards::AroundQuery) → [`LeaderboardPage`](crate::leaderboards::LeaderboardPage) |
//! | GET | [`notifications::LIST`] | auth | query [`NotificationQuery`](crate::notifications::NotificationQuery) → [`Page`](crate::Page)`<`[`Notification`](crate::notifications::Notification)`>` (newest first) |
//! | GET | [`notifications::COUNT`] | auth | → [`NotificationCount`](crate::notifications::NotificationCount) |
//! | POST | [`notifications::MARK`] | auth | [`MarkNotifications`](crate::notifications::MarkNotifications) → [`MarkAck`](crate::notifications::MarkAck) |
//! | DELETE | [`notifications::ONE`] | auth | → [`Ack`](crate::Ack) |
//! | GET | [`friends::LIST`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`FriendEntry`](crate::friends::FriendEntry)`>` (with online state) |
//! | DELETE | [`friends::ONE`] | auth | → [`Ack`](crate::Ack) (end a friendship) |
//! | GET | [`friends::REQUESTS`] | auth | query [`RequestQuery`](crate::friends::RequestQuery) → [`Page`](crate::Page)`<`[`FriendEntry`](crate::friends::FriendEntry)`>` |
//! | POST | [`friends::REQUESTS`] | auth | [`AddFriend`](crate::friends::AddFriend) → [`FriendEntry`](crate::friends::FriendEntry) |
//! | DELETE | [`friends::REQUEST`] | auth | → [`Ack`](crate::Ack) (withdraw a sent request) |
//! | POST | [`friends::ACCEPT`] | auth | (none) → [`FriendEntry`](crate::friends::FriendEntry) |
//! | POST | [`friends::DECLINE`] | auth | (none) → [`Ack`](crate::Ack) |
//! | GET | [`friends::BLOCKS`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`FriendEntry`](crate::friends::FriendEntry)`>` |
//! | PUT | [`friends::BLOCK`] | auth | (none) → [`Ack`](crate::Ack) (block) |
//! | DELETE | [`friends::BLOCK`] | auth | → [`Ack`](crate::Ack) (unblock) |
//! | GET | [`friends::CODE`] | auth | → [`FriendCode`](crate::friends::FriendCode) |
//! | POST | [`friends::CODE`] | auth | (none) → [`FriendCode`](crate::friends::FriendCode) (a new code) |
//! | POST | [`friends::PRESENCE`] | auth | (none) → [`Ack`](crate::Ack) (online heartbeat) |
//! | POST | [`friends::STEAM`] | auth | [`SteamMatch`](crate::friends::SteamMatch) → [`SteamMatchResult`](crate::friends::SteamMatchResult) (Steam IDs → accounts here) |
//! | GET | [`friends::SETTINGS`] | auth | → [`FriendSettings`](crate::friends::FriendSettings) |
//! | PUT | [`friends::SETTINGS`] | auth | [`UpdateFriendSettings`](crate::friends::UpdateFriendSettings) → [`FriendSettings`](crate::friends::FriendSettings) |
//! | GET | [`groups::LIST`] | auth | query [`GroupQuery`](crate::groups::GroupQuery) → [`Page`](crate::Page)`<`[`GroupInfo`](crate::groups::GroupInfo)`>` (by name) |
//! | POST | [`groups::LIST`] | auth | [`CreateGroup`](crate::groups::CreateGroup) → [`GroupInfo`](crate::groups::GroupInfo) |
//! | GET | [`groups::MINE`] | auth | → [`GroupList`](crate::groups::GroupList) |
//! | GET | [`groups::INVITES`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`GroupInvite`](crate::groups::GroupInvite)`>` |
//! | GET | [`groups::ONE`] | auth | → [`GroupInfo`](crate::groups::GroupInfo) |
//! | PATCH | [`groups::ONE`] | auth (owner, admins) | [`UpdateGroup`](crate::groups::UpdateGroup) → [`GroupInfo`](crate::groups::GroupInfo) |
//! | DELETE | [`groups::ONE`] | auth (owner) | → [`Ack`](crate::Ack) |
//! | GET | [`groups::MEMBERS`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`GroupMember`](crate::groups::GroupMember)`>` |
//! | POST | [`groups::JOIN`] | auth | (none) → [`GroupInfo`](crate::groups::GroupInfo) |
//! | POST | [`groups::LEAVE`] | auth | (none) → [`Ack`](crate::Ack) |
//! | POST | [`groups::GROUP_INVITES`] | auth (owner, admins) | [`Invitee`](crate::groups::Invitee) → [`Ack`](crate::Ack) |
//! | POST | [`groups::ACCEPT`] | auth | (none) → [`GroupInfo`](crate::groups::GroupInfo) |
//! | POST | [`groups::DECLINE`] | auth | (none) → [`Ack`](crate::Ack) |
//! | DELETE | [`groups::INVITE`] | auth (owner, admins) | → [`Ack`](crate::Ack) |
//! | DELETE | [`groups::MEMBER`] | auth (owner, admins) | → [`Ack`](crate::Ack) (kick) |
//! | PUT | [`groups::ROLE`] | auth (owner) | [`RoleChange`](crate::groups::RoleChange) → [`Ack`](crate::Ack) |
//! | POST | [`groups::TRANSFER`] | auth (owner) | [`Invitee`](crate::groups::Invitee) → [`Ack`](crate::Ack) |
//! | POST | [`lobbies::LIST`] | auth | [`CreateLobby`](crate::lobbies::CreateLobby) → [`LobbyInfo`](crate::lobbies::LobbyInfo) |
//! | GET | [`lobbies::MINE`] | auth | → [`LobbyList`](crate::lobbies::LobbyList) |
//! | POST | [`lobbies::SEARCH`] | auth | [`LobbySearch`](crate::lobbies::LobbySearch) → [`Page`](crate::Page)`<`[`LobbyInfo`](crate::lobbies::LobbyInfo)`>` |
//! | POST | [`lobbies::JOIN_CODE`] | auth | [`JoinLobbyByCode`](crate::lobbies::JoinLobbyByCode) → [`LobbyInfo`](crate::lobbies::LobbyInfo) |
//! | GET | [`lobbies::ONE`] | auth | → [`LobbyInfo`](crate::lobbies::LobbyInfo) (with its members) |
//! | PATCH | [`lobbies::ONE`] | auth (host) | [`UpdateLobby`](crate::lobbies::UpdateLobby) → [`LobbyInfo`](crate::lobbies::LobbyInfo) |
//! | POST | [`lobbies::JOIN`] | auth | (none) → [`LobbyInfo`](crate::lobbies::LobbyInfo) |
//! | POST | [`lobbies::LEAVE`] | auth | (none) → [`Ack`](crate::Ack) |
//! | PUT | [`lobbies::READY`] | auth | [`SetReady`](crate::lobbies::SetReady) → [`Ack`](crate::Ack) |
//! | POST | [`lobbies::CODE`] | auth (host) | (none) → [`LobbyInfo`](crate::lobbies::LobbyInfo) (a new join code) |
//! | POST | [`lobbies::HOST`] | auth (host) | [`LobbyPlayer`](crate::lobbies::LobbyPlayer) → [`Ack`](crate::Ack) |
//! | DELETE | [`lobbies::MEMBER`] | auth (host) | → [`Ack`](crate::Ack) (kick) |
//! | GET | [`matchmaking::QUEUES`] | auth | → [`Queues`](crate::matchmaking::Queues) |
//! | POST | [`matchmaking::TICKET`] | auth | [`CreateTicket`](crate::matchmaking::CreateTicket) → [`MatchTicket`](crate::matchmaking::MatchTicket) |
//! | GET | [`matchmaking::TICKET`] | auth | → [`MatchTicket`](crate::matchmaking::MatchTicket) |
//! | DELETE | [`matchmaking::TICKET`] | auth | → [`Ack`](crate::Ack) (cancel) |
//! | GET | [`admin::USERS`] | admin | query [`UserListQuery`](crate::admin::UserListQuery) → [`Page`](crate::Page)`<`[`AdminUser`](crate::admin::AdminUser)`>` |
//! | GET | [`admin::USER`] | admin | → [`AdminUser`](crate::admin::AdminUser) |
//! | POST | [`admin::BAN`] | admin | [`BanRequest`](crate::admin::BanRequest) → [`Ack`](crate::Ack) |
//! | POST | [`admin::UNBAN`] | admin | (none) → [`Ack`](crate::Ack) |
//! | DELETE | [`admin::SESSIONS`] | admin | → [`Ack`](crate::Ack) (every session of the user revoked) |
//! | DELETE | [`admin::IDENTITY`] | admin | → [`Ack`](crate::Ack) (unlink a login provider) |
//! | PUT | [`admin::ROLE`] | admin | (none) → [`Ack`](crate::Ack) (grant) |
//! | DELETE | [`admin::ROLE`] | admin | → [`Ack`](crate::Ack) (revoke) |
//! | GET | [`admin::AUDIT`] | admin | query [`AuditQuery`](crate::admin::AuditQuery) → [`Page`](crate::Page)`<`[`AuditEntry`](crate::admin::AuditEntry)`>` |
//! | GET | [`admin::USER_STORAGE`] | admin | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`StorageObjectInfo`](crate::storage::StorageObjectInfo)`>` (a user's collection) |
//! | GET | [`admin::USER_OBJECT`] | admin | → [`StorageObject`](crate::storage::StorageObject) |
//! | PUT | [`admin::USER_OBJECT`] | admin | [`AdminPutObject`](crate::admin::AdminPutObject) → [`ObjectAck`](crate::storage::ObjectAck) (may set the write lock) |
//! | DELETE | [`admin::USER_OBJECT`] | admin | query [`DeleteObject`](crate::storage::DeleteObject) → [`Ack`](crate::Ack) |
//! | POST | [`files::LIST`] | auth | `multipart/form-data` (`meta` JSON [`FileMeta`](crate::files::FileMeta), `file` bytes) → [`FileInfo`](crate::files::FileInfo) (in [`BINARY`]) |
//! | GET | [`files::LIST`] | auth | query [`FileQuery`](crate::files::FileQuery) → [`Page`](crate::Page)`<`[`FileInfo`](crate::files::FileInfo)`>` |
//! | GET | [`files::USAGE`] | auth | → [`FileUsage`](crate::files::FileUsage) |
//! | GET | [`files::ONE`] | auth | → [`FileInfo`](crate::files::FileInfo) |
//! | PATCH | [`files::ONE`] | auth (owner) | [`UpdateFile`](crate::files::UpdateFile) → [`FileInfo`](crate::files::FileInfo) |
//! | DELETE | [`files::ONE`] | auth (owner) | → [`Ack`](crate::Ack) |
//! | GET | [`files::CONTENT`] | auth | → the bytes (in [`BINARY`]) |
//! | GET (upgrade) | [`WS`] | header or first message | the WebSocket (see [`envelope`](crate::envelope)) |
//!
//! Every route has exactly one [`HttpCall`](crate::HttpCall) type naming its payload and answer
//! (see [`http_call`](crate::http_call)).
//!
//! Unversioned operational routes (not part of `/v1`): [`HEALTH`], [`READY`].
//!
//! Request body limits: [`DEFAULT_BODY_LIMIT_BYTES`] for every JSON route, except the storage
//! PUT ([`PUT_BODY_LIMIT_BYTES`](crate::storage::PUT_BODY_LIMIT_BYTES)) and batch put
//! ([`BATCH_BODY_LIMIT_BYTES`](crate::storage::BATCH_BODY_LIMIT_BYTES)). Answers stay far below
//! the 10 MiB `bevy_net_backend` accepts by default (see [`storage`](crate::storage)).

use std::fmt;

use crate::ids::RoomId;

/// The request body limit of every JSON route without its own (64 KiB).
pub const DEFAULT_BODY_LIMIT_BYTES: usize = 64 * 1024;

/// The version prefix of every API route.
pub const PREFIX: &str = "/v1";

/// The WebSocket endpoint (HTTP GET with an upgrade).
pub const WS: &str = "/v1/ws";

/// Server facts a client can check before anything else ([`ServerInfo`](crate::ServerInfo)).
pub const INFO: &str = "/v1/info";

/// Liveness: 200 while the process runs (unversioned, for load balancers and systemd).
pub const HEALTH: &str = "/healthz";

/// Readiness: 200 when the server can serve (database reachable), 503 while starting or stopping.
pub const READY: &str = "/readyz";

/// Authentication routes.
pub mod auth {
    /// Create an account with email + password.
    pub const REGISTER: &str = "/v1/auth/register";
    /// Log in with email + password.
    pub const LOGIN: &str = "/v1/auth/login";
    /// Log in (or create the account) with a Steam Web API ticket.
    pub const STEAM: &str = "/v1/auth/steam";
    /// Log in (or create the account, or link) with an OpenID Connect ID token of a provider.
    pub const OAUTH: &str = "/v1/auth/oauth/{provider}";
    /// Exchange a refresh token for a new token pair (the refresh token rotates).
    pub const REFRESH: &str = "/v1/auth/refresh";
    /// Revoke the current session (or every session); a valid Bearer token OR the session's
    /// refresh token in the body.
    pub const LOGOUT: &str = "/v1/auth/logout";
    /// Confirm an email address with the token from the mail.
    pub const VERIFY_EMAIL: &str = "/v1/auth/email/verify";
    /// Send the verification mail again.
    pub const RESEND_VERIFICATION: &str = "/v1/auth/email/resend";
    /// Ask for a password-reset mail.
    pub const FORGOT_PASSWORD: &str = "/v1/auth/password/forgot";
    /// Set a new password with the token from the mail.
    pub const RESET_PASSWORD: &str = "/v1/auth/password/reset";
}

/// The caller's own account.
pub mod account {
    /// GET: the account; PATCH: change it.
    pub const ME: &str = "/v1/account";
    /// Change the password (knowing the current one).
    pub const PASSWORD: &str = "/v1/account/password";
    /// DELETE: unlink a login provider (`steam`) from the caller's account (needs a recent login;
    /// refused if no other way to log in would remain).
    pub const IDENTITY: &str = "/v1/account/identities/{provider}";
}

/// Storage (saves and key-value objects).
pub mod storage {
    /// GET: list the caller's objects in a collection (metadata only, no values).
    pub const COLLECTION: &str = "/v1/storage/{collection}";
    /// GET / PUT / DELETE one object.
    pub const OBJECT: &str = "/v1/storage/{collection}/{key}";
    /// POST: read several objects. (`_batch` cannot be a collection name: names start with a
    /// letter or digit.)
    pub const BATCH_GET: &str = "/v1/storage/_batch/get";
    /// POST: write several objects in one transaction (all or nothing).
    pub const BATCH_PUT: &str = "/v1/storage/_batch/put";
    /// GET: list another player's objects in a collection that the caller may read (no values).
    pub const PLAYER_COLLECTION: &str = "/v1/users/{user}/storage/{collection}";
    /// GET: one of another player's objects that the caller may read.
    pub const PLAYER_OBJECT: &str = "/v1/users/{user}/storage/{collection}/{key}";
}

/// Chat (HTTP part; joining, sending and pushes go over the WebSocket).
pub mod chat {
    /// GET: the public rooms (the server's). POST: create a player room.
    pub const ROOMS: &str = "/v1/chat/rooms";
    /// GET: the caller's player rooms and invitations.
    pub const ROOMS_MINE: &str = "/v1/chat/rooms/mine";
    /// GET: the public player rooms.
    pub const ROOMS_PUBLIC: &str = "/v1/chat/rooms/public";
    /// GET: one room. PATCH: rename a player room or change its visibility. DELETE: delete a
    /// player room.
    pub const ROOM: &str = "/v1/chat/rooms/{room}";
    /// POST: become a member of a player room (public, or invited).
    pub const ROOM_JOIN: &str = "/v1/chat/rooms/{room}/join";
    /// POST: stop being a member of a player room (or decline an invitation).
    pub const ROOM_LEAVE: &str = "/v1/chat/rooms/{room}/leave";
    /// GET: the members of a player room with their roles.
    pub const ROOM_MEMBERS: &str = "/v1/chat/rooms/{room}/members";
    /// POST: invite a player into a player room.
    pub const ROOM_INVITES: &str = "/v1/chat/rooms/{room}/invites";
    /// DELETE: kick a player from a player room (or withdraw an invitation).
    pub const ROOM_MEMBER: &str = "/v1/chat/rooms/{room}/members/{user}";
    /// PUT: a member's role in a player room.
    pub const ROOM_ROLE: &str = "/v1/chat/rooms/{room}/members/{user}/role";
    /// POST: hand a player room to another member.
    pub const ROOM_OWNER: &str = "/v1/chat/rooms/{room}/owner";
    /// PUT: the caller's read marker of a room.
    pub const READ: &str = "/v1/chat/rooms/{room}/read";
    /// GET: the read markers of a room.
    pub const RECEIPTS: &str = "/v1/chat/rooms/{room}/receipts";
    /// POST: the caller's unread counts of some rooms.
    pub const UNREAD: &str = "/v1/chat/unread";
    /// GET: a page of a room's history (newest first).
    pub const HISTORY: &str = "/v1/chat/rooms/{room}/messages";
    /// DELETE: delete one message (its sender, or a moderator); the room gets `chat.deleted`.
    /// PATCH: change its text (its sender, or a moderator); the room gets `chat.edited`.
    pub const MESSAGE: &str = "/v1/chat/rooms/{room}/messages/{message}";
    /// POST: open (or find) the direct-message room with another user.
    pub const DM: &str = "/v1/chat/dm";
    /// GET: the caller's direct-message rooms (newest activity first).
    pub const DMS: &str = "/v1/chat/dms";
}

/// Leaderboards.
pub mod leaderboards {
    /// GET: every board with its current period.
    pub const BOARDS: &str = "/v1/leaderboards";
    /// GET: a page of a board, best first.
    pub const BOARD: &str = "/v1/leaderboards/{board}";
    /// POST: submit a score for the caller.
    pub const SCORES: &str = "/v1/leaderboards/{board}/scores";
    /// GET: the caller's rank on a board.
    pub const ME: &str = "/v1/leaderboards/{board}/me";
    /// GET: the entries around the caller.
    pub const AROUND: &str = "/v1/leaderboards/{board}/around";
}

/// Notifications (the WebSocket kinds `notify.*` answer the same).
pub mod notifications {
    /// GET: a page of the caller's notifications, newest first.
    pub const LIST: &str = "/v1/notifications";
    /// GET: how many notifications the caller has (unread and total).
    pub const COUNT: &str = "/v1/notifications/count";
    /// POST: mark notifications read or unread.
    pub const MARK: &str = "/v1/notifications/mark";
    /// DELETE: one of the caller's notifications.
    pub const ONE: &str = "/v1/notifications/{id}";
}

/// Friends.
pub mod friends {
    /// GET: the caller's friends.
    pub const LIST: &str = "/v1/friends";
    /// DELETE: end a friendship.
    pub const ONE: &str = "/v1/friends/{user}";
    /// GET: the caller's friend requests (received or sent); POST: send one.
    pub const REQUESTS: &str = "/v1/friends/requests";
    /// DELETE: withdraw a friend request the caller sent.
    pub const REQUEST: &str = "/v1/friends/requests/{user}";
    /// POST: accept a friend request the caller received.
    pub const ACCEPT: &str = "/v1/friends/requests/{user}/accept";
    /// POST: decline a friend request the caller received.
    pub const DECLINE: &str = "/v1/friends/requests/{user}/decline";
    /// GET: the players the caller blocked.
    pub const BLOCKS: &str = "/v1/friends/blocks";
    /// PUT: block a player; DELETE: lift the block.
    pub const BLOCK: &str = "/v1/friends/blocks/{user}";
    /// GET: the caller's friend code; POST: a new one.
    pub const CODE: &str = "/v1/friends/code";
    /// POST: the caller is online (a heartbeat for clients without a WebSocket).
    pub const PRESENCE: &str = "/v1/friends/presence";
    /// POST: which of these Steam IDs belong to accounts here (the caller has Steam linked).
    pub const STEAM: &str = "/v1/friends/steam";
    /// GET: the caller's friends settings; PUT: change them.
    pub const SETTINGS: &str = "/v1/friends/settings";
}

/// Groups (guilds, clans).
pub mod groups {
    /// GET: the groups by name (a name prefix search); POST: create one.
    pub const LIST: &str = "/v1/groups";
    /// GET: the caller's groups.
    pub const MINE: &str = "/v1/groups/mine";
    /// GET: the caller's invitations.
    pub const INVITES: &str = "/v1/groups/invites";
    /// GET: one group; PATCH: change it; DELETE: delete it.
    pub const ONE: &str = "/v1/groups/{group}";
    /// GET: a group's members.
    pub const MEMBERS: &str = "/v1/groups/{group}/members";
    /// POST: join (an open group, or with an invitation).
    pub const JOIN: &str = "/v1/groups/{group}/join";
    /// POST: leave.
    pub const LEAVE: &str = "/v1/groups/{group}/leave";
    /// POST: invite a player.
    pub const GROUP_INVITES: &str = "/v1/groups/{group}/invites";
    /// POST: accept the caller's invitation.
    pub const ACCEPT: &str = "/v1/groups/{group}/invites/accept";
    /// POST: decline the caller's invitation.
    pub const DECLINE: &str = "/v1/groups/{group}/invites/decline";
    /// DELETE: withdraw a player's invitation.
    pub const INVITE: &str = "/v1/groups/{group}/invites/{user}";
    /// DELETE: remove a member.
    pub const MEMBER: &str = "/v1/groups/{group}/members/{user}";
    /// PUT: a member's role.
    pub const ROLE: &str = "/v1/groups/{group}/members/{user}/role";
    /// POST: hand the group to a member.
    pub const TRANSFER: &str = "/v1/groups/{group}/transfer";
}

/// Lobbies.
pub mod lobbies {
    /// POST: create a lobby.
    pub const LIST: &str = "/v1/lobbies";
    /// GET: the caller's lobbies.
    pub const MINE: &str = "/v1/lobbies/mine";
    /// POST: search open lobbies by metadata.
    pub const SEARCH: &str = "/v1/lobbies/search";
    /// POST: join with a join code.
    pub const JOIN_CODE: &str = "/v1/lobbies/join";
    /// GET: one lobby; PATCH: change it.
    pub const ONE: &str = "/v1/lobbies/{lobby}";
    /// POST: join by id.
    pub const JOIN: &str = "/v1/lobbies/{lobby}/join";
    /// POST: leave.
    pub const LEAVE: &str = "/v1/lobbies/{lobby}/leave";
    /// PUT: the caller's ready flag.
    pub const READY: &str = "/v1/lobbies/{lobby}/ready";
    /// POST: a new join code.
    pub const CODE: &str = "/v1/lobbies/{lobby}/code";
    /// POST: hand the lobby to another member.
    pub const HOST: &str = "/v1/lobbies/{lobby}/host";
    /// DELETE: remove a member.
    pub const MEMBER: &str = "/v1/lobbies/{lobby}/members/{user}";
}

/// Matchmaking.
pub mod matchmaking {
    /// GET: the server's queues.
    pub const QUEUES: &str = "/v1/matchmaking/queues";
    /// POST: put a ticket into a queue; GET: the caller's ticket; DELETE: cancel it.
    pub const TICKET: &str = "/v1/matchmaking/ticket";
}

/// Files (binary uploads).
pub mod files {
    /// POST (multipart): upload a file ([`BINARY`](super::BINARY)); GET: the caller's files, or
    /// another player's readable ones.
    pub const LIST: &str = "/v1/files";
    /// GET: what the caller's files use and the limits.
    pub const USAGE: &str = "/v1/files/usage";
    /// GET: a file's settings; PATCH: change them; DELETE: delete the file.
    pub const ONE: &str = "/v1/files/{file}";
    /// GET: the file's bytes ([`BINARY`](super::BINARY)).
    pub const CONTENT: &str = "/v1/files/{file}/content";
}

/// Administration (operator tools; every route needs the `admin` role, see [`crate::admin`]).
pub mod admin {
    /// GET: list accounts.
    pub const USERS: &str = "/v1/admin/users";
    /// GET: one account.
    pub const USER: &str = "/v1/admin/users/{user}";
    /// POST: ban the account.
    pub const BAN: &str = "/v1/admin/users/{user}/ban";
    /// POST: lift the ban.
    pub const UNBAN: &str = "/v1/admin/users/{user}/unban";
    /// DELETE: revoke every session of the account.
    pub const SESSIONS: &str = "/v1/admin/users/{user}/sessions";
    /// DELETE: unlink a login provider from the account.
    pub const IDENTITY: &str = "/v1/admin/users/{user}/identities/{provider}";
    /// PUT: grant the role; DELETE: revoke it.
    pub const ROLE: &str = "/v1/admin/users/{user}/roles/{role}";
    /// GET: the audit log.
    pub const AUDIT: &str = "/v1/admin/audit";
    /// GET: list one collection of a user's storage (metadata only).
    pub const USER_STORAGE: &str = "/v1/admin/users/{user}/storage/{collection}";
    /// GET / PUT / DELETE one of a user's storage objects.
    pub const USER_OBJECT: &str = "/v1/admin/users/{user}/storage/{collection}/{key}";
}

/// An HTTP method of a [`Route`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HttpMethod {
    /// GET.
    Get,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// PATCH.
    Patch,
    /// DELETE.
    Delete,
}

impl HttpMethod {
    /// The method's name as on the wire (`"GET"`, …).
    pub const fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One route of the table [`ALL`]: method, path and whether it needs an access token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Route {
    /// The method.
    pub method: HttpMethod,
    /// The path (with `{param}` placeholders).
    pub path: &'static str,
    /// Whether `Authorization: Bearer <access token>` is required (logout: a Bearer token or the
    /// refresh token in the body, so `false` here).
    pub auth: bool,
}

impl Route {
    /// A route (modules and games can describe their own routes the same way).
    pub const fn new(method: HttpMethod, path: &'static str, auth: bool) -> Self {
        Route { method, path, auth }
    }
}

const fn route(method: HttpMethod, path: &'static str, auth: bool) -> Route {
    Route::new(method, path, auth)
}

/// Every `/v1` HTTP route this crate defines with a JSON (or query, or empty) request and a JSON
/// answer: each has an `HttpCall` (the table above, without the WebSocket upgrade; the file upload
/// and download are in [`BINARY`]).
pub const ALL: &[Route] = &[
    route(HttpMethod::Get, INFO, false),
    route(HttpMethod::Post, auth::REGISTER, false),
    route(HttpMethod::Post, auth::LOGIN, false),
    route(HttpMethod::Post, auth::STEAM, false),
    route(HttpMethod::Post, auth::OAUTH, false),
    route(HttpMethod::Post, auth::REFRESH, false),
    route(HttpMethod::Post, auth::LOGOUT, false),
    route(HttpMethod::Post, auth::VERIFY_EMAIL, false),
    route(HttpMethod::Post, auth::RESEND_VERIFICATION, true),
    route(HttpMethod::Post, auth::FORGOT_PASSWORD, false),
    route(HttpMethod::Post, auth::RESET_PASSWORD, false),
    route(HttpMethod::Get, account::ME, true),
    route(HttpMethod::Patch, account::ME, true),
    route(HttpMethod::Post, account::PASSWORD, true),
    route(HttpMethod::Delete, account::IDENTITY, true),
    route(HttpMethod::Get, storage::COLLECTION, true),
    route(HttpMethod::Get, storage::OBJECT, true),
    route(HttpMethod::Put, storage::OBJECT, true),
    route(HttpMethod::Delete, storage::OBJECT, true),
    route(HttpMethod::Post, storage::BATCH_GET, true),
    route(HttpMethod::Post, storage::BATCH_PUT, true),
    route(HttpMethod::Get, storage::PLAYER_COLLECTION, true),
    route(HttpMethod::Get, storage::PLAYER_OBJECT, true),
    route(HttpMethod::Get, chat::ROOMS, true),
    route(HttpMethod::Get, chat::HISTORY, true),
    route(HttpMethod::Delete, chat::MESSAGE, true),
    route(HttpMethod::Post, chat::DM, true),
    route(HttpMethod::Get, chat::DMS, true),
    route(HttpMethod::Patch, chat::MESSAGE, true),
    route(HttpMethod::Put, chat::READ, true),
    route(HttpMethod::Get, chat::RECEIPTS, true),
    route(HttpMethod::Post, chat::UNREAD, true),
    route(HttpMethod::Post, chat::ROOMS, true),
    route(HttpMethod::Get, chat::ROOMS_MINE, true),
    route(HttpMethod::Get, chat::ROOMS_PUBLIC, true),
    route(HttpMethod::Get, chat::ROOM, true),
    route(HttpMethod::Patch, chat::ROOM, true),
    route(HttpMethod::Delete, chat::ROOM, true),
    route(HttpMethod::Post, chat::ROOM_JOIN, true),
    route(HttpMethod::Post, chat::ROOM_LEAVE, true),
    route(HttpMethod::Get, chat::ROOM_MEMBERS, true),
    route(HttpMethod::Post, chat::ROOM_INVITES, true),
    route(HttpMethod::Delete, chat::ROOM_MEMBER, true),
    route(HttpMethod::Put, chat::ROOM_ROLE, true),
    route(HttpMethod::Post, chat::ROOM_OWNER, true),
    route(HttpMethod::Get, leaderboards::BOARDS, true),
    route(HttpMethod::Get, leaderboards::BOARD, true),
    route(HttpMethod::Post, leaderboards::SCORES, true),
    route(HttpMethod::Get, leaderboards::ME, true),
    route(HttpMethod::Get, leaderboards::AROUND, true),
    route(HttpMethod::Get, notifications::LIST, true),
    route(HttpMethod::Get, notifications::COUNT, true),
    route(HttpMethod::Post, notifications::MARK, true),
    route(HttpMethod::Delete, notifications::ONE, true),
    route(HttpMethod::Get, friends::LIST, true),
    route(HttpMethod::Delete, friends::ONE, true),
    route(HttpMethod::Get, friends::REQUESTS, true),
    route(HttpMethod::Post, friends::REQUESTS, true),
    route(HttpMethod::Delete, friends::REQUEST, true),
    route(HttpMethod::Post, friends::ACCEPT, true),
    route(HttpMethod::Post, friends::DECLINE, true),
    route(HttpMethod::Get, friends::BLOCKS, true),
    route(HttpMethod::Put, friends::BLOCK, true),
    route(HttpMethod::Delete, friends::BLOCK, true),
    route(HttpMethod::Get, friends::CODE, true),
    route(HttpMethod::Post, friends::CODE, true),
    route(HttpMethod::Post, friends::PRESENCE, true),
    route(HttpMethod::Post, friends::STEAM, true),
    route(HttpMethod::Get, friends::SETTINGS, true),
    route(HttpMethod::Put, friends::SETTINGS, true),
    route(HttpMethod::Get, groups::LIST, true),
    route(HttpMethod::Post, groups::LIST, true),
    route(HttpMethod::Get, groups::MINE, true),
    route(HttpMethod::Get, groups::INVITES, true),
    route(HttpMethod::Get, groups::ONE, true),
    route(HttpMethod::Patch, groups::ONE, true),
    route(HttpMethod::Delete, groups::ONE, true),
    route(HttpMethod::Get, groups::MEMBERS, true),
    route(HttpMethod::Post, groups::JOIN, true),
    route(HttpMethod::Post, groups::LEAVE, true),
    route(HttpMethod::Post, groups::GROUP_INVITES, true),
    route(HttpMethod::Post, groups::ACCEPT, true),
    route(HttpMethod::Post, groups::DECLINE, true),
    route(HttpMethod::Delete, groups::INVITE, true),
    route(HttpMethod::Delete, groups::MEMBER, true),
    route(HttpMethod::Put, groups::ROLE, true),
    route(HttpMethod::Post, groups::TRANSFER, true),
    route(HttpMethod::Post, lobbies::LIST, true),
    route(HttpMethod::Get, lobbies::MINE, true),
    route(HttpMethod::Post, lobbies::SEARCH, true),
    route(HttpMethod::Post, lobbies::JOIN_CODE, true),
    route(HttpMethod::Get, lobbies::ONE, true),
    route(HttpMethod::Patch, lobbies::ONE, true),
    route(HttpMethod::Post, lobbies::JOIN, true),
    route(HttpMethod::Post, lobbies::LEAVE, true),
    route(HttpMethod::Put, lobbies::READY, true),
    route(HttpMethod::Post, lobbies::CODE, true),
    route(HttpMethod::Post, lobbies::HOST, true),
    route(HttpMethod::Delete, lobbies::MEMBER, true),
    route(HttpMethod::Get, matchmaking::QUEUES, true),
    route(HttpMethod::Post, matchmaking::TICKET, true),
    route(HttpMethod::Get, matchmaking::TICKET, true),
    route(HttpMethod::Delete, matchmaking::TICKET, true),
    route(HttpMethod::Get, admin::USERS, true),
    route(HttpMethod::Get, admin::USER, true),
    route(HttpMethod::Post, admin::BAN, true),
    route(HttpMethod::Post, admin::UNBAN, true),
    route(HttpMethod::Delete, admin::SESSIONS, true),
    route(HttpMethod::Delete, admin::IDENTITY, true),
    route(HttpMethod::Put, admin::ROLE, true),
    route(HttpMethod::Delete, admin::ROLE, true),
    route(HttpMethod::Get, admin::AUDIT, true),
    route(HttpMethod::Get, admin::USER_STORAGE, true),
    route(HttpMethod::Get, admin::USER_OBJECT, true),
    route(HttpMethod::Put, admin::USER_OBJECT, true),
    route(HttpMethod::Delete, admin::USER_OBJECT, true),
    route(HttpMethod::Get, files::LIST, true),
    route(HttpMethod::Get, files::USAGE, true),
    route(HttpMethod::Get, files::ONE, true),
    route(HttpMethod::Patch, files::ONE, true),
    route(HttpMethod::Delete, files::ONE, true),
];

/// The `/v1` routes whose body or answer is not JSON (no `HttpCall`; clients send them
/// themselves): the file upload (`multipart/form-data` with the parts
/// [`UPLOAD_META_PART`](crate::files::UPLOAD_META_PART) and
/// [`UPLOAD_FILE_PART`](crate::files::UPLOAD_FILE_PART) → a JSON
/// [`FileInfo`](crate::files::FileInfo)) and the download (→ the bytes).
pub const BINARY: &[Route] = &[route(HttpMethod::Post, files::LIST, true), route(HttpMethod::Get, files::CONTENT, true)];

/// The path of one file's settings (`/v1/files/12`).
pub fn file_path(file: crate::FileId) -> String {
    format!("/v1/files/{file}")
}

/// The path of one file's bytes (`/v1/files/12/content`).
pub fn file_content_path(file: crate::FileId) -> String {
    format!("/v1/files/{file}/content")
}

/// The path of one storage object, or `None` if a name is not a valid storage name
/// ([`storage::is_valid_name`](crate::storage::is_valid_name)); valid names need no escaping.
pub fn storage_object_path(collection: &str, key: &str) -> Option<String> {
    (crate::storage::is_valid_name(collection) && crate::storage::is_valid_name(key)).then(|| format!("/v1/storage/{collection}/{key}"))
}

/// The path of a storage collection, or `None` if the name is not a valid storage name.
pub fn storage_collection_path(collection: &str) -> Option<String> {
    crate::storage::is_valid_name(collection).then(|| format!("/v1/storage/{collection}"))
}

/// The history path of a chat room.
pub fn chat_history_path(room: RoomId) -> String {
    format!("/v1/chat/rooms/{room}/messages")
}

/// The path of one chat message (DELETE, PATCH).
pub fn chat_message_path(room: RoomId, message: crate::MessageId) -> String {
    format!("/v1/chat/rooms/{room}/messages/{message}")
}

/// The path of one collection of a user's storage in the administration routes, or `None` for an
/// invalid storage name.
pub fn admin_storage_path(user: crate::UserId, collection: &str) -> Option<String> {
    crate::storage::is_valid_name(collection).then(|| format!("/v1/admin/users/{user}/storage/{collection}"))
}

/// The path of one of a user's storage objects in the administration routes, or `None` for an
/// invalid storage name.
pub fn admin_object_path(user: crate::UserId, collection: &str, key: &str) -> Option<String> {
    (crate::storage::is_valid_name(collection) && crate::storage::is_valid_name(key)).then(|| format!("/v1/admin/users/{user}/storage/{collection}/{key}"))
}

/// The path of one account in the administration routes, e.g. `/v1/admin/users/42`.
pub fn admin_user_path(user: crate::UserId) -> String {
    format!("/v1/admin/users/{user}")
}

/// The ban path of an account.
pub fn admin_ban_path(user: crate::UserId) -> String {
    format!("/v1/admin/users/{user}/ban")
}

/// The unban path of an account.
pub fn admin_unban_path(user: crate::UserId) -> String {
    format!("/v1/admin/users/{user}/unban")
}

/// The sessions path of an account.
pub fn admin_sessions_path(user: crate::UserId) -> String {
    format!("/v1/admin/users/{user}/sessions")
}

/// Whether `provider` is a plausible provider name (`[a-z][a-z0-9_]*`, at most 32 bytes): such
/// names need no escaping in a path.
fn is_provider_name(provider: &str) -> bool {
    provider.len() <= 32
        && provider.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && provider.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The OpenID Connect login path of a provider (`/v1/auth/oauth/google`), or `None` for an
/// implausible provider name.
pub fn oauth_login_path(provider: &str) -> Option<String> {
    is_provider_name(provider).then(|| format!("/v1/auth/oauth/{provider}"))
}

/// The caller's identity path of a provider (`/v1/account/identities/steam`), or `None` for an
/// implausible provider name.
pub fn account_identity_path(provider: &str) -> Option<String> {
    is_provider_name(provider).then(|| format!("/v1/account/identities/{provider}"))
}

/// An account's identity path in the administration routes, or `None` for an implausible
/// provider name.
pub fn admin_identity_path(user: crate::UserId, provider: &str) -> Option<String> {
    is_provider_name(provider).then(|| format!("/v1/admin/users/{user}/identities/{provider}"))
}

/// The path of one role of an account, or `None` if the role name is not valid
/// ([`is_valid_role`](crate::admin::is_valid_role); valid names need no escaping).
pub fn admin_role_path(user: crate::UserId, role: &str) -> Option<String> {
    crate::admin::is_valid_role(role).then(|| format!("/v1/admin/users/{user}/roles/{role}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_versioned_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for r in ALL {
            assert!(r.path.starts_with("/v1/"), "{}", r.path);
            assert!(seen.insert((r.method, r.path)), "duplicate {} {}", r.method, r.path);
        }
        assert!(WS.starts_with(PREFIX));
        for r in BINARY {
            assert!(r.path.starts_with("/v1/") && !ALL.contains(r), "{}", r.path);
        }
        assert_eq!(file_content_path(crate::FileId(12)), "/v1/files/12/content");
        assert_eq!(file_path(crate::FileId(12)), "/v1/files/12");
    }

    #[test]
    fn path_helpers() {
        assert_eq!(storage_object_path("saves", "slot-1").as_deref(), Some("/v1/storage/saves/slot-1"));
        assert_eq!(storage_object_path("saves", "../x"), None);
        assert_eq!(storage_object_path("_batch", "get"), None);
        assert_eq!(storage_collection_path("saves").as_deref(), Some("/v1/storage/saves"));
        assert_eq!(chat_history_path(RoomId(12)), "/v1/chat/rooms/12/messages");
        assert_eq!(chat_message_path(RoomId(12), crate::MessageId(9)), "/v1/chat/rooms/12/messages/9");
        assert_eq!(admin_storage_path(crate::UserId(4), "saves").as_deref(), Some("/v1/admin/users/4/storage/saves"));
        assert_eq!(admin_object_path(crate::UserId(4), "saves", "a").as_deref(), Some("/v1/admin/users/4/storage/saves/a"));
        assert_eq!(admin_object_path(crate::UserId(4), "saves", "../a"), None);
        assert_eq!(HttpMethod::Patch.to_string(), "PATCH");
        let user = crate::UserId(42);
        assert_eq!(admin_user_path(user), "/v1/admin/users/42");
        assert_eq!(admin_ban_path(user), "/v1/admin/users/42/ban");
        assert_eq!(admin_unban_path(user), "/v1/admin/users/42/unban");
        assert_eq!(admin_sessions_path(user), "/v1/admin/users/42/sessions");
        assert_eq!(admin_role_path(user, "moderator").as_deref(), Some("/v1/admin/users/42/roles/moderator"));
        assert_eq!(admin_role_path(user, "../x"), None);
        assert_eq!(account_identity_path("steam").as_deref(), Some("/v1/account/identities/steam"));
        assert_eq!(oauth_login_path("google").as_deref(), Some("/v1/auth/oauth/google"));
        assert_eq!(oauth_login_path("../x"), None);
        assert_eq!(admin_identity_path(user, "steam").as_deref(), Some("/v1/admin/users/42/identities/steam"));
        assert_eq!(account_identity_path("../x"), None);
    }
}
