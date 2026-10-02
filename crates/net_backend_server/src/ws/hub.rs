//! The hub: the registry of open sockets (by connection, by user, by room), pushes, rooms, caps,
//! revocations, role refreshes and the shutdown sequence.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::time::{Duration, Instant};

use axum::extract::ws::Utf8Bytes;
use http::StatusCode;
use net_backend_protocol::{codes, CloseCode, ServerPush, UnixMillis, UserId, WsPushFrame};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::handlers::HandlerMap;
use crate::auth::events::Revocation;
use crate::auth::{AuthService, Authenticator};
use crate::config::WsConfig;
use crate::error::AppError;
use crate::rate_limit::{ip_key, KeyedBuckets, RateDecision, DEFAULT_IPV6_PREFIX};
use crate::state::AppState;

/// The longest room name, in bytes.
pub const MAX_ROOM_NAME_BYTES: usize = 128;

/// How long the hub remembers revocations, to refuse a socket whose token was checked just before
/// its session (or its user) was revoked. A token checked longer ago than this is checked again
/// before its socket is registered.
const RECENT_REVOCATIONS: Duration = Duration::from_secs(30);

/// The most revocations the hub remembers; evicting a younger one than the window forces a
/// re-check of every token checked before it.
const RECENT_REVOCATIONS_MAX: usize = 4096;

/// Users per role query of the periodic role refresh.
const ROLE_REFRESH_CHUNK: usize = 500;

/// A socket's id: unique in this process, never reused while it runs. Not unique across
/// instances: a [`Target::Connection`] push never leaves the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// The number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// An id for tests of handlers (`WsCtx::for_tests`); never one of a real socket's.
    #[doc(hidden)]
    pub const fn for_tests(n: u64) -> Self {
        Self(n)
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Who a push goes to. Only authenticated sockets receive pushes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Target {
    /// One socket of THIS instance (never published to other instances: ids are per process).
    Connection(ConnectionId),
    /// Every socket of a user.
    User(UserId),
    /// Every socket in a room.
    Room(Arc<str>),
    /// Every authenticated socket.
    All,
}

/// A change of the hub's state that a [`Delivery`] carries to every instance instead of a frame
/// (applied to the sockets its target names).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Control {
    /// The target's sockets leave this room (e.g. a member removed from a group: on every
    /// instance, not only the one that removed it).
    LeaveRoom(Arc<str>),
}

/// An encoded frame and where it goes, or a [`Control`] change: what a [`Broadcaster`] publishes.
/// Serializes as `{"target":…,"frame":"<the JSON text>"}` (plus `"control":…` for a control
/// delivery, whose frame is empty) for a pub/sub transport.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Delivery {
    /// Who receives it.
    pub target: Target,
    /// The frame's JSON text (encoded once, shared by every socket); empty for a control delivery.
    pub frame: Utf8Bytes,
    /// A change of the hub's state instead of a frame ([`Delivery::control`]); `None` for a frame.
    pub control: Option<Control>,
}

impl Delivery {
    /// A delivery of `frame` to `target`.
    pub fn new(target: Target, frame: impl Into<Utf8Bytes>) -> Self {
        Self { target, frame: frame.into(), control: None }
    }

    /// A control delivery: `control` applied to the sockets `target` names, on every instance.
    pub fn control(target: Target, control: Control) -> Self {
        Self { target, frame: Utf8Bytes::from_static(""), control: Some(control) }
    }
}

#[derive(Serialize)]
struct DeliveryOut<'a> {
    target: &'a Target,
    frame: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    control: Option<&'a Control>,
}

#[derive(Deserialize)]
struct DeliveryIn {
    target: Target,
    frame: String,
    #[serde(default)]
    control: Option<Control>,
}

impl Serialize for Delivery {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        DeliveryOut { target: &self.target, frame: self.frame.as_str(), control: self.control.as_ref() }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Delivery {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = DeliveryIn::deserialize(deserializer)?;
        let mut delivery = Delivery::new(raw.target, raw.frame);
        delivery.control = raw.control;
        Ok(delivery)
    }
}

/// The seam for running several server instances: pushes to users, rooms and everyone go through
/// it ([`Target::Connection`] pushes stay in this process: connection ids are per instance), and
/// so do [`Control`] deliveries (e.g. "these sockets leave this room", made by
/// [`Hub::remove_from_room`]).
///
/// The default [`LocalBroadcaster`] delivers in this process. A pub/sub implementation (e.g.
/// Redis / Valkey) publishes each [`Delivery`] AS A WHOLE (it is serde-serializable, `control`
/// included) to every instance and, on each instance, hands what arrives to the [`LocalDelivery`]
/// it got in [`start`](Broadcaster::start); it must never drop or rewrite the `control` field.
/// Everything else is per instance: rooms, `is_online`, `connections_of`, `close` / `close_user`,
/// the connection caps.
///
/// Only [`publish`](Broadcaster::publish) is required; [`start`](Broadcaster::start) has a default.
pub trait Broadcaster: Send + Sync + 'static {
    /// Called once when the server starts serving, with the sink that delivers to this
    /// instance's sockets (keep it to deliver what other instances publish).
    fn start(&self, local: LocalDelivery) {
        let _ = local;
    }

    /// Publish a delivery (never a `Target::Connection` one). Must not block (spawn any I/O);
    /// `local` delivers in this process.
    fn publish(&self, local: &LocalDelivery, delivery: Delivery);
}

/// Delivers in this process only (the default).
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalBroadcaster;

impl Broadcaster for LocalBroadcaster {
    fn publish(&self, local: &LocalDelivery, delivery: Delivery) {
        local.deliver(&delivery);
    }
}

/// Delivers a [`Delivery`] to this process's sockets (given to [`Broadcaster`]s).
#[derive(Clone)]
pub struct LocalDelivery(Weak<Inner>);

impl fmt::Debug for LocalDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalDelivery")
    }
}

impl LocalDelivery {
    /// Queue the frame on every local socket the target names; returns how many got it. A socket
    /// whose outbox is full is closed with 1013 instead; a frame over `ws.max_message_bytes` is
    /// dropped (logged). A control delivery is applied to those sockets instead (the count: how
    /// many it changed).
    pub fn deliver(&self, delivery: &Delivery) -> usize {
        match self.0.upgrade() {
            Some(inner) => Hub(inner).deliver_local(delivery),
            None => 0,
        }
    }
}

/// Why a push could not be sent.
#[derive(Debug)]
#[non_exhaustive]
pub enum PushError {
    /// The kind is empty or reserved for authentication (`auth`, `auth.ok`, `auth.failed`).
    ReservedKind,
    /// The payload could not be encoded as JSON.
    Encode(serde_json::Error),
    /// The encoded frame is larger than `ws.max_message_bytes` (clients close for such a frame).
    #[non_exhaustive]
    TooLarge {
        /// The frame's size in bytes.
        size: usize,
        /// The limit.
        limit: usize,
    },
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PushError::ReservedKind => f.write_str("a push kind must not be empty or an auth kind"),
            PushError::Encode(error) => write!(f, "the push could not be encoded: {error}"),
            PushError::TooLarge { size, limit } => write!(f, "the push is {size} bytes, over ws.max_message_bytes ({limit})"),
        }
    }
}

impl std::error::Error for PushError {}

impl From<PushError> for AppError {
    fn from(error: PushError) -> Self {
        AppError::internal(error)
    }
}

/// Why a socket could not join a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum JoinError {
    /// The socket is gone (or not authenticated).
    NotConnected,
    /// The room has as many members as its cap.
    RoomFull,
    /// The socket is in `ws.max_rooms_per_connection` rooms already.
    TooManyRooms,
    /// The room name is empty, longer than [`MAX_ROOM_NAME_BYTES`] or has control characters.
    InvalidRoom,
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            JoinError::NotConnected => "the connection is gone",
            JoinError::RoomFull => "the room is full",
            JoinError::TooManyRooms => "too many rooms joined on this connection",
            JoinError::InvalidRoom => "invalid room name",
        })
    }
}

impl std::error::Error for JoinError {}

/// `room_full` (409), `quota_exceeded` (403), `bad_request` (400) or `unavailable` (503).
impl From<JoinError> for AppError {
    fn from(error: JoinError) -> Self {
        match error {
            JoinError::RoomFull => AppError::new(codes::ROOM_FULL, "the room is full"),
            JoinError::TooManyRooms => AppError::new(codes::QUOTA_EXCEEDED, "too many rooms joined on this connection"),
            JoinError::InvalidRoom => AppError::bad_request("invalid room name"),
            JoinError::NotConnected => AppError::with_status(StatusCode::SERVICE_UNAVAILABLE, codes::UNAVAILABLE, "the connection is gone"),
        }
    }
}

/// A snapshot of one socket.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ConnectionInfo {
    /// The socket.
    pub id: ConnectionId,
    /// The user, once authenticated.
    pub user_id: Option<UserId>,
    /// The session of the token it authenticated with.
    pub session_id: Option<i64>,
    /// The user's roles as last known (refreshed on role changes).
    pub roles: Vec<String>,
    /// The client address (trusted-proxy aware).
    pub ip: Option<IpAddr>,
    /// When the socket opened.
    pub connected_at: UnixMillis,
    /// The rooms it is in.
    pub rooms: Vec<Arc<str>>,
}

/// Counts of the hub, for dashboards and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HubStats {
    /// Open sockets (incl. handshakes in progress and unauthenticated ones).
    pub connections: usize,
    /// Sockets not authenticated yet.
    pub pending: usize,
    /// Authenticated sockets.
    pub authenticated: usize,
    /// Distinct users with at least one socket.
    pub users: usize,
    /// Rooms with at least one member.
    pub rooms: usize,
}

tokio::task_local! {
    /// The socket whose request handler is running in this task (set by the socket's task while it
    /// polls the handler): pushes that handler makes to its own socket follow its answer.
    pub(crate) static HANDLING: ConnectionId;
}

/// A frame in a socket's outbox.
pub(crate) struct Outgoing {
    pub(crate) frame: Utf8Bytes,
    /// Pushed by this socket's running handler: written after the handler's answer.
    pub(crate) after_answer: bool,
}

/// A close request: code and reason.
pub(crate) type CloseRequest = Option<(CloseCode, Cow<'static, str>)>;

/// A code that may go into a close frame: 1000–1003, 1007–1014, 3000–4999; anything else
/// (1004–1006 and 1015 are reserved, below 1000 invalid) becomes 1011.
pub(crate) fn sendable(code: CloseCode) -> CloseCode {
    match code.get() {
        1000..=1003 | 1007..=1014 | 3000..=4999 => code,
        _ => CloseCode::INTERNAL_ERROR,
    }
}

struct Conn {
    outbox: mpsc::Sender<Outgoing>,
    close: watch::Sender<CloseRequest>,
    user: Option<UserId>,
    session: Option<i64>,
    roles: Vec<String>,
    ip: Option<IpAddr>,
    rooms: Vec<Arc<str>>,
    connected_at: UnixMillis,
}

impl Conn {
    fn request_close(&self, code: CloseCode, reason: Cow<'static, str>) -> bool {
        let code = sendable(code);
        self.close.send_if_modified(|current| {
            if current.is_none() {
                *current = Some((code, reason));
                true
            } else {
                false
            }
        })
    }
}

#[derive(Default)]
struct Registry {
    conns: HashMap<ConnectionId, Conn>,
    users: HashMap<UserId, Vec<ConnectionId>>,
    rooms: HashMap<Arc<str>, HashSet<ConnectionId>>,
    /// Recent revocations (at, revocation), under the same lock as `set_user`.
    recent: VecDeque<(Instant, Revocation)>,
    /// The newest revocation evicted from `recent` within the window (the cap overflowed): a token
    /// checked at or before it must be checked again.
    overflow_at: Option<Instant>,
}

/// Told when the hub took a socket out of a room on a [`Control::LeaveRoom`] (the chat module's
/// presence), after the registry lock is released.
pub(crate) type RoomLeft = Arc<dyn Fn(&Hub, ConnectionId, UserId, &str) + Send + Sync>;

pub(crate) struct Inner {
    config: WsConfig,
    room_left: RwLock<Vec<RoomLeft>>,
    registry: RwLock<Registry>,
    next_id: AtomicU64,
    /// Reserved slots: accepted handshakes + open sockets.
    live: AtomicUsize,
    /// Slots not authenticated yet.
    pending: AtomicUsize,
    /// Open slots per client address key.
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    closing: AtomicBool,
    deadline: Mutex<Option<Instant>>,
    kill: watch::Sender<bool>,
    broadcaster: Arc<dyn Broadcaster>,
    pub(crate) handlers: HandlerMap,
    pub(crate) authenticators: Arc<[Arc<dyn Authenticator>]>,
    handshakes: Option<KeyedBuckets<IpAddr>>,
    metrics: bool,
    /// `RECENT_REVOCATIONS` in milliseconds (shorter only in tests).
    revocation_window_ms: AtomicU64,
}

/// The WebSocket hub: pushes, rooms and the open sockets. Cheap to clone. Get it with
/// [`AppState::ws`] (also `State<Hub>` in handlers, [`WsCtx::hub`](super::WsCtx::hub) in WebSocket
/// handlers). Everything it knows is per instance (see [`Broadcaster`]).
#[derive(Clone)]
pub struct Hub(pub(crate) Arc<Inner>);

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub").field("stats", &self.stats()).field("kinds", &self.0.handlers.len()).finish_non_exhaustive()
    }
}

/// A reserved connection slot (released on drop): the global count, the per-address count and,
/// until the socket authenticates, the pending count.
pub(crate) struct Slot {
    hub: Hub,
    ip: Option<IpAddr>,
    pending: AtomicBool,
}

impl Slot {
    /// The socket authenticated: it no longer counts as pending.
    pub(crate) fn authenticated(&self) {
        if self.pending.swap(false, Ordering::SeqCst) {
            self.hub.0.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.authenticated();
        if let Some(ip) = self.ip {
            let mut per_ip = self.hub.0.per_ip.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(n) = per_ip.get_mut(&ip) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    per_ip.remove(&ip);
                }
            }
        }
        let left = self.hub.0.live.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
        if self.hub.0.metrics {
            metrics::gauge!("nbs_ws_connections").set(left as f64);
        }
    }
}

/// Why a slot could not be reserved.
pub(crate) enum Refusal {
    /// `ws.max_connections`.
    Full,
    /// `ws.max_pending_connections`.
    TooManyPending,
    /// `ws.max_connections_per_ip`.
    TooManyFromAddress,
}

/// A registered socket: unregistered (rooms, user index) on drop, then its slot is released.
pub(crate) struct Registered {
    pub(crate) id: ConnectionId,
    hub: Hub,
    pub(crate) slot: Slot,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.hub.unregister(self.id);
    }
}

/// The receiving ends a socket's task owns.
pub(crate) struct ConnHandle {
    pub(crate) outbox: mpsc::Receiver<Outgoing>,
    pub(crate) close: watch::Receiver<CloseRequest>,
    pub(crate) kill: watch::Receiver<bool>,
}

fn valid_room(room: &str) -> bool {
    !room.is_empty() && room.len() <= MAX_ROOM_NAME_BYTES && !room.chars().any(char::is_control)
}

impl Hub {
    pub(crate) fn new(
        config: WsConfig,
        handlers: HandlerMap,
        authenticators: Arc<[Arc<dyn Authenticator>]>,
        broadcaster: Arc<dyn Broadcaster>,
        metrics: bool,
    ) -> Self {
        let handshakes =
            (config.handshakes_per_ip_per_minute > 0).then(|| KeyedBuckets::new(config.handshakes_per_ip_per_minute, Duration::from_secs(60), 100_000));
        Hub(Arc::new(Inner {
            config,
            room_left: RwLock::new(Vec::new()),
            registry: RwLock::new(Registry::default()),
            next_id: AtomicU64::new(1),
            live: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            per_ip: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
            deadline: Mutex::new(None),
            kill: watch::channel(false).0,
            broadcaster,
            handlers,
            authenticators,
            handshakes,
            metrics,
            revocation_window_ms: AtomicU64::new(u64::try_from(RECENT_REVOCATIONS.as_millis()).unwrap_or(30_000)),
        }))
    }

    /// How long revocations are remembered (see `RECENT_REVOCATIONS`).
    fn revocation_window(&self) -> Duration {
        Duration::from_millis(self.0.revocation_window_ms.load(Ordering::Relaxed))
    }

    /// Shorten how long revocations are remembered (default 30 s), so a test can check the re-check
    /// of tokens checked longer ago. Not for production use.
    #[doc(hidden)]
    pub fn set_revocation_window_for_tests(&self, window: Duration) {
        self.0.revocation_window_ms.store(u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1), Ordering::Relaxed);
    }

    /// Whether a token checked at `checked_at` must be checked again before its socket is
    /// registered: longer ago than the revocation window, or before a revocation the full memory
    /// had to evict.
    pub(crate) fn needs_recheck(&self, checked_at: Instant) -> bool {
        Self::stale(&self.read(), checked_at, self.revocation_window())
    }

    fn stale(registry: &Registry, checked_at: Instant, window: Duration) -> bool {
        checked_at.elapsed() >= window || registry.overflow_at.is_some_and(|at| checked_at <= at)
    }

    fn read(&self) -> RwLockReadGuard<'_, Registry> {
        self.0.registry.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Registry> {
        self.0.registry.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The `[ws]` settings.
    pub fn config(&self) -> &WsConfig {
        &self.0.config
    }

    /// The sink delivering to this process's sockets.
    pub fn local(&self) -> LocalDelivery {
        LocalDelivery(Arc::downgrade(&self.0))
    }

    pub(crate) fn metrics(&self) -> bool {
        self.0.metrics
    }

    // ---- pushes -------------------------------------------------------------------------------

    /// Encode a typed push once: `{"type":P::KIND,"data":…}` (no size check; the push calls check).
    pub fn encode_push<P: ServerPush>(push: &P) -> Result<Utf8Bytes, PushError> {
        Self::encode_raw(P::KIND, push)
    }

    /// Encode a push of any kind once (auth kinds and the empty kind are refused).
    pub fn encode_raw<T: Serialize + ?Sized>(kind: &str, data: &T) -> Result<Utf8Bytes, PushError> {
        let frame = WsPushFrame::checked(kind, data).ok_or(PushError::ReservedKind)?;
        serde_json::to_string(&frame).map(Utf8Bytes::from).map_err(PushError::Encode)
    }

    fn check_size(&self, frame: &Utf8Bytes) -> Result<(), PushError> {
        let (size, limit) = (frame.as_str().len(), self.0.config.max_message_bytes);
        if size > limit {
            tracing::warn!(size, limit, "WebSocket push refused: larger than ws.max_message_bytes");
            if self.0.metrics {
                metrics::counter!("nbs_ws_dropped_frames_total", "reason" => "push_too_big").increment(1);
            }
            return Err(PushError::TooLarge { size, limit });
        }
        Ok(())
    }

    /// Push a typed message (encoded once) to `target`.
    pub fn push<P: ServerPush>(&self, target: Target, push: &P) -> Result<(), PushError> {
        self.publish(Delivery::new(target, Self::encode_push(push)?))
    }

    /// Push to every socket of `user`.
    pub fn push_user<P: ServerPush>(&self, user: UserId, push: &P) -> Result<(), PushError> {
        self.push(Target::User(user), push)
    }

    /// Push to every socket in `room`.
    pub fn push_room<P: ServerPush>(&self, room: &str, push: &P) -> Result<(), PushError> {
        self.push(Target::Room(Arc::from(room)), push)
    }

    /// Push to every authenticated socket.
    pub fn push_all<P: ServerPush>(&self, push: &P) -> Result<(), PushError> {
        self.push(Target::All, push)
    }

    /// Push to one socket of this instance.
    pub fn push_connection<P: ServerPush>(&self, connection: ConnectionId, push: &P) -> Result<(), PushError> {
        self.push(Target::Connection(connection), push)
    }

    /// Push a message of any kind (a game's own, untyped) to `target`.
    pub fn push_raw<T: Serialize + ?Sized>(&self, target: Target, kind: &str, data: &T) -> Result<(), PushError> {
        self.publish(Delivery::new(target, Self::encode_raw(kind, data)?))
    }

    /// Publish an encoded delivery: refused when over `ws.max_message_bytes`; a
    /// `Target::Connection` delivery is delivered here directly, everything else goes through the
    /// [`Broadcaster`].
    pub fn publish(&self, delivery: Delivery) -> Result<(), PushError> {
        self.check_size(&delivery.frame)?;
        if matches!(delivery.target, Target::Connection(_)) {
            self.deliver_local(&delivery);
        } else {
            self.0.broadcaster.publish(&self.local(), delivery);
        }
        Ok(())
    }

    fn deliver_local(&self, delivery: &Delivery) -> usize {
        if let Some(control) = &delivery.control {
            return self.apply_control(&delivery.target, control);
        }
        if self.check_size(&delivery.frame).is_err() {
            return 0;
        }
        let handling = HANDLING.try_with(|id| *id).ok();
        let registry = self.read();
        let mut sent = 0usize;
        let mut slow = 0usize;
        let mut send = |(id, conn): (&ConnectionId, &Conn)| {
            if conn.user.is_none() {
                return;
            }
            let outgoing = Outgoing { frame: delivery.frame.clone(), after_answer: handling == Some(*id) };
            match conn.outbox.try_send(outgoing) {
                Ok(()) => sent += 1,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if conn.request_close(CloseCode::TRY_AGAIN_LATER, Cow::Borrowed("too slow to read its messages")) {
                        slow += 1;
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        };
        match &delivery.target {
            Target::Connection(id) => registry.conns.get_key_value(id).into_iter().for_each(&mut send),
            Target::User(user) => registry.users.get(user).into_iter().flatten().filter_map(|id| registry.conns.get_key_value(id)).for_each(&mut send),
            Target::Room(room) => registry.rooms.get(room).into_iter().flatten().filter_map(|id| registry.conns.get_key_value(id)).for_each(&mut send),
            Target::All => registry.conns.iter().for_each(&mut send),
        }
        drop(registry);
        if slow > 0 {
            tracing::warn!(sockets = slow, "WebSocket outbox full: closing slow sockets with 1013");
            if self.0.metrics {
                metrics::counter!("nbs_ws_slow_consumers_total").increment(slow as u64);
            }
        }
        sent
    }

    /// Apply a control delivery to the local sockets `target` names; how many it changed.
    fn apply_control(&self, target: &Target, control: &Control) -> usize {
        match control {
            Control::LeaveRoom(room) => {
                let ids: Vec<ConnectionId> = {
                    let registry = self.read();
                    match target {
                        Target::Connection(id) => vec![*id],
                        Target::User(user) => registry.users.get(user).cloned().unwrap_or_default(),
                        Target::Room(name) => registry.rooms.get(name).map(|m| m.iter().copied().collect()).unwrap_or_default(),
                        Target::All => registry.conns.keys().copied().collect(),
                    }
                };
                let mut left = Vec::new();
                for id in ids {
                    if self.leave(id, room) {
                        if let Some(user) = self.read().conns.get(&id).and_then(|c| c.user) {
                            left.push((id, user));
                        }
                    }
                }
                let listeners: Vec<RoomLeft> = self.0.room_left.read().unwrap_or_else(|p| p.into_inner()).clone();
                for (id, user) in &left {
                    for listener in &listeners {
                        listener(self, *id, *user, room);
                    }
                }
                left.len()
            }
        }
    }

    /// Be told when a [`Control::LeaveRoom`] took a socket out of a room.
    #[cfg_attr(not(feature = "chat"), allow(dead_code))]
    pub(crate) fn on_room_left(&self, listener: RoomLeft) {
        self.0.room_left.write().unwrap_or_else(|p| p.into_inner()).push(listener);
    }

    /// Take every socket of `user` out of `room` on EVERY instance (a [`Control::LeaveRoom`]
    /// delivery through the [`Broadcaster`]); here at once with the default broadcaster.
    pub fn remove_from_room(&self, user: UserId, room: &str) -> Result<(), PushError> {
        self.publish(Delivery::control(Target::User(user), Control::LeaveRoom(Arc::from(room))))
    }

    // ---- rooms ----------------------------------------------------------------------------------

    /// Put an authenticated socket into `room` (cap: `ws.max_room_members`). `Ok(true)` if it
    /// joined, `Ok(false)` if it already was a member. Membership ends with the socket.
    pub fn join(&self, connection: ConnectionId, room: impl Into<Arc<str>>) -> Result<bool, JoinError> {
        self.join_with_cap(connection, room, self.0.config.max_room_members)
    }

    /// [`join`](Self::join) with this room's own member cap (e.g. a large world room).
    pub fn join_with_cap(&self, connection: ConnectionId, room: impl Into<Arc<str>>, cap: usize) -> Result<bool, JoinError> {
        let room = room.into();
        if !valid_room(&room) {
            return Err(JoinError::InvalidRoom);
        }
        let max_rooms = self.0.config.max_rooms_per_connection;
        let mut registry = self.write();
        let Registry { conns, rooms, .. } = &mut *registry;
        let conn = conns.get_mut(&connection).filter(|c| c.user.is_some()).ok_or(JoinError::NotConnected)?;
        if conn.rooms.iter().any(|r| **r == *room) {
            return Ok(false);
        }
        if conn.rooms.len() >= max_rooms {
            return Err(JoinError::TooManyRooms);
        }
        let members = rooms.entry(room.clone()).or_default();
        if members.len() >= cap {
            if members.is_empty() {
                rooms.remove(&room);
            }
            return Err(JoinError::RoomFull);
        }
        members.insert(connection);
        conn.rooms.push(room);
        Ok(true)
    }

    /// Take a socket out of `room`; `true` if it was a member.
    pub fn leave(&self, connection: ConnectionId, room: &str) -> bool {
        let mut registry = self.write();
        let Registry { conns, rooms, .. } = &mut *registry;
        let Some(conn) = conns.get_mut(&connection) else { return false };
        let Some(index) = conn.rooms.iter().position(|r| &**r == room) else { return false };
        conn.rooms.swap_remove(index);
        if let Some(members) = rooms.get_mut(room) {
            members.remove(&connection);
            if members.is_empty() {
                rooms.remove(room);
            }
        }
        true
    }

    /// The sockets in `room`.
    pub fn room_members(&self, room: &str) -> Vec<ConnectionId> {
        self.read().rooms.get(room).map(|m| m.iter().copied().collect()).unwrap_or_default()
    }

    /// How many sockets are in `room`.
    pub fn room_size(&self, room: &str) -> usize {
        self.read().rooms.get(room).map_or(0, HashSet::len)
    }

    /// The rooms a socket is in.
    pub fn rooms_of(&self, connection: ConnectionId) -> Vec<Arc<str>> {
        self.read().conns.get(&connection).map(|c| c.rooms.clone()).unwrap_or_default()
    }

    // ---- sockets and users ----------------------------------------------------------------------

    /// A snapshot of one socket.
    pub fn connection(&self, connection: ConnectionId) -> Option<ConnectionInfo> {
        self.read().conns.get(&connection).map(|c| ConnectionInfo {
            id: connection,
            user_id: c.user,
            session_id: c.session,
            roles: c.roles.clone(),
            ip: c.ip,
            connected_at: c.connected_at,
            rooms: c.rooms.clone(),
        })
    }

    /// Every socket of a user (oldest first).
    pub fn connections_of(&self, user: UserId) -> Vec<ConnectionId> {
        self.read().users.get(&user).cloned().unwrap_or_default()
    }

    /// Whether the user has an authenticated socket on this instance.
    pub fn is_online(&self, user: UserId) -> bool {
        self.read().users.get(&user).is_some_and(|c| !c.is_empty())
    }

    /// Counts.
    pub fn stats(&self) -> HubStats {
        let connections = self.0.live.load(Ordering::SeqCst);
        let pending = self.0.pending.load(Ordering::SeqCst);
        let registry = self.read();
        HubStats {
            connections,
            pending,
            authenticated: registry.conns.values().filter(|c| c.user.is_some()).count(),
            users: registry.users.len(),
            rooms: registry.rooms.len(),
        }
    }

    /// Close one socket with `code` (the first close requested wins; a code that may not be sent
    /// — below 1000, 1004–1006, 1015 — becomes 1011). `false` if it is gone.
    pub fn close(&self, connection: ConnectionId, code: CloseCode, reason: impl Into<Cow<'static, str>>) -> bool {
        self.read().conns.get(&connection).is_some_and(|c| c.request_close(code, reason.into()))
    }

    /// Close every socket of a user on this instance; returns how many.
    pub fn close_user(&self, user: UserId, code: CloseCode, reason: impl Into<Cow<'static, str>>) -> usize {
        let reason = reason.into();
        let registry = self.read();
        registry.users.get(&user).into_iter().flatten().filter_map(|id| registry.conns.get(id)).filter(|c| c.request_close(code, reason.clone())).count()
    }

    /// The current roles of a socket's user (refreshed on role changes).
    pub(crate) fn roles_of(&self, connection: ConnectionId) -> Option<Vec<String>> {
        self.read().conns.get(&connection).map(|c| c.roles.clone())
    }

    /// Replace the roles of every socket of `user`.
    pub(crate) fn set_roles(&self, user: UserId, roles: &[String]) {
        let mut registry = self.write();
        let Registry { conns, users, .. } = &mut *registry;
        for id in users.get(&user).into_iter().flatten() {
            if let Some(conn) = conns.get_mut(id) {
                if conn.roles != roles {
                    conn.roles = roles.to_vec();
                }
            }
        }
    }

    // ---- the socket lifecycle (crate) -------------------------------------------------------------

    /// Whether the hub is shutting down (new handshakes are refused).
    pub(crate) fn is_closing(&self) -> bool {
        self.0.closing.load(Ordering::SeqCst)
    }

    /// A handshake from `ip`: allowed by the per-address rate limit?
    pub(crate) fn handshake_allowed(&self, ip: Option<IpAddr>) -> RateDecision {
        match (&self.0.handshakes, ip) {
            (Some(buckets), Some(ip)) => buckets.check(ip_key(ip, DEFAULT_IPV6_PREFIX)),
            _ => RateDecision::Allow,
        }
    }

    /// Reserve a slot: under `ws.max_connections`, `ws.max_connections_per_ip` and, for a socket
    /// that is not authenticated yet, `ws.max_pending_connections`.
    pub(crate) fn try_reserve(&self, ip: Option<IpAddr>, pending: bool) -> Result<Slot, Refusal> {
        let config = &self.0.config;
        let key = ip.map(|ip| ip_key(ip, DEFAULT_IPV6_PREFIX));
        if pending && self.0.pending.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < config.max_pending_connections).then_some(n + 1)).is_err() {
            return Err(Refusal::TooManyPending);
        }
        let undo_pending = || {
            if pending {
                self.0.pending.fetch_sub(1, Ordering::SeqCst);
            }
        };
        if let Some(key) = key {
            let mut per_ip = self.0.per_ip.lock().unwrap_or_else(|p| p.into_inner());
            let n = per_ip.entry(key).or_insert(0);
            if *n >= config.max_connections_per_ip {
                if *n == 0 {
                    per_ip.remove(&key);
                }
                drop(per_ip);
                undo_pending();
                return Err(Refusal::TooManyFromAddress);
            }
            *n += 1;
        }
        let slot = Slot { hub: self.clone(), ip: key, pending: AtomicBool::new(pending) };
        match self.0.live.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < config.max_connections).then_some(n + 1)) {
            Ok(reserved) => {
                if self.0.metrics {
                    metrics::gauge!("nbs_ws_connections").set((reserved + 1) as f64);
                }
                Ok(slot)
            }
            Err(_) => {
                // The slot's drop gives back the address and pending counts; live was not taken.
                self.0.live.fetch_add(1, Ordering::SeqCst);
                drop(slot);
                Err(Refusal::Full)
            }
        }
    }

    /// Register an upgraded socket.
    pub(crate) fn register(&self, slot: Slot, ip: Option<IpAddr>, now: UnixMillis) -> (Registered, ConnHandle) {
        let id = ConnectionId(self.0.next_id.fetch_add(1, Ordering::Relaxed));
        let (outbox_tx, outbox) = mpsc::channel(self.0.config.outbox_frames.max(1));
        let (close_tx, close) = watch::channel(None);
        let kill = self.0.kill.subscribe();
        let conn = Conn { outbox: outbox_tx, close: close_tx, user: None, session: None, roles: Vec::new(), ip, rooms: Vec::new(), connected_at: now };
        self.write().conns.insert(id, conn);
        // A shutdown that began meanwhile still reaches this socket.
        if self.is_closing() {
            self.close(id, CloseCode::GOING_AWAY, "the server is shutting down");
        }
        (Registered { id, hub: self.clone(), slot }, ConnHandle { outbox, close, kill })
    }

    /// Mark a socket as authenticated for `user` / `session` with `roles`, unless a revocation
    /// covering it arrived after `checked_at` (when the token was checked): then `Err` with that
    /// revocation's close code (4001 / 4003). `Err(1013)` when the check is too old to tell (see
    /// [`needs_recheck`](Self::needs_recheck)): check the token again. When the user then has more than
    /// `ws.max_connections_per_user` sockets, the oldest are closed with 4009, those of the same
    /// session first.
    pub(crate) fn set_user(&self, id: ConnectionId, user: UserId, session: Option<i64>, roles: Vec<String>, checked_at: Instant) -> Result<(), CloseCode> {
        let cap = self.0.config.max_connections_per_user.max(1);
        let window = self.revocation_window();
        let mut registry = self.write();
        if Self::stale(&registry, checked_at, window) {
            return Err(CloseCode::TRY_AGAIN_LATER);
        }
        if let Some((_, revocation)) = registry.recent.iter().rev().find(|(at, r)| *at >= checked_at && r.applies_to(user, session)) {
            return Err(revocation.close_code());
        }
        let Registry { conns, users, .. } = &mut *registry;
        let Some(conn) = conns.get_mut(&id) else { return Ok(()) };
        conn.user = Some(user);
        conn.session = session;
        conn.roles = roles;
        let list = users.entry(user).or_default();
        if !list.contains(&id) {
            list.push(id);
        }
        let excess = list.len().saturating_sub(cap);
        if excess > 0 {
            // Oldest first, the same session before the others; never the new socket.
            let same = |c: &ConnectionId| conns.get(c).is_some_and(|conn| session.is_some() && conn.session == session);
            let mut order: Vec<ConnectionId> = list.iter().copied().filter(|c| *c != id && same(c)).collect();
            order.extend(list.iter().copied().filter(|c| *c != id && !same(c)));
            for old in order.into_iter().take(excess) {
                if let Some(conn) = conns.get(&old) {
                    conn.request_close(CloseCode::REPLACED, Cow::Borrowed("replaced by a newer connection"));
                }
            }
        }
        Ok(())
    }

    fn unregister(&self, id: ConnectionId) {
        let mut registry = self.write();
        let Some(conn) = registry.conns.remove(&id) else { return };
        if let Some(user) = conn.user {
            if let Some(list) = registry.users.get_mut(&user) {
                list.retain(|c| *c != id);
                if list.is_empty() {
                    registry.users.remove(&user);
                }
            }
        }
        for room in &conn.rooms {
            if let Some(members) = registry.rooms.get_mut(room) {
                members.remove(&id);
                if members.is_empty() {
                    registry.rooms.remove(room);
                }
            }
        }
    }

    // ---- revocations ------------------------------------------------------------------------------

    /// Close every socket a revocation covers (4001, a ban 4003) and remember it (so a socket
    /// authenticating with a token checked before it is refused); returns how many closed.
    pub(crate) fn apply_revocation(&self, revocation: &Revocation) -> usize {
        let code = revocation.close_code();
        let reason = if code == CloseCode::BANNED { "the account is banned" } else { "the session was revoked" };
        let window = self.revocation_window();
        let mut registry = self.write();
        let now = Instant::now();
        while registry.recent.front().is_some_and(|(at, _)| now.duration_since(*at) > window) {
            registry.recent.pop_front();
        }
        while registry.recent.len() >= RECENT_REVOCATIONS_MAX {
            // Still inside the window: tokens checked before it can no longer be judged here.
            if let Some((at, _)) = registry.recent.pop_front() {
                registry.overflow_at = Some(registry.overflow_at.map_or(at, |o| o.max(at)));
            }
        }
        registry.recent.push_back((now, *revocation));
        let closed = registry
            .users
            .get(&revocation.user_id)
            .into_iter()
            .flatten()
            .filter_map(|id| registry.conns.get(id))
            .filter(|c| c.user.is_some_and(|u| revocation.applies_to(u, c.session)))
            .filter(|c| c.request_close(code, Cow::Borrowed(reason)))
            .count();
        drop(registry);
        if closed > 0 {
            tracing::info!(user = revocation.user_id.get(), sockets = closed, code = code.get(), "WebSocket: closing revoked sockets");
        }
        closed
    }

    async fn refresh_roles(&self, state: &AppState, service: &AuthService, users: Vec<UserId>) {
        for chunk in users.chunks(ROLE_REFRESH_CHUNK) {
            match service.roles_of_users(state, chunk).await {
                Ok(roles) => {
                    for user in chunk {
                        self.set_roles(*user, roles.get(user).map(Vec::as_slice).unwrap_or_default());
                    }
                }
                Err(error) => tracing::warn!(%error, "WebSocket: refreshing roles failed"),
            }
        }
    }

    // ---- start / shutdown -------------------------------------------------------------------------

    /// Start the hub's background work: the broadcaster, the revocation sink and the role listener
    /// (with the [`Auth`](crate::Auth) module) and the shutdown watcher. Revocations are applied
    /// synchronously by the revoking call itself (also those the revocation poll reads back from
    /// other processes), so a socket that authenticates after a ban returned is refused.
    pub(crate) fn start(&self, state: &AppState) -> Vec<JoinHandle<()>> {
        self.0.broadcaster.start(self.local());
        let mut tasks = Vec::new();
        if let Some(service) = state.get::<AuthService>() {
            // A weak handle: the service lives in the state the hub's sockets hold.
            let weak = Arc::downgrade(&self.0);
            service.add_revocation_sink(Arc::new(move |revocation: &Revocation| {
                if let Some(inner) = weak.upgrade() {
                    Hub(inner).apply_revocation(revocation);
                }
            }));
            let hub = self.clone();
            let state = state.clone();
            let mut role_changes = service.subscribe_role_changes();
            let refresh_every = self.0.config.roles_refresh_secs;
            tasks.push(tokio::spawn(async move {
                let period = Duration::from_secs(refresh_every.max(1));
                let mut refresh = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = state.shutdown().wait() => break,
                        changed = role_changes.recv() => match changed {
                            Ok(user) => hub.refresh_roles(&state, &service, vec![user]).await,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let users: Vec<UserId> = hub.read().users.keys().copied().collect();
                                hub.refresh_roles(&state, &service, users).await;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        },
                        _ = refresh.tick(), if refresh_every > 0 => {
                            let users: Vec<UserId> = hub.read().users.keys().copied().collect();
                            hub.refresh_roles(&state, &service, users).await;
                        }
                    }
                }
            }));
        }
        let hub = self.clone();
        let shutdown = state.shutdown().clone();
        let grace = Duration::from_secs(state.config().server.shutdown_grace_secs);
        tasks.push(tokio::spawn(async move {
            shutdown.wait().await;
            hub.begin_shutdown(grace);
        }));
        tasks
    }

    /// Refuse new handshakes and close every socket with 1001 (idempotent).
    pub(crate) fn begin_shutdown(&self, grace: Duration) {
        if self.0.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.0.deadline.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now() + grace);
        let registry = self.read();
        let count = registry.conns.values().filter(|c| c.request_close(CloseCode::GOING_AWAY, Cow::Borrowed("the server is shutting down"))).count();
        drop(registry);
        if count > 0 {
            tracing::info!(sockets = count, ">>> NBS: closing WebSockets (1001)");
        }
    }

    /// Wait for the sockets to close until the grace deadline, then end the rest.
    pub(crate) async fn finish(&self, grace: Duration) {
        self.begin_shutdown(grace);
        let deadline = self.0.deadline.lock().unwrap_or_else(|p| p.into_inner()).unwrap_or_else(|| Instant::now() + grace);
        while self.0.live.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let left = self.0.live.load(Ordering::SeqCst);
        if left > 0 {
            tracing::warn!(sockets = left, "shutdown grace period over; the remaining WebSockets are dropped");
            self.0.kill.send_replace(true);
            let until = Instant::now() + Duration::from_secs(2);
            while self.0.live.load(Ordering::SeqCst) > 0 && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

impl axum::extract::FromRef<AppState> for Hub {
    fn from_ref(state: &AppState) -> Hub {
        state.ws().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_are_sendable() {
        for (code, sent) in [
            (1000, 1000),
            (1001, 1001),
            (1004, 1011),
            (1005, 1011),
            (1006, 1011),
            (1015, 1011),
            (999, 1011),
            (2999, 1011),
            (4001, 4001),
            (4999, 4999),
            (5000, 1011),
        ] {
            assert_eq!(sendable(CloseCode(code)).get(), sent, "{code}");
        }
    }

    #[test]
    fn deliveries_serialize() {
        let delivery = Delivery::new(Target::Room(Arc::from("lobby")), r#"{"type":"x","data":1}"#);
        let json = serde_json::to_string(&delivery).unwrap_or_default();
        assert_eq!(json, r#"{"target":{"Room":"lobby"},"frame":"{\"type\":\"x\",\"data\":1}"}"#);
        let back: Delivery = serde_json::from_str(&json).unwrap_or_else(|_| Delivery::new(Target::All, ""));
        assert_eq!(back.target, delivery.target);
        assert_eq!(back.frame.as_str(), delivery.frame.as_str());
        assert_eq!(back.control, None);
        let leave = Delivery::control(Target::User(UserId(7)), Control::LeaveRoom(Arc::from("chat:3")));
        let json = serde_json::to_string(&leave).unwrap_or_default();
        assert_eq!(json, r#"{"target":{"User":7},"frame":"","control":{"leave_room":"chat:3"}}"#);
        let back: Delivery = serde_json::from_str(&json).unwrap_or_else(|_| Delivery::new(Target::All, ""));
        assert_eq!(back.control, leave.control);
    }
}
