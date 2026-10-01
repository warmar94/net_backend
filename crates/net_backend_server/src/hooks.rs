//! Hooks: where a game plugs its own rules into the framework and its modules.
//!
//! - **Events** are plain types implementing [`Event`] (a module defines e.g. a `BeforeSend` with
//!   the message). A module runs its hook points with [`Hooks::run_before`] /
//!   [`Hooks::run_after`].
//! - **`before` hooks** may pass the event on unchanged, pass on a modified event
//!   ([`Decision::Continue`]) or refuse it ([`Decision::Reject`] → the client gets that error).
//!   They run in registration order; the first rejection stops the chain. **Order across
//!   sources:** the game's hooks (registered on the builder) come first, then each module's
//!   (from [`Module::register_hooks`](crate::Module::register_hooks)), modules in registration order.
//! - **`in_tx` hooks** run INSIDE the module's database transaction, after its own write and
//!   before the commit ([`Hooks::in_tx`]): write your own rows atomically with the module's
//!   (e.g. an inventory table updated with a save), or refuse ([`AppError`] → the whole
//!   transaction rolls back and the client gets that error). They get the transaction and borrow
//!   the event (the `for<'a>` + `Box::pin` form: a `Send` bound on an `async` closure borrowing the
//!   transaction cannot be written on stable Rust). Keep them short: the transaction holds its
//!   locks while they run (and SQLite has one writer at a time). A hook that runs out of time
//!   answers 503 `hook_timeout`, a panic 500 `internal`; either way the transaction rolls back.
//!   **Use only the given transaction** (never a service of the module, e.g. `StorageService`,
//!   or a second transaction: on SQLite it waits for the outer write lock until the hook times
//!   out; elsewhere it can deadlock with it). **It may run more than once:** a transaction the
//!   database aborts as a deadlock is rolled back and run again from the start
//!   ([`Retry`](crate::db::Retry)), hooks included, so an `in_tx` hook does nothing outside the
//!   transaction (no mail, no pushes, no HTTP calls: those belong in an `after` hook).
//! - **`after` hooks** run after the work is done (e.g. after the commit); their errors are
//!   logged and never undo anything.
//! - **Lifecycle hooks:** [`on_start`](Hooks::on_start) (after the modules started; an error
//!   aborts the start) and [`on_shutdown`](Hooks::on_shutdown) (after in-flight requests drained).
//!
//! Every hook call has a time limit (`server.hook_timeout_ms`, default 2 s): a `before` hook
//! that runs out answers 503 `hook_timeout`. A panic inside a hook is caught: a `before` hook's
//! panic answers 500 `internal`, an `after` hook's panic is logged. The server keeps running.
//!
//! ```
//! use net_backend_server::hooks::{Decision, Event, Hooks};
//! use net_backend_server::AppError;
//!
//! struct BeforeCraft { item: String }
//! impl Event for BeforeCraft { const NAME: &'static str = "game.before_craft"; }
//!
//! let mut hooks = Hooks::default();
//! hooks.before::<BeforeCraft, _, _>(|_ctx, event| async move {
//!     if event.item == "sword" { Ok(Decision::Continue(event)) } else { Ok(Decision::Reject(AppError::forbidden("not craftable"))) }
//! });
//! ```

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use net_backend_protocol::codes;

use crate::db::DbTx;
use crate::error::AppError;
use crate::http::RequestId;
use crate::state::AppState;

/// An event that hooks can observe. `NAME` names it in logs (`"chat.before_send"`).
///
/// Methods added later always come with a default implementation.
pub trait Event: Send + Sync + 'static {
    /// The event's name for logs.
    const NAME: &'static str;
}

/// What a `before` hook decides.
#[derive(Debug)]
#[non_exhaustive]
pub enum Decision<E> {
    /// Go on with this event (the same one, or a modified one).
    Continue(E),
    /// Refuse: the client gets this error and nothing is done.
    Reject(AppError),
}

/// What a hook receives besides the event: the app state and the request it runs for.
#[derive(Clone, Debug)]
pub struct HookCtx {
    state: AppState,
    request_id: Option<RequestId>,
}

impl HookCtx {
    /// A context for this state and (optional) request.
    pub fn new(state: AppState, request_id: Option<RequestId>) -> Self {
        Self { state, request_id }
    }

    /// The app state (database, config, clock, the game's own state).
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// The database.
    pub fn db(&self) -> &crate::db::Db {
        self.state.db()
    }

    /// The id of the request this hook runs for, if any.
    pub fn request_id(&self) -> Option<&RequestId> {
        self.request_id.as_ref()
    }
}

type BeforeFn<E> = Arc<dyn Fn(HookCtx, E) -> BoxFuture<'static, Result<Decision<E>, AppError>> + Send + Sync>;
type AfterFn<E> = Arc<dyn Fn(HookCtx, Arc<E>) -> BoxFuture<'static, Result<(), AppError>> + Send + Sync>;
type InTxFn<E> = Arc<dyn for<'a> Fn(&'a mut DbTx, &'a HookCtx, &'a E) -> BoxFuture<'a, Result<(), AppError>> + Send + Sync>;
type StartFn = Arc<dyn Fn(HookCtx) -> BoxFuture<'static, Result<(), AppError>> + Send + Sync>;
type ShutdownFn = Arc<dyn Fn(HookCtx) -> BoxFuture<'static, ()> + Send + Sync>;

/// The hook registry: `before` / `after` hooks per event type, plus lifecycle hooks.
pub struct Hooks {
    before: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    in_tx: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    after: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    on_start: Vec<StartFn>,
    on_shutdown: Vec<ShutdownFn>,
    timeout: Duration,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            before: HashMap::new(),
            in_tx: HashMap::new(),
            after: HashMap::new(),
            on_start: Vec::new(),
            on_shutdown: Vec::new(),
            timeout: Duration::from_secs(2),
        }
    }
}

impl fmt::Debug for Hooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hooks")
            .field("before_events", &self.before.len())
            .field("in_tx_events", &self.in_tx.len())
            .field("after_events", &self.after.len())
            .field("on_start", &self.on_start.len())
            .field("on_shutdown", &self.on_shutdown.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// How one hook call ended.
pub(crate) enum Outcome<T> {
    Done(T),
    TimedOut,
    Panicked,
}

pub(crate) async fn guarded<T>(timeout: Duration, future: BoxFuture<'_, T>) -> Outcome<T> {
    match tokio::time::timeout(timeout, AssertUnwindSafe(future).catch_unwind()).await {
        Ok(Ok(value)) => Outcome::Done(value),
        Ok(Err(_panic)) => Outcome::Panicked,
        Err(_) => Outcome::TimedOut,
    }
}

impl Hooks {
    /// The time limit of one hook call.
    pub(crate) fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Register a `before` hook for events of type `E`.
    pub fn before<E, F, Fut>(&mut self, hook: F)
    where
        E: Event,
        F: Fn(HookCtx, E) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Decision<E>, AppError>> + Send + 'static,
    {
        // The closure itself is called inside the returned future, so a panic in its synchronous
        // part is caught by `guarded` too.
        let hook = Arc::new(hook);
        let hook: BeforeFn<E> = Arc::new(move |ctx, event| {
            let hook = hook.clone();
            Box::pin(async move { hook(ctx, event).await })
        });
        let list = self.before.entry(TypeId::of::<E>()).or_insert_with(|| Box::new(Vec::<BeforeFn<E>>::new()));
        if let Some(list) = list.downcast_mut::<Vec<BeforeFn<E>>>() {
            list.push(hook);
        }
    }

    /// Register an `in_tx` hook for events of type `E`: it runs inside the module's transaction
    /// (after the module's own write, before the commit) and may write with the transaction or
    /// refuse (the transaction rolls back, the client gets the error). It uses only `tx` and may
    /// run more than once (a transaction retried after a deadlock; see the module docs).
    ///
    /// ```
    /// use net_backend_server::hooks::{Event, Hooks};
    ///
    /// struct SaveWritten { user: i64 }
    /// impl Event for SaveWritten { const NAME: &'static str = "game.save_written"; }
    ///
    /// let mut hooks = Hooks::default();
    /// hooks.in_tx::<SaveWritten, _>(|tx, _ctx, event| Box::pin(async move {
    ///     let _ = (tx, event.user); // e.g. tx.execute(&update_inventory_statement).await?;
    ///     Ok(())
    /// }));
    /// ```
    pub fn in_tx<E, F>(&mut self, hook: F)
    where
        E: Event,
        F: for<'a> Fn(&'a mut DbTx, &'a HookCtx, &'a E) -> BoxFuture<'a, Result<(), AppError>> + Send + Sync + 'static,
    {
        let hook: InTxFn<E> = Arc::new(hook);
        let list = self.in_tx.entry(TypeId::of::<E>()).or_insert_with(|| Box::new(Vec::<InTxFn<E>>::new()));
        if let Some(list) = list.downcast_mut::<Vec<InTxFn<E>>>() {
            list.push(hook);
        }
    }

    /// Register an `after` hook for events of type `E`.
    pub fn after<E, F, Fut>(&mut self, hook: F)
    where
        E: Event,
        F: Fn(HookCtx, Arc<E>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), AppError>> + Send + 'static,
    {
        let hook = Arc::new(hook);
        let hook: AfterFn<E> = Arc::new(move |ctx, event| {
            let hook = hook.clone();
            Box::pin(async move { hook(ctx, event).await })
        });
        let list = self.after.entry(TypeId::of::<E>()).or_insert_with(|| Box::new(Vec::<AfterFn<E>>::new()));
        if let Some(list) = list.downcast_mut::<Vec<AfterFn<E>>>() {
            list.push(hook);
        }
    }

    /// Register a start hook (runs after every module started; an error aborts the start).
    pub fn on_start<F, Fut>(&mut self, hook: F)
    where
        F: Fn(HookCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), AppError>> + Send + 'static,
    {
        let hook = Arc::new(hook);
        self.on_start.push(Arc::new(move |ctx| {
            let hook = hook.clone();
            Box::pin(async move { hook(ctx).await })
        }));
    }

    /// Register a shutdown hook (runs after in-flight requests drained, before the database closes).
    pub fn on_shutdown<F, Fut>(&mut self, hook: F)
    where
        F: Fn(HookCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let hook = Arc::new(hook);
        self.on_shutdown.push(Arc::new(move |ctx| {
            let hook = hook.clone();
            Box::pin(async move { hook(ctx).await })
        }));
    }

    /// How many `before` hooks are registered for `E`.
    pub fn before_count<E: Event>(&self) -> usize {
        self.before.get(&TypeId::of::<E>()).and_then(|l| l.downcast_ref::<Vec<BeforeFn<E>>>()).map_or(0, Vec::len)
    }

    /// How many `in_tx` hooks are registered for `E`.
    pub fn in_tx_count<E: Event>(&self) -> usize {
        self.in_tx.get(&TypeId::of::<E>()).and_then(|l| l.downcast_ref::<Vec<InTxFn<E>>>()).map_or(0, Vec::len)
    }

    /// Run the `in_tx` hooks of `E` in order inside `tx`. The first error stops the chain and is
    /// returned (roll the transaction back); a timeout is 503 `hook_timeout`, a panic 500
    /// `internal`.
    pub async fn run_in_tx<E: Event>(&self, tx: &mut DbTx, ctx: &HookCtx, event: &E) -> Result<(), AppError> {
        let Some(list) = self.in_tx.get(&TypeId::of::<E>()).and_then(|l| l.downcast_ref::<Vec<InTxFn<E>>>()) else {
            return Ok(());
        };
        for (index, hook) in list.iter().enumerate() {
            // The hook is called inside the guarded future, so a panic in its synchronous part is
            // caught too.
            let call: BoxFuture<'_, Result<(), AppError>> = Box::pin(async { hook(&mut *tx, ctx, event).await });
            match guarded(self.timeout, call).await {
                Outcome::Done(Ok(())) => {}
                Outcome::Done(Err(error)) => return Err(error),
                Outcome::TimedOut => {
                    tracing::warn!(event = E::NAME, hook = index, "in_tx hook timed out");
                    return Err(AppError::new(codes::HOOK_TIMEOUT, "a server hook did not answer in time"));
                }
                Outcome::Panicked => {
                    tracing::error!(event = E::NAME, hook = index, "in_tx hook panicked");
                    return Err(AppError::internal_plain());
                }
            }
        }
        Ok(())
    }

    /// How many `after` hooks are registered for `E`.
    pub fn after_count<E: Event>(&self) -> usize {
        self.after.get(&TypeId::of::<E>()).and_then(|l| l.downcast_ref::<Vec<AfterFn<E>>>()).map_or(0, Vec::len)
    }

    /// Run the `before` hooks of `E` in order. Returns the (possibly modified) event, or the
    /// error to answer: a rejection, a hook's own error, `hook_timeout` (503) or `internal`
    /// (500, after a panic).
    pub async fn run_before<E: Event>(&self, ctx: &HookCtx, mut event: E) -> Result<E, AppError> {
        let Some(list) = self.before.get(&TypeId::of::<E>()).and_then(|l| l.downcast_ref::<Vec<BeforeFn<E>>>()) else {
            return Ok(event);
        };
        for (index, hook) in list.iter().enumerate() {
            match guarded(self.timeout, hook(ctx.clone(), event)).await {
                Outcome::Done(Ok(Decision::Continue(next))) => event = next,
                Outcome::Done(Ok(Decision::Reject(error))) => return Err(error),
                Outcome::Done(Err(error)) => return Err(error),
                Outcome::TimedOut => {
                    tracing::warn!(event = E::NAME, hook = index, "before hook timed out");
                    return Err(AppError::new(codes::HOOK_TIMEOUT, "a server hook did not answer in time"));
                }
                Outcome::Panicked => {
                    tracing::error!(event = E::NAME, hook = index, "before hook panicked");
                    return Err(AppError::internal_plain());
                }
            }
        }
        Ok(event)
    }

    /// Run the `after` hooks of `E` in order. Errors, timeouts and panics are logged; every
    /// hook runs.
    pub async fn run_after<E: Event>(&self, ctx: &HookCtx, event: Arc<E>) {
        let Some(list) = self.after.get(&TypeId::of::<E>()).and_then(|l| l.downcast_ref::<Vec<AfterFn<E>>>()) else {
            return;
        };
        for (index, hook) in list.iter().enumerate() {
            match guarded(self.timeout, hook(ctx.clone(), event.clone())).await {
                Outcome::Done(Ok(())) => {}
                Outcome::Done(Err(error)) => tracing::warn!(event = E::NAME, hook = index, %error, "after hook failed"),
                Outcome::TimedOut => tracing::warn!(event = E::NAME, hook = index, "after hook timed out"),
                Outcome::Panicked => tracing::error!(event = E::NAME, hook = index, "after hook panicked"),
            }
        }
    }

    pub(crate) async fn run_start(&self, ctx: &HookCtx) -> Result<(), String> {
        for (index, hook) in self.on_start.iter().enumerate() {
            match guarded(self.timeout.max(Duration::from_secs(30)), hook(ctx.clone())).await {
                Outcome::Done(Ok(())) => {}
                Outcome::Done(Err(error)) => return Err(format!("start hook {index} failed: {error}")),
                Outcome::TimedOut => return Err(format!("start hook {index} timed out")),
                Outcome::Panicked => return Err(format!("start hook {index} panicked")),
            }
        }
        Ok(())
    }

    pub(crate) async fn run_shutdown(&self, ctx: &HookCtx) {
        for (index, hook) in self.on_shutdown.iter().enumerate() {
            match guarded(self.timeout.max(Duration::from_secs(10)), hook(ctx.clone())).await {
                Outcome::Done(()) => {}
                Outcome::TimedOut => tracing::warn!(hook = index, "shutdown hook timed out"),
                Outcome::Panicked => tracing::error!(hook = index, "shutdown hook panicked"),
            }
        }
    }
}
