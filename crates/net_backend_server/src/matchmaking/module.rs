//! [`Matchmaking`]: the matchmaking module (`.module(Matchmaking::new())`, after `Auth`).

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use net_backend_protocol::matchmaking::{CancelTicket, CreateTicket, GetTicket, ListQueues, MatchFound, TicketExpired};
use utoipa_axum::router::OpenApiRouter;

use super::config::MatchmakingConfig;
use super::openapi as doc;
use super::routes;
use super::service::MatchmakingService;
use crate::call_route;
use crate::error::Error;
use crate::hooks::Hooks;
use crate::module::{Module, Setup};
use crate::state::AppState;
use crate::ws::events::AfterWsDisconnect;
use crate::ws::WsHandlers;

/// The matchmaking module: queues, one ticket per player, matching rounds by the game's rules
/// (hooks), the `match.found` / `match.expired` pushes (see [`crate::matchmaking`]). Name
/// `matchmaking`; settings in `[modules.matchmaking]` ([`MatchmakingConfig`]); needs the `auth`
/// module registered first. No tables: tickets live in the instance's memory.
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::matchmaking::{Matchmaking, MatchmakingConfig, QueueSpec};
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// let queues = MatchmakingConfig::default().with_queue(QueueSpec::new("duel", 2));
/// NetBackendServer::new(Config::load()?).module(Auth::new()).module(Matchmaking::new().with_config(queues)).run().await
/// # }
/// ```
#[derive(Debug, Default)]
pub struct Matchmaking {
    config: Option<MatchmakingConfig>,
    service: OnceLock<MatchmakingService>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Matchmaking {
    /// The module with settings from `[modules.matchmaking]` (defaults, without queues, if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.matchmaking]` (which must then be absent).
    pub fn with_config(mut self, config: MatchmakingConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&MatchmakingService> {
        self.service.get()
    }
}

impl Module for Matchmaking {
    fn name(&self) -> &'static str {
        "matchmaking"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<MatchmakingConfig>("matchmaking")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec![
                    "modules.matchmaking: settings given both in code (Matchmaking::with_config) and in [modules.matchmaking]; use one".into(),
                ]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => MatchmakingConfig::default(),
        };
        config.validate()?;
        let service = MatchmakingService::new(config);
        self.service.set(service.clone()).map_err(|_| Error::Module("the matchmaking module was set up twice (build it once)".into()))?;
        setup.insert_state(service);
        Ok(())
    }

    fn register_hooks(&self, hooks: &mut Hooks) {
        // A player whose last connection on this instance closed leaves its queue.
        hooks.after::<AfterWsDisconnect, _, _>(|ctx, event| async move {
            if let Some(service) = ctx.state().get::<MatchmakingService>() {
                service.disconnected(ctx.state(), event.user_id);
            }
            Ok(())
        });
    }

    fn ws_handlers(&self, ws: &mut WsHandlers) {
        ws.push::<MatchFound>()
            .summary("The player's matchmaking ticket was matched")
            .description(
                "To every open connection of each matched player: the receiving player's ticket, the queue, every player of the match and the \
                 `data` the game's rules attached (a lobby, a server address, the teams). `GET /v1/matchmaking/ticket` answers the same match for a while.",
            )
            .schema::<doc::MatchFound>();
        ws.push::<TicketExpired>()
            .summary("The player's matchmaking ticket ran out unmatched")
            .description("The ticket waited its queue's `timeout_secs` without a match and is gone; queue again to keep searching.")
            .schema::<doc::TicketExpired>();
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new()
            .routes(call_route!(ListQueues, routes::queues))
            .routes(call_route!(CreateTicket, routes::create))
            .routes(call_route!(GetTicket, routes::ticket))
            .routes(call_route!(CancelTicket, routes::cancel))
    }

    fn start<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let Some(service) = self.service.get().cloned() else { return Ok(()) };
            let every = service.config().interval_ms;
            if every == 0 || service.config().queues.is_empty() {
                return Ok(());
            }
            let state = state.clone();
            let task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(every));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = tick.tick() => if let Err(error) = service.run_round(&state).await {
                            tracing::warn!(%error, "matchmaking: a round failed");
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
