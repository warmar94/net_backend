//! [`Auth`]: the accounts module (`.module(Auth::new())`).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use http::request::Parts;
use net_backend_protocol::admin::{AuditQuery, BanUser, GetUser, GrantRole, RevokeRole, RevokeSessions, UnbanUser, UnlinkUserIdentity, UserListQuery};
use net_backend_protocol::auth::{
    ChangePasswordRequest, ForgotPasswordRequest, GetAccount, LoginRequest, LogoutRequest, RefreshRequest, RegisterRequest, ResendVerification,
    ResetPasswordRequest, SteamLoginRequest, UnlinkIdentity, UpdateAccountRequest, VerifyEmailRequest,
};
use net_backend_protocol::routes;
use utoipa_axum::router::OpenApiRouter;

use super::config::{AuthConfig, MailerKind};
use super::password::Hasher;
use super::service::AuthService;
use super::steam::SteamVerifier;
use super::tokens::ACCESS_PREFIX;
use super::{admin_routes, migrations, routes as handlers, AuthContext, Authenticator};
use crate::call_route;
use crate::command::AppCommand;
use crate::db::Dialect;
use crate::error::{AppError, Error};
use crate::http::call::undocumented;
use crate::mail::{LogMailer, MailQueue, Mailer};
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::rate_limit::{MemoryRateLimiter, RateRule};
use crate::state::AppState;

/// The accounts module: email + password and Steam logins, access and rotating refresh tokens,
/// sessions, email verification and password reset, roles, the audit log, `/v1/admin`, rate
/// limits, hooks and the `user:*` commands. Name `auth`; settings in `[modules.auth]`
/// ([`AuthConfig`]).
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// NetBackendServer::new(Config::load()?).module(Auth::new()).run().await
/// # }
/// ```
pub struct Auth {
    config: Option<AuthConfig>,
    mailer: Option<Arc<dyn Mailer>>,
    steam: Option<Arc<dyn SteamVerifier>>,
    service: OnceLock<AuthService>,
    purge_task: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("config", &self.config)
            .field("mailer", &self.mailer.is_some())
            .field("steam", &self.steam.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for Auth {
    fn default() -> Self {
        Self::new()
    }
}

impl Auth {
    /// The module with settings from `[modules.auth]` (defaults if absent).
    pub fn new() -> Self {
        Self { config: None, mailer: None, steam: None, service: OnceLock::new(), purge_task: Mutex::new(Vec::new()) }
    }

    /// Use these settings instead of `[modules.auth]` (which must then be absent).
    pub fn with_config(mut self, config: AuthConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Send mail with this mailer (instead of the one `mailer` in the settings names).
    pub fn mailer(mut self, mailer: impl Mailer) -> Self {
        self.mailer = Some(Arc::new(mailer));
        self
    }

    /// Check Steam tickets with this verifier (instead of the built-in Web API verifier, which
    /// needs the `steam` feature and `steam_app_id` + `steam_web_api_key`).
    pub fn steam_verifier(mut self, verifier: impl SteamVerifier) -> Self {
        self.steam = Some(Arc::new(verifier));
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&AuthService> {
        self.service.get()
    }

    fn settings(&self, setup: &Setup<'_>) -> Result<AuthConfig, Error> {
        let from_file = setup.config().module_config::<AuthConfig>("auth")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec!["modules.auth: settings given both in code (Auth::with_config) and in [modules.auth]; use one".into()]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => AuthConfig::default(),
        };
        let config = config.resolve()?;
        if config.revocation_poll_secs == 0 && setup.config().ws.enabled {
            // Without the poll, a ban or revocation made by another process (the command line,
            // another instance) would never close this server's open sockets.
            return Err(Error::Config(vec![
                "modules.auth.revocation_poll_secs must be at least 1 while ws.enabled = true (revocations made by the command line or another instance close open WebSockets through it)".into(),
            ]));
        }
        Ok(config)
    }

    fn mailer_for(&self, config: &AuthConfig) -> Result<Arc<dyn Mailer>, Error> {
        if let Some(mailer) = &self.mailer {
            return Ok(mailer.clone());
        }
        match config.mailer {
            MailerKind::Smtp => smtp_mailer(config),
            _ => Ok(Arc::new(LogMailer::new(config.log_mailer_show_links))),
        }
    }

    fn steam_for(&self, config: &AuthConfig) -> Result<Option<Arc<dyn SteamVerifier>>, Error> {
        if let Some(steam) = &self.steam {
            return Ok(Some(steam.clone()));
        }
        match (config.steam_app_id, &config.steam_web_api_key) {
            (Some(app_id), Some(key)) => web_api_verifier(config, app_id, key.clone()),
            _ => Ok(None),
        }
    }

    fn limiter(config: &AuthConfig) -> MemoryRateLimiter {
        let minute = Duration::from_secs(60);
        MemoryRateLimiter::new()
            .ipv6_prefix(config.rate_limit_ipv6_prefix)
            .rule(RateRule::per_ip(config.login_per_minute, minute).routes([routes::auth::LOGIN, routes::auth::STEAM]))
            .rule(RateRule::per_ip(config.register_per_hour, Duration::from_secs(3600)).routes([routes::auth::REGISTER]))
            .rule(RateRule::per_ip(config.refresh_per_minute, minute).routes([routes::auth::REFRESH]))
            .rule(RateRule::per_ip(config.email_routes_per_minute, minute).routes([
                routes::auth::FORGOT_PASSWORD,
                routes::auth::RESET_PASSWORD,
                routes::auth::VERIFY_EMAIL,
                routes::auth::RESEND_VERIFICATION,
                routes::account::PASSWORD,
            ]))
    }
}

#[cfg(feature = "smtp")]
fn smtp_mailer(config: &AuthConfig) -> Result<Arc<dyn Mailer>, Error> {
    let host = config.smtp_host.clone().unwrap_or_default();
    let from = config.mail_from.clone().unwrap_or_default();
    let credentials = match (&config.smtp_username, &config.smtp_password) {
        (Some(user), Some(password)) => Some((user.clone(), password.clone())),
        _ => None,
    };
    let mailer = crate::mail::SmtpMailer::new(&host, config.smtp_port, config.smtp_tls, credentials, &from, Duration::from_secs(config.smtp_timeout_secs))?;
    Ok(Arc::new(mailer))
}

#[cfg(not(feature = "smtp"))]
fn smtp_mailer(_config: &AuthConfig) -> Result<Arc<dyn Mailer>, Error> {
    Err(Error::Config(vec!["modules.auth.mailer = \"smtp\" needs the `smtp` feature of net_backend_server".into()]))
}

#[cfg(feature = "steam")]
fn web_api_verifier(config: &AuthConfig, app_id: u32, key: crate::config::SecretString) -> Result<Option<Arc<dyn SteamVerifier>>, Error> {
    let verifier = super::steam::SteamWebApiVerifier::new(config.steam_api_url.clone(), key, app_id, Duration::from_secs(config.steam_timeout_secs))?;
    Ok(Some(Arc::new(verifier)))
}

#[cfg(not(feature = "steam"))]
fn web_api_verifier(_config: &AuthConfig, _app_id: u32, _key: crate::config::SecretString) -> Result<Option<Arc<dyn SteamVerifier>>, Error> {
    Err(Error::Config(vec!["modules.auth.steam_app_id needs the `steam` feature of net_backend_server (or give a verifier with Auth::steam_verifier)".into()]))
}

/// Reads `Authorization: Bearer nbsa_…` (other schemes and token kinds are left to other
/// authenticators).
struct BearerAuthenticator {
    service: AuthService,
}

/// The token of an `Authorization: Bearer <token>` header (scheme case-insensitive).
pub(crate) fn bearer(parts: &Parts) -> Option<&str> {
    let value = parts.headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

impl Authenticator for BearerAuthenticator {
    fn authenticate<'a>(&'a self, parts: &'a Parts, state: &'a AppState) -> BoxFuture<'a, Result<Option<AuthContext>, AppError>> {
        Box::pin(async move {
            match bearer(parts) {
                Some(token) if token.starts_with(ACCESS_PREFIX) => self.service.authenticate_token(state, token).await.map(Some),
                _ => Ok(None),
            }
        })
    }
}

impl Module for Auth {
    fn name(&self) -> &'static str {
        "auth"
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let config = self.settings(setup)?;
        let mailer = self.mailer_for(&config)?;
        let steam = self.steam_for(&config)?;
        if steam.is_some() && config.steam_identity.as_deref().is_none_or(|i| i.trim().is_empty()) {
            // The configured identity is what Steam checks the ticket against: without it a ticket
            // issued to another service could be replayed here.
            return Err(Error::Config(vec!["modules.auth.steam_identity is required when a Steam verifier is set".into()]));
        }
        let hasher = Hasher::new(&config)?;
        let queue = MailQueue::new(mailer, config.mail_queue, config.mail_concurrency, Duration::from_secs(config.smtp_timeout_secs.saturating_add(5)));
        let limiter = config.rate_limits.then(|| Self::limiter(&config));
        let service = AuthService::new(config, hasher, queue, steam);
        self.service.set(service.clone()).map_err(|_| Error::Module("the auth module was set up twice (build it once)".into()))?;
        setup.insert_state(service.clone());
        setup.add_authenticator(Arc::new(BearerAuthenticator { service }));
        if let Some(limiter) = limiter {
            setup.add_rate_limiter(Arc::new(limiter));
        }
        Ok(())
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        // Every route is mounted from its protocol call: path and method cannot drift.
        let mut router = OpenApiRouter::new()
            .routes(call_route!(RegisterRequest, handlers::register))
            .routes(call_route!(LoginRequest, handlers::login))
            .routes(call_route!(SteamLoginRequest, handlers::steam))
            .routes(call_route!(RefreshRequest, handlers::refresh))
            .routes(call_route!(LogoutRequest, handlers::logout))
            .routes(call_route!(VerifyEmailRequest, handlers::verify_email))
            .routes(call_route!(ResendVerification, handlers::resend_verification))
            .routes(call_route!(ForgotPasswordRequest, handlers::forgot_password))
            .routes(call_route!(ResetPasswordRequest, handlers::reset_password))
            .routes(call_route!(GetAccount, handlers::account))
            .routes(call_route!(UpdateAccountRequest, handlers::update_account))
            .routes(call_route!(ChangePasswordRequest, handlers::change_password))
            .routes(call_route!(UnlinkIdentity, handlers::unlink_identity));
        let documented = self.service.get().is_some_and(|s| s.config().admin_in_openapi);
        if documented {
            router = router
                .routes(call_route!(UserListQuery, admin_routes::list_users))
                .routes(call_route!(GetUser, admin_routes::get_user))
                .routes(call_route!(BanUser, admin_routes::ban))
                .routes(call_route!(UnbanUser, admin_routes::unban))
                .routes(call_route!(RevokeSessions, admin_routes::revoke_sessions))
                .routes(call_route!(UnlinkUserIdentity, admin_routes::unlink_identity))
                .routes(call_route!(GrantRole, admin_routes::grant_role))
                .routes(call_route!(RevokeRole, admin_routes::revoke_role))
                .routes(call_route!(AuditQuery, admin_routes::audit));
        } else {
            router = undocumented::<UserListQuery, _, _, _>(router, admin_routes::list_users);
            router = undocumented::<GetUser, _, _, _>(router, admin_routes::get_user);
            router = undocumented::<BanUser, _, _, _>(router, admin_routes::ban);
            router = undocumented::<UnbanUser, _, _, _>(router, admin_routes::unban);
            router = undocumented::<RevokeSessions, _, _, _>(router, admin_routes::revoke_sessions);
            router = undocumented::<UnlinkUserIdentity, _, _, _>(router, admin_routes::unlink_identity);
            router = undocumented::<GrantRole, _, _, _>(router, admin_routes::grant_role);
            router = undocumented::<RevokeRole, _, _, _>(router, admin_routes::revoke_role);
            router = undocumented::<AuditQuery, _, _, _>(router, admin_routes::audit);
        }
        router
    }

    fn openapi(&self) -> Option<utoipa::openapi::OpenApi> {
        Some(super::openapi::document())
    }

    fn commands(&self) -> Vec<Arc<dyn AppCommand>> {
        super::commands::all()
    }

    fn start<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let Some(service) = self.service.get().cloned() else { return Ok(()) };
            // The dummy hash now, not at the first unknown-address login (timing).
            service.warm_up().await.map_err(|e| Error::Startup(format!("auth: {e}")))?;
            let mut tasks = Vec::new();
            // Revocations of other processes reach this process's subscribers; rotation nonces are
            // forgotten once their grace window ended.
            let poll = service.config().revocation_poll_secs;
            {
                let (service, state) = (service.clone(), state.clone());
                tasks.push(tokio::spawn(async move {
                    let mut cursor = super::service::PollCursor::starting_at(state.now().get());
                    let mut tick = tokio::time::interval(Duration::from_secs(if poll == 0 { 30 } else { poll }));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tokio::select! {
                            _ = tick.tick() => if let Err(error) = service.poll_revocations(&state, &mut cursor, poll > 0).await {
                                tracing::warn!(%error, "auth: the revocation poll failed");
                            },
                            _ = state.shutdown().wait() => break,
                        }
                    }
                }));
            }
            let every = service.config().purge_interval_secs;
            if every == 0 {
                *self.purge_task.lock().unwrap_or_else(|e| e.into_inner()) = tasks;
                return Ok(());
            }
            let state = state.clone();
            let task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(every));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = tick.tick() => match service.purge(&state).await {
                            Ok(0) => {}
                            Ok(n) => tracing::info!(rows = n, "auth: deleted expired tokens and old sessions"),
                            Err(error) => tracing::warn!(%error, "auth: purging expired tokens failed"),
                        },
                        _ = state.shutdown().wait() => break,
                    }
                }
            });
            tasks.push(task);
            *self.purge_task.lock().unwrap_or_else(|e| e.into_inner()) = tasks;
            Ok(())
        })
    }

    fn shutdown<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let tasks = std::mem::take(&mut *self.purge_task.lock().unwrap_or_else(|e| e.into_inner()));
            for task in tasks {
                task.abort();
            }
            if let Some(service) = self.service.get() {
                let limit = Duration::from_secs(state.config().server.module_shutdown_timeout_secs.saturating_sub(1).max(1));
                service.mail_queue().drain(limit).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_header() {
        let parts = |value: &str| http::Request::get("/").header("authorization", value).body(()).map(|r| r.into_parts().0);
        assert_eq!(parts("Bearer nbsa_x").ok().as_ref().and_then(bearer), Some("nbsa_x"));
        assert_eq!(parts("bearer  nbsa_y ").ok().as_ref().and_then(bearer), Some("nbsa_y"));
        assert_eq!(parts("Basic abc").ok().as_ref().and_then(bearer), None);
        assert_eq!(parts("Bearer").ok().as_ref().and_then(bearer), None);
    }
}
