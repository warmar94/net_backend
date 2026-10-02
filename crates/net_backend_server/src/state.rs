//! The shared application state ("service container"): configuration, database, clock, hooks,
//! the shutdown signal and the game's own state values.
//!
//! Handlers get it with `State<AppState>` (or pick parts: `State<Db>`, `State<Arc<Config>>`,
//! [`Ext<T>`](crate::http::Ext) for a value registered with
//! [`NetBackendServer::state`](crate::NetBackendServer::state)); modules and hooks get it as
//! `&AppState` / [`HookCtx`](crate::hooks::HookCtx).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use axum::extract::FromRef;
use net_backend_protocol::UnixMillis;

use crate::config::Config;
use crate::db::Db;
use crate::hooks::Hooks;
use crate::http::client_ip::IpNet;
use crate::shutdown::Shutdown;
use crate::ws::Hub;

/// The source of "now". The server reads time only through this, so tests can control it.
pub trait Clock: Send + Sync + 'static {
    /// The current time.
    fn now(&self) -> UnixMillis;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UnixMillis {
        UnixMillis::now()
    }
}

/// A clock that only moves when told to (for tests).
#[derive(Debug, Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    /// A clock showing `now`.
    pub fn new(now: UnixMillis) -> Self {
        Self(AtomicI64::new(now.get()))
    }

    /// Set the time.
    pub fn set(&self, now: UnixMillis) {
        self.0.store(now.get(), Ordering::SeqCst);
    }

    /// Move the time forward by `millis` (saturating).
    pub fn advance(&self, millis: i64) {
        let _ = self.0.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |t| Some(t.saturating_add(millis)));
    }
}

impl Clock for ManualClock {
    fn now(&self) -> UnixMillis {
        UnixMillis(self.0.load(Ordering::SeqCst))
    }
}

impl<C: Clock> Clock for Arc<C> {
    fn now(&self) -> UnixMillis {
        (**self).now()
    }
}

/// Values of any type, one per type (the game's own state, module services).
#[derive(Clone, Default)]
pub(crate) struct Extensions(HashMap<TypeId, Arc<dyn Any + Send + Sync>>);

impl Extensions {
    pub(crate) fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.0.insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub(crate) fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.0.get(&TypeId::of::<T>()).cloned().and_then(|v| v.downcast::<T>().ok())
    }
}

struct Inner {
    config: Arc<Config>,
    db: Db,
    clock: Arc<dyn Clock>,
    hooks: Arc<Hooks>,
    extensions: Extensions,
    modules: Vec<&'static str>,
    shutdown: Shutdown,
    trusted_proxies: Vec<IpNet>,
    ws: Hub,
}

/// The shared state. Cheap to clone.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl fmt::Debug for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState").field("db", &self.0.db).field("modules", &self.0.modules).finish_non_exhaustive()
    }
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: Arc<Config>,
        db: Db,
        clock: Arc<dyn Clock>,
        hooks: Arc<Hooks>,
        extensions: Extensions,
        modules: Vec<&'static str>,
        shutdown: Shutdown,
        ws: Hub,
    ) -> Self {
        let trusted_proxies = config.http.trusted_proxies.iter().filter_map(|p| IpNet::parse(p)).collect();
        Self(Arc::new(Inner { config, db, clock, hooks, extensions, modules, shutdown, trusted_proxies, ws }))
    }

    /// The configuration.
    pub fn config(&self) -> &Arc<Config> {
        &self.0.config
    }

    /// The database.
    pub fn db(&self) -> &Db {
        &self.0.db
    }

    /// The clock.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.0.clock
    }

    /// The current time from the clock.
    pub fn now(&self) -> UnixMillis {
        self.0.clock.now()
    }

    /// The hooks registry (for modules running their hook points).
    pub fn hooks(&self) -> &Arc<Hooks> {
        &self.0.hooks
    }

    /// A value registered with [`NetBackendServer::state`](crate::NetBackendServer::state).
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.0.extensions.get::<T>()
    }

    /// The registered module names, in registration order.
    pub fn modules(&self) -> &[&'static str] {
        &self.0.modules
    }

    /// The parsed `http.trusted_proxies`.
    pub(crate) fn trusted_proxies(&self) -> &[IpNet] {
        &self.0.trusted_proxies
    }

    /// The WebSocket hub: pushes to connections, users, rooms (see [`crate::ws`]).
    pub fn ws(&self) -> &Hub {
        &self.0.ws
    }

    /// The shutdown signal (background tasks wait on it; `/readyz` reports 503 once it fired).
    pub fn shutdown(&self) -> &Shutdown {
        &self.0.shutdown
    }
}

impl FromRef<AppState> for Db {
    fn from_ref(state: &AppState) -> Db {
        state.0.db.clone()
    }
}

impl FromRef<AppState> for Arc<Config> {
    fn from_ref(state: &AppState) -> Arc<Config> {
        state.0.config.clone()
    }
}

impl FromRef<AppState> for Arc<dyn Clock> {
    fn from_ref(state: &AppState) -> Arc<dyn Clock> {
        state.0.clock.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock() {
        let clock = ManualClock::new(UnixMillis(1000));
        clock.advance(500);
        assert_eq!(clock.now(), UnixMillis(1500));
        clock.set(UnixMillis(i64::MAX));
        clock.advance(1);
        assert_eq!(clock.now(), UnixMillis(i64::MAX));
        assert!(SystemClock.now() > UnixMillis(1_700_000_000_000));
    }

    #[test]
    fn extensions_by_type() {
        let mut ext = Extensions::default();
        ext.insert(5u32);
        ext.insert(String::from("x"));
        assert_eq!(ext.get::<u32>().as_deref(), Some(&5));
        assert_eq!(ext.get::<String>().as_deref().map(String::as_str), Some("x"));
        assert!(ext.get::<u64>().is_none());
    }
}
