//! [`OAuth`]: the OpenID Connect login module (`.module(OAuth::new())`, after `Auth`).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use net_backend_protocol::oauth::OAuthLogin;
use net_backend_protocol::routes;
use utoipa_axum::router::OpenApiRouter;

use super::config::OAuthConfig;
use super::service::OAuthService;
use super::{migrations, routes as handlers};
use crate::call_route;
use crate::db::Dialect;
use crate::error::Error;
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::rate_limit::{MemoryRateLimiter, RateRule};
use crate::state::AppState;

/// The OpenID Connect login module: `POST /v1/auth/oauth/{provider}` checks a provider's ID token
/// (see [`crate::oauth`]) and logs in, links or creates the account through the accounts module.
/// Name `oauth`; settings in `[modules.oauth]` ([`OAuthConfig`]); needs the `auth` module
/// registered first. Without providers every login answers 404.
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::oauth::{OAuth, OAuthConfig, ProviderConfig};
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// let mut oauth = OAuthConfig::default();
/// oauth.providers.insert("google".into(), ProviderConfig::google(vec!["1234-abc.apps.googleusercontent.com".into()]));
/// NetBackendServer::new(Config::load()?).module(Auth::new()).module(OAuth::new().with_config(oauth)).run().await
/// # }
/// ```
#[derive(Debug, Default)]
pub struct OAuth {
    config: Option<OAuthConfig>,
    service: OnceLock<OAuthService>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl OAuth {
    /// The module with settings from `[modules.oauth]` (defaults, no providers, if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.oauth]` (which must then be absent).
    pub fn with_config(mut self, config: OAuthConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&OAuthService> {
        self.service.get()
    }
}

impl Module for OAuth {
    fn name(&self) -> &'static str {
        "oauth"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<OAuthConfig>("oauth")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec!["modules.oauth: settings given both in code (OAuth::with_config) and in [modules.oauth]; use one".into()]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => OAuthConfig::default(),
        };
        config.validate()?;
        let per_minute = config.login_per_minute;
        let service = OAuthService::new(config)?;
        self.service.set(service.clone()).map_err(|_| Error::Module("the oauth module was set up twice (build it once)".into()))?;
        setup.insert_state(service);
        if per_minute > 0 {
            let limiter = MemoryRateLimiter::new().rule(RateRule::per_ip(per_minute, Duration::from_secs(60)).routes([routes::auth::OAUTH]));
            setup.add_rate_limiter(Arc::new(limiter));
        }
        Ok(())
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new().routes(call_route!(OAuthLogin, handlers::login))
    }

    fn start<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let Some(service) = self.service.get().cloned() else { return Ok(()) };
            let every = service.config().purge_interval_secs;
            if every == 0 {
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
                            Ok(n) => tracing::info!(rows = n, "oauth: deleted used nonces of expired tokens"),
                            Err(error) => tracing::warn!(%error, "oauth: purging used nonces failed"),
                        },
                        _ = state.shutdown().wait() => break,
                    }
                }
            });
            self.tasks.lock().unwrap_or_else(|e| e.into_inner()).push(task);
            Ok(())
        })
    }

    fn shutdown<'a>(&'a self, _state: &'a AppState) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for task in std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner())) {
                task.abort();
            }
        })
    }
}
