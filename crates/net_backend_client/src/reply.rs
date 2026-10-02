//! [`Reply`]: one answer on its way, usable as a future, polled from a game loop, or waited for.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::oneshot;

use crate::Error;

/// The answer to one request that is on its way: exactly one `Result` arrives.
///
/// - **async:** `.await` it;
/// - **game loop:** call [`try_take`](Self::try_take) every frame (never blocks, no runtime needed);
/// - **blocking:** [`wait`](Self::wait).
///
/// Dropping it does not cancel the request (it was already handed over); the answer is discarded.
/// If whatever answers it is gone (the client shut down), the answer is [`Error::Shutdown`].
#[must_use = "a Reply carries the request's one answer"]
pub struct Reply<T> {
    receiver: Option<oneshot::Receiver<Result<T, Error>>>,
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply").field("taken", &self.receiver.is_none()).finish()
    }
}

impl<T> Reply<T> {
    pub(crate) fn channel() -> (oneshot::Sender<Result<T, Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (sender, Self { receiver: Some(receiver) })
    }

    /// A reply that is already answered.
    #[cfg_attr(not(feature = "ws"), allow(dead_code))]
    pub(crate) fn ready(result: Result<T, Error>) -> Self {
        let (sender, reply) = Self::channel();
        let _ = sender.send(result);
        reply
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

    /// Block this thread until the answer arrives. Works on any thread (also tokio's
    /// `spawn_blocking` threads); in async code `.await` the reply instead (waiting there stalls that
    /// worker). Refused (`InvalidRequest`, never a panic) only on the client's own runtime thread.
    pub fn wait(mut self) -> Result<T, Error> {
        crate::runtime::refuse_on_client_thread()?;
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

#[cfg(test)]
mod tests {
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
}
