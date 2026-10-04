//! [`Files`]: the files module (`.module(Files::new())`, after `Auth`).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;

use net_backend_protocol::files::{DeleteFile, EditFile, GetFile, GetFileUsage, ListFiles};
use utoipa_axum::router::OpenApiRouter;

use super::backend::{FileStore, LocalFileStore};
use super::config::{FilesConfig, UPLOAD_OVERHEAD_BYTES};
use super::service::FileService;
use super::{migrations, routes};
use crate::call_route;
use crate::db::Dialect;
use crate::error::Error;
use crate::http::{body_limit, upload_timeout};
use crate::migrate::Migration;
use crate::module::{Module, Setup};
use crate::state::AppState;

/// The files module: binary uploads with a content type, a SHA-256 and per-player quotas, downloads,
/// listings, settings (name, visibility, a share list, metadata) and deletes (see [`crate::files`]).
/// Name `files`; settings in `[modules.files]` ([`FilesConfig`]); needs the `auth` module registered
/// first. The bytes go to the local disk store in `dir`, or to the store given with
/// [`store`](Self::store).
///
/// ```no_run
/// use net_backend_server::auth::Auth;
/// use net_backend_server::files::Files;
/// use net_backend_server::{Config, NetBackendServer};
///
/// # async fn demo() -> Result<(), net_backend_server::Error> {
/// NetBackendServer::new(Config::load()?).module(Auth::new()).module(Files::new()).run().await
/// # }
/// ```
#[derive(Default)]
pub struct Files {
    config: Option<FilesConfig>,
    store: Option<Arc<dyn FileStore>>,
    service: OnceLock<FileService>,
    upload_limit: OnceLock<usize>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Files").field("config", &self.config).field("own_store", &self.store.is_some()).finish_non_exhaustive()
    }
}

impl Files {
    /// The module with settings from `[modules.files]` (defaults if absent).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use these settings instead of `[modules.files]` (which must then be absent).
    pub fn with_config(mut self, config: FilesConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Keep the bytes in this store instead of the local disk store in `dir`.
    pub fn store(mut self, store: impl FileStore) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    /// The service, once the server is built.
    pub fn service(&self) -> Option<&FileService> {
        self.service.get()
    }
}

impl Module for Files {
    fn name(&self) -> &'static str {
        "files"
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["auth"]
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        migrations::all(dialect)
    }

    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let from_file = setup.config().module_config::<FilesConfig>("files")?;
        let config = match (&self.config, from_file) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(vec!["modules.files: settings given both in code (Files::with_config) and in [modules.files]; use one".into()]))
            }
            (Some(config), None) => config.clone(),
            (None, Some(config)) => config,
            (None, None) => FilesConfig::default(),
        };
        config.validate(setup.config().http.max_body_bytes)?;
        let store: Arc<dyn FileStore> = match &self.store {
            Some(store) => store.clone(),
            None => Arc::new(LocalFileStore::new(config.dir.clone())),
        };
        let limit = usize::try_from(config.max_file_bytes.saturating_add(UPLOAD_OVERHEAD_BYTES)).unwrap_or(usize::MAX);
        let service = FileService::new(config, store);
        self.service.set(service.clone()).map_err(|_| Error::Module("the files module was set up twice (build it once)".into()))?;
        let _ = self.upload_limit.set(limit);
        setup.insert_state(service);
        Ok(())
    }

    fn routes(&self) -> OpenApiRouter<AppState> {
        let limit = self.upload_limit.get().copied().unwrap_or(16 * 1024 * 1024);
        let (schemas, paths, upload) = utoipa_axum::routes!(routes::upload);
        OpenApiRouter::new()
            .routes((schemas, paths, upload.layer((body_limit(limit), upload_timeout()))))
            .routes(utoipa_axum::routes!(routes::content))
            .routes(call_route!(ListFiles, routes::list))
            .routes(call_route!(GetFileUsage, routes::usage))
            .routes(call_route!(GetFile, routes::get))
            .routes(call_route!(EditFile, routes::edit))
            .routes(call_route!(DeleteFile, routes::delete))
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
                        _ = tick.tick() => match service.purge_orphans(&state, ORPHAN_MIN_AGE).await {
                            Ok(0) => {}
                            Ok(n) => tracing::info!(files = n, "files: removed stored bytes no file names"),
                            Err(error) => tracing::warn!(%error, "files: the purge of stored bytes no file names failed"),
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

/// Bytes younger than this are never purged (an upload's row follows its bytes within moments).
const ORPHAN_MIN_AGE: Duration = Duration::from_secs(3600);
