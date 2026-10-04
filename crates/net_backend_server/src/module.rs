//! The module system: a [`Module`] bundles routes, migrations, hooks and OpenAPI parts under a
//! name, like a service provider.
//!
//! **Order is deterministic: registration order.** Modules' migrations run in that order (then
//! the app's own), their routes are merged in that order and `start` runs in that order;
//! `shutdown` runs for every module at the same time (started in the reverse order), within one
//! `server.module_shutdown_timeout_secs`. A name may be registered once.
//!
//! **Build order:** for each module in registration order, [`setup`](Module::setup) (state values,
//! authenticators, rate limiters), then [`register_hooks`](Module::register_hooks), then the routes
//! are assembled. Command-line commands ([`commands`](Module::commands)) are collected before the
//! command line is parsed.
//!
//! **Defaults:** only [`name`](Module::name) is required; every other method has a default
//! implementation.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::Authenticator;
use crate::command::AppCommand;
use crate::config::Config;
use crate::db::{Db, Dialect};
use crate::error::Error;
use crate::hooks::Hooks;
use crate::migrate::Migration;
use crate::permissions::Permission;
use crate::rate_limit::RateLimiter;
use crate::state::{AppState, Extensions};
use crate::ws::WsHandlers;

/// Names a module may not use: `app` is the app's own migrations namespace; `core` and `nbs` are
/// reserved for the framework.
pub const RESERVED_MODULE_NAMES: &[&str] = &["app", "core", "nbs"];

/// The longest module name, in bytes.
pub const MAX_MODULE_NAME_BYTES: usize = 32;

/// A pluggable part of the server (chat, storage, a game's own subsystem).
///
/// Only [`name`](Module::name) is required; every other method has a default
/// implementation.
///
/// ```
/// use net_backend_server::{Module, Dialect, Migration};
///
/// struct Scores;
///
/// impl Module for Scores {
///     fn name(&self) -> &'static str { "scores" }
///     fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
///         let sql = match dialect {
///             Dialect::MySql => "CREATE TABLE scores (id BIGINT AUTO_INCREMENT PRIMARY KEY, points BIGINT NOT NULL)",
///             Dialect::Postgres => "CREATE TABLE scores (id BIGSERIAL PRIMARY KEY, points BIGINT NOT NULL)",
///             _ => "CREATE TABLE scores (id INTEGER PRIMARY KEY AUTOINCREMENT, points BIGINT NOT NULL)",
///         };
///         vec![Migration::new(2026_10_01_0001, "create_scores", sql)]
///     }
/// }
/// ```
pub trait Module: Send + Sync + 'static {
    /// The module's name: `[a-z][a-z0-9_]*`, at most 32 bytes, not reserved
    /// ([`RESERVED_MODULE_NAMES`]). It names its migrations namespace, its config section
    /// (`[modules.<name>]`) and its entry in `/v1/info`. Stable public API once released.
    fn name(&self) -> &'static str;

    /// The modules this one needs, by name (e.g. `["auth"]` for tables with a foreign key to the
    /// accounts). Each must be registered BEFORE this module, so its migrations run first and its
    /// state values exist in [`setup`](Module::setup); the build fails otherwise.
    fn depends_on(&self) -> &'static [&'static str] {
        &[]
    }

    /// The module's migrations for a dialect, any order (they are sorted by version). Embedded
    /// SQL (e.g. `include_str!`); the app can publish and own them (`migrations publish <name>`).
    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        let _ = dialect;
        Vec::new()
    }

    /// The module's HTTP routes (full paths, e.g. the protocol's `/v1/storage/...` constants).
    /// Handlers registered with `utoipa_axum::routes!` appear in the OpenAPI document.
    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new()
    }

    /// Extra OpenAPI parts (schemas, tags) not tied to a route.
    fn openapi(&self) -> Option<utoipa::openapi::OpenApi> {
        None
    }

    /// Register hooks (the module's own defaults, or hooks into other modules' events).
    fn register_hooks(&self, hooks: &mut Hooks) {
        let _ = hooks;
    }

    /// Prepare the module when the server is built (after the database connected, before the
    /// hooks and routes): read its `[modules.<name>]` section, register its services as state
    /// values, add an [`Authenticator`] or a [`RateLimiter`]. An error stops the build.
    fn setup(&self, setup: &mut Setup<'_>) -> Result<(), Error> {
        let _ = setup;
        Ok(())
    }

    /// The module's WebSocket request handlers and documented pushes (see [`crate::ws`]), e.g.
    /// `handlers.call::<JoinRoom, _, _>(join)`. A kind may be registered once across the app and
    /// all modules (a duplicate stops the build).
    fn ws_handlers(&self, handlers: &mut WsHandlers) {
        let _ = handlers;
    }

    /// The module's command-line commands (see [`crate::command`]), e.g. `user:create`.
    fn commands(&self) -> Vec<Arc<dyn AppCommand>> {
        Vec::new()
    }

    /// The permissions the module checks (see [`crate::permissions`]): names starting with the
    /// module's name and a dot, each with the roles that hold it by default (`admin` holds every
    /// one). The operator grants them to other roles in `[permissions]`.
    fn permissions(&self) -> Vec<Permission> {
        Vec::new()
    }

    /// Start background work (called once before the server accepts requests, in registration
    /// order). Wait on `state.shutdown()` to stop. An error, a panic or running longer than
    /// `server.module_start_timeout_secs` aborts the start (modules already started shut down).
    fn start<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        let _ = state;
        Box::pin(async { Ok(()) })
    }

    /// Clean up after the in-flight requests drained. Every module shuts down at the same time
    /// (started in reverse registration order), all within one `server.module_shutdown_timeout_secs`;
    /// a panic is caught and logged.
    fn shutdown<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, ()> {
        let _ = state;
        Box::pin(async {})
    }
}

/// What a module may contribute in [`Module::setup`].
pub struct Setup<'a> {
    pub(crate) config: &'a Config,
    pub(crate) db: &'a Db,
    pub(crate) extensions: &'a mut Extensions,
    pub(crate) authenticators: &'a mut Vec<Arc<dyn Authenticator>>,
    pub(crate) rate_limiters: &'a mut Vec<Arc<dyn RateLimiter>>,
}

impl std::fmt::Debug for Setup<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Setup").field("authenticators", &self.authenticators.len()).field("rate_limiters", &self.rate_limiters.len()).finish_non_exhaustive()
    }
}

impl Setup<'_> {
    /// The configuration (read a module section with [`Config::module_config`]).
    pub fn config(&self) -> &Config {
        self.config
    }

    /// The database (connected, or a lazy pool; migrations may not have run yet).
    pub fn db(&self) -> &Db {
        self.db
    }

    /// Register a state value (one per type): handlers get it with [`Ext<T>`](crate::http::Ext),
    /// hooks and modules with [`AppState::get`]. Replaces a value of the same type the app
    /// registered.
    pub fn insert_state<T: Send + Sync + 'static>(&mut self, value: T) {
        self.extensions.insert(value);
    }

    /// Whether a state value of this type is registered (by the app or an earlier module).
    pub fn has_state<T: Send + Sync + 'static>(&self) -> bool {
        self.extensions.get::<T>().is_some()
    }

    /// Add an authenticator. Authenticators are asked in order (the app's first, then the
    /// modules'); the first that recognises the request decides.
    pub fn add_authenticator(&mut self, authenticator: Arc<dyn Authenticator>) {
        self.authenticators.push(authenticator);
    }

    /// Add a rate limiter (every limiter is asked; the first refusal answers).
    pub fn add_rate_limiter(&mut self, limiter: Arc<dyn RateLimiter>) {
        self.rate_limiters.push(limiter);
    }
}

/// Check a module name (see [`Module::name`]).
pub fn validate_module_name(name: &str) -> Result<(), String> {
    let mut bytes = name.bytes();
    let first_ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase());
    if !first_ok || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return Err(format!("module name `{name}` must match [a-z][a-z0-9_]*"));
    }
    if name.len() > MAX_MODULE_NAME_BYTES {
        return Err(format!("module name `{name}` is longer than {MAX_MODULE_NAME_BYTES} bytes"));
    }
    if RESERVED_MODULE_NAMES.contains(&name) {
        return Err(format!("module name `{name}` is reserved"));
    }
    Ok(())
}

/// The registered modules, in registration order, with unique valid names.
#[derive(Default)]
pub(crate) struct ModuleSet(Vec<Box<dyn Module>>);

impl ModuleSet {
    pub(crate) fn push(&mut self, module: Box<dyn Module>) {
        self.0.push(module);
    }

    /// Every problem with the names (invalid, reserved, duplicate) and the dependencies (missing,
    /// registered after the module needing them).
    pub(crate) fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for module in &self.0 {
            let name = module.name();
            if let Err(problem) = validate_module_name(name) {
                problems.push(problem);
            }
            for dependency in module.depends_on() {
                if !seen.contains(dependency) {
                    let later = self.0.iter().any(|m| m.name() == *dependency);
                    problems.push(if later {
                        format!("module `{name}` needs `{dependency}` registered before it (register `{dependency}` first)")
                    } else {
                        format!("module `{name}` needs the module `{dependency}` (register it first)")
                    });
                }
            }
            if !seen.insert(name) {
                problems.push(format!("module `{name}` is registered twice"));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Module(problems.join("; ")))
        }
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &dyn Module> {
        self.0.iter().map(|m| m.as_ref())
    }

    pub(crate) fn names(&self) -> Vec<&'static str> {
        self.0.iter().map(|m| m.name()).collect()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&dyn Module> {
        self.iter().find(|m| m.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);
    impl Module for Named {
        fn name(&self) -> &'static str {
            self.0
        }
    }

    #[test]
    fn names() {
        for ok in ["chat", "storage", "a", "game_2", "abcdefghijabcdefghijabcdefghij12"] {
            assert!(validate_module_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Chat", "2chat", "_x", "chat-x", "chat.x", "app", "core", "nbs", "abcdefghijabcdefghijabcdefghij123", "ch at"] {
            assert!(validate_module_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn order_and_duplicates() {
        let mut set = ModuleSet::default();
        for name in ["zeta", "alpha", "mid"] {
            set.push(Box::new(Named(name)));
        }
        assert!(set.validate().is_ok());
        assert_eq!(set.names(), ["zeta", "alpha", "mid"], "registration order, not sorted");
        assert_eq!(set.iter().rev().map(|m| m.name()).collect::<Vec<_>>(), ["mid", "alpha", "zeta"]);
        set.push(Box::new(Named("alpha")));
        set.push(Box::new(Named("App")));
        let error = set.validate().err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("`alpha` is registered twice") && error.contains("`App`"), "{error}");
    }

    struct Needs(&'static str, &'static [&'static str]);
    impl Module for Needs {
        fn name(&self) -> &'static str {
            self.0
        }
        fn depends_on(&self) -> &'static [&'static str] {
            self.1
        }
    }

    #[test]
    fn dependencies_come_first() {
        let mut ok = ModuleSet::default();
        ok.push(Box::new(Needs("auth", &[])));
        ok.push(Box::new(Needs("chat", &["auth"])));
        assert!(ok.validate().is_ok());
        let mut late = ModuleSet::default();
        late.push(Box::new(Needs("chat", &["auth"])));
        late.push(Box::new(Needs("auth", &[])));
        let error = late.validate().err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("needs `auth` registered before it"), "{error}");
        let mut missing = ModuleSet::default();
        missing.push(Box::new(Needs("chat", &["auth"])));
        let error = missing.validate().err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("needs the module `auth`"), "{error}");
    }
}
