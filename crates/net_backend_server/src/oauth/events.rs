//! What games can hook into in the OpenID Connect module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeOAuthLogin`] | before | an ID token passed every check; the login, link or account creation is about to happen. Refuse with any error (e.g. only addresses of one domain) |
//!
//! The accounts module's hooks run as well: `BeforeRegister` / `AfterRegister` for a new account
//! (with the provider account as `identity`), `BeforeLogin` (method `OpenId`, with `identity`) and
//! `AfterLogin`.
//!
//! ```
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::oauth::events::BeforeOAuthLogin;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeOAuthLogin, _, _>(|_ctx, login| async move {
//!     let staff = login.email_verified && login.email.as_deref().is_some_and(|e| e.ends_with("@studio.example"));
//!     if login.provider == "company" && !staff {
//!         return Ok(Decision::Reject(AppError::forbidden("staff accounts only")));
//!     }
//!     Ok(Decision::Continue(login))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::UserId;

use crate::hooks::Event;

/// A verified ID token is about to log in, link or create an account. Refuse with any error (the
/// client gets it; the token's nonce stays used); changed fields are ignored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeOAuthLogin {
    /// The server's provider name (`google`).
    pub provider: String,
    /// The provider's account id (`sub`).
    pub subject: String,
    /// The token's issuer (`iss`).
    pub issuer: String,
    /// The token's `email`, if any (as the provider wrote it; not stored by the module).
    pub email: Option<String>,
    /// The token's `email_verified`.
    pub email_verified: bool,
    /// The token's `name`, if any (not stored by the module).
    pub name: Option<String>,
    /// The caller's account when the request carries a Bearer token (a link), else `None`.
    pub linking_to: Option<UserId>,
}

impl Event for BeforeOAuthLogin {
    const NAME: &'static str = "oauth.before_login";
}
