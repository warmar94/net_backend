//! [`Friends`]: the friends module (`.module(Friends::new())`, after `Auth`).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use net_backend_protocol::friends::{
    AcceptFriend, AddFriend, BlockUser, CancelFriendRequest, DeclineFriend, FriendPresence, FriendsHeartbeat, GetFriendCode, GetFriendSettings, ListBlocks,
    ListFriendRequests, ListFriends, RemoveFriend, ResetFriendCode, SteamMatch, UnblockUser, UpdateFriendSettings,
};
use utoipa_axum::router::OpenApiRouter;

use super::config::FriendsConfig;
use super::openapi as doc;
use super::service::FriendService;
use super::{migrations, routes};
use crate::call_route;
use crate::db::Dialect;
use crate::error::Error;
use crate::hooks::Hooks;
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::state::AppState;
use crate::ws::events::{AfterWsConnect, AfterWsDisconnect};
use crate::ws::WsHandlers;

/// The friends module: friend requests by account id, display name or friend code, friendships,
/// blocks, the online state of friends and the `friends.presence` push (see [`crate::friends`]).
/// Name `friends`; settings in `[modules.friends]` ([`FriendsConfig`]); needs the `auth` module
/// registered first. Pushes need the WebSocket hub (`ws.enabled`); the HTTP routes work without
/// it. With the notifications module registered, requests and acceptances are also notifications.
/// With Steam login on in the auth module, players who linked Steam find which of a list of Steam
/// IDs belong to accounts here (`POST /v1/friends/steam`).
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::friends::Friends;
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// NetBackendServer::new(Config::load()?).module(Auth::new()).module(Friends::new()).run().await
/// # }
/// ```
#[derive(Debug, Default)]
pub struct Friends {
    config: Option<FriendsConfig>,
    service: OnceLock<FriendService>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Ends the presence task.
    stop: Arc<tokio::sync::Notify>,
}

impl Friends {
    /// The module with settings from `[modules.friends]` (defaults if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.friends]` (which must then be absent).
    pub fn with_config(mut self, config: FriendsConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&FriendService> {
        self.service.get()
    }
}

impl Module for Friends {
    fn name(&self) -> &'static str {
        "friends"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<FriendsConfig>("friends")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec!["modules.friends: settings given both in code (Friends::with_config) and in [modules.friends]; use one".into()]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => FriendsConfig::default(),
        };
        config.validate()?;
        let service = FriendService::new(config);
        self.service.set(service.clone()).map_err(|_| Error::Module("the friends module was set up twice (build it once)".into()))?;
        setup.insert_state(service);
        Ok(())
    }

    fn register_hooks(&self, hooks: &mut Hooks) {
        // The online state follows the player's WebSocket connections on this instance. The hooks
        // only note the player: the module's presence task does the database work and the pushes
        // (in batches, one player's decisions in order), so opening or closing a socket waits for
        // none of it, and the hook time limit (`server.hook_timeout_ms`) cannot cut it off.
        hooks.after::<AfterWsConnect, _, _>(|ctx, event| async move {
            if let Some(service) = ctx.state().get::<FriendService>() {
                service.changed(event.user_id);
            }
            Ok(())
        });
        hooks.after::<AfterWsDisconnect, _, _>(|ctx, event| async move {
            if let Some(service) = ctx.state().get::<FriendService>() {
                service.changed(event.user_id);
            }
            Ok(())
        });
    }

    fn ws_handlers(&self, ws: &mut WsHandlers) {
        ws.push::<FriendPresence>()
            .summary("A friend came online or went offline")
            .description(
                "To every open connection of each of the player's friends: when the player's first connection opens (`online: true`), \
                 when its last one closes (`online: false` with `last_seen`; with several server instances: its last one on every instance), \
                 and when a heartbeat (`POST /v1/friends/presence`) brings an offline player online. \
                 `GET /v1/friends` answers the current state.",
            )
            .schema::<doc::FriendPresence>();
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new()
            .routes(call_route!(ListFriends, routes::friends))
            .routes(call_route!(RemoveFriend, routes::remove))
            .routes(call_route!(ListFriendRequests, routes::requests))
            .routes(call_route!(AddFriend, routes::add))
            .routes(call_route!(CancelFriendRequest, routes::cancel))
            .routes(call_route!(AcceptFriend, routes::accept))
            .routes(call_route!(DeclineFriend, routes::decline))
            .routes(call_route!(ListBlocks, routes::blocks))
            .routes(call_route!(BlockUser, routes::block))
            .routes(call_route!(UnblockUser, routes::unblock))
            .routes(call_route!(GetFriendCode, routes::code))
            .routes(call_route!(ResetFriendCode, routes::reset_code))
            .routes(call_route!(FriendsHeartbeat, routes::heartbeat))
            .routes(call_route!(SteamMatch, routes::steam))
            .routes(call_route!(GetFriendSettings, routes::settings))
            .routes(call_route!(UpdateFriendSettings, routes::update_settings))
    }

    fn start<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let Some(service) = self.service.get().cloned() else { return Ok(()) };
            if !state.config().ws.enabled {
                return Ok(());
            }
            // A third of the window: a connected player's stored online time never runs out.
            let every = Duration::from_secs(u64::from(service.config().online_window_secs / 3).max(1));
            let (state, stop) = (state.clone(), self.stop.clone());
            let task = tokio::spawn(async move { service.run_presence(state, every, stop).await });
            self.tasks.lock().unwrap_or_else(|e| e.into_inner()).push(task);
            Ok(())
        })
    }

    fn shutdown<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // The presence task ends after the batch it is on (it also ends with the server's
            // shutdown signal); cut off only when it takes too long.
            self.stop.notify_one();
            let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
            for mut task in tasks {
                if tokio::time::timeout(Duration::from_secs(5), &mut task).await.is_err() {
                    task.abort();
                }
            }
            // Every WebSocket of this instance is closed by now; the players it held go offline
            // here, while the database pool is still open (the disconnect hooks of the closing
            // sockets may come after the presence task ended).
            if let Some(service) = self.service.get().cloned() {
                service.stopping(state).await;
            }
        })
    }
}
