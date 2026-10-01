//! [`Storage`]: the storage module (`.module(Storage::new())`, after `Auth`).

use std::sync::OnceLock;

use net_backend_protocol::admin::{GetUserObject, ListUserObjects, RemoveUserObject, WriteUserObject};
use net_backend_protocol::storage::{BatchGet, BatchPut, GetObject, ListObjects, RemoveObject, WriteObject, BATCH_BODY_LIMIT_BYTES};
use utoipa_axum::router::{OpenApiRouter, UtoipaMethodRouter};

use super::config::StorageConfig;
use super::service::StorageService;
use super::{admin_routes, migrations, routes as handlers};
use crate::call_route;
use crate::db::Dialect;
use crate::error::Error;
use crate::http::body_limit;
use crate::http::call::method_router;
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::state::AppState;

/// The storage module: per-user JSON objects with versions, conditional writes, batches, the
/// server write lock, quotas, hooks and audited admin access (see [`crate::storage`]). Name
/// `storage`; settings in `[modules.storage]` ([`StorageConfig`]); needs the `auth` module
/// registered first.
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::storage::Storage;
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// NetBackendServer::new(Config::load()?).module(Auth::new()).module(Storage::new()).run().await
/// # }
/// ```
#[derive(Debug, Default)]
pub struct Storage {
    config: Option<StorageConfig>,
    service: OnceLock<StorageService>,
}

impl Storage {
    /// The module with settings from `[modules.storage]` (defaults if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.storage]` (which must then be absent).
    pub fn with_config(mut self, config: StorageConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&StorageService> {
        self.service.get()
    }
}

/// A documented route with its own body limit.
fn limited(route: UtoipaMethodRouter<AppState>, bytes: usize) -> UtoipaMethodRouter<AppState> {
    let (schemas, paths, router) = route;
    (schemas, paths, router.layer(body_limit(bytes)))
}

impl Module for Storage {
    fn name(&self) -> &'static str {
        "storage"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<StorageConfig>("storage")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec!["modules.storage: settings given both in code (Storage::with_config) and in [modules.storage]; use one".into()]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => StorageConfig::default(),
        };
        config.validate()?;
        let hard_cap = setup.config().http.max_body_bytes;
        let largest = config.put_body_limit().max(BATCH_BODY_LIMIT_BYTES);
        if largest > hard_cap {
            return Err(Error::Config(vec![format!("http.max_body_bytes ({hard_cap}) is below the storage routes' body limit ({largest}): raise it")]));
        }
        let service = StorageService::new(config);
        self.service.set(service.clone()).map_err(|_| Error::Module("the storage module was set up twice (build it once)".into()))?;
        setup.insert_state(service);
        Ok(())
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        let Some(service) = self.service.get() else { return OpenApiRouter::new() };
        let put_limit = service.config().put_body_limit();
        let mut router = OpenApiRouter::new()
            .routes(call_route!(ListObjects, handlers::list))
            .routes(call_route!(GetObject, handlers::get))
            .routes(limited(call_route!(WriteObject, handlers::put), put_limit))
            .routes(call_route!(RemoveObject, handlers::delete))
            .routes(call_route!(BatchGet, handlers::batch_get))
            .routes(limited(call_route!(BatchPut, handlers::batch_put), BATCH_BODY_LIMIT_BYTES));
        if service.config().admin_in_openapi {
            router = router
                .routes(call_route!(ListUserObjects, admin_routes::list))
                .routes(call_route!(GetUserObject, admin_routes::get))
                .routes(limited(call_route!(WriteUserObject, admin_routes::put), put_limit))
                .routes(call_route!(RemoveUserObject, admin_routes::delete));
        } else {
            use net_backend_protocol::HttpCall;
            router = router
                .route(ListUserObjects::ROUTE.path, method_router::<ListUserObjects, _, _, _>(admin_routes::list))
                .route(GetUserObject::ROUTE.path, method_router::<GetUserObject, _, _, _>(admin_routes::get))
                .route(WriteUserObject::ROUTE.path, method_router::<WriteUserObject, _, _, _>(admin_routes::put).layer(body_limit(put_limit)))
                .route(RemoveUserObject::ROUTE.path, method_router::<RemoveUserObject, _, _, _>(admin_routes::delete));
        }
        router
    }
}
