//! [`Notifications`]: the notifications module (`.module(Notifications::new())`, after `Auth`).

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use net_backend_protocol::notifications::{CountNotifications, DeleteNotification, MarkNotifications, NotificationQuery};
use utoipa_axum::router::OpenApiRouter;

use super::config::NotificationsConfig;
use super::service::NotificationService;
use super::{handlers, migrations};
use crate::call_route;
use crate::db::Dialect;
use crate::error::Error;
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::state::AppState;
use crate::ws::WsHandlers;

/// The notifications module: notifications stored per player, pushed live (`notify.new`), listed,
/// marked read / unread and deleted by their player, deleted after a retention; created by server
/// code and other modules through [`NotificationService::send`] (see [`crate::notifications`]).
/// Name `notifications`; settings in `[modules.notifications]` ([`NotificationsConfig`]); needs the
/// `auth` module registered first. The live push and the `notify.*` requests use the WebSocket hub
/// (`ws.enabled`); the HTTP routes work without it.
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::notifications::{NewNotification, NotificationService, Notifications};
/// use net_backend_server::protocol::UserId;
/// use net_backend_server::{AppState, Config, NetBackendServer};
///
/// // Anywhere in server code (a hook, a route, a module):
/// async fn reward(state: &AppState, player: UserId) -> Result<(), net_backend_server::AppError> {
///     if let Some(notifications) = state.get::<NotificationService>() {
///         notifications.send(state, player, NewNotification::new("reward").with_text("You won 50 gold")).await?;
///     }
///     Ok(())
/// }
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// NetBackendServer::new(Config::load()?).module(Auth::new()).module(Notifications::new()).run().await
/// # }
/// ```
#[derive(Debug, Default)]
pub struct Notifications {
    config: Option<NotificationsConfig>,
    service: OnceLock<NotificationService>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Notifications {
    /// The module with settings from `[modules.notifications]` (defaults if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.notifications]` (which must then be absent).
    pub fn with_config(mut self, config: NotificationsConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&NotificationService> {
        self.service.get()
    }
}

impl Module for Notifications {
    fn name(&self) -> &'static str {
        "notifications"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<NotificationsConfig>("notifications")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec![
                    "modules.notifications: settings given both in code (Notifications::with_config) and in [modules.notifications]; use one".into(),
                ]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => NotificationsConfig::default(),
        };
        config.validate()?;
        let service = NotificationService::new(config);
        self.service.set(service.clone()).map_err(|_| Error::Module("the notifications module was set up twice (build it once)".into()))?;
        setup.insert_state(service);
        Ok(())
    }

    fn ws_handlers(&self, ws: &mut WsHandlers) {
        handlers::register(ws);
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new()
            .routes(call_route!(NotificationQuery, handlers::list))
            .routes(call_route!(CountNotifications, handlers::count))
            .routes(call_route!(MarkNotifications, handlers::mark))
            .routes(call_route!(DeleteNotification, handlers::delete))
    }

    fn start<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let Some(service) = self.service.get().cloned() else { return Ok(()) };
            let every = service.config().purge_interval_secs;
            if every == 0 || service.config().retention_days == 0 {
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
                            Ok(n) => tracing::info!(notifications = n, "notifications: deleted notifications past the retention"),
                            Err(error) => tracing::warn!(%error, "notifications: the retention purge failed"),
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
