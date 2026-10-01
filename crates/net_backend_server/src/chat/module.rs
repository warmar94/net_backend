//! [`Chat`]: the chat module (`.module(Chat::new())`, after `Auth`).

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use net_backend_protocol::chat::{DeleteMessage, ListDirects, ListMessages, ListRooms, OpenDirect};
use utoipa_axum::router::OpenApiRouter;

use super::config::ChatConfig;
use super::handlers;
use super::migrations;
use super::service::ChatService;
use crate::call_route;
use crate::db::Dialect;
use crate::error::Error;
use crate::hooks::Hooks;
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::state::AppState;
use crate::ws::events::AfterWsDisconnect;
use crate::ws::WsHandlers;

/// The chat module: public rooms, direct messages, group rooms, history, presence, moderation
/// (see [`crate::chat`]). Name `chat`; settings in `[modules.chat]` ([`ChatConfig`]); needs the
/// `auth` module registered first and the WebSocket hub on (`ws.enabled`).
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::chat::{Chat, ChatConfig, RoomSpec};
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// let mut chat = ChatConfig::default();
/// chat.rooms = vec![RoomSpec::new("world").with_name("World").with_max_members(500)];
/// NetBackendServer::new(Config::load()?).module(Auth::new()).module(Chat::new().with_config(chat)).run().await
/// # }
/// ```
#[derive(Debug, Default)]
pub struct Chat {
    config: Option<ChatConfig>,
    service: OnceLock<ChatService>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Chat {
    /// The module with settings from `[modules.chat]` (defaults if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.chat]` (which must then be absent).
    pub fn with_config(mut self, config: ChatConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&ChatService> {
        self.service.get()
    }
}

impl Module for Chat {
    fn name(&self) -> &'static str {
        "chat"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<ChatConfig>("chat")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec!["modules.chat: settings given both in code (Chat::with_config) and in [modules.chat]; use one".into()]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => ChatConfig::default(),
        };
        config.validate()?;
        if !setup.config().ws.enabled {
            return Err(Error::Config(vec!["modules.chat needs the WebSocket hub: ws.enabled = true".into()]));
        }
        let service = ChatService::new(config);
        self.service.set(service.clone()).map_err(|_| Error::Module("the chat module was set up twice (build it once)".into()))?;
        setup.insert_state(service);
        Ok(())
    }

    fn register_hooks(&self, hooks: &mut Hooks) {
        // Presence: a closed socket's chat rooms lose it (the hub left its rooms already).
        hooks.after::<AfterWsDisconnect, _, _>(|ctx, event| async move {
            if let Some(service) = ctx.state().get::<ChatService>() {
                service.disconnected(ctx.state().ws(), event.connection, event.user_id, &event.rooms);
            }
            Ok(())
        });
    }

    fn ws_handlers(&self, ws: &mut WsHandlers) {
        handlers::register(ws);
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new()
            .routes(call_route!(ListRooms, handlers::rooms))
            .routes(call_route!(ListMessages, handlers::messages))
            .routes(call_route!(DeleteMessage, handlers::delete_message))
            .routes(call_route!(OpenDirect, handlers::open_dm))
            .routes(call_route!(ListDirects, handlers::dms))
    }

    fn start<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let Some(service) = self.service.get().cloned() else { return Ok(()) };
            // Presence follows removals that reach this instance through the Broadcaster.
            let listener = service.clone();
            state.ws().on_room_left(std::sync::Arc::new(move |hub, connection, user, room| listener.hub_room_left(hub, connection, user, room)));
            for room in &service.config().rooms {
                service.create_room(state, room).await.map_err(|e| Error::Startup(format!("chat: creating the room `{}`: {e}", room.key)))?;
            }
            let every = service.config().purge_interval_secs;
            if every == 0 || service.config().history_retention_days == 0 {
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
                            Ok(n) => tracing::info!(messages = n, "chat: deleted messages past the retention"),
                            Err(error) => tracing::warn!(%error, "chat: the retention purge failed"),
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
