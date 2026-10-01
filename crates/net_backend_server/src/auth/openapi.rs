//! OpenAPI schemas of the protocol's auth and admin types (mirror structs: the protocol crate has
//! no OpenAPI dependency). A test serializes the real types and compares the field names with
//! these schemas, so the document cannot drift from the wire format.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{ComponentsBuilder, OpenApi, OpenApiBuilder};
use utoipa::ToSchema;

/// `POST /v1/auth/register` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct RegisterRequest {
    /// The email address (normalised and unique per server, case-insensitive).
    email: String,
    /// The password (10 characters to 128 bytes, no control characters).
    password: String,
    /// The public name (at most 32 characters).
    display_name: Option<String>,
}

/// `POST /v1/auth/login` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct LoginRequest {
    /// The email address.
    email: String,
    /// The password.
    password: String,
}

/// `POST /v1/auth/steam` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct SteamLoginRequest {
    /// The ticket from `GetAuthTicketForWebApi`, hex-encoded (at most 8192 characters).
    ticket_hex: String,
    /// The identity string the ticket was requested for.
    identity: String,
}

/// `POST /v1/auth/refresh` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct RefreshRequest {
    /// The current refresh token (single use; a retry within 30 s answers the same new pair).
    refresh_token: String,
}

/// `POST /v1/auth/logout` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct LogoutRequest {
    /// Revoke every session of the account.
    everywhere: Option<bool>,
    /// The session's refresh token (when the access token expired).
    refresh_token: Option<String>,
}

/// An access and a refresh token.
#[derive(Serialize, ToSchema)]
pub(crate) struct TokenPair {
    /// Always `Bearer`.
    token_type: String,
    /// For `Authorization: Bearer …`.
    access_token: String,
    /// Unix milliseconds.
    access_expires_at: i64,
    /// For `POST /v1/auth/refresh` (single use).
    refresh_token: String,
    /// Unix milliseconds.
    refresh_expires_at: i64,
}

/// A linked login provider.
#[derive(Serialize, ToSchema)]
pub(crate) struct LinkedIdentity {
    /// `steam`.
    provider: String,
    /// The id at the provider (Steam: the SteamID64 as text).
    subject: String,
}

/// The caller's account.
#[derive(Serialize, ToSchema)]
pub(crate) struct Account {
    /// The user id.
    id: i64,
    /// The email address (none for a Steam account).
    email: Option<String>,
    /// Whether the address is confirmed.
    email_verified: bool,
    /// The public name.
    display_name: Option<String>,
    /// Roles (`admin`, …).
    roles: Vec<String>,
    /// Linked providers.
    identities: Vec<LinkedIdentity>,
    /// Unix milliseconds.
    created_at: i64,
}

/// A login's answer.
#[derive(Serialize, ToSchema)]
pub(crate) struct AuthSession {
    /// The account.
    account: Account,
    /// The tokens.
    tokens: TokenPair,
}

/// `PATCH /v1/account` body (absent fields stay).
#[derive(Serialize, ToSchema)]
pub(crate) struct UpdateAccountRequest {
    /// A new public name.
    display_name: Option<String>,
}

/// `POST /v1/account/password` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct ChangePasswordRequest {
    /// The current password.
    current_password: String,
    /// The new password.
    new_password: String,
}

/// `POST /v1/auth/email/verify` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct VerifyEmailRequest {
    /// The one-time token from the mail.
    token: String,
}

/// `POST /v1/auth/password/forgot` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct ForgotPasswordRequest {
    /// The email address.
    email: String,
}

/// `POST /v1/auth/password/reset` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct ResetPasswordRequest {
    /// The one-time token from the mail.
    token: String,
    /// The new password.
    new_password: String,
}

/// An empty success answer (`{}`).
#[derive(Serialize, ToSchema)]
pub(crate) struct Ack {}

/// A ban.
#[derive(Serialize, ToSchema)]
pub(crate) struct BanInfo {
    /// Unix milliseconds.
    banned_at: i64,
    /// Unix milliseconds; absent = until lifted.
    until: Option<i64>,
    /// Why.
    reason: Option<String>,
}

/// An account as an admin sees it.
#[derive(Serialize, ToSchema)]
pub(crate) struct AdminUser {
    /// The account.
    account: Account,
    /// The ban, if any.
    ban: Option<BanInfo>,
    /// Unix milliseconds.
    last_seen_at: Option<i64>,
    /// Open sessions.
    active_sessions: u32,
}

/// A page of accounts.
#[derive(Serialize, ToSchema)]
pub(crate) struct AdminUserPage {
    /// The accounts, newest first.
    items: Vec<AdminUser>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// `POST /v1/admin/users/{user}/ban` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct BanRequest {
    /// Why (at most 255 characters).
    reason: Option<String>,
    /// Unix milliseconds in the future; absent = until lifted.
    until: Option<i64>,
}

/// One audit-log entry.
#[derive(Serialize, ToSchema)]
pub(crate) struct AuditEntry {
    /// Grows with time.
    id: i64,
    /// Who acted.
    actor: Option<i64>,
    /// What happened (`auth.login`, `admin.ban`, …).
    action: String,
    /// `user`, …
    target_type: Option<String>,
    /// The target's id.
    target_id: Option<String>,
    /// The client address.
    ip: Option<String>,
    /// The request id.
    request_id: Option<String>,
    /// Details.
    #[schema(value_type = Option<Object>)]
    data: Option<serde_json::Value>,
    /// Unix milliseconds.
    created_at: i64,
}

/// A page of audit entries.
#[derive(Serialize, ToSchema)]
pub(crate) struct AuditPage {
    /// The entries, newest first.
    items: Vec<AuditEntry>,
    /// Pass as `cursor` for the next page.
    next_cursor: Option<String>,
}

/// The `bearer` security scheme and the `auth` / `admin` tags.
pub(crate) fn document() -> OpenApi {
    let scheme = SecurityScheme::Http(
        HttpBuilder::new().scheme(HttpAuthScheme::Bearer).description(Some("An access token from login, registration or refresh")).build(),
    );
    OpenApiBuilder::new().components(Some(ComponentsBuilder::new().security_scheme("bearer", scheme).build())).build()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::admin as p_admin;
    use net_backend_protocol::auth as p;
    use net_backend_protocol::{Ack as PAck, UnixMillis, UserId};
    use serde_json::Value;
    use utoipa::openapi::schema::Schema;
    use utoipa::openapi::RefOr;
    use utoipa::PartialSchema;

    use super::*;

    fn properties<T: PartialSchema>() -> BTreeSet<String> {
        match T::schema() {
            RefOr::T(Schema::Object(object)) => object.properties.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    fn keys(value: impl serde::Serialize) -> BTreeSet<String> {
        match serde_json::to_value(value) {
            Ok(Value::Object(map)) => map.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    /// Every mirror has exactly the fields of the real type with all optional fields set.
    #[test]
    fn mirrors_match_the_protocol() {
        let t = UnixMillis(1);
        let account = p::Account::new(UserId(1), t).with_email("a@example.com", true).with_display_name("A");
        let pair = p::TokenPair::new(p::AccessToken::new("a"), t, p::RefreshToken::new("r"), t);
        assert_eq!(properties::<RegisterRequest>(), keys(p::RegisterRequest::new("a@b", "p").with_display_name("x")));
        assert_eq!(properties::<LoginRequest>(), keys(p::LoginRequest::new("a@b", "p")));
        assert_eq!(properties::<SteamLoginRequest>(), keys(p::SteamLoginRequest::new("00", "g")));
        assert_eq!(properties::<RefreshRequest>(), keys(p::RefreshRequest::new("r")));
        assert_eq!(properties::<LogoutRequest>(), keys(p::LogoutRequest::everywhere().with_refresh_token("r")));
        assert_eq!(properties::<TokenPair>(), keys(&pair));
        assert_eq!(properties::<Account>(), keys(&account));
        assert_eq!(properties::<AuthSession>(), keys(p::AuthSession::new(account.clone(), pair)));
        assert_eq!(properties::<LinkedIdentity>(), keys(p::LinkedIdentity::new("steam", "1")));
        assert_eq!(properties::<UpdateAccountRequest>(), keys(p::UpdateAccountRequest::new().with_display_name("x")));
        assert_eq!(properties::<ChangePasswordRequest>(), keys(p::ChangePasswordRequest::new("a", "b")));
        assert_eq!(properties::<VerifyEmailRequest>(), keys(p::VerifyEmailRequest::new("t")));
        assert_eq!(properties::<ForgotPasswordRequest>(), keys(p::ForgotPasswordRequest::new("a@b")));
        assert_eq!(properties::<ResetPasswordRequest>(), keys(p::ResetPasswordRequest::new("t", "p")));
        assert_eq!(properties::<Ack>(), keys(PAck::new()));
        let ban = p_admin::BanInfo::new(t).with_until(t).with_reason("r");
        assert_eq!(properties::<BanInfo>(), keys(&ban));
        assert_eq!(properties::<AdminUser>(), keys(p_admin::AdminUser::new(account).with_ban(ban).with_last_seen_at(t)));
        assert_eq!(properties::<BanRequest>(), keys(p_admin::BanRequest::new().with_reason("r").with_until(t)));
        let entry = p_admin::AuditEntry::new(1, "x", t)
            .with_actor(UserId(1))
            .with_target("user", "1")
            .with_ip("1.2.3.4")
            .with_request_id("r")
            .with_data(serde_json::json!({}));
        assert_eq!(properties::<AuditEntry>(), keys(entry));
        assert_eq!(properties::<AdminUserPage>(), keys(net_backend_protocol::Page::new(Vec::<u8>::new(), Some(net_backend_protocol::Cursor::new("c")))));
    }
}
