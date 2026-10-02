//! The WebSocket (feature `ws`): one connection per client carrying typed requests and answers
//! ([`WsCall`]), server pushes ([`ServerPush`]), heartbeats and reconnects that obey the protocol's
//! close codes.
//!
//! ```no_run
//! use net_backend_client::protocol::chat::{ChatMessage, JoinRoom, SendMessage};
//! use net_backend_client::ws::{WsEvent, WsSettings};
//! use net_backend_client::Client;
//!
//! # async fn run(client: Client) -> Result<(), net_backend_client::Error> {
//! let ws = client.connect_ws(WsSettings::default()).await?;
//! let mut messages = ws.subscribe::<ChatMessage>();
//! let room = ws.request(&JoinRoom::new("world")).await?;
//! ws.request(&SendMessage::new(room.id, "hello")).await?;
//! while let Some(message) = messages.next().await {
//!     println!("{:?}", message?.text);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! **Rules** (the server's, see its API reference):
//! - Authentication: the `Authorization: Bearer` header on the handshake by default
//!   ([`WsAuthMode`]), or the first-message `auth`. The token is refreshed first when it is about
//!   to expire; a handshake refused with 401 (`token_expired` / `unauthorized`) gets ONE refresh
//!   and one more try.
//! - **Never** an automatic reconnect after a close code 4000–4099, except ONE refresh + one new
//!   connection after 4001 (a revoked token); 4003 (banned), 4009 (replaced), 4010 (unsupported
//!   protocol) and a second 4001 end the connection ([`WsEvent::Closed`]).
//! - Every other loss (1000, 1001, 1006, 1008, 1009, 1011, 1013, network, heartbeat) reconnects
//!   with exponential backoff and full jitter ([`Reconnect`]); a 429 / 503 handshake waits at least
//!   its `Retry-After`.
//! - **After every reconnect** ([`WsEvent::Connected`] with `reconnected: true`) join your chat
//!   rooms again and reload what you may have missed: membership ends with each connection, and
//!   pushes sent while the link was down are gone. The client does not do this for you (it does not
//!   know which rooms still matter).
//! - Every request gets exactly one answer. A request that went out on a link that was then lost
//!   is answered [`Error::Disconnected`] with `sent: Some(true)` (it is never resent: it may have
//!   run); requests made while reconnecting wait (until their timeout) and go out on the new link.

mod link;
mod task;

use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use net_backend_protocol::{ServerPush, WsCall, WsRequestFrame};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;

use crate::runtime::RuntimeThread;
use crate::{Client, Error, Reply, MAX_TIMEOUT};

pub(crate) use task::{Command, Pending};

/// How the socket authenticates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WsAuthMode {
    /// `Authorization: Bearer <token>` on the handshake (default): checked before the upgrade.
    #[default]
    Header,
    /// The first-message `auth` (`{"type":"auth","data":{"token":…}}`), answered `auth.ok` /
    /// `auth.failed`; requests wait for `auth.ok`. What browsers must use; works everywhere.
    FirstMessage,
    /// Both: the header, and the `auth` message (whose `auth.ok` is awaited).
    Both,
}

/// Automatic reconnects: exponential backoff with full jitter. The delay before attempt `n` is a
/// random value in `0..=min(cap, base · 2^(n-1))`; the counter resets once a connection stayed up
/// for `stable_after`. Defaults: base 500 ms, cap 30 s, no attempt limit, stable after 10 s.
#[derive(Clone, Debug)]
pub struct Reconnect {
    base: Duration,
    cap: Duration,
    max_attempts: Option<u32>,
    stable_after: Duration,
    jitter: bool,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self { base: Duration::from_millis(500), cap: Duration::from_secs(30), max_attempts: None, stable_after: Duration::from_secs(10), jitter: true }
    }
}

impl Reconnect {
    /// The first delay bound (default 500 ms, 1 ms..=1 h).
    pub fn with_base(mut self, base: Duration) -> Self {
        self.base = base.clamp(Duration::from_millis(1), MAX_TIMEOUT);
        self
    }

    /// The largest delay (default 30 s, at most 1 h).
    pub fn with_cap(mut self, cap: Duration) -> Self {
        self.cap = cap.min(MAX_TIMEOUT);
        self
    }

    /// Give up after this many failed attempts in a row (`None` = never; default).
    pub fn with_max_attempts(mut self, max: Option<u32>) -> Self {
        self.max_attempts = max;
        self
    }

    /// How long a connection must stay up before the attempt counter resets (default 10 s).
    pub fn with_stable_after(mut self, stable_after: Duration) -> Self {
        self.stable_after = stable_after.min(MAX_TIMEOUT);
        self
    }

    /// Random jitter on (default) or off (exact delays, for tests).
    pub fn with_jitter(mut self, jitter: bool) -> Self {
        self.jitter = jitter;
        self
    }

    /// The upper bound of the delay before attempt `attempt` (1-based).
    pub fn delay_bound(&self, attempt: u32) -> Duration {
        let factor = 2u32.checked_pow(attempt.saturating_sub(1).min(30)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap.max(self.base))
    }

    pub(crate) fn delay(&self, attempt: u32, random: u64) -> Duration {
        let bound = self.delay_bound(attempt);
        if !self.jitter {
            return bound;
        }
        let nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
        Duration::from_nanos(random % nanos.saturating_add(1))
    }

    pub(crate) fn may_retry(&self, attempt: u32) -> bool {
        self.max_attempts.is_none_or(|max| attempt <= max)
    }
}

/// WebSocket settings ([`Client::connect_ws`]). Private fields + builder.
#[derive(Clone, Debug)]
pub struct WsSettings {
    pub(crate) auth: WsAuthMode,
    pub(crate) connect_timeout: Duration,
    pub(crate) request_timeout: Duration,
    pub(crate) ping_interval: Duration,
    pub(crate) dead_after: Duration,
    pub(crate) reconnect: Option<Reconnect>,
    pub(crate) push_buffer: usize,
    pub(crate) event_buffer: usize,
    pub(crate) max_message_bytes: usize,
    pub(crate) max_pending: usize,
}

impl Default for WsSettings {
    fn default() -> Self {
        Self {
            auth: WsAuthMode::Header,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(15),
            dead_after: Duration::from_secs(45),
            reconnect: Some(Reconnect::default()),
            push_buffer: 256,
            event_buffer: 64,
            max_message_bytes: net_backend_protocol::envelope::MAX_MESSAGE_BYTES,
            max_pending: 256,
        }
    }
}

impl WsSettings {
    /// How the socket authenticates (default [`WsAuthMode::Header`]).
    pub fn with_auth(mut self, auth: WsAuthMode) -> Self {
        self.auth = auth;
        self
    }

    /// One deadline for TCP connect + TLS + handshake (+ `auth.ok` with first-message auth)
    /// (default 10 s, 100 ms..=1 h).
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout.clamp(Duration::from_millis(100), MAX_TIMEOUT);
        self
    }

    /// The default time a request waits for its answer, from the moment it is made (default 10 s,
    /// 1 ms..=1 h); waiting for a reconnect included.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout.clamp(Duration::from_millis(1), MAX_TIMEOUT);
        self
    }

    /// Heartbeat: a ping every `interval` (default 15 s); the link counts as dead (and reconnects)
    /// after `dead_after` without any frame from the server (default 45 s; the server pings every
    /// 20 s). Both 10 ms..=1 h; `dead_after` at least `interval`.
    pub fn with_heartbeat(mut self, interval: Duration, dead_after: Duration) -> Self {
        self.ping_interval = interval.clamp(Duration::from_millis(10), MAX_TIMEOUT);
        self.dead_after = dead_after.clamp(self.ping_interval, MAX_TIMEOUT);
        self
    }

    /// Reconnect policy (default on, [`Reconnect::default`]).
    pub fn with_reconnect(mut self, reconnect: Reconnect) -> Self {
        self.reconnect = Some(reconnect);
        self
    }

    /// Never reconnect: the first loss ends the connection ([`WsEvent::Closed`]).
    pub fn without_reconnect(mut self) -> Self {
        self.reconnect = None;
        self
    }

    /// How many pushes each push stream buffers (default 256, at least 1); a stream that falls
    /// further behind gets [`Error::Lagged`].
    pub fn with_push_buffer(mut self, pushes: usize) -> Self {
        self.push_buffer = pushes.max(1);
        self
    }

    /// How many events each event stream buffers (default 64, at least 1).
    pub fn with_event_buffer(mut self, events: usize) -> Self {
        self.event_buffer = events.max(1);
        self
    }

    /// The largest message in both directions (default 1 MiB, the server's limit; at least 1 KiB).
    /// A bigger request is refused unsent ([`Error::RequestTooLarge`]).
    pub fn with_max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = bytes.max(1024);
        self
    }

    /// How many requests may wait or run at once (default 256, at least 1); more are answered
    /// `InvalidRequest` at once, never sent.
    pub fn with_max_pending(mut self, requests: usize) -> Self {
        self.max_pending = requests.max(1);
        self
    }
}

/// The state of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WsState {
    /// Connected and authenticated: requests go out at once.
    Connected,
    /// The link was lost; the next attempt starts after `retry_in` (requests wait for it).
    Reconnecting {
        /// The number of the next attempt (1 for the first retry).
        attempt: u32,
        /// How long until it starts.
        retry_in: Duration,
    },
    /// Connecting again (the attempt is running).
    Connecting,
    /// Closed for good (by the app, a permanent close code, or out of attempts): see the last
    /// [`WsEvent::Closed`].
    Closed,
}

/// What happened to a connection ([`WsConnection::events`]).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum WsEvent {
    /// Connected (again). With `reconnected: true`: join your rooms again and resync.
    Connected {
        /// Whether this is a reconnect (pushes may have been missed meanwhile).
        reconnected: bool,
    },
    /// The link was lost (or an attempt failed); the next attempt starts after `retry_in`.
    Reconnecting {
        /// The number of the next attempt (1 for the first retry).
        attempt: u32,
        /// How long until it starts.
        retry_in: Duration,
        /// Why the link was lost or the attempt failed.
        error: Error,
    },
    /// Closed for good: `None` = closed by the app; otherwise why ([`Error::Closed`] with the close
    /// code, [`Error::SessionEnded`], the last attempt's error, …). The last event.
    Closed {
        /// Why, unless the app closed it.
        error: Option<Error>,
    },
}

/// One server push as received: its kind and its `data`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct WsPush {
    /// The `type` (e.g. `chat.message`, or a game's own).
    pub kind: String,
    /// The `data`.
    pub data: Value,
}

impl WsPush {
    /// The data decoded as a typed push.
    pub fn decode<P: ServerPush>(&self) -> Result<P, Error> {
        P::deserialize(&self.data).map_err(|e| Error::Decode { status: None, message: e.to_string() })
    }
}

pub(crate) struct Shared {
    pub(crate) commands: mpsc::UnboundedSender<Command>,
    pub(crate) next_id: AtomicU64,
    pub(crate) state: watch::Receiver<WsState>,
    /// Templates for new subscriptions (only the task holds the senders, so streams end with it).
    pub(crate) events: Mutex<broadcast::Receiver<WsEvent>>,
    pub(crate) pushes: Mutex<broadcast::Receiver<Arc<WsPush>>>,
    pub(crate) settings: WsSettings,
    /// Keeps the blocking interface's runtime thread alive while the connection is used.
    pub(crate) _runtime: Option<Arc<RuntimeThread>>,
}

/// A WebSocket connection (cheap to clone; clones share it). A background task owns the socket:
/// it answers requests, fans out pushes, sends heartbeats and reconnects. The connection closes
/// (code 1000) when [`close`](Self::close) is called or the last clone is dropped.
///
/// Everything here also works from a game loop without a runtime: requests return a [`Reply`]
/// ([`Reply::try_take`]), streams have `try_next`.
#[derive(Clone)]
pub struct WsConnection {
    shared: Arc<Shared>,
}

impl fmt::Debug for WsConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsConnection").field("state", &self.state()).finish()
    }
}

impl Client {
    /// Open the WebSocket (`/v1/ws`, `wss://` for an `https://` server). Waits for the first
    /// connection: an error here leaves nothing running. Later losses reconnect by themselves
    /// (see the [`ws`](crate::ws) rules).
    pub async fn connect_ws(&self, settings: WsSettings) -> Result<WsConnection, Error> {
        crate::runtime::current()?;
        task::connect(self.clone(), settings, None).await
    }
}

impl crate::blocking::Client {
    /// Open the WebSocket and block until the first connection is up (see
    /// [`Client::connect_ws`]). The connection runs on the client's runtime thread; use it with
    /// [`Reply::try_take`] / `try_next` from a game loop.
    pub fn connect_ws(&self, settings: WsSettings) -> Result<WsConnection, Error> {
        let client = self.inner.clone();
        let runtime = Arc::clone(&self.runtime);
        self.runtime.block(async move { task::connect(client, settings, Some(runtime)).await })
    }
}

impl WsConnection {
    pub(crate) fn new(shared: Shared) -> Self {
        Self { shared: Arc::new(shared) }
    }

    /// Send a typed request; the [`Reply`] carries its answer (`C::Response`, or the server's error
    /// as [`Error::Api`]). The request is queued at once (answers come in the order requests were
    /// made); the default timeout applies ([`WsSettings::with_request_timeout`]).
    /// [`Reply::cancel`] answers it [`Error::Cancelled`]: `sent: Some(false)` when it was still
    /// waiting for the connection (it is never sent), `Some(true)` when it had been written (it may
    /// have run; its late answer is dropped).
    pub fn request<C: WsCall>(&self, call: &C) -> Reply<C::Response>
    where
        C::Response: Send + 'static,
    {
        self.request_with_timeout(call, self.shared.settings.request_timeout)
    }

    /// [`request`](Self::request) with its own timeout (1 ms..=1 h).
    pub fn request_with_timeout<C: WsCall>(&self, call: &C, timeout: Duration) -> Reply<C::Response>
    where
        C::Response: Send + 'static,
    {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let text = match serde_json::to_string(&WsRequestFrame::new(id, C::KIND, call)) {
            Ok(text) => text,
            Err(e) => return Reply::ready(Err(Error::invalid(format!("the request cannot be encoded: {e}")))),
        };
        let (sender, reply) = Reply::channel();
        let answer = Box::new(move |result: Result<Value, Error>| {
            let typed =
                result.and_then(|value| serde_json::from_value::<C::Response>(value).map_err(|e| Error::Decode { status: None, message: e.to_string() }));
            let _ = sender.send(typed);
        });
        self.enqueue(id, text, timeout, answer);
        reply.with_cancel(self.cancel_hook(id))
    }

    /// A request of any kind with untyped JSON (a game's own kinds without a [`WsCall`] type).
    pub fn request_raw(&self, kind: &str, data: Value) -> Reply<Value> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let text = match serde_json::to_string(&WsRequestFrame::new(id, kind, data)) {
            Ok(text) => text,
            Err(e) => return Reply::ready(Err(Error::invalid(format!("the request cannot be encoded: {e}")))),
        };
        let (sender, reply) = Reply::channel();
        let answer = Box::new(move |result: Result<Value, Error>| {
            let _ = sender.send(result);
        });
        self.enqueue(id, text, self.shared.settings.request_timeout, answer);
        reply.with_cancel(self.cancel_hook(id))
    }

    /// What [`Reply::cancel`] does for request `id`: tell the connection task, which answers it
    /// `Cancelled` (never sent if it was still waiting; a late answer is dropped). A weak sender:
    /// a reply never keeps the connection open.
    fn cancel_hook(&self, id: u64) -> impl Fn() + Send + Sync + 'static {
        let commands = self.shared.commands.downgrade();
        move || {
            if let Some(commands) = commands.upgrade() {
                let _ = commands.send(Command::Cancel(id));
            }
        }
    }

    fn enqueue(&self, id: u64, text: String, timeout: Duration, answer: Box<dyn FnOnce(Result<Value, Error>) + Send>) {
        let limit = self.shared.settings.max_message_bytes;
        if text.len() > limit {
            answer(Err(Error::RequestTooLarge { limit: limit as u64, size: text.len() as u64 }));
            return;
        }
        let now = Instant::now();
        let deadline = now.checked_add(timeout.clamp(Duration::from_millis(1), MAX_TIMEOUT)).unwrap_or(now);
        let pending = Pending { id, text, deadline, answer };
        if let Err(mpsc::error::SendError(Command::Request(pending))) = self.shared.commands.send(Command::Request(pending)) {
            (pending.answer)(Err(Error::disconnected("the connection is closed", Some(false))));
        }
    }

    /// Typed pushes of one kind (`P::KIND`), e.g. `subscribe::<ChatMessage>()`. Only pushes that
    /// arrive after this call; each stream has its own buffer.
    pub fn subscribe<P: ServerPush>(&self) -> PushStream<P> {
        PushStream { receiver: self.shared.pushes.lock().unwrap_or_else(PoisonError::into_inner).resubscribe(), kind: Some(P::KIND), _type: PhantomData }
    }

    /// Every push, untyped ([`WsPush`]).
    pub fn pushes(&self) -> PushStream<WsPush> {
        PushStream { receiver: self.shared.pushes.lock().unwrap_or_else(PoisonError::into_inner).resubscribe(), kind: None, _type: PhantomData }
    }

    /// What happens to the connection (connected, reconnecting, closed). Only events after this call.
    pub fn events(&self) -> WsEvents {
        WsEvents { receiver: self.shared.events.lock().unwrap_or_else(PoisonError::into_inner).resubscribe() }
    }

    /// The current state.
    pub fn state(&self) -> WsState {
        *self.shared.state.borrow()
    }

    /// Whether it is closed for good.
    pub fn is_closed(&self) -> bool {
        self.state() == WsState::Closed
    }

    /// Close (code 1000), for every clone. Waiting requests are answered `Disconnected`
    /// (`sent: Some(false)`), running ones `Disconnected` (`sent: Some(true)`). Never blocks.
    pub fn close(&self) {
        let _ = self.shared.commands.send(Command::Close);
    }

    /// Wait until the connection is closed for good (async).
    pub async fn closed(&self) {
        let mut state = self.shared.state.clone();
        let _ = state.wait_for(|s| *s == WsState::Closed).await;
    }
}

/// A stream of pushes ([`WsConnection::subscribe`], [`WsConnection::pushes`]).
pub struct PushStream<T> {
    receiver: broadcast::Receiver<Arc<WsPush>>,
    kind: Option<&'static str>,
    _type: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for PushStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushStream").field("kind", &self.kind).finish()
    }
}

/// How a stream turns a received push into its item.
pub trait PushItem: Sized {
    /// The item, or `None` when the push is not for this stream.
    #[doc(hidden)]
    fn from_push(push: &WsPush, kind: Option<&str>) -> Option<Result<Self, Error>>;
}

impl<P: ServerPush> PushItem for P {
    fn from_push(push: &WsPush, kind: Option<&str>) -> Option<Result<Self, Error>> {
        (kind == Some(push.kind.as_str())).then(|| push.decode::<P>())
    }
}

impl<T: PushItem> PushStream<T> {
    /// The next push: `Some(Ok(push))`, `Some(Err(Error::Lagged))` when the stream fell behind
    /// (pushes were dropped for it: resync), `Some(Err(Error::Decode))` for a push of this kind that
    /// does not decode, `None` once the connection is closed for good and the buffer is empty.
    pub async fn next(&mut self) -> Option<Result<T, Error>> {
        loop {
            match self.receiver.recv().await {
                Ok(push) => {
                    if let Some(item) = T::from_push(&push, self.kind) {
                        return Some(item);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => return Some(Err(Error::Lagged { missed })),
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// The next push if one is buffered (never blocks; no runtime needed). `None`: nothing now (or
    /// the connection is closed and the buffer empty).
    pub fn try_next(&mut self) -> Option<Result<T, Error>> {
        loop {
            match self.receiver.try_recv() {
                Ok(push) => {
                    if let Some(item) = T::from_push(&push, self.kind) {
                        return Some(item);
                    }
                }
                Err(broadcast::error::TryRecvError::Lagged(missed)) => return Some(Err(Error::Lagged { missed })),
                Err(_) => return None,
            }
        }
    }
}

impl PushItem for WsPush {
    fn from_push(push: &WsPush, _kind: Option<&str>) -> Option<Result<Self, Error>> {
        Some(Ok(push.clone()))
    }
}

/// A stream of [`WsEvent`]s ([`WsConnection::events`]).
pub struct WsEvents {
    receiver: broadcast::Receiver<WsEvent>,
}

impl fmt::Debug for WsEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WsEvents")
    }
}

impl WsEvents {
    /// The next event; `None` after the connection closed for good (its `Closed` event was
    /// delivered first). A reader that fell behind skips the oldest events.
    pub async fn next(&mut self) -> Option<WsEvent> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// The next event if one is buffered (never blocks; no runtime needed).
    pub fn try_next(&mut self) -> Option<WsEvent> {
        loop {
            match self.receiver.try_recv() {
                Ok(event) => return Some(event),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded() {
        let policy = Reconnect::default().with_base(Duration::from_millis(100)).with_cap(Duration::from_secs(2)).with_jitter(false);
        assert_eq!(policy.delay_bound(1), Duration::from_millis(100));
        assert_eq!(policy.delay_bound(3), Duration::from_millis(400));
        assert_eq!(policy.delay_bound(40), Duration::from_secs(2));
        assert_eq!(policy.delay(u32::MAX, u64::MAX), Duration::from_secs(2));
        let jitter = Reconnect::default().with_base(Duration::MAX).with_cap(Duration::MAX);
        assert!(jitter.delay(5, u64::MAX) <= MAX_TIMEOUT);
        assert!(Reconnect::default().with_max_attempts(Some(2)).may_retry(2));
        assert!(!Reconnect::default().with_max_attempts(Some(2)).may_retry(3));
        let settings = WsSettings::default().with_heartbeat(Duration::from_secs(20), Duration::from_secs(1));
        assert_eq!(settings.dead_after, Duration::from_secs(20), "dead_after is at least the interval");
    }
}
