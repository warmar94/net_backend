//! `POST /v1/auth/oauth/{provider}`, exactly as the protocol defines it.

use axum::extract::State;
use net_backend_protocol::oauth::OAuthLogin;

use super::openapi as doc;
use super::service::OAuthService;
use crate::auth::{AuthSessionSchema, MaybeAuth, ReqInfo};
use crate::http::call::{Call, CallResult, Reply};
use crate::http::Ext;
use crate::openapi::ErrorBody;
use crate::state::AppState;

/// Log in with an OpenID Connect ID token of a provider (the first login creates the account;
/// with a Bearer token of a recent login the provider's account is linked to the caller's account
/// instead; a Bearer token that is sent but invalid or expired is refused, never ignored).
#[utoipa::path(post, path = "/v1/auth/oauth/{provider}", tag = "auth", operation_id = "oauth_login", request_body = doc::OAuthToken,
    params(("provider" = String, Path, description = "The server's provider name, e.g. `google`")),
    responses(
        (status = 200, description = "Logged in (or linked)", body = AuthSessionSchema),
        (status = 401, description = "`oauth_failed`: the token was refused (signature, issuer, audience, expiry, nonce, used before); or the Bearer token sent is `token_expired` / `unauthorized`", body = ErrorBody),
        (status = 403, description = "`banned`, `reauthentication_required` (linking needs a recent login), or refused by a hook", body = ErrorBody),
        (status = 404, description = "No such provider on this server", body = ErrorBody),
        (status = 409, description = "`conflict`: the provider account is linked to another account, or this account already has one of this provider", body = ErrorBody),
        (status = 422, description = "`validation_failed`: not a compact JWT, or a bad nonce", body = ErrorBody),
        (status = 429, description = "`rate_limited`", body = ErrorBody),
        (status = 503, description = "`unavailable`: the provider's keys could not be fetched", body = ErrorBody),
    ))]
pub(crate) async fn login(
    State(state): State<AppState>,
    Ext(service): Ext<OAuthService>,
    info: ReqInfo,
    MaybeAuth(current): MaybeAuth,
    Call(call): Call<OAuthLogin>,
) -> CallResult<OAuthLogin> {
    service.login(&state, &info, current, &call.provider, call.token).await.map(Reply::new)
}
