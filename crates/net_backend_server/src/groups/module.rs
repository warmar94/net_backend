//! [`Groups`]: the groups module (`.module(Groups::new())`, after `Auth`).

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;

use net_backend_protocol::groups::{
    AcceptGroupInvite, CreateGroup, DeclineGroupInvite, DeleteGroup, EditGroup, GetGroup, InviteToGroup, JoinGroup, KickMember, LeaveGroup, ListGroupInvites,
    ListGroupMembers, ListGroups, MyGroups, RevokeGroupInvite, SetMemberRole, TransferGroup,
};
use utoipa_axum::router::OpenApiRouter;

use super::config::GroupsConfig;
use super::service::GroupService;
use super::{migrations, routes};
use crate::call_route;
use crate::db::Dialect;
use crate::error::Error;
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::state::AppState;

/// The groups module: groups (guilds, clans) with an owner, admins and members, invitations,
/// open groups, metadata, a list with a name search, and a chat group room per group when the chat
/// module is registered (see [`crate::groups`]). Name `groups`; settings in `[modules.groups]`
/// ([`GroupsConfig`]); needs the `auth` module registered first. With the `chat` module registered,
/// each group gets a chat room; with the `notifications` module, invitations arrive as notifications.
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::groups::Groups;
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// NetBackendServer::new(Config::load()?)
///     .module(Auth::new())
///     .module(Groups::new())
///     .run()
///     .await
/// # }
/// ```
#[derive(Debug, Default)]
pub struct Groups {
    config: Option<GroupsConfig>,
    service: OnceLock<GroupService>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Groups {
    /// The module with settings from `[modules.groups]` (defaults if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.groups]` (which must then be absent).
    pub fn with_config(mut self, config: GroupsConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&GroupService> {
        self.service.get()
    }
}

impl Module for Groups {
    fn name(&self) -> &'static str {
        "groups"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<GroupsConfig>("groups")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec!["modules.groups: settings given both in code (Groups::with_config) and in [modules.groups]; use one".into()]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => GroupsConfig::default(),
        };
        config.validate()?;
        let service = GroupService::new(config);
        self.service.set(service.clone()).map_err(|_| Error::Module("the groups module was set up twice (build it once)".into()))?;
        setup.insert_state(service);
        Ok(())
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new()
            .routes(call_route!(ListGroups, routes::list))
            .routes(call_route!(CreateGroup, routes::create))
            .routes(call_route!(MyGroups, routes::mine))
            .routes(call_route!(ListGroupInvites, routes::invites))
            .routes(call_route!(GetGroup, routes::get))
            .routes(call_route!(EditGroup, routes::update))
            .routes(call_route!(DeleteGroup, routes::delete))
            .routes(call_route!(ListGroupMembers, routes::members))
            .routes(call_route!(JoinGroup, routes::join))
            .routes(call_route!(LeaveGroup, routes::leave))
            .routes(call_route!(InviteToGroup, routes::invite))
            .routes(call_route!(AcceptGroupInvite, routes::accept))
            .routes(call_route!(DeclineGroupInvite, routes::decline))
            .routes(call_route!(RevokeGroupInvite, routes::revoke))
            .routes(call_route!(KickMember, routes::kick))
            .routes(call_route!(SetMemberRole, routes::role))
            .routes(call_route!(TransferGroup, routes::transfer))
    }

    fn start<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            // The background task: groups whose owner's account was deleted get a new owner.
            let Some(service) = self.service.get().cloned() else { return Ok(()) };
            let every = service.config().upkeep_interval_secs;
            if every == 0 {
                return Ok(());
            }
            let state = state.clone();
            let task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(every));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = tick.tick() => match service.upkeep(&state).await {
                            Ok(0) => {}
                            Ok(n) => tracing::info!(groups = n, "groups: groups without an owner got one (or were deleted)"),
                            Err(error) => tracing::warn!(%error, "groups: the upkeep failed"),
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
