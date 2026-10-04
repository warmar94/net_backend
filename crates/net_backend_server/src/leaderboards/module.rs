//! [`Leaderboards`]: the leaderboards module (`.module(Leaderboards::new())`, after `Auth`).

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use net_backend_protocol::leaderboards::{GetAroundMe, GetLeaderboard, GetMyRank, ListBoards, PostScore};
use utoipa_axum::router::OpenApiRouter;

use super::config::LeaderboardsConfig;
use super::service::LeaderboardService;
use super::{migrations, routes as handlers};
use crate::call_route;
use crate::db::Dialect;
use crate::error::Error;
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::state::AppState;

/// The leaderboards module: boards with a score mode (best / latest / sum), an order and a period
/// (all-time / daily / weekly), score submission with hooks, the top, the caller's rank and the
/// ranks around it (see [`crate::leaderboards`]). Name `leaderboards`; settings in
/// `[modules.leaderboards]` ([`LeaderboardsConfig`]); needs the `auth` module registered first.
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::leaderboards::{BoardSpec, Leaderboards, LeaderboardsConfig};
/// use net_backend_server::protocol::leaderboards::{Period, ScoreOrder};
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// let mut boards = LeaderboardsConfig::default();
/// boards.boards = vec![
///     BoardSpec::new("highscore").with_name("High score"),
///     BoardSpec::new("weekly-race").with_order(ScoreOrder::Asc).with_period(Period::Weekly),
/// ];
/// NetBackendServer::new(Config::load()?).module(Auth::new()).module(Leaderboards::new().with_config(boards)).run().await
/// # }
/// ```
#[derive(Debug, Default)]
pub struct Leaderboards {
    config: Option<LeaderboardsConfig>,
    service: OnceLock<LeaderboardService>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Leaderboards {
    /// The module with settings from `[modules.leaderboards]` (defaults if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.leaderboards]` (which must then be absent).
    pub fn with_config(mut self, config: LeaderboardsConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&LeaderboardService> {
        self.service.get()
    }
}

impl Module for Leaderboards {
    fn name(&self) -> &'static str {
        "leaderboards"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<LeaderboardsConfig>("leaderboards")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec![
                    "modules.leaderboards: settings given both in code (Leaderboards::with_config) and in [modules.leaderboards]; use one".into(),
                ]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => LeaderboardsConfig::default(),
        };
        config.validate()?;
        let service = LeaderboardService::new(config);
        self.service.set(service.clone()).map_err(|_| Error::Module("the leaderboards module was set up twice (build it once)".into()))?;
        setup.insert_state(service);
        Ok(())
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new()
            .routes(call_route!(ListBoards, handlers::boards))
            .routes(call_route!(GetLeaderboard, handlers::top))
            .routes(call_route!(PostScore, handlers::submit))
            .routes(call_route!(GetMyRank, handlers::me))
            .routes(call_route!(GetAroundMe, handlers::around))
    }

    fn start<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let Some(service) = self.service.get().cloned() else { return Ok(()) };
            let every = service.config().purge_interval_secs;
            if every == 0 || service.config().keep_periods == 0 {
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
                            Ok(n) => tracing::info!(scores = n, "leaderboards: deleted scores of old periods"),
                            Err(error) => tracing::warn!(%error, "leaderboards: the purge of old periods failed"),
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
