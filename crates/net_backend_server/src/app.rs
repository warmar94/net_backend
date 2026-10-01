//! The app builder: [`NetBackendServer`] collects the configuration, modules, routes, hooks and
//! state; [`build`](NetBackendServer::build) connects the database and assembles the router into
//! a [`PreparedServer`], which serves (or migrates).

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::IntoResponse;
use axum::routing::{get, MethodRouter};
use axum::Router;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderName, HeaderValue, Method};
use net_backend_protocol::{routes as proto_routes, PROTOCOL_HEADER};
use tokio::net::TcpListener;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use utoipa_axum::router::{OpenApiRouter, UtoipaMethodRouter};
use utoipa_axum::routes;

use crate::auth::Authenticator;
use crate::command::AppCommand;
use crate::config::Config;
use crate::db::{Db, Dialect};
use crate::error::Error;
use crate::hooks::{guarded, Decision, Event, HookCtx, Hooks, Outcome};
use crate::http::middleware::{self, Mw};
use crate::http::{routes as core, RequestId, REQUEST_ID_HEADER};
use crate::migrate::{self, MigrateReport, MigrationStatus, PublishReport};
use crate::module::{Module, ModuleSet, Setup};
use crate::rate_limit::RateLimiter;
use crate::serve::serve_connections;
use crate::shutdown::{os_signal, Shutdown};
use crate::state::{AppState, Clock, Extensions, SystemClock};
use crate::ws::{Broadcaster, Hub, LocalBroadcaster, WsCtx, WsHandlers};
use crate::AppError;

/// A route registration, applied when the router is assembled (so a conflict becomes an
/// [`Error`] instead of a panic at the call site).
type RouteOp = Box<dyn FnOnce(OpenApiRouter<AppState>) -> OpenApiRouter<AppState> + Send>;

/// The server builder.
///
/// ```no_run
/// use net_backend_server::{Config, NetBackendServer};
/// use net_backend_server::axum::routing::get;
///
/// #[tokio::main]
/// async fn main() -> std::process::ExitCode {
///     let config = match Config::load() {
///         Ok(config) => config,
///         Err(error) => { eprintln!("{error}"); return std::process::ExitCode::FAILURE; }
///     };
///     let server = NetBackendServer::new(config).route("/v1/game/motd", get(|| async { "hello" }));
///     match server.run().await {   // CLI: serve (default), migrate, migrations publish, config check
///         Ok(()) => std::process::ExitCode::SUCCESS,
///         Err(error) => { eprintln!("{error}"); std::process::ExitCode::FAILURE }
///     }
/// }
/// ```
pub struct NetBackendServer {
    pub(crate) config: Config,
    modules: ModuleSet,
    route_ops: Vec<RouteOp>,
    hooks: Hooks,
    extensions: Extensions,
    clock: Option<Arc<dyn Clock>>,
    db: Option<Db>,
    authenticators: Vec<Arc<dyn Authenticator>>,
    rate_limiters: Vec<Arc<dyn RateLimiter>>,
    commands: Vec<Arc<dyn AppCommand>>,
    ws_handlers: WsHandlers,
    broadcaster: Option<Arc<dyn Broadcaster>>,
    /// Registration mistakes found before build (reported by it).
    problems: Vec<String>,
}

impl NetBackendServer {
    /// A server with this configuration and nothing registered yet.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            modules: ModuleSet::default(),
            route_ops: Vec::new(),
            hooks: Hooks::default(),
            extensions: Extensions::default(),
            clock: None,
            db: None,
            authenticators: Vec::new(),
            rate_limiters: Vec::new(),
            commands: Vec::new(),
            ws_handlers: WsHandlers::new(),
            broadcaster: None,
            problems: Vec::new(),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Register a module (order matters: see [`crate::module`]).
    pub fn module(mut self, module: impl Module) -> Self {
        self.modules.push(Box::new(module));
        self
    }

    /// The registered module names, in registration order.
    pub fn module_names(&self) -> Vec<&'static str> {
        self.modules.names()
    }

    /// Add a game route (a full path; by convention under `/v1/`, e.g. `/v1/game/craft`). It
    /// works like `axum::Router::route` but stays out of the OpenAPI document; use
    /// [`routes`](Self::routes) for documented handlers.
    pub fn route(mut self, path: &str, method_router: MethodRouter<AppState>) -> Self {
        let path = path.to_string();
        self.route_ops.push(Box::new(move |api| api.route(&path, method_router)));
        self
    }

    /// Add documented game routes: `.routes(utoipa_axum::routes!(craft))` for handlers carrying
    /// `#[utoipa::path(..)]`.
    pub fn routes(mut self, routes: UtoipaMethodRouter<AppState>) -> Self {
        self.route_ops.push(Box::new(move |api| api.routes(routes)));
        self
    }

    /// Add an undocumented typed route for the protocol call `C` (or a game's own
    /// [`HttpCall`](net_backend_protocol::HttpCall)): mounted at `C::ROUTE` with its method; the
    /// handler takes [`Call<C>`](crate::http::call::Call) last and answers
    /// [`CallResult<C>`](crate::http::call::CallResult). Documented ones: `.routes(call_route!(C, handler))`.
    /// With `C::ROUTE.auth` the route answers 401 to a request without a valid token before the
    /// handler runs. A method this server version does not know: `build` fails.
    pub fn call<C, H, T, M>(mut self, handler: H) -> Self
    where
        C: net_backend_protocol::HttpCall + 'static,
        H: crate::http::call::CallHandler<C, T> + axum::handler::Handler<M, AppState>,
        T: 'static,
        M: 'static,
    {
        if !crate::http::call::supported_method(C::ROUTE.method) {
            self.problems.push(format!("the route {} {} uses a method this server version cannot serve", C::ROUTE.method, C::ROUTE.path));
        }
        self.route_ops.push(Box::new(move |api| crate::http::call::undocumented::<C, H, T, M>(api, handler)));
        self
    }

    /// Nest a whole router under a path prefix.
    pub fn nest(mut self, path: &str, router: Router<AppState>) -> Self {
        let path = path.to_string();
        self.route_ops.push(Box::new(move |api| api.nest(&path, OpenApiRouter::from(router))));
        self
    }

    /// Merge an OpenAPI router (documented and plain routes together).
    pub fn merge(mut self, router: OpenApiRouter<AppState>) -> Self {
        self.route_ops.push(Box::new(move |api| api.merge(router)));
        self
    }

    /// Register a value of the game's own (one per type); handlers get it with
    /// [`Ext<T>`](crate::http::Ext), hooks and modules with [`AppState::get`].
    pub fn state<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.extensions.insert(value);
        self
    }

    /// Register a `before` hook for events of type `E` (see [`crate::hooks`]).
    pub fn before<E, F, Fut>(mut self, hook: F) -> Self
    where
        E: Event,
        F: Fn(HookCtx, E) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Decision<E>, AppError>> + Send + 'static,
    {
        self.hooks.before(hook);
        self
    }

    /// Register an `in_tx` hook for events of type `E` (runs inside the module's transaction; see
    /// [`Hooks::in_tx`]).
    pub fn in_tx<E, F>(mut self, hook: F) -> Self
    where
        E: Event,
        F: for<'a> Fn(&'a mut crate::db::DbTx, &'a HookCtx, &'a E) -> futures_util::future::BoxFuture<'a, Result<(), AppError>> + Send + Sync + 'static,
    {
        self.hooks.in_tx(hook);
        self
    }

    /// Register an `after` hook for events of type `E`.
    pub fn after<E, F, Fut>(mut self, hook: F) -> Self
    where
        E: Event,
        F: Fn(HookCtx, Arc<E>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), AppError>> + Send + 'static,
    {
        self.hooks.after(hook);
        self
    }

    /// Run this after every module started, before requests are accepted (an error aborts).
    pub fn on_start<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(HookCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), AppError>> + Send + 'static,
    {
        self.hooks.on_start(hook);
        self
    }

    /// Run this on shutdown, after in-flight requests drained and the modules shut down.
    pub fn on_shutdown<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(HookCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.hooks.on_shutdown(hook);
        self
    }

    /// Use this clock instead of the system clock (tests).
    pub fn clock(mut self, clock: impl Clock) -> Self {
        self.clock = Some(Arc::new(clock));
        self
    }

    /// Use this database instead of connecting with `[database]` (tests, or a pool the app built).
    pub fn db(mut self, db: Db) -> Self {
        self.db = Some(db);
        self
    }

    /// Add an authenticator (see [`crate::auth`]). Authenticators are asked in order: the app's
    /// (in the order added), then the modules' (e.g. the [`Auth`](crate::auth::Auth) module's
    /// token check); the first that recognises the request decides.
    pub fn authenticator(mut self, authenticator: impl Authenticator) -> Self {
        self.authenticators.push(Arc::new(authenticator));
        self
    }

    /// Add a rate limiter (see [`crate::rate_limit`]); every limiter is asked, the first refusal
    /// answers.
    pub fn rate_limiter(mut self, limiter: impl RateLimiter) -> Self {
        self.rate_limiters.push(Arc::new(limiter));
        self
    }

    /// Register the game's WebSocket handlers (see [`crate::ws`]): `.ws(|ws| { ws.call::<Shout, _, _>(shout).summary("…"); })`.
    pub fn ws(mut self, register: impl FnOnce(&mut WsHandlers)) -> Self {
        register(&mut self.ws_handlers);
        self
    }

    /// A typed WebSocket handler for `C::KIND` (the request's `data` decodes as `C`; the `Ok`
    /// value is the answer's `data`). A kind may be registered once.
    pub fn ws_call<C, F, Fut>(mut self, handler: F) -> Self
    where
        C: net_backend_protocol::WsCall + Send + 'static,
        F: Fn(WsCtx, C) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<C::Response, AppError>> + Send + 'static,
    {
        self.ws_handlers.call::<C, F, Fut>(handler);
        self
    }

    /// An untyped WebSocket handler for `kind` (JSON in, JSON out).
    pub fn ws_handler<F, Fut>(mut self, kind: &str, handler: F) -> Self
    where
        F: Fn(WsCtx, serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<serde_json::Value, AppError>> + Send + 'static,
    {
        self.ws_handlers.raw(kind, handler);
        self
    }

    /// Deliver WebSocket pushes through this [`Broadcaster`] (default: in this process only).
    pub fn broadcaster(mut self, broadcaster: impl Broadcaster) -> Self {
        self.broadcaster = Some(Arc::new(broadcaster));
        self
    }

    /// Add a command-line command (see [`crate::command`]).
    pub fn command(mut self, command: impl AppCommand) -> Self {
        self.commands.push(Arc::new(command));
        self
    }

    /// Every app command: the app's, then the modules' (registration order).
    pub(crate) fn app_commands(&self) -> Vec<Arc<dyn AppCommand>> {
        let mut commands = self.commands.clone();
        for module in self.modules.iter() {
            commands.extend(module.commands());
        }
        commands
    }

    /// Copy a module's migrations into the app's migrations directory (all dialects, or the given
    /// ones); see [`crate::migrate`]. Needs no database.
    pub fn publish_migrations(&self, module: &str, dialects: &[Dialect], force: bool) -> Result<PublishReport, Error> {
        self.modules.validate()?;
        let dialects = if dialects.is_empty() { Dialect::ALL } else { dialects };
        migrate::publish(&self.modules, module, &self.config.database.migrations_dir, dialects, force)
    }

    /// Validate, connect the database (unless one was given) and assemble the router.
    pub async fn build(self) -> Result<PreparedServer, Error> {
        self.config.validate()?;
        self.modules.validate()?;
        if !self.problems.is_empty() {
            return Err(Error::Module(self.problems.join("; ")));
        }
        let unknown = self.config.unknown_module_sections(&self.modules.names());
        if !unknown.is_empty() {
            return Err(Error::Config(unknown.iter().map(|name| format!("[modules.{name}]: no module named `{name}` is registered (a typo?)")).collect()));
        }
        let NetBackendServer {
            config,
            modules,
            route_ops,
            mut hooks,
            mut extensions,
            clock,
            db,
            mut authenticators,
            mut rate_limiters,
            commands: _,
            mut ws_handlers,
            broadcaster,
            problems: _,
        } = self;
        let db = match db {
            Some(db) => db,
            None => Db::connect(&config.database).await?,
        };
        for module in modules.iter() {
            let mut setup =
                Setup { config: &config, db: &db, extensions: &mut extensions, authenticators: &mut authenticators, rate_limiters: &mut rate_limiters };
            if let Err(error) = module.setup(&mut setup) {
                db.close().await;
                return Err(match error {
                    Error::Config(problems) => Error::Config(problems),
                    other => Error::Module(format!("module `{}`: setup failed: {other}", module.name())),
                });
            }
        }
        hooks.set_timeout(Duration::from_millis(config.server.hook_timeout_ms.max(1)));
        for module in modules.iter() {
            module.register_hooks(&mut hooks);
        }
        for module in modules.iter() {
            ws_handlers.set_owner(module.name());
            module.ws_handlers(&mut ws_handlers);
        }
        let handlers = match ws_handlers.finish() {
            Ok(handlers) => handlers,
            Err(problems) => {
                db.close().await;
                return Err(Error::Module(problems.join("; ")));
            }
        };
        let authenticators: Arc<[Arc<dyn Authenticator>]> = Arc::from(authenticators);
        let hub =
            Hub::new(config.ws.clone(), handlers, authenticators.clone(), broadcaster.unwrap_or_else(|| Arc::new(LocalBroadcaster)), config.metrics.enabled);
        let state = AppState::new(
            Arc::new(config),
            db,
            clock.unwrap_or_else(|| Arc::new(SystemClock)),
            Arc::new(hooks),
            extensions,
            modules.names(),
            Shutdown::new(),
            hub,
        );
        let mw = Mw { state: state.clone(), authenticators, rate_limiters: Arc::from(rate_limiters) };
        let (router, metrics_router, openapi, asyncapi) = assemble(&state, &modules, route_ops, mw)?;
        Ok(PreparedServer { state, router, metrics_router, openapi, asyncapi, modules: Arc::new(modules) })
    }

    /// Build and serve on this listener until SIGTERM / Ctrl-C.
    pub async fn serve(self, listener: TcpListener) -> Result<(), Error> {
        self.build().await?.serve(listener).await
    }

    /// Build and serve on this listener until `signal` resolves (or SIGTERM / Ctrl-C).
    pub async fn serve_with_shutdown<F>(self, listener: TcpListener, signal: F) -> Result<(), Error>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.build().await?.serve_with_shutdown(listener, signal).await
    }

    /// Run the command line from the process arguments (see [`crate::cli`]): `serve` (the
    /// default), `migrate [up|status]`, `migrations publish <module>`, `config check`. Sets up
    /// logging from `[log]` first (unless the app already installed a subscriber).
    pub async fn run(self) -> Result<(), Error> {
        crate::cli::init_logging(&self.config.log);
        self.run_with_args(std::env::args_os()).await
    }

    /// [`run`](Self::run) with explicit arguments (the first is the program name), without
    /// setting up logging.
    pub async fn run_with_args<I, T>(self, args: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        crate::cli::run(self, args, &mut std::io::stdout()).await
    }

    /// [`run_with_args`](Self::run_with_args) writing the command output to `output` instead
    /// of stdout (tests, tools).
    pub async fn run_with_output<I, T>(self, args: I, output: &mut (dyn std::io::Write + Send)) -> Result<(), Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        crate::cli::run(self, args, output).await
    }
}

fn cors_layer(config: &Config) -> Option<CorsLayer> {
    let origins = &config.cors.allowed_origins;
    if origins.is_empty() {
        return None;
    }
    let allow =
        if origins.iter().any(|o| o == "*") { AllowOrigin::any() } else { AllowOrigin::list(origins.iter().filter_map(|o| HeaderValue::from_str(o).ok())) };
    let protocol = HeaderName::from_static(PROTOCOL_HEADER);
    let request_id = HeaderName::from_static(REQUEST_ID_HEADER);
    Some(
        CorsLayer::new()
            .allow_origin(allow)
            .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
            .allow_headers([AUTHORIZATION, CONTENT_TYPE, protocol.clone(), request_id.clone()])
            .expose_headers([protocol, request_id])
            .max_age(Duration::from_secs(config.cors.max_age_secs)),
    )
}

fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown".into())
}

/// Assemble every route and layer. axum panics on conflicting routes; that panic is turned into
/// an [`Error::Module`].
/// The assembled parts: the API router, the metrics router, the OpenAPI and AsyncAPI documents (JSON).
type Assembled = (Router, Option<Router>, Arc<str>, Arc<str>);

/// Where the AsyncAPI document of the WebSocket endpoint is served (with `openapi.enabled` and
/// `ws.enabled`).
pub const ASYNCAPI_PATH: &str = "/v1/asyncapi.json";

fn assemble(state: &AppState, modules: &ModuleSet, route_ops: Vec<RouteOp>, mw: Mw) -> Result<Assembled, Error> {
    let config = state.config().clone();
    let asyncapi: Arc<str> = Arc::from(crate::ws::asyncapi::document(&config, &state.ws().0.handlers));
    let metrics = if config.metrics.enabled { crate::metrics::handle() } else { None };
    let (routed, spec) = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut api: OpenApiRouter<AppState> =
            OpenApiRouter::with_openapi(crate::openapi::base(&config)).routes(routes!(core::health)).routes(routes!(core::ready)).routes(routes!(core::info));
        for module in modules.iter() {
            api = api.merge(module.routes());
            if let Some(extra) = module.openapi() {
                api.get_openapi_mut().merge(extra);
            }
        }
        for op in route_ops {
            api = op(api);
        }
        let (mut router, spec) = api.split_for_parts();
        router = if config.ws.enabled {
            router.route(proto_routes::WS, get(crate::ws::connection::endpoint))
        } else {
            router.route(proto_routes::WS, get(core::ws_reserved))
        };
        let spec: Arc<str> = Arc::from(spec.to_json().unwrap_or_else(|_| "{}".into()));
        if config.openapi.enabled && config.ws.enabled {
            let asyncapi = asyncapi.clone();
            router = router.route(
                ASYNCAPI_PATH,
                get(move || {
                    let asyncapi = asyncapi.clone();
                    async move { ([(CONTENT_TYPE, HeaderValue::from_static("application/json"))], asyncapi.to_string()).into_response() }
                }),
            );
        }
        if config.openapi.enabled {
            let spec = spec.clone();
            router = router.route(
                "/v1/openapi.json",
                get(move || {
                    let spec = spec.clone();
                    async move { ([(CONTENT_TYPE, HeaderValue::from_static("application/json"))], spec.to_string()).into_response() }
                }),
            );
            if config.openapi.ui {
                let openapi = config.openapi.clone();
                router = router.route("/v1/docs", get(move || std::future::ready(core::docs_page(&openapi))));
            }
        }
        (router, spec)
    }))
    .map_err(|panic| Error::Module(format!("route registration failed: {}", panic_text(panic.as_ref()))))?;

    // Route layers run outermost-last: rate limit (address + route) → authenticate → rate limit
    // (with the user) → handler.
    let mut router = routed
        .route_layer(from_fn_with_state(mw.clone(), middleware::rate_limit_after_auth))
        .route_layer(from_fn_with_state(mw.clone(), middleware::authenticate))
        .route_layer(from_fn_with_state(mw.clone(), middleware::rate_limit_before_auth));
    if metrics.is_some() {
        router = router.route_layer(from_fn(middleware::track));
    }
    let trace = TraceLayer::new_for_http()
        .make_span_with(|req: &axum::extract::Request| {
            let id = req.extensions().get::<RequestId>().map(|id| id.to_string()).unwrap_or_default();
            // The path only: query strings may carry credentials.
            tracing::info_span!("request", method = %req.method(), path = %req.uri().path(), request_id = %id)
        })
        .on_response(DefaultOnResponse::new().level(tracing::Level::INFO))
        // Failures are logged once, by AppError / the error normaliser.
        .on_failure(());
    router = router
        .fallback(middleware::not_found)
        .layer(axum::extract::DefaultBodyLimit::max(config.http.body_limit_bytes))
        // The hard cap, also for raw body streams and raised per-route limits.
        .layer(RequestBodyLimitLayer::new(config.http.max_body_bytes))
        .layer(CatchPanicLayer::custom(middleware::panic_response))
        .layer(from_fn_with_state(state.clone(), middleware::timeout))
        .layer(from_fn(middleware::normalize_errors))
        .layer(from_fn(middleware::protocol))
        .layer(trace)
        .layer(from_fn_with_state(state.clone(), crate::http::client_ip::attach))
        .layer(from_fn_with_state(state.clone(), middleware::request_id));
    if let Some(cors) = cors_layer(&config) {
        router = router.layer(cors);
    }
    let metrics_router = metrics.map(|handle| {
        Router::new().route(
            "/metrics",
            get(move || {
                let handle = handle.clone();
                async move {
                    handle.run_upkeep();
                    ([(CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4"))], handle.render()).into_response()
                }
            }),
        )
    });
    Ok((router.with_state(state.clone()), metrics_router, spec, asyncapi))
}

/// A built server: database connected, router assembled. Serve it, or run migrations with it.
pub struct PreparedServer {
    state: AppState,
    router: Router,
    metrics_router: Option<Router>,
    openapi: Arc<str>,
    asyncapi: Arc<str>,
    modules: Arc<ModuleSet>,
}

impl std::fmt::Debug for PreparedServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedServer").field("state", &self.state).finish_non_exhaustive()
    }
}

impl PreparedServer {
    /// The shared state.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// The assembled router (with every layer and the state), e.g. for in-process tests with
    /// `tower::ServiceExt::oneshot`.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// The metrics router (`GET /metrics`), served on its own listener (`metrics.bind`); `None`
    /// when metrics are off (or another recorder is installed).
    pub fn metrics_router(&self) -> Option<Router> {
        self.metrics_router.clone()
    }

    /// The OpenAPI document as JSON (also when `openapi.enabled` is off).
    pub fn openapi_json(&self) -> &str {
        &self.openapi
    }

    /// The AsyncAPI 3.0 document of the WebSocket endpoint as JSON (served at
    /// [`ASYNCAPI_PATH`] with `openapi.enabled` and `ws.enabled`; available here either way).
    pub fn asyncapi_json(&self) -> &str {
        &self.asyncapi
    }

    /// The migrations directory from the configuration.
    pub fn migrations_dir(&self) -> PathBuf {
        self.state.config().database.migrations_dir.clone()
    }

    fn plan(&self) -> Result<migrate::Plan, Error> {
        migrate::plan(&self.modules, self.state.db().dialect(), &self.migrations_dir())
    }

    /// Apply every pending migration (modules in registration order, then the app's).
    pub async fn migrate(&self) -> Result<MigrateReport, Error> {
        let plan = self.plan()?;
        let clock = self.state.clock().clone();
        let wait = Duration::from_secs(self.state.config().database.migrate_lock_timeout_secs);
        migrate::run(self.state.db(), &plan, move || clock.now(), wait).await
    }

    /// The state of every migration (applied, pending, modified, missing), in plan order.
    pub async fn migration_status(&self) -> Result<Vec<MigrationStatus>, Error> {
        let plan = self.plan()?;
        migrate::status(self.state.db(), &plan).await
    }

    /// Serve until SIGTERM / Ctrl-C.
    pub async fn serve(self, listener: TcpListener) -> Result<(), Error> {
        self.serve_with_shutdown(listener, std::future::pending()).await
    }

    /// Serve until `signal` resolves (or SIGTERM / Ctrl-C), then shut down gracefully: stop
    /// accepting, let in-flight requests finish within `server.shutdown_grace_secs` (then drop the
    /// remaining connections, aborting their handlers), shut the modules down (reverse order, each
    /// bounded), run the shutdown hooks, close the database.
    pub async fn serve_with_shutdown<F>(self, listener: TcpListener, signal: F) -> Result<(), Error>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let PreparedServer { state, router, metrics_router, modules, .. } = self;
        let config = state.config().clone();
        let metrics_listener = match &metrics_router {
            Some(_) => {
                let bind = config.metrics.bind;
                Some(TcpListener::bind(bind).await.map_err(|e| Error::io(format!("binding the metrics listener {bind}"), e))?)
            }
            None => None,
        };
        if config.database.migrate_on_start {
            let plan = migrate::plan(&modules, state.db().dialect(), &config.database.migrations_dir)?;
            let clock = state.clock().clone();
            let wait = Duration::from_secs(config.database.migrate_lock_timeout_secs);
            migrate::run(state.db(), &plan, move || clock.now(), wait).await?;
        }
        let ctx = HookCtx::new(state.clone(), None);
        let start_limit = Duration::from_secs(config.server.module_start_timeout_secs);
        let stop_limit = Duration::from_secs(config.server.module_shutdown_timeout_secs);
        let mut started = 0usize;
        let mut start_error = None;
        for module in modules.iter() {
            let failed = match guarded(start_limit, module.start(&state)).await {
                Outcome::Done(Ok(())) => None,
                Outcome::Done(Err(error)) => Some(error.to_string()),
                Outcome::TimedOut => Some(format!("start did not finish within {} s", start_limit.as_secs())),
                Outcome::Panicked => Some("start panicked".to_string()),
            };
            if let Some(reason) = failed {
                start_error = Some(Error::Startup(format!("module `{}`: {reason}", module.name())));
                break;
            }
            started += 1;
        }
        if start_error.is_none() {
            if let Err(error) = state.hooks().run_start(&ctx).await {
                start_error = Some(Error::Startup(error));
            }
        }
        if let Some(error) = start_error {
            let begun: Vec<&dyn Module> = modules.iter().take(started).collect();
            shutdown_modules(begun.into_iter().rev(), &state, stop_limit).await;
            state.db().close().await;
            return Err(error);
        }
        let hub_tasks = state.ws().start(&state);

        let shutdown = state.shutdown().clone();
        let trigger = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = signal => {}
                _ = os_signal() => {}
                _ = trigger.wait() => {}
            }
            trigger.trigger();
        });
        let grace = Duration::from_secs(config.server.shutdown_grace_secs);
        let header_timeout = Duration::from_secs(config.server.header_read_timeout_secs.max(1));
        let mut background = Vec::new();
        if let (Some(handle), true) = (crate::metrics::handle(), metrics_router.is_some()) {
            let stop = shutdown.clone();
            background.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tokio::select! {
                        _ = tick.tick() => handle.run_upkeep(),
                        _ = stop.wait() => break,
                    }
                }
            }));
        }
        if let (Some(metrics_router), Some(metrics_listener)) = (metrics_router, metrics_listener) {
            if let Ok(addr) = metrics_listener.local_addr() {
                tracing::info!(%addr, ">>> NBS: metrics listening");
            }
            let stop = shutdown.clone();
            background.push(tokio::spawn(serve_connections(metrics_listener, metrics_router, stop, header_timeout, Duration::from_secs(1), "metrics")));
        }

        let addr = listener.local_addr().map_err(|e| Error::io("reading the listener address", e))?;
        tracing::info!(%addr, modules = ?state.modules(), ">>> NBS: listening");
        let drain_notice = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                shutdown.wait().await;
                tracing::info!(grace_secs = grace.as_secs(), ">>> NBS: shutting down, draining requests");
            })
        };
        serve_connections(listener, router, shutdown.clone(), header_timeout, grace, "api").await;
        shutdown.trigger();
        // WebSockets got 1001 when the signal came; wait for them within the same grace period.
        state.ws().finish(grace).await;
        let _ = drain_notice.await;
        for task in background.into_iter().chain(hub_tasks) {
            let _ = task.await;
        }
        shutdown_modules(modules.iter().rev(), &state, stop_limit).await;
        state.hooks().run_shutdown(&ctx).await;
        if tokio::time::timeout(Duration::from_secs(5), state.db().close()).await.is_err() {
            tracing::warn!("the database pool did not close within 5 s");
        }
        tracing::info!(">>> NBS: stopped");
        Ok(())
    }
}

/// Each module's `shutdown`, bounded and with panics contained; a failure is logged and the next
/// module still shuts down.
async fn shutdown_modules<'a>(modules: impl Iterator<Item = &'a dyn Module>, state: &AppState, limit: Duration) {
    for module in modules {
        match guarded(limit, module.shutdown(state)).await {
            Outcome::Done(()) => {}
            Outcome::TimedOut => tracing::warn!(module = module.name(), limit_secs = limit.as_secs(), "module shutdown timed out"),
            Outcome::Panicked => tracing::error!(module = module.name(), "module shutdown panicked"),
        }
    }
}
