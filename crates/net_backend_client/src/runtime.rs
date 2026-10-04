//! Runtime checks (never a panic for calling from the wrong place), the private runtime thread
//! behind the blocking interface, and the park-based wait the blocking interface uses.

use std::cell::Cell;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::{oneshot, Notify};

use crate::{Error, Reply};

thread_local! {
    /// Set on the client's own runtime threads (the `net-backend-client` thread and its blocking pool).
    static CLIENT_THREAD: Cell<bool> = const { Cell::new(false) };
    /// Set while [`RuntimeThread::enter`] runs: work spawned from there runs on the client's thread.
    static ENTERED: Cell<bool> = const { Cell::new(false) };
}

tokio::task_local! {
    /// Set inside a cancellable blocking-interface task once an HTTP request was handed to a
    /// connection (from then on a cancel cannot promise "never sent").
    static HANDED: Arc<AtomicBool>;
}

/// An HTTP request is about to be handed to a connection (it may reach the server from now on).
pub(crate) fn mark_handed() {
    let _ = HANDED.try_with(|handed| handed.store(true, Ordering::SeqCst));
}

/// The current tokio runtime, or `InvalidRequest` (an async call made outside tokio would panic
/// inside hyper / tokio).
pub(crate) fn current() -> Result<Handle, Error> {
    Handle::try_current().map_err(|_| Error::invalid("this async call needs a tokio runtime (or use net_backend_client::blocking)"))
}

/// `InvalidRequest` on the client's own runtime threads, where a blocking wait would wait for itself.
/// Every other thread may block: a plain thread, tokio's `spawn_blocking` threads, and also an async
/// worker or a `block_on` (there the wait stalls that thread until the answer, as any blocking call
/// does). The blocking interface's work runs on its own thread, so its waits always end; for the
/// async client's replies [`refuse_wait`] also refuses the one wait that could not.
pub(crate) fn refuse_on_client_thread() -> Result<(), Error> {
    if CLIENT_THREAD.with(Cell::get) {
        return Err(Error::invalid("a blocking call made on the client's own runtime thread (e.g. from an SSH prompt responder) would wait for itself"));
    }
    Ok(())
}

/// Whether work spawned from here runs on a current-thread tokio runtime that is not the client's
/// own thread: its answer cannot arrive while a blocking wait holds that runtime's thread.
pub(crate) fn spawns_on_current_thread_runtime() -> bool {
    if CLIENT_THREAD.with(Cell::get) || ENTERED.with(Cell::get) {
        return false;
    }
    Handle::try_current().is_ok_and(|handle| handle.runtime_flavor() == RuntimeFlavor::CurrentThread)
}

/// The checks before a blocking wait: refused on the client's own thread, and refused inside a
/// current-thread runtime when the answer comes from work on a current-thread runtime
/// (`produced_on_current_thread`): the wait would hold the only thread that can produce it.
pub(crate) fn refuse_wait(produced_on_current_thread: bool) -> Result<(), Error> {
    refuse_on_client_thread()?;
    if produced_on_current_thread && Handle::try_current().is_ok_and(|handle| handle.runtime_flavor() == RuntimeFlavor::CurrentThread) {
        return Err(Error::invalid(
            "a blocking wait inside a current-thread tokio runtime for work of the async client on such a runtime would never end: .await it instead",
        ));
    }
    Ok(())
}

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Wait for `future` on this thread by parking it (no tokio `block_on`: it never panics, in any context).
pub(crate) fn park_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// One private std thread (`net-backend-client`) running a current-thread tokio runtime. Every
/// blocking client clone shares it; it stops when the last clone is dropped.
pub(crate) struct RuntimeThread {
    handle: Handle,
    stop: Option<oneshot::Sender<()>>,
}

impl RuntimeThread {
    pub(crate) fn start() -> Result<Arc<Self>, Error> {
        let (ready_sender, ready) = std::sync::mpsc::channel::<Result<Handle, String>>();
        let (stop, stopped) = oneshot::channel::<()>();
        std::thread::Builder::new()
            .name("net-backend-client".into())
            .spawn(move || {
                CLIENT_THREAD.with(|flag| flag.set(true));
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .max_blocking_threads(2)
                    .thread_name("net-backend-client-io")
                    .on_thread_start(|| CLIENT_THREAD.with(|flag| flag.set(true)))
                    .build();
                match runtime {
                    Ok(runtime) => {
                        let _ = ready_sender.send(Ok(runtime.handle().clone()));
                        runtime.block_on(async {
                            let _ = stopped.await;
                        });
                        runtime.shutdown_timeout(Duration::from_secs(1));
                    }
                    Err(e) => {
                        let _ = ready_sender.send(Err(e.to_string()));
                    }
                }
            })
            .map_err(|e| Error::network(format!("could not start the client thread: {e}"), Some(false)))?;
        let handle = ready
            .recv()
            .map_err(|_| Error::network("the client thread stopped at once", Some(false)))?
            .map_err(|e| Error::network(format!("could not start the client runtime: {e}"), Some(false)))?;
        Ok(Arc::new(Self { handle, stop: Some(stop) }))
    }

    /// Run `future` on the runtime thread; its result arrives in the reply. [`Reply::cancel`] stops
    /// it and answers `Cancelled` (`sent: Some(false)` when no HTTP request had been handed to a
    /// connection, `None` after).
    pub(crate) fn spawn<T: Send + 'static>(&self, future: impl Future<Output = Result<T, Error>> + Send + 'static) -> Reply<T> {
        let (sender, reply) = Reply::channel();
        let cancel = Arc::new(Notify::new());
        let cancelled = Arc::clone(&cancel);
        self.handle.spawn(async move {
            let handed = Arc::new(AtomicBool::new(false));
            // The cancel first. The work (and whatever it holds open: a connection, a listener) is
            // dropped at the end of this block, before the answer goes out.
            let outcome = {
                let mut work = pin!(HANDED.scope(Arc::clone(&handed), future));
                let mut cancel = pin!(cancelled.notified());
                std::future::poll_fn(|cx| {
                    if cancel.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(None);
                    }
                    work.as_mut().poll(cx).map(Some)
                })
                .await
            };
            let answer = outcome.unwrap_or_else(|| Err(Error::Cancelled { sent: if handed.load(Ordering::SeqCst) { None } else { Some(false) } }));
            let _ = sender.send(answer);
        });
        // `notify_one` keeps the cancel for a task that is not waiting at that moment.
        reply.produced_on_client_thread().with_cancel(move || cancel.notify_one())
    }

    /// Run `future` on the runtime thread and block until it is done.
    pub(crate) fn block<T: Send + 'static>(&self, future: impl Future<Output = Result<T, Error>> + Send + 'static) -> Result<T, Error> {
        self.spawn(future).wait()
    }

    /// Run a synchronous closure inside the runtime's context (for code that spawns on the
    /// current runtime without awaiting).
    pub(crate) fn enter<T>(&self, work: impl FnOnce() -> T) -> T {
        /// Restores the flag (also when `work` unwinds).
        struct Entered(bool);
        impl Drop for Entered {
            fn drop(&mut self) {
                ENTERED.with(|flag| flag.set(self.0));
            }
        }
        let _guard = self.handle.enter();
        let _entered = Entered(ENTERED.with(|flag| flag.replace(true)));
        work()
    }
}

impl Drop for RuntimeThread {
    fn drop(&mut self) {
        // Not joined: the last clone may be dropped on the runtime thread itself.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl std::fmt::Debug for RuntimeThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RuntimeThread")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_client_thread_refuses_and_waiting_never_panics() {
        assert!(refuse_on_client_thread().is_ok());
        let refused = std::thread::spawn(|| {
            CLIENT_THREAD.with(|flag| flag.set(true));
            refuse_on_client_thread()
        })
        .join()
        .expect("join");
        assert!(matches!(refused, Err(Error::InvalidRequest(_))));
        // The park-based wait works inside a runtime too (tokio's blocking_recv would panic there).
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            assert!(Reply::ready(Ok(5)).wait().ok() == Some(5));
        });
    }

    #[test]
    fn a_wait_that_would_hold_its_own_producer_is_refused() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            // Answered by a task on this current-thread runtime: waiting here would never end.
            assert!(spawns_on_current_thread_runtime());
            let (sender, reply) = Reply::<u32>::channel();
            tokio::spawn(async move {
                let _ = sender.send(Ok(1));
            });
            assert!(matches!(reply.wait(), Err(Error::InvalidRequest(_))));
            // The same kind of reply awaited works.
            let (sender, reply) = Reply::<u32>::channel();
            tokio::spawn(async move {
                let _ = sender.send(Ok(2));
            });
            assert_eq!(reply.await.ok(), Some(2));
        });
        // Work on the client's own thread may be waited for inside a current-thread runtime.
        let client = RuntimeThread::start().expect("client thread");
        let reply = client.spawn(async { Ok(3u32) });
        runtime.block_on(async move {
            assert_eq!(reply.wait().ok(), Some(3));
        });
        let reply = client.enter(|| {
            assert!(!spawns_on_current_thread_runtime(), "inside enter the work runs on the client's thread");
            let (sender, reply) = Reply::<u32>::channel();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = sender.send(Ok(4));
            });
            reply
        });
        assert!(!ENTERED.with(Cell::get), "the flag is restored");
        runtime.block_on(async move {
            assert_eq!(reply.wait().ok(), Some(4));
        });
        // A multi-thread runtime's work may be waited for inside a current-thread runtime.
        let multi = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("runtime");
        let reply = {
            let _inside = multi.enter();
            assert!(!spawns_on_current_thread_runtime());
            let (sender, reply) = Reply::<u32>::channel();
            multi.spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = sender.send(Ok(5));
            });
            reply
        };
        runtime.block_on(async move {
            assert_eq!(reply.wait().ok(), Some(5));
        });
    }
}
