//! [`Reply`]: one answer on its way, usable as a future, polled from a game loop, or waited for;
//! [`CancelHandle`]: a way to cancel that request.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::oneshot;

use crate::Error;

/// The answer to one request that is on its way: exactly one `Result` arrives.
///
/// - **async:** `.await` it;
/// - **game loop:** call [`try_take`](Self::try_take) every frame (never blocks, no runtime needed);
/// - **blocking:** [`wait`](Self::wait).
///
/// [`cancel`](Self::cancel) cancels the request (a [`blocking::Client::send`](crate::blocking::Client::send),
/// or a WebSocket request with feature `ws`): it is then answered [`Error::Cancelled`], unless its
/// answer arrived first. Dropping a `Reply` does not cancel the request (it was already handed
/// over); the answer is discarded. If whatever answers it is gone (the client shut down), the
/// answer is [`Error::Shutdown`].
#[must_use = "a Reply carries the request's one answer"]
pub struct Reply<T> {
    receiver: Option<oneshot::Receiver<Result<T, Error>>>,
    cancel: CancelHandle,
    /// The answer comes from work on a current-thread runtime other than the client's own thread
    /// (a blocking wait inside such a runtime is refused).
    on_current_thread: bool,
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply").field("taken", &self.receiver.is_none()).field("cancellable", &self.cancel.hook.is_some()).finish()
    }
}

impl<T> Reply<T> {
    /// A reply answered through the sender by work spawned from here (on the current runtime).
    pub(crate) fn channel() -> (oneshot::Sender<Result<T, Error>>, Self) {
        Self::channel_from(crate::runtime::spawns_on_current_thread_runtime())
    }

    /// A reply answered through the sender by work that runs on a current-thread runtime other
    /// than the client's own thread when `on_current_thread` is set.
    pub(crate) fn channel_from(on_current_thread: bool) -> (oneshot::Sender<Result<T, Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (sender, Self { receiver: Some(receiver), cancel: CancelHandle { hook: None }, on_current_thread })
    }

    /// This reply, answered by work on the client's own runtime thread (every other thread may
    /// wait for it).
    pub(crate) fn produced_on_client_thread(mut self) -> Self {
        self.on_current_thread = false;
        self
    }

    /// A reply that is already answered.
    #[cfg_attr(not(feature = "ws"), allow(dead_code))]
    pub(crate) fn ready(result: Result<T, Error>) -> Self {
        let (sender, reply) = Self::channel();
        let _ = sender.send(result);
        reply
    }

    /// This reply, cancelled by `hook` (which makes whatever answers the request answer
    /// `Cancelled`).
    pub(crate) fn with_cancel(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.cancel = CancelHandle { hook: Some(Arc::new(hook)) };
        self
    }

    /// The answer, if it arrived; `None` while it is still on its way, and after it was taken.
    pub fn try_take(&mut self) -> Option<Result<T, Error>> {
        let receiver = self.receiver.as_mut()?;
        match receiver.try_recv() {
            Ok(result) => {
                self.receiver = None;
                Some(result)
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                self.receiver = None;
                Some(Err(Error::Shutdown))
            }
        }
    }

    /// Whether the answer was taken already.
    pub fn is_taken(&self) -> bool {
        self.receiver.is_none()
    }

    /// Cancel the request. Its answer becomes [`Error::Cancelled`] (`sent` says whether it had
    /// gone out), delivered like any answer, right after the client's task saw the cancel; an
    /// answer that arrived before stays. A request that had not gone out is never sent. Does
    /// nothing for a reply that cannot be cancelled (see [`Reply`]) or that was answered.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// A handle that cancels this request from elsewhere (another task, a UI callback) while the
    /// reply is awaited.
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// Block this thread until the answer arrives. Works on any thread (also tokio's
    /// `spawn_blocking` threads); in async code `.await` the reply instead (waiting there stalls that
    /// thread). Refused with `InvalidRequest` (never a panic, never a hang):
    /// - on the client's own runtime thread (e.g. inside an SSH prompt responder);
    /// - inside a current-thread tokio runtime (`flavor = "current_thread"`, a `block_on`) for an
    ///   unanswered reply of the async [`Client`](crate::Client) whose work runs on a
    ///   current-thread runtime: that work could not go on while this thread waits. `.await` it.
    ///
    /// A reply of the [`blocking::Client`](crate::blocking::Client) (its work runs on its own
    /// thread) can be waited for anywhere else.
    pub fn wait(mut self) -> Result<T, Error> {
        crate::runtime::refuse_on_client_thread()?;
        if let Some(answer) = self.try_take() {
            return answer;
        }
        crate::runtime::refuse_wait(self.on_current_thread)?;
        match self.receiver.take() {
            Some(receiver) => crate::runtime::park_on(receiver).unwrap_or(Err(Error::Shutdown)),
            None => Err(Error::invalid("the answer was already taken")),
        }
    }
}

impl<T> Future for Reply<T> {
    type Output = Result<T, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(receiver) = self.receiver.as_mut() else { return Poll::Ready(Err(Error::invalid("the answer was already taken"))) };
        match Pin::new(receiver).poll(cx) {
            Poll::Ready(result) => {
                self.receiver = None;
                Poll::Ready(result.unwrap_or(Err(Error::Shutdown)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Cancels one request ([`Reply::cancel_handle`]); cheap to clone, usable from any thread. Never
/// blocks. Cancelling an answered request does nothing.
#[derive(Clone)]
pub struct CancelHandle {
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelHandle").field("cancellable", &self.hook.is_some()).finish()
    }
}

impl CancelHandle {
    /// Cancel the request (see [`Reply::cancel`]).
    pub fn cancel(&self) {
        if let Some(hook) = &self.hook {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn exactly_one_answer() {
        let (sender, mut reply) = Reply::<u32>::channel();
        assert!(reply.try_take().is_none());
        let _ = sender.send(Ok(7));
        assert_eq!(reply.try_take().map(|r| r.ok()), Some(Some(7)));
        assert!(reply.try_take().is_none() && reply.is_taken());
        let (sender, mut reply) = Reply::<u32>::channel();
        drop(sender);
        assert!(matches!(reply.try_take(), Some(Err(Error::Shutdown))));
        assert_eq!(Reply::ready(Ok(3)).wait().ok(), Some(3));
    }

    #[test]
    fn cancel_calls_the_hook_and_a_plain_reply_ignores_it() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let (_sender, reply) = Reply::<u32>::channel();
        let reply = reply.with_cancel(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let handle = reply.cancel_handle();
        reply.cancel();
        handle.cancel();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        Reply::ready(Ok(1)).cancel();
        assert!(format!("{reply:?}").contains("cancellable: true"));
    }
}
