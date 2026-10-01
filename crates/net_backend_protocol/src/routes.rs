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
//! | GET | [`chat::ROOMS`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`RoomInfo`](crate::chat::RoomInfo)`>` |
//! | GET | [`chat::HISTORY`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`ChatMessage`](crate::chat::ChatMessage)`>` |
//! | DELETE | [`chat::MESSAGE`] | auth (sender or moderator) | → [`Ack`](crate::Ack) (pushes `chat.deleted`) |
//! | POST | [`chat::DM`] | auth | [`OpenDirect`](crate::chat::OpenDirect) → [`RoomInfo`](crate::chat::RoomInfo) |
//! | GET | [`chat::DMS`] | auth | query [`PageRequest`](crate::PageRequest) → [`Page`](crate::Page)`<`[`RoomInfo`](crate::chat::RoomInfo)`>` (the caller's DMs, with `peer`) |
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
}

/// Chat (HTTP part; joining, sending and pushes go over the WebSocket).
pub mod chat {
    /// GET: the public rooms.
    pub const ROOMS: &str = "/v1/chat/rooms";
    /// GET: a page of a room's history (newest first).
    pub const HISTORY: &str = "/v1/chat/rooms/{room}/messages";
    /// DELETE: delete one message (its sender, or a moderator); the room gets `chat.deleted`.
    pub const MESSAGE: &str = "/v1/chat/rooms/{room}/messages/{message}";
    /// POST: open (or find) the direct-message room with another user.
    pub const DM: &str = "/v1/chat/dm";
    /// GET: the caller's direct-message rooms (newest activity first).
    pub const DMS: &str = "/v1/chat/dms";
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

/// Every `/v1` HTTP route this crate defines (the table above, without the WebSocket upgrade).
pub const ALL: &[Route] = &[
    route(HttpMethod::Get, INFO, false),
    route(HttpMethod::Post, auth::REGISTER, false),
    route(HttpMethod::Post, auth::LOGIN, false),
    route(HttpMethod::Post, auth::STEAM, false),
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
    route(HttpMethod::Get, chat::ROOMS, true),
    route(HttpMethod::Get, chat::HISTORY, true),
    route(HttpMethod::Delete, chat::MESSAGE, true),
    route(HttpMethod::Post, chat::DM, true),
    route(HttpMethod::Get, chat::DMS, true),
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
];

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

/// The path of one chat message (DELETE).
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
        assert_eq!(admin_identity_path(user, "steam").as_deref(), Some("/v1/admin/users/42/identities/steam"));
        assert_eq!(account_identity_path("../x"), None);
    }
}
