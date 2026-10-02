//! Runtime checks (never a panic for calling from the wrong place), the private runtime thread
//! behind the blocking interface, and the park-based wait the blocking interface uses.

use std::cell::Cell;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{oneshot, Notify};

use crate::{Error, Reply};

thread_local! {
    /// Set on the client's own runtime threads (the `net-backend-client` thread and its blocking pool).
    static CLIENT_THREAD: Cell<bool> = const { Cell::new(false) };
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
/// worker or a `block_on` (tokio offers no public way to tell those from a `spawn_blocking` thread;
/// there the wait stalls that worker until the answer, as any blocking call does, but never
/// deadlocks: the client's work runs on its own thread).
pub(crate) fn refuse_on_client_thread() -> Result<(), Error> {
    if CLIENT_THREAD.with(Cell::get) {
        return Err(Error::invalid("a blocking call made on the client's own runtime thread (e.g. from an SSH prompt responder) would wait for itself"));
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
            let mut work = pin!(HANDED.scope(Arc::clone(&handed), future));
            let mut cancel = pin!(cancelled.notified());
            // The cancel first; the work is dropped with this task when the cancel wins.
            let outcome = std::future::poll_fn(|cx| {
                if cancel.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
                work.as_mut().poll(cx).map(Some)
            })
            .await;
            let answer = outcome.unwrap_or_else(|| Err(Error::Cancelled { sent: if handed.load(Ordering::SeqCst) { None } else { Some(false) } }));
            let _ = sender.send(answer);
        });
        // `notify_one` keeps the cancel for a task that is not waiting at that moment.
        reply.with_cancel(move || cancel.notify_one())
    }

    /// Run `future` on the runtime thread and block until it is done.
    pub(crate) fn block<T: Send + 'static>(&self, future: impl Future<Output = Result<T, Error>> + Send + 'static) -> Result<T, Error> {
        self.spawn(future).wait()
    }

    /// Run a synchronous closure inside the runtime's context (for code that spawns on the
    /// current runtime without awaiting).
    #[cfg(feature = "ssh")]
    pub(crate) fn enter<T>(&self, work: impl FnOnce() -> T) -> T {
        let _guard = self.handle.enter();
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
}
