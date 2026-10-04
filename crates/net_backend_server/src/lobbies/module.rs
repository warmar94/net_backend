//! [`Lobbies`]: the lobbies module (`.module(Lobbies::new())`, after `Auth`).

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use net_backend_protocol::lobbies::{
    CreateLobby, EditLobby, GetLobby, JoinLobby, JoinLobbyByCode, KickFromLobby, LeaveLobby, LobbyMemberUpdate, LobbySearch, LobbyUpdate, MyLobbies,
    NewLobbyCode, SetLobbyReady, TransferLobby,
};
use utoipa_axum::router::OpenApiRouter;

use super::config::LobbiesConfig;
use super::openapi as doc;
use super::service::LobbyService;
use super::{migrations, routes};
use crate::call_route;
use crate::db::Dialect;
use crate::error::Error;
use crate::hooks::Hooks;
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::permissions::Permission;
use crate::state::AppState;
use crate::ws::events::AfterWsDisconnect;
use crate::ws::WsHandlers;

/// The lobbies module: lobbies with a host, join codes, ready flags, metadata, a search by
/// metadata, the `lobby.member` / `lobby.changed` pushes and a chat room per lobby when the chat
/// module is registered (see [`crate::lobbies`]). Name `lobbies`; settings in `[modules.lobbies]`
/// ([`LobbiesConfig`]); needs the `auth` module registered first. Pushes need the WebSocket hub
/// (`ws.enabled`); the HTTP routes work without it. With the friends module registered, lobbies may
/// be friends-only and a host's blocks keep blocked players out.
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::chat::Chat;
/// use net_backend_server::lobbies::Lobbies;
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// NetBackendServer::new(Config::load()?)
///     .module(Auth::new())
///     .module(Chat::new()) // a chat room per lobby
///     .module(Lobbies::new())
///     .run()
///     .await
/// # }
/// ```
#[derive(Debug, Default)]
pub struct Lobbies {
    config: Option<LobbiesConfig>,
    service: OnceLock<LobbyService>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Lobbies {
    /// The module with settings from `[modules.lobbies]` (defaults if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.lobbies]` (which must then be absent).
    pub fn with_config(mut self, config: LobbiesConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&LobbyService> {
        self.service.get()
    }
}

impl Module for Lobbies {
    fn name(&self) -> &'static str {
        "lobbies"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<LobbiesConfig>("lobbies")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec!["modules.lobbies: settings given both in code (Lobbies::with_config) and in [modules.lobbies]; use one".into()]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => LobbiesConfig::default(),
        };
        config.validate()?;
        let service = LobbyService::new(config);
        self.service.set(service.clone()).map_err(|_| Error::Module("the lobbies module was set up twice (build it once)".into()))?;
        setup.insert_state(service);
        Ok(())
    }

    fn permissions(&self) -> Vec<Permission> {
        vec![super::MANAGE]
    }

    fn register_hooks(&self, hooks: &mut Hooks) {
        // A player whose last connection on this instance closed leaves its lobbies after the grace.
        hooks.after::<AfterWsDisconnect, _, _>(|ctx, event| async move {
            if let Some(service) = ctx.state().get::<LobbyService>() {
                service.disconnected(ctx.state(), event.user_id);
            }
            Ok(())
        });
    }

    fn ws_handlers(&self, ws: &mut WsHandlers) {
        ws.push::<LobbyMemberUpdate>()
            .summary("A lobby member joined, left, was kicked or changed its ready flag")
            .description(
                "To every member of the lobby (a leaving or kicked member gets it too). `change` is `joined`, `left`, `kicked` or `ready`; `member` is \
                 the member as it is now. `GET /v1/lobbies/{lobby}` answers the current state.",
            )
            .schema::<doc::LobbyMemberUpdate>();
        ws.push::<LobbyUpdate>()
            .summary("A lobby's host, metadata, settings, state or join code changed")
            .description(
                "To every member, with the lobby as it is now (without its member list). `changes` names what changed (`host`, `metadata`, \
                 `settings`, `state`, `code`). A lobby that closed sends a last one with state `closed`.",
            )
            .schema::<doc::LobbyUpdate>();
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new()
            .routes(call_route!(CreateLobby, routes::create))
            .routes(call_route!(MyLobbies, routes::mine))
            .routes(call_route!(LobbySearch, routes::search))
            .routes(call_route!(JoinLobbyByCode, routes::join_code))
            .routes(call_route!(GetLobby, routes::get))
            .routes(call_route!(EditLobby, routes::update))
            .routes(call_route!(JoinLobby, routes::join))
            .routes(call_route!(LeaveLobby, routes::leave))
            .routes(call_route!(SetLobbyReady, routes::ready))
            .routes(call_route!(NewLobbyCode, routes::code))
            .routes(call_route!(TransferLobby, routes::host))
            .routes(call_route!(KickFromLobby, routes::kick))
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
                        _ = tick.tick() => if let Err(error) = service.purge(&state).await {
                            tracing::warn!(%error, "lobbies: the purge failed");
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
