//! The time limits of one request: `http.request_timeout_secs` for every route, and for routes
//! with [`upload_timeout`] an idle limit while the body arrives (`http.upload_idle_timeout_secs`)
//! plus an overall one (`http.upload_timeout_secs`).
//!
//! The request-timeout middleware puts a [`Deadline`] into every request and answers 503
//! `unavailable` when it passes. [`upload_timeout`] moves it: while the body streams in, each piece
//! of data moves the deadline to "now + idle limit" (never past the overall upload limit); once the
//! body has ended, the handler has `http.request_timeout_secs` for the rest of its work.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::BoxError;
use http_body::{Frame, SizeHint};
use tokio::sync::Notify;
use tokio::time::Instant;

/// Which limit the current deadline is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Limit {
    /// `http.request_timeout_secs`.
    Request,
    /// `http.upload_idle_timeout_secs`: no data arrived for that long.
    UploadIdle,
    /// `http.upload_timeout_secs`: the whole upload took that long.
    Upload,
}

struct Moving {
    at: Instant,
    limit: Limit,
    /// The end of the overall upload limit while an upload body streams in.
    upload_ends: Option<Instant>,
}

struct Shared {
    moving: Mutex<Moving>,
    changed: Notify,
    request: Duration,
    idle: Duration,
    total: Option<Duration>,
}

/// The deadline of one request (a request extension, shared with its body).
#[derive(Clone)]
pub(crate) struct Deadline(Arc<Shared>);

impl Deadline {
    /// A request's deadline: `request` from now. `idle` / `total` apply once the route opts in with
    /// [`upload_timeout`] (`total = None`: no overall upload limit).
    pub(crate) fn new(request: Duration, idle: Duration, total: Option<Duration>) -> Self {
        let moving = Moving { at: Instant::now() + request, limit: Limit::Request, upload_ends: None };
        Self(Arc::new(Shared { moving: Mutex::new(moving), changed: Notify::new(), request, idle, total }))
    }

    fn moving(&self) -> std::sync::MutexGuard<'_, Moving> {
        self.0.moving.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// When the request runs out of time, and which limit that is.
    pub(crate) fn current(&self) -> (Instant, Limit) {
        let moving = self.moving();
        (moving.at, moving.limit)
    }

    /// Resolves after the deadline was moved (a permit is kept if nobody waits).
    pub(crate) async fn changed(&self) {
        self.0.changed.notified().await;
    }

    /// The configured limit of `limit`.
    pub(crate) fn length(&self, limit: Limit) -> Duration {
        match limit {
            Limit::Request => self.0.request,
            Limit::UploadIdle => self.0.idle,
            Limit::Upload => self.0.total.unwrap_or(Duration::MAX),
        }
    }

    fn idle_deadline(&self, moving: &mut Moving, now: Instant) {
        let idle = now + self.0.idle;
        match moving.upload_ends {
            Some(ends) if ends <= idle => {
                moving.at = ends;
                moving.limit = Limit::Upload;
            }
            _ => {
                moving.at = idle;
                moving.limit = Limit::UploadIdle;
            }
        }
    }

    /// The upload body starts streaming: the idle and overall upload limits apply from now.
    fn upload_started(&self) {
        self.upload_started_at(Instant::now());
    }

    fn upload_started_at(&self, now: Instant) {
        {
            let mut moving = self.moving();
            moving.upload_ends = self.0.total.map(|total| now + total);
            self.idle_deadline(&mut moving, now);
        }
        self.0.changed.notify_one();
    }

    /// A piece of the upload body arrived (the deadline only moves later: no wake-up needed).
    fn data_arrived(&self) {
        self.data_arrived_at(Instant::now());
    }

    fn data_arrived_at(&self, now: Instant) {
        let mut moving = self.moving();
        if moving.limit != Limit::Request {
            self.idle_deadline(&mut moving, now);
        }
    }

    /// The upload body ended: the handler has the request limit for the rest of its work.
    fn body_finished(&self) {
        self.body_finished_at(Instant::now());
    }

    fn body_finished_at(&self, now: Instant) {
        {
            let mut moving = self.moving();
            if moving.limit == Limit::Request {
                return;
            }
            moving.at = now + self.0.request;
            moving.limit = Limit::Request;
            moving.upload_ends = None;
        }
        self.0.changed.notify_one();
    }
}

/// A request body that reports its progress to the request's [`Deadline`].
struct TimedBody {
    inner: Body,
    deadline: Deadline,
}

/// A handler that stops reading the body early (a refused upload) has the request limit for the
/// rest of its work, not the upload idle limit.
impl Drop for TimedBody {
    fn drop(&mut self) {
        self.deadline.body_finished();
    }
}

impl http_body::Body for TimedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(_))) => {
                if self.inner.is_end_stream() {
                    self.deadline.body_finished();
                } else {
                    self.deadline.data_arrived();
                }
            }
            Poll::Ready(None) => self.deadline.body_finished(),
            Poll::Ready(Some(Err(_))) | Poll::Pending => {}
        }
        // The inner error's own error (e.g. a length limit) stays the source, so extractors still
        // tell "too large" from "broken".
        polled.map(|frame| frame.map(|result| result.map_err(axum::Error::into_inner)))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Switch a request to the upload limits (without a [`Deadline`], e.g. a router used without the
/// server's middleware, the request is unchanged).
fn upload(request: Request) -> Request {
    let Some(deadline) = request.extensions().get::<Deadline>().cloned() else {
        return request;
    };
    deadline.upload_started();
    request.map(|inner| Body::new(TimedBody { inner, deadline }))
}

/// The layer of [`upload_timeout`].
#[derive(Clone, Copy, Debug, Default)]
pub struct UploadTimeoutLayer;

impl<S> tower::Layer<S> for UploadTimeoutLayer {
    type Service = UploadTimeout<S>;

    fn layer(&self, inner: S) -> Self::Service {
        UploadTimeout { inner }
    }
}

/// The service of [`UploadTimeoutLayer`].
#[derive(Clone, Debug)]
pub struct UploadTimeout<S> {
    inner: S,
}

impl<S> tower::Service<Request> for UploadTimeout<S>
where
    S: tower::Service<Request>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        self.inner.call(upload(request))
    }
}

/// Upload time limits for a route that receives large bodies, instead of
/// `http.request_timeout_secs` for the whole request: while the body arrives, the request fails
/// only when no data comes for `http.upload_idle_timeout_secs` (default 30) or the upload takes
/// longer than `http.upload_timeout_secs` (default 3600; 0 = no overall limit). Once the body has
/// ended, the handler has `http.request_timeout_secs` for the rest of its work. Either limit answers
/// 503 `unavailable`. The files module's upload route has it.
///
/// ```no_run
/// use axum::routing::post;
/// use net_backend_server::http::{body_limit, upload_timeout};
///
/// # async fn upload_replay(body: axum::body::Bytes) {}
/// // Both layers in one tuple (two `.layer` calls in a row need a type annotation).
/// let route = post(upload_replay).layer((body_limit(64 * 1024 * 1024), upload_timeout()));
/// # let _: axum::routing::MethodRouter<net_backend_server::AppState> = route;
/// ```
pub fn upload_timeout() -> UploadTimeoutLayer {
    UploadTimeoutLayer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deadline_follows_the_upload() {
        let secs = Duration::from_secs;
        let deadline = Deadline::new(secs(30), secs(10), Some(secs(100)));
        assert_eq!(deadline.current().1, Limit::Request);
        let start = Instant::now();
        deadline.upload_started_at(start);
        assert_eq!(deadline.current(), (start + secs(10), Limit::UploadIdle));
        deadline.data_arrived_at(start + secs(8));
        assert_eq!(deadline.current(), (start + secs(18), Limit::UploadIdle));
        // Near the overall limit the overall limit wins.
        deadline.data_arrived_at(start + secs(93));
        assert_eq!(deadline.current(), (start + secs(100), Limit::Upload));
        deadline.body_finished_at(start + secs(93));
        assert_eq!(deadline.current(), (start + secs(123), Limit::Request));
        // After the body, data reports change nothing.
        deadline.data_arrived_at(start + secs(200));
        assert_eq!(deadline.current(), (start + secs(123), Limit::Request));
        // Without an overall limit only the idle limit applies.
        let open = Deadline::new(secs(30), secs(10), None);
        open.upload_started_at(start);
        open.data_arrived_at(start + secs(10_000));
        assert_eq!(open.current(), (start + secs(10_010), Limit::UploadIdle));
        assert_eq!(open.length(Limit::Upload), Duration::MAX);
        assert_eq!((open.length(Limit::Request), open.length(Limit::UploadIdle)), (secs(30), secs(10)));
    }

    #[tokio::test]
    async fn the_body_reports_its_progress() {
        use http_body_util::BodyExt;
        let deadline = Deadline::new(Duration::from_secs(30), Duration::from_secs(10), None);
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))];
        let mut request = Request::new(Body::from_stream(futures_util::stream::iter(chunks)));
        request.extensions_mut().insert(deadline.clone());
        let request = upload(request);
        assert_eq!(deadline.current().1, Limit::UploadIdle);
        let bytes = request.into_body().collect().await.map(|c| c.to_bytes()).unwrap_or_default();
        assert_eq!(&bytes[..], b"abcd");
        assert_eq!(deadline.current().1, Limit::Request);
        // A request without a deadline passes unchanged.
        let plain = upload(Request::new(Body::from("x")));
        assert!(plain.extensions().get::<Deadline>().is_none());
    }

    /// NIT8: a handler that drops the body before its end gets the request limit back.
    #[tokio::test]
    async fn a_dropped_body_ends_the_upload_limits() {
        let deadline = Deadline::new(Duration::from_secs(30), Duration::from_secs(10), None);
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))];
        let mut request = Request::new(Body::from_stream(futures_util::stream::iter(chunks)));
        request.extensions_mut().insert(deadline.clone());
        let request = upload(request);
        assert_eq!(deadline.current().1, Limit::UploadIdle);
        drop(request);
        assert_eq!(deadline.current().1, Limit::Request);
    }
}
