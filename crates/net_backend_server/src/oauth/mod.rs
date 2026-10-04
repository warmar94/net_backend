//! OpenID Connect logins: a provider's ID token (Google, or any OpenID Connect provider) logs a
//! player in, links the provider account, or creates an account (cargo feature `oauth`, module
//! [`OAuth`]).
//!
//! - **Route:** `POST /v1/auth/oauth/{provider}` with `{"id_token": "…", "nonce": "…"}` answers an
//!   `AuthSession`, like `/v1/auth/login`. The provider account (the token's `sub`) is a linked
//!   identity of the account (`provider` = the configured name): the first login creates the
//!   account (no email, no password; `BeforeRegister` may set a display name). With the Bearer
//!   token of a recent login (`modules.auth.link_reauth_secs`, 10 minutes) the route links the
//!   provider account to the caller's account instead; a provider account linked to another
//!   account answers 409 (accounts are never merged), and an account has at most one account per
//!   provider. Unlinking is the accounts module's `DELETE /v1/account/identities/{provider}` (it
//!   refuses to remove the last way to log in).
//! - **Checks** (in this order; any failure answers 401 `oauth_failed`, the reason is logged):
//!   the compact JWS form; `alg` one of the provider's algorithms (RS256 / ES256 only: never
//!   `none`, never HMAC; a `crit` header is refused); the key from the provider's JWKS by `kid`,
//!   of the algorithm's key type; the signature (ring); `iss` one of the accepted issuers; `aud`
//!   containing one of the game's client ids (and `azp`, when present or when there are several
//!   audiences, one of them); `exp` in the future, `iat` not in the future and not older than
//!   `max_token_age_secs`, `nbf` passed (each with `clock_skew_secs`); a `sub`; the nonce equal to
//!   the one the login sends (required by default). Then the nonce (or, without one, the token) is
//!   recorded per provider until the token expires: a second login with it is refused, on every
//!   instance (the database's unique index).
//! - **Keys:** fetched over HTTPS (hyper + rustls with ring, webpki roots) from the provider's
//!   `jwks_uri` or its discovery document, kept for the answer's `max-age`, fetched again for an
//!   unknown key id (rotation, at most once per `jwks_refetch_secs`); when a fetch fails the last
//!   keys stay usable for `jwks_stale_secs`; without usable keys a login answers 503
//!   `unavailable`.
//! - **Google** is a preset (`preset = "google"`): issuer `https://accounts.google.com` (and
//!   `accounts.google.com`), keys from `https://www.googleapis.com/oauth2/v3/certs`, RS256.
//! - **Hooks** ([`events`]): [`BeforeOAuthLogin`](events::BeforeOAuthLogin) sees the verified
//!   claims (email, name) and may refuse; the accounts module's hooks run as for any login.
//! - **Limits:** `login_per_minute` per client address (10).
//!
//! The module needs [`Auth`](crate::auth::Auth) registered before it.

// Without any database backend `Db` has no variants: code after a query is unreachable.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

pub mod config;
pub mod events;
mod jwks;
mod jwt;
mod migrations;
mod module;
mod openapi;
mod routes;
mod service;
mod store;

pub use config::{OAuthConfig, ProviderConfig};
pub use module::OAuth;
pub use service::OAuthService;
