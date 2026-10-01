//! The account routes (`/v1/auth/*`, `/v1/account*`), exactly as the protocol defines them.

use axum::extract::{FromRequestParts, Path, State};
use http::request::Parts;
use net_backend_protocol::auth::{
    Account, AuthSession, ChangePasswordRequest, ForgotPasswordRequest, LoginRequest, LogoutRequest, RefreshRequest, RegisterRequest, ResetPasswordRequest,
    SteamLoginRequest, TokenPair, UpdateAccountRequest, VerifyEmailRequest,
};
use net_backend_protocol::Ack;

use super::openapi as doc;
use super::service::{user_agent, AuthService, ReqInfo};
use super::{AuthContext, MaybeAuth};
use crate::error::AppError;
use crate::http::{ApiJson, ClientIp, Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;

impl<S: Send + Sync> FromRequestParts<S> for ReqInfo {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let ip = ClientIp::from_request_parts(parts, state).await?.ip();
        let request_id = parts.extensions.get::<RequestId>().cloned();
        Ok(ReqInfo { ip, request_id, user_agent: user_agent(&parts.headers) })
    }
}

/// Create an account with email and password; answers the account and a token pair.
#[utoipa::path(post, path = "/v1/auth/register", tag = "auth", operation_id = "register", request_body = doc::RegisterRequest,
    responses(
        (status = 200, description = "Registered and logged in", body = doc::AuthSession),
        (status = 403, description = "`forbidden`: registration is closed; or refused by a hook", body = ErrorBody),
        (status = 409, description = "`email_taken`", body = ErrorBody),
        (status = 422, description = "`validation_failed` (details per field)", body = ErrorBody),
        (status = 429, description = "`rate_limited`", body = ErrorBody),
    ))]
pub(crate) async fn register(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    ApiJson(body): ApiJson<RegisterRequest>,
) -> Result<ApiJson<AuthSession>, AppError> {
    service.register(&state, &info, body).await.map(ApiJson)
}

/// Log in with email and password.
#[utoipa::path(post, path = "/v1/auth/login", tag = "auth", operation_id = "login", request_body = doc::LoginRequest,
    responses(
        (status = 200, description = "Logged in", body = doc::AuthSession),
        (status = 401, description = "`invalid_credentials` (never says which part is wrong)", body = ErrorBody),
        (status = 403, description = "`banned` (details: `until`), `email_not_verified`, or refused by a hook", body = ErrorBody),
        (status = 429, description = "`rate_limited`: too many attempts from this address or for this account", body = ErrorBody),
        (status = 503, description = "`unavailable`: password hashing is saturated", body = ErrorBody),
    ))]
pub(crate) async fn login(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    ApiJson(body): ApiJson<LoginRequest>,
) -> Result<ApiJson<AuthSession>, AppError> {
    service.login(&state, &info, body).await.map(ApiJson)
}

/// Log in with a Steam Web API ticket (the first login creates the account; with a Bearer token
/// of a recent login the Steam account is linked to the caller's account instead; a Bearer token
/// that is sent but invalid or expired is refused, never ignored).
#[utoipa::path(post, path = "/v1/auth/steam", tag = "auth", operation_id = "steam_login", request_body = doc::SteamLoginRequest,
    responses(
        (status = 200, description = "Logged in", body = doc::AuthSession),
        (status = 401, description = "`steam_auth_failed`; or the Bearer token sent is `token_expired` / `unauthorized`", body = ErrorBody),
        (status = 403, description = "`banned`, a refused borrowed copy, or `reauthentication_required` (linking needs a recent login)", body = ErrorBody),
        (status = 404, description = "Steam login is not enabled on this server", body = ErrorBody),
        (status = 409, description = "`conflict`: the Steam account is linked to another account, or this account already has one", body = ErrorBody),
    ))]
pub(crate) async fn steam(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    MaybeAuth(current): MaybeAuth,
    ApiJson(body): ApiJson<SteamLoginRequest>,
) -> Result<ApiJson<AuthSession>, AppError> {
    service.steam_login(&state, &info, current, body).await.map(ApiJson)
}

/// Exchange a refresh token for a new pair (rotation; a retry within 30 s answers the same pair,
/// a later reuse revokes the session).
#[utoipa::path(post, path = "/v1/auth/refresh", tag = "auth", operation_id = "refresh", request_body = doc::RefreshRequest,
    responses(
        (status = 200, description = "A new token pair", body = doc::TokenPair),
        (status = 401, description = "`unauthorized` (invalid, expired or revoked) or `refresh_token_reused` (the session was revoked)", body = ErrorBody),
        (status = 403, description = "`banned`", body = ErrorBody),
    ))]
pub(crate) async fn refresh(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    ApiJson(body): ApiJson<RefreshRequest>,
) -> Result<ApiJson<TokenPair>, AppError> {
    service.refresh(&state, &info, body).await.map(ApiJson)
}

/// Revoke this session (or every session) with the access token or the refresh token.
#[utoipa::path(post, path = "/v1/auth/logout", tag = "auth", operation_id = "logout", request_body = doc::LogoutRequest, security((), ("bearer" = [])),
    responses(
        (status = 200, description = "Logged out", body = doc::Ack),
        (status = 401, description = "`unauthorized`: neither a valid access token nor a valid refresh token", body = ErrorBody),
    ))]
pub(crate) async fn logout(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    current: Option<AuthContext>,
    ApiJson(body): ApiJson<LogoutRequest>,
) -> Result<ApiJson<Ack>, AppError> {
    service.logout(&state, &info, current, body).await.map(|()| ApiJson(Ack::new()))
}

/// Confirm an email address with the token from the mail.
#[utoipa::path(post, path = "/v1/auth/email/verify", tag = "auth", operation_id = "verify_email", request_body = doc::VerifyEmailRequest,
    responses((status = 200, description = "Confirmed", body = doc::Ack), (status = 400, description = "`invalid_token`", body = ErrorBody)))]
pub(crate) async fn verify_email(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    ApiJson(body): ApiJson<VerifyEmailRequest>,
) -> Result<ApiJson<Ack>, AppError> {
    service.verify_email(&state, &info, body).await.map(|()| ApiJson(Ack::new()))
}

/// Send the verification mail again (nothing happens when the address is confirmed already).
#[utoipa::path(post, path = "/v1/auth/email/resend", tag = "auth", operation_id = "resend_verification", security(("bearer" = [])),
    responses((status = 200, description = "Queued (or nothing to do)", body = doc::Ack), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody), (status = 429, description = "`rate_limited`", body = ErrorBody)))]
pub(crate) async fn resend_verification(State(state): State<AppState>, Ext(service): Ext<AuthService>, current: AuthContext) -> Result<ApiJson<Ack>, AppError> {
    service.resend_verification(&state, &current).await.map(|()| ApiJson(Ack::new()))
}

/// Ask for a password-reset mail. Always answers the same, whether or not the address has an
/// account.
#[utoipa::path(post, path = "/v1/auth/password/forgot", tag = "auth", operation_id = "forgot_password", request_body = doc::ForgotPasswordRequest,
    responses((status = 200, description = "Accepted (a mail is sent if the address has an account)", body = doc::Ack), (status = 429, description = "`rate_limited`", body = ErrorBody)))]
pub(crate) async fn forgot_password(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    ApiJson(body): ApiJson<ForgotPasswordRequest>,
) -> ApiJson<Ack> {
    service.forgot_password(&state, &info, body);
    ApiJson(Ack::new())
}

/// Set a new password with the token from the reset mail; every session is revoked.
#[utoipa::path(post, path = "/v1/auth/password/reset", tag = "auth", operation_id = "reset_password", request_body = doc::ResetPasswordRequest,
    responses((status = 200, description = "Changed", body = doc::Ack), (status = 400, description = "`invalid_token`", body = ErrorBody), (status = 422, description = "`validation_failed`", body = ErrorBody)))]
pub(crate) async fn reset_password(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    ApiJson(body): ApiJson<ResetPasswordRequest>,
) -> Result<ApiJson<Ack>, AppError> {
    service.reset_password(&state, &info, body).await.map(|()| ApiJson(Ack::new()))
}

/// The caller's account.
#[utoipa::path(get, path = "/v1/account", tag = "account", operation_id = "account", security(("bearer" = [])),
    responses((status = 200, description = "The account", body = doc::Account), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn account(State(state): State<AppState>, Ext(service): Ext<AuthService>, current: AuthContext) -> Result<ApiJson<Account>, AppError> {
    service.account(&state, current.user_id).await.map(ApiJson)
}

/// Change the caller's account (absent fields stay).
#[utoipa::path(patch, path = "/v1/account", tag = "account", operation_id = "update_account", request_body = doc::UpdateAccountRequest, security(("bearer" = [])),
    responses((status = 200, description = "The changed account", body = doc::Account), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody), (status = 422, description = "`validation_failed`", body = ErrorBody)))]
pub(crate) async fn update_account(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    current: AuthContext,
    ApiJson(body): ApiJson<UpdateAccountRequest>,
) -> Result<ApiJson<Account>, AppError> {
    service.update_account(&state, &info, &current, body).await.map(ApiJson)
}

/// Change the password (knowing the current one); the other sessions are revoked.
#[utoipa::path(post, path = "/v1/account/password", tag = "account", operation_id = "change_password", request_body = doc::ChangePasswordRequest, security(("bearer" = [])),
    responses((status = 200, description = "Changed", body = doc::Ack), (status = 401, description = "`invalid_credentials` (the current password is wrong), `unauthorized` / `token_expired`", body = ErrorBody), (status = 422, description = "`validation_failed`", body = ErrorBody)))]
pub(crate) async fn change_password(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    current: AuthContext,
    ApiJson(body): ApiJson<ChangePasswordRequest>,
) -> Result<ApiJson<Ack>, AppError> {
    service.change_password(&state, &info, &current, body).await.map(|()| ApiJson(Ack::new()))
}

/// Unlink a login provider (`steam`) from the caller's account. Needs a recent login; refused when
/// it is the account's only way to log in.
#[utoipa::path(delete, path = "/v1/account/identities/{provider}", tag = "account", operation_id = "unlink_identity", security(("bearer" = [])),
    params(("provider" = String, Path, description = "The provider, e.g. `steam`")),
    responses(
        (status = 200, description = "Unlinked", body = doc::Ack),
        (status = 403, description = "`reauthentication_required`: log in again first", body = ErrorBody),
        (status = 404, description = "`not_found`: no such linked provider", body = ErrorBody),
        (status = 409, description = "`conflict`: the account's only way to log in", body = ErrorBody),
    ))]
pub(crate) async fn unlink_identity(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    info: ReqInfo,
    current: AuthContext,
    Path(provider): Path<String>,
) -> Result<ApiJson<Ack>, AppError> {
    service.unlink_own_identity(&state, &info, &current, &provider).await.map(|()| ApiJson(Ack::new()))
}
