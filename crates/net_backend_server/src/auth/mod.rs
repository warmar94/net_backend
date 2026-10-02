//! Authentication: the seam every request passes through ([`Authenticator`], [`AuthContext`],
//! [`RequireRole`]) and the built-in accounts module [`Auth`].
//!
//! **The seam.** Authenticators (the app's, then the modules') run for every routed request:
//! `Ok(Some(ctx))` attaches an [`AuthContext`], `Ok(None)` leaves the request anonymous (the next
//! authenticator may know it), `Err(e)` stops the chain and remembers `e`. Handlers that need a
//! user take [`AuthContext`]: without one they answer the remembered error (e.g. 401
//! `token_expired`) or 401 `unauthorized`. Handlers for both choose:
//!
//! - `Option<AuthContext>`: **bad credentials count as anonymous** (`None`), so a client that always
//!   sends its last token can still log in, refresh or log out;
//! - [`MaybeAuth`]: `None` only when no credential was sent; a credential that was sent but is
//!   invalid, expired or revoked answers its error (use it where acting anonymously by mistake
//!   would be harmful, e.g. linking a login provider).
//!
//! **The module.** [`Auth`] (`.module(Auth::new())`, name `auth`) provides accounts (email +
//! password with argon2id, Steam tickets), opaque access and rotating refresh tokens stored only
//! as SHA-256 hashes, sessions and revocation, email verification and password reset through a
//! [`Mailer`](crate::mail::Mailer), roles, an audit log, `/v1/admin` routes, rate limits, hooks
//! ([`events`]) and the `user:*` commands. Its service, [`AuthService`], is a state value
//! (`Ext<AuthService>` in handlers, `state.get::<AuthService>()` elsewhere): the WebSocket hub uses
//! [`AuthService::authenticate_token`] and [`AuthService::subscribe_revocations`].

// Without any database backend `Db` has no variants: code after a query is unreachable.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

mod admin_routes;
pub mod audit;
mod commands;
pub mod config;
pub mod events;
mod migrations;
mod module;
mod openapi;
mod password;
mod routes;
mod service;
pub mod steam;
mod store;
mod tokens;

use std::marker::PhantomData;

use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use futures_util::future::BoxFuture;
use http::request::Parts;
use http::StatusCode;
use net_backend_protocol::{codes, ApiError, UnixMillis, UserId};

use crate::error::AppError;
use crate::state::AppState;

pub use config::{AuthConfig, MailerKind, SmtpTls};
pub use events::{Revocation, RevocationReason, RevokedSessions};
pub use module::Auth;
pub use service::normalize_email;
pub use service::AuthService;

/// Who is calling: attached to a request by an [`Authenticator`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthContext {
    /// The authenticated user.
    pub user_id: UserId,
    /// The session (token family) the request's token belongs to; `None` for authenticators
    /// without sessions (e.g. a game's API key).
    pub session_id: Option<i64>,
    /// The user's roles (`admin`, a game's own); empty for a normal player.
    pub roles: Vec<String>,
    /// When the session was started (the login); for "log in again first" checks.
    pub session_started_at: Option<UnixMillis>,
}

impl AuthContext {
    /// A context for this user, without a session or roles.
    pub fn new(user_id: UserId) -> Self {
        Self { user_id, session_id: None, roles: Vec::new(), session_started_at: None }
    }

    /// The same context with the session's start (login) time.
    pub fn with_session_started_at(mut self, at: UnixMillis) -> Self {
        self.session_started_at = Some(at);
        self
    }

    /// Whether the session was started (logged in) at most `max_age_ms` before `now`.
    pub fn is_recent_login(&self, now: UnixMillis, max_age_ms: i64) -> bool {
        self.session_started_at.is_some_and(|at| now.get().saturating_sub(at.get()) <= max_age_ms)
    }

    /// The same context with a session id.
    pub fn with_session(mut self, session_id: i64) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// The same context with these roles.
    pub fn with_roles(mut self, roles: Vec<String>) -> Self {
        self.roles = roles;
        self
    }

    /// Whether the user has this role.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    /// `Ok` if the user has this role, else 403 `forbidden` (for roles chosen at run time; the
    /// [`RequireRole`] extractor covers fixed ones).
    pub fn require_role(&self, role: &str) -> Result<(), AppError> {
        if self.has_role(role) {
            Ok(())
        } else {
            Err(AppError::forbidden("this needs another role"))
        }
    }
}

/// Why authentication failed, remembered for handlers that need a user.
#[derive(Clone, Debug)]
pub(crate) struct AuthFailure {
    status: StatusCode,
    error: ApiError,
}

impl AuthFailure {
    pub(crate) fn from_error(error: AppError) -> Self {
        // An internal failure (the database is down) is logged once here; the answer stays generic.
        if error.status().is_server_error() {
            tracing::error!(error = %error, "authentication failed");
        }
        Self { status: error.status(), error: error.api_error().clone() }
    }

    pub(crate) fn to_error(&self) -> AppError {
        AppError::from_parts(self.status, self.error.clone())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for AuthContext {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(context) = parts.extensions.get::<AuthContext>() {
            return Ok(context.clone());
        }
        Err(parts.extensions.get::<AuthFailure>().map_or_else(AppError::unauthorized, AuthFailure::to_error))
    }
}

impl<S: Send + Sync> OptionalFromRequestParts<S> for AuthContext {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<AuthContext>().cloned())
    }
}

/// An extractor: the caller's [`AuthContext`] if a valid credential was sent, `None` if none was
/// sent, and the authenticator's error (401 `token_expired` / `unauthorized`, 403 `banned`) if one
/// was sent but refused. Unlike `Option<AuthContext>`, a stale token never turns into an anonymous
/// request.
#[derive(Clone, Debug)]
pub struct MaybeAuth(pub Option<AuthContext>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(context) = parts.extensions.get::<AuthContext>() {
            return Ok(MaybeAuth(Some(context.clone())));
        }
        match parts.extensions.get::<AuthFailure>() {
            Some(failure) => Err(failure.to_error()),
            None => Ok(MaybeAuth(None)),
        }
    }
}

/// Decides who is calling, from the request head (headers, URI). Never reads the body.
pub trait Authenticator: Send + Sync + 'static {
    /// `Ok(Some(..))`: authenticated; `Ok(None)`: not this authenticator's credentials (anonymous
    /// unless a later one knows them); `Err(..)`: invalid credentials of this authenticator (the
    /// request is anonymous, and handlers needing a user answer this error).
    fn authenticate<'a>(&'a self, parts: &'a Parts, state: &'a AppState) -> BoxFuture<'a, Result<Option<AuthContext>, AppError>>;
}

/// A role name for [`RequireRole`].
pub trait Role: Send + Sync + 'static {
    /// The role (`admin`).
    const NAME: &'static str;
}

/// The `admin` role.
#[derive(Clone, Copy, Debug)]
pub struct AdminRole;

impl Role for AdminRole {
    const NAME: &'static str = net_backend_protocol::admin::ADMIN_ROLE;
}

/// An extractor: the caller's [`AuthContext`], only if the user has role `R` (401 without a
/// user, 403 `forbidden` without the role).
///
/// ```
/// use net_backend_server::auth::{RequireAdmin, Role, RequireRole};
///
/// async fn wipe_world(admin: RequireAdmin) -> String { format!("wiped by {}", admin.0.user_id) }
///
/// struct Moderator;
/// impl Role for Moderator { const NAME: &'static str = "moderator"; }
/// async fn mute(RequireRole(ctx, ..): RequireRole<Moderator>) -> &'static str { "muted" }
/// ```
pub struct RequireRole<R: Role>(pub AuthContext, pub PhantomData<R>);

impl<R: Role> std::fmt::Debug for RequireRole<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RequireRole").field(&R::NAME).field(&self.0).finish()
    }
}

/// [`RequireRole`] for the `admin` role.
pub type RequireAdmin = RequireRole<AdminRole>;

impl<S: Send + Sync, R: Role> FromRequestParts<S> for RequireRole<R> {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let context = <AuthContext as FromRequestParts<S>>::from_request_parts(parts, state).await?;
        context.require_role(R::NAME)?;
        Ok(RequireRole(context, PhantomData))
    }
}

/// The error for an invalid or revoked access token.
pub(crate) fn invalid_token() -> AppError {
    AppError::new(codes::UNAUTHORIZED, "the access token is invalid or revoked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles() {
        let ctx = AuthContext::new(UserId(1)).with_roles(vec!["admin".into()]).with_session(3);
        assert!(ctx.has_role("admin") && !ctx.has_role("moderator"));
        assert!(ctx.require_role("admin").is_ok());
        assert_eq!(ctx.require_role("moderator").err().map(|e| e.status()), Some(StatusCode::FORBIDDEN));
        assert_eq!(ctx.session_id, Some(3));
        let ctx = ctx.with_session_started_at(UnixMillis(1_000));
        assert!(ctx.is_recent_login(UnixMillis(1_500), 500) && !ctx.is_recent_login(UnixMillis(1_501), 500));
        assert!(!AuthContext::new(UserId(1)).is_recent_login(UnixMillis(0), i64::MAX));
    }
}
