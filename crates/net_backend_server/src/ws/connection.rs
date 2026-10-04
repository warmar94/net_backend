//! The `/v1/ws` endpoint: the handshake checks, then one task per socket (reads, answers,
//! pushes from its outbox, heartbeats, the auth deadline, closes). A request handler runs while
//! the task keeps writing pushes and serving closes. The upgrade is answered here (not by axum's
//! `WebSocketUpgrade`) so that the socket's bytes reach tungstenite through [`AuthTap`]: `auth`
//! messages never pass through tungstenite, which logs what it receives at TRACE.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::ws::Utf8Bytes;
use axum::extract::{Query, Request, State};
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use http::header::{AUTHORIZATION, CONNECTION, ORIGIN, RETRY_AFTER, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, Version};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use net_backend_protocol::{
    codes, routes, ApiError, CloseCode, WsAuth, WsAuthOk, WsClientFrame, WsRequestFrame, WsResponseFrame, WsServerFrame, PROTOCOL_HEADER, PROTOCOL_VERSION,
};
use serde::Serialize;
use serde_json::{json, Value};
use tokio_tungstenite::WebSocketStream;
use tungstenite::protocol::frame::coding::CloseCode as WireCloseCode;
use tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tungstenite::{Message, Utf8Bytes as WireText};

use super::events::{AfterWsConnect, AfterWsDisconnect, BeforeWsConnect, BeforeWsFrame};
use super::handlers::WsCtx;
use super::hub::{ConnHandle, ConnectionId, Hub, Refusal, Registered, HANDLING};
use super::tap::{AuthTap, Diverted, TapLimit, MARKER, PRE_AUTH_MAX_BYTES};
use crate::auth::{AuthContext, AuthFailure};
use crate::error::AppError;
use crate::hooks::{guarded, HookCtx, Outcome};
use crate::http::middleware::{rate_limited_response, AuthCheckedAt, HandshakeCounted, MIN_PROTOCOL_VERSION};
use crate::http::{ClientIp, RequestId};
use crate::rate_limit::RateDecision;
use crate::state::AppState;

/// Room in the write buffer above the largest message (pongs, the close frame).
const WRITE_HEADROOM: usize = 64 * 1024;

/// How long a close waits for the peer's close frame (so ours is not overtaken by a reset).
const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// How long a socket whose read failed stays open after its close frame (see `close`).
const LINGER_AFTER_ERROR: Duration = Duration::from_millis(500);

/// The longest close reason the protocol allows, in bytes.
const MAX_CLOSE_REASON: usize = 123;

/// `Retry-After` for a full or closing hub and the per-address cap, in seconds.
const BUSY_RETRY_SECS: u64 = 5;

/// The socket of one connection.
type Socket = WebSocketStream<AuthTap<TokioIo<Upgraded>>>;

/// Whether the request is a WebSocket upgrade (HTTP/1.1, the same checks as axum's
/// `WebSocketUpgrade`): its `Sec-WebSocket-Key`.
fn upgrade_key(parts: &Parts) -> Option<HeaderValue> {
    let header = |name| parts.headers.get(name).map(HeaderValue::as_bytes);
    let connection_upgrade = header(CONNECTION).and_then(|v| std::str::from_utf8(v).ok()).is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
    let ok = parts.version <= Version::HTTP_11
        && parts.method == Method::GET
        && connection_upgrade
        && header(UPGRADE).is_some_and(|v| v.eq_ignore_ascii_case(b"websocket"))
        && header(SEC_WEBSOCKET_VERSION).is_some_and(|v| v == b"13")
        && parts.extensions.get::<OnUpgrade>().is_some();
    if ok {
        parts.headers.get(SEC_WEBSOCKET_KEY).filter(|key| valid_key(key.as_bytes())).cloned()
    } else {
        None
    }
}

/// Whether `Sec-WebSocket-Key` is what RFC 6455 section 4.1 asks for: 16 bytes in base64 (22
/// base64 characters and `==`).
fn valid_key(key: &[u8]) -> bool {
    key.len() == 24 && key.ends_with(b"==") && key[..22].iter().all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
}

/// The plain-GET answer: 426 with `Upgrade: websocket`.
fn upgrade_required() -> Response {
    let mut response = AppError::with_status(StatusCode::UPGRADE_REQUIRED, codes::BAD_REQUEST, "this endpoint only accepts WebSocket upgrades").into_response();
    response.headers_mut().insert(UPGRADE, HeaderValue::from_static("websocket"));
    response
}

fn count(hub: &Hub, name: &'static str, label: &'static str, value: impl Into<String>) {
    if hub.metrics() {
        metrics::counter!(name, label => value.into()).increment(1);
    }
}

/// Whether the handshake names a supported protocol version (none = 1).
fn version_supported(headers: &HeaderMap) -> bool {
    match headers.get(PROTOCOL_HEADER) {
        None => true,
        Some(value) => value.to_str().ok().and_then(|s| s.trim().parse::<u32>().ok()).is_some_and(|v| (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&v)),
    }
}

fn version_details() -> Value {
    json!({ "supported_min": MIN_PROTOCOL_VERSION, "supported_max": PROTOCOL_VERSION })
}

/// The answer for a refused handshake: never 400 on `/v1/ws` (the client would retry forever).
fn refuse(error: AppError) -> Response {
    if error.status() == StatusCode::BAD_REQUEST {
        return AppError::from_parts(StatusCode::UNAUTHORIZED, error.api_error().clone()).into_response();
    }
    error.into_response()
}

fn busy(message: &'static str) -> Response {
    let mut response = AppError::unavailable(message).into_response();
    response.headers_mut().insert(RETRY_AFTER, HeaderValue::from(BUSY_RETRY_SECS));
    response
}

/// The reason given when a revocation that arrived during authentication refuses a socket.
fn revoked_reason(code: CloseCode) -> &'static str {
    if code == CloseCode::BANNED {
        "the account is banned"
    } else {
        "the session was revoked"
    }
}

/// A temporary failure (the database is down, a hook timed out, rate limited): the client should
/// come back, so it never becomes a permanent refusal.
fn is_transient(error: &AppError) -> bool {
    error.status().is_server_error() || error.status() == StatusCode::TOO_MANY_REQUESTS
}

/// The token of `?token=…` (or `?access_token=…`).
fn query_token(uri: &Uri) -> Option<String> {
    let Query(query) = Query::<HashMap<String, String>>::try_from_uri(uri).ok()?;
    query.get("token").or_else(|| query.get("access_token")).filter(|t| !t.is_empty()).cloned()
}

/// Ask the authenticators (in order) about `Authorization: Bearer <token>`: the same chain the
/// HTTP routes use, so a game's own authenticator works for first-message auth too.
pub(crate) async fn authenticate_token(state: &AppState, hub: &Hub, token: &str, ip: Option<IpAddr>) -> Result<AuthContext, AppError> {
    let refused = || AppError::new(codes::UNAUTHORIZED, "the access token is not accepted");
    let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| refused())?;
    let mut request = http::Request::new(());
    *request.uri_mut() = Uri::from_static(routes::WS);
    request.headers_mut().insert(AUTHORIZATION, value);
    request.extensions_mut().insert(ClientIp(ip));
    let (parts, ()) = request.into_parts();
    for authenticator in hub.0.authenticators.iter() {
        match authenticator.authenticate(&parts, state).await {
            Ok(Some(context)) => return Ok(context),
            Ok(None) => {}
            Err(error) => return Err(error),
        }
    }
    Err(refused())
}

/// Who the handshake authenticated (and when the credentials were checked): the route's
/// authenticators ran already (header); else the query token; `Ok(None)` = anonymous.
async fn handshake_auth(state: &AppState, hub: &Hub, parts: &Parts, ip: Option<IpAddr>) -> Result<Option<(AuthContext, Instant)>, AppError> {
    let checked_at = parts.extensions.get::<AuthCheckedAt>().map_or_else(Instant::now, |c| c.0);
    if let Some(context) = parts.extensions.get::<AuthContext>() {
        return Ok(Some((context.clone(), checked_at)));
    }
    if let Some(failure) = parts.extensions.get::<AuthFailure>() {
        return Err(failure.to_error());
    }
    if parts.headers.contains_key(AUTHORIZATION) {
        // A credential nobody recognised: refuse now (a 401 is final for the client), instead of
        // an auth timeout the client would retry forever.
        return Err(AppError::new(codes::UNAUTHORIZED, "the credentials are not accepted"));
    }
    if hub.config().query_token {
        if let Some(token) = query_token(&parts.uri) {
            let checked_at = Instant::now();
            return authenticate_token(state, hub, &token, ip).await.map(|context| Some((context, checked_at)));
        }
    }
    Ok(None)
}

/// The handshake's credentials checked again (the first check is older than the hub's revocation
/// memory, e.g. after a slow connect hook): the header through the authenticators, else the query
/// token.
async fn recheck_handshake(state: &AppState, hub: &Hub, parts: &Parts, ip: Option<IpAddr>) -> Result<(AuthContext, Instant), AppError> {
    let checked_at = Instant::now();
    if parts.headers.contains_key(AUTHORIZATION) {
        for authenticator in hub.0.authenticators.iter() {
            if let Some(context) = authenticator.authenticate(parts, state).await? {
                return Ok((context, checked_at));
            }
        }
        return Err(AppError::new(codes::UNAUTHORIZED, "the credentials are not accepted"));
    }
    match query_token(&parts.uri) {
        Some(token) => authenticate_token(state, hub, &token, ip).await.map(|context| (context, checked_at)),
        None => Err(AppError::new(codes::UNAUTHORIZED, "the credentials are not accepted")),
    }
}

async fn before_connect(
    state: &AppState,
    connection: Option<ConnectionId>,
    auth: &AuthContext,
    ip: Option<IpAddr>,
    origin: Option<String>,
    request_id: Option<RequestId>,
) -> Result<(), AppError> {
    let hooks = state.hooks();
    if hooks.before_count::<BeforeWsConnect>() == 0 {
        return Ok(());
    }
    let event = BeforeWsConnect { connection, user_id: auth.user_id, session_id: auth.session_id, ip, origin };
    hooks.run_before(&HookCtx::new(state.clone(), request_id), event).await.map(|_| ())
}

/// What the socket starts as.
enum Start {
    Authenticated(AuthContext, Instant),
    Anonymous,
    RefuseVersion,
}

/// `GET /v1/ws`.
pub(crate) async fn endpoint(State(state): State<AppState>, request: Request) -> Response {
    let hub = state.ws().clone();
    let (mut parts, _body) = request.into_parts();
    let version_ok = version_supported(&parts.headers);
    let key = match upgrade_key(&parts) {
        Some(key) => key,
        // Never 400 here: a version refusal before an upgrade is 403 (final for the client).
        None if !version_ok => {
            count(&hub, "nbs_ws_handshakes_refused_total", "reason", "version");
            return AppError::with_status(StatusCode::FORBIDDEN, codes::UNSUPPORTED_PROTOCOL, "this protocol version is not supported")
                .with_details(version_details())
                .into_response();
        }
        None => return upgrade_required(),
    };
    if hub.is_closing() {
        count(&hub, "nbs_ws_handshakes_refused_total", "reason", "shutting_down");
        return busy("the server is shutting down");
    }
    let ip = parts.extensions.get::<ClientIp>().and_then(|c| c.0);
    // Normally counted before the authenticators ran (`rate_limit_before_auth`).
    let counted = parts.extensions.get::<HandshakeCounted>().is_some();
    let decision = if counted { RateDecision::Allow } else { hub.handshake_allowed(ip) };
    if let RateDecision::Deny { retry_after_ms } = decision {
        count(&hub, "nbs_ws_handshakes_refused_total", "reason", "rate_limited");
        return rate_limited_response(retry_after_ms);
    }
    let request_id = parts.extensions.get::<RequestId>().cloned();
    let origin = parts.headers.get(ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_string);
    let start = if version_ok {
        match handshake_auth(&state, &hub, &parts, ip).await {
            Ok(Some((context, checked_at))) => {
                if let Err(error) = before_connect(&state, None, &context, ip, origin.clone(), request_id.clone()).await {
                    count(&hub, "nbs_ws_handshakes_refused_total", "reason", "hook");
                    return refuse(error);
                }
                if hub.needs_recheck(checked_at) {
                    // Checked too long ago for the hub's revocation memory: check again.
                    match recheck_handshake(&state, &hub, &parts, ip).await {
                        Ok((context, checked_at)) => Start::Authenticated(context, checked_at),
                        Err(error) => {
                            count(&hub, "nbs_ws_handshakes_refused_total", "reason", "auth");
                            return refuse(error);
                        }
                    }
                } else {
                    Start::Authenticated(context, checked_at)
                }
            }
            Ok(None) => Start::Anonymous,
            Err(error) => {
                count(&hub, "nbs_ws_handshakes_refused_total", "reason", "auth");
                return refuse(error);
            }
        }
    } else {
        Start::RefuseVersion
    };
    let slot = match hub.try_reserve(ip, !matches!(start, Start::Authenticated(..))) {
        Ok(slot) => slot,
        Err(Refusal::Full) => {
            count(&hub, "nbs_ws_handshakes_refused_total", "reason", "full");
            tracing::warn!(max = hub.config().max_connections, "WebSocket handshake refused: ws.max_connections reached");
            return busy("too many connections; retry later");
        }
        Err(Refusal::TooManyPending) => {
            count(&hub, "nbs_ws_handshakes_refused_total", "reason", "pending");
            return busy("too many connections waiting to authenticate; retry later");
        }
        Err(Refusal::TooManyFromAddress) => {
            count(&hub, "nbs_ws_handshakes_refused_total", "reason", "per_ip");
            return rate_limited_response(BUSY_RETRY_SECS * 1000);
        }
    };
    let Some(on_upgrade) = parts.extensions.remove::<OnUpgrade>() else { return upgrade_required() };
    let config = hub.config().clone();
    let max = config.max_message_bytes;
    let ws_config = WebSocketConfig::default()
        .read_buffer_size(config.read_buffer_bytes)
        .write_buffer_size(0)
        .max_write_buffer_size(max.saturating_add(WRITE_HEADROOM))
        .max_message_size(Some(max))
        .max_frame_size(Some(max));
    tokio::spawn(async move {
        let upgraded = match on_upgrade.await {
            Ok(upgraded) => upgraded,
            Err(error) => {
                tracing::debug!(%error, "WebSocket upgrade failed");
                return;
            }
        };
        let diverted = Diverted::default();
        // Small until the socket authenticated (`become_user` raises it).
        let limit = TapLimit::new(PRE_AUTH_MAX_BYTES.min(max));
        let tap = AuthTap::new(TokioIo::new(upgraded), limit.clone(), max, diverted.clone());
        let socket = WebSocketStream::from_raw_socket(tap, Role::Server, Some(ws_config)).await;
        let (registered, handle) = hub.register(slot, ip, state.now());
        let connection = Connection::new(state, hub, socket, Tap { diverted, limit }, registered, handle, ip, origin, request_id);
        connection.run(start).await;
    });
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    if let Ok(accept) = HeaderValue::from_str(&tungstenite::handshake::derive_accept_key(key.as_bytes())) {
        headers.insert(SEC_WEBSOCKET_ACCEPT, accept);
    }
    response
}

/// A token bucket for the incoming frames of one socket.
struct Bucket {
    tokens: f64,
    burst: f64,
    per_sec: f64,
    last: Instant,
}

impl Bucket {
    fn new(per_sec: u32, burst: u32) -> Self {
        let burst = f64::from(burst.max(1));
        Self { tokens: burst, burst, per_sec: f64::from(per_sec.max(1)), last: Instant::now() }
    }

    /// Take a token, or the milliseconds until one is back.
    fn take(&mut self) -> Result<(), u64> {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.per_sec).min(self.burst);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            let ms = ((1.0 - self.tokens) / self.per_sec * 1000.0).ceil();
            Err(if ms.is_finite() && ms >= 1.0 { ms as u64 } else { 1 })
        }
    }
}

/// A request whose handler is running.
struct Pending {
    id: u64,
    kind: String,
    future: BoxFuture<'static, Result<Value, ApiError>>,
}

/// What handling a frame led to.
enum Flow {
    /// Go on reading.
    Go,
    /// The socket is closed or must close.
    Stop,
    /// A request handler to run while the socket keeps writing.
    Run(Pending),
}

/// The error a client sees: internal details are logged, never sent.
fn client_error(error: AppError, kind: &str) -> ApiError {
    if error.status().is_server_error() {
        if let Some(source) = std::error::Error::source(&error) {
            if error.status() == StatusCode::INTERNAL_SERVER_ERROR {
                tracing::error!(kind, code = %error.code(), error = %source, "WebSocket request failed");
            } else {
                tracing::warn!(kind, code = %error.code(), error = %source, "WebSocket request failed");
            }
        }
        if error.code() == codes::INTERNAL {
            return ApiError::new(codes::INTERNAL, "internal server error");
        }
    }
    error.api_error().clone()
}

/// Whether `text` is an `auth` frame (no `id`), however broken its data: the rule of
/// `WsClientFrame::parse` (a JSON object without an `id` key whose `type` - the last one, as in a
/// parsed document - is the string `auth`), read without building the document.
pub(super) fn is_auth_frame(text: &str) -> bool {
    serde_json::from_str::<probe::Probe>(text).is_ok_and(|probe| probe.is_auth())
}

/// A JSON reader that only looks at the top-level `id` and `type` keys (see [`is_auth_frame`]).
mod probe {
    use std::fmt;

    use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

    pub(super) struct Probe {
        id: bool,
        auth: bool,
    }

    impl Probe {
        pub(super) fn is_auth(&self) -> bool {
            !self.id && self.auth
        }
    }

    /// Any JSON value, read and dropped (validated like a parsed document).
    struct Skip;

    impl<'de> Deserialize<'de> for Skip {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(SkipVisitor)
        }
    }

    struct SkipVisitor;

    impl<'de> Visitor<'de> for SkipVisitor {
        type Value = Skip;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("any JSON value")
        }
        fn visit_bool<E>(self, _: bool) -> Result<Skip, E> {
            Ok(Skip)
        }
        fn visit_i64<E>(self, _: i64) -> Result<Skip, E> {
            Ok(Skip)
        }
        fn visit_u64<E>(self, _: u64) -> Result<Skip, E> {
            Ok(Skip)
        }
        fn visit_f64<E>(self, _: f64) -> Result<Skip, E> {
            Ok(Skip)
        }
        fn visit_str<E>(self, _: &str) -> Result<Skip, E> {
            Ok(Skip)
        }
        fn visit_unit<E>(self) -> Result<Skip, E> {
            Ok(Skip)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Skip, A::Error> {
            while seq.next_element::<Skip>()?.is_some() {}
            Ok(Skip)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Skip, A::Error> {
            while map.next_entry::<Skip, Skip>()?.is_some() {}
            Ok(Skip)
        }
    }

    /// A key: `id`, `type` or another (compared without allocating when it has no escapes).
    enum Key {
        Id,
        Type,
        Other,
    }

    impl<'de> Deserialize<'de> for Key {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct KeyVisitor;
            impl Visitor<'_> for KeyVisitor {
                type Value = Key;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a key")
                }
                fn visit_str<E>(self, key: &str) -> Result<Key, E> {
                    Ok(match key {
                        "id" => Key::Id,
                        "type" => Key::Type,
                        _ => Key::Other,
                    })
                }
            }
            deserializer.deserialize_str(KeyVisitor)
        }
    }

    /// The value of `type`: whether it is the string `auth`.
    struct IsAuth(bool);

    impl<'de> Deserialize<'de> for IsAuth {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct IsAuthVisitor;
            impl<'de> Visitor<'de> for IsAuthVisitor {
                type Value = IsAuth;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("any JSON value")
                }
                fn visit_str<E>(self, value: &str) -> Result<IsAuth, E> {
                    Ok(IsAuth(value == net_backend_protocol::kinds::AUTH))
                }
                fn visit_bool<E>(self, _: bool) -> Result<IsAuth, E> {
                    Ok(IsAuth(false))
                }
                fn visit_i64<E>(self, _: i64) -> Result<IsAuth, E> {
                    Ok(IsAuth(false))
                }
                fn visit_u64<E>(self, _: u64) -> Result<IsAuth, E> {
                    Ok(IsAuth(false))
                }
                fn visit_f64<E>(self, _: f64) -> Result<IsAuth, E> {
                    Ok(IsAuth(false))
                }
                fn visit_unit<E>(self) -> Result<IsAuth, E> {
                    Ok(IsAuth(false))
                }
                fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<IsAuth, A::Error> {
                    SkipVisitor.visit_seq(seq).map(|_| IsAuth(false))
                }
                fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<IsAuth, A::Error> {
                    SkipVisitor.visit_map(map).map(|_| IsAuth(false))
                }
            }
            deserializer.deserialize_any(IsAuthVisitor)
        }
    }

    impl<'de> Deserialize<'de> for Probe {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct ProbeVisitor;
            impl<'de> Visitor<'de> for ProbeVisitor {
                type Value = Probe;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a JSON object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Probe, A::Error> {
                    let mut probe = Probe { id: false, auth: false };
                    while let Some(key) = map.next_key::<Key>()? {
                        match key {
                            Key::Id => {
                                probe.id = true;
                                map.next_value::<Skip>()?;
                            }
                            Key::Type => probe.auth = map.next_value::<IsAuth>()?.0,
                            Key::Other => {
                                map.next_value::<Skip>()?;
                            }
                        }
                    }
                    Ok(probe)
                }
            }
            deserializer.deserialize_any(ProbeVisitor)
        }
    }
}

/// Whether a read error is a message over the size limit.
fn is_too_big(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Capacity(_))
}

fn close_reason(reason: Cow<'static, str>) -> WireText {
    match reason {
        Cow::Borrowed(text) if text.len() <= MAX_CLOSE_REASON => WireText::from_static(text),
        other => {
            let mut text = other.into_owned();
            if text.len() > MAX_CLOSE_REASON {
                let mut end = MAX_CLOSE_REASON;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            WireText::from(text)
        }
    }
}

/// The connection's side of its [`AuthTap`].
struct Tap {
    /// The `auth` messages the tap took out of the socket's stream ([`MARKER`] stands for each).
    diverted: Diverted,
    /// The tap's size limit: [`PRE_AUTH_MAX_BYTES`] until the socket authenticated.
    limit: TapLimit,
}

struct Connection {
    state: AppState,
    hub: Hub,
    id: ConnectionId,
    socket: Socket,
    tap: Tap,
    handle: ConnHandle,
    registered: Option<Registered>,
    auth: Option<AuthContext>,
    ip: Option<IpAddr>,
    origin: Option<String>,
    request_id: Option<RequestId>,
    bucket: Bucket,
    refused: u32,
    last_seen: Instant,
    sent_close: Option<CloseCode>,
    connected: bool,
    killed: bool,
    read_failed: bool,
    /// The end was already counted in the metrics (dead peer, stuck write).
    end_counted: bool,
    write_timeout: Duration,
}

impl Connection {
    #[allow(clippy::too_many_arguments)]
    fn new(
        state: AppState,
        hub: Hub,
        socket: Socket,
        tap: Tap,
        registered: Registered,
        handle: ConnHandle,
        ip: Option<IpAddr>,
        origin: Option<String>,
        request_id: Option<RequestId>,
    ) -> Self {
        let config = hub.config();
        let bucket = Bucket::new(config.frames_per_second, config.frame_burst);
        let write_timeout = Duration::from_secs(config.write_timeout_secs.max(1));
        Self {
            id: registered.id,
            state,
            hub,
            socket,
            tap,
            handle,
            registered: Some(registered),
            auth: None,
            ip,
            origin,
            request_id,
            bucket,
            refused: 0,
            last_seen: Instant::now(),
            sent_close: None,
            connected: false,
            killed: false,
            read_failed: false,
            end_counted: false,
            write_timeout,
        }
    }

    fn hook_ctx(&self) -> HookCtx {
        HookCtx::new(self.state.clone(), self.request_id.clone())
    }

    async fn run(mut self, start: Start) {
        match start {
            Start::RefuseVersion => self.close(CloseCode::UNSUPPORTED_PROTOCOL, Cow::Borrowed("unsupported protocol version")).await,
            Start::Authenticated(context, checked_at) => match self.become_user(context, checked_at) {
                Ok(()) => self.serve().await,
                // Too old to judge (rare: the revocation memory overflowed meanwhile): reconnect.
                Err(CloseCode::TRY_AGAIN_LATER) => self.close(CloseCode::TRY_AGAIN_LATER, Cow::Borrowed("authentication unavailable; try again later")).await,
                Err(code) => self.close(code, Cow::Borrowed(revoked_reason(code))).await,
            },
            Start::Anonymous => self.serve().await,
        }
        self.finish().await;
    }

    /// Register the socket as the user's (the `BeforeWsConnect` hook already ran), unless a
    /// revocation arrived after `checked_at`; then the `AfterWsConnect` hook, spawned (a slow hook
    /// must not hold the socket).
    fn become_user(&mut self, context: AuthContext, checked_at: Instant) -> Result<(), CloseCode> {
        self.hub.set_user(self.id, context.user_id, context.session_id, context.roles.clone(), checked_at)?;
        if let Some(registered) = &self.registered {
            registered.slot.authenticated();
        }
        self.tap.limit.set(self.hub.config().max_message_bytes);
        if !self.connected {
            self.connected = true;
            let event = AfterWsConnect { connection: self.id, user_id: context.user_id, session_id: context.session_id, ip: self.ip };
            let (state, ctx) = (self.state.clone(), self.hook_ctx());
            tokio::spawn(async move { state.hooks().run_after(&ctx, Arc::new(event)).await });
        }
        self.auth = Some(context);
        Ok(())
    }

    async fn serve(&mut self) {
        let config = self.hub.config().clone();
        let ping_every = Duration::from_secs(config.ping_interval_secs.max(1));
        let idle = Duration::from_secs(config.idle_timeout_secs.max(2));
        let held_max = config.outbox_frames.max(1);
        let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + ping_every, ping_every);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let auth_deadline = tokio::time::sleep(Duration::from_secs(config.auth_timeout_secs.max(1)));
        tokio::pin!(auth_deadline);
        if *self.handle.kill.borrow() {
            self.killed = true;
            return;
        }
        let mut pending: Option<Pending> = None;
        let mut held: VecDeque<Utf8Bytes> = VecDeque::new();
        loop {
            let anonymous = self.auth.is_none();
            let running = pending.is_some();
            tokio::select! {
                _ = self.handle.kill.changed() => {
                    self.killed = true;
                    return;
                }
                changed = self.handle.close.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let request = self.handle.close.borrow_and_update().clone();
                    if let Some((code, reason)) = request {
                        self.close(code, reason).await;
                        return;
                    }
                }
                frame = self.handle.outbox.recv() => match frame {
                    // Pushed by the running handler to its own socket: after its answer.
                    Some(outgoing) if running && outgoing.after_answer => {
                        if held.len() >= held_max {
                            count(&self.hub, "nbs_ws_slow_consumers_total", "reason", "held");
                            self.close(CloseCode::TRY_AGAIN_LATER, Cow::Borrowed("too many messages from one request")).await;
                            return;
                        }
                        held.push_back(outgoing.frame);
                    }
                    Some(outgoing) => {
                        if !self.send_text(outgoing.frame).await {
                            return;
                        }
                    }
                    None => return,
                },
                result = async { match pending.as_mut() { Some(p) => (&mut p.future).await, None => std::future::pending().await } }, if running => {
                    let Some(done) = pending.take() else { continue };
                    if matches!(self.answer(done.id, &done.kind, result).await, Flow::Stop) {
                        return;
                    }
                    while let Some(frame) = held.pop_front() {
                        if !self.send_text(frame).await {
                            return;
                        }
                    }
                    // The socket was not read meanwhile: give the peer a fresh idle window.
                    self.last_seen = Instant::now();
                }
                message = self.socket.next(), if !running => match message {
                    None => return,
                    Some(Err(error)) => {
                        if is_too_big(&error) {
                            self.read_failed = true;
                            count(&self.hub, "nbs_ws_dropped_frames_total", "reason", "too_big");
                            self.close(CloseCode::MESSAGE_TOO_BIG, Cow::Borrowed("message too big")).await;
                        } else {
                            tracing::debug!(%error, "WebSocket read failed");
                        }
                        return;
                    }
                    Some(Ok(message)) => {
                        self.last_seen = Instant::now();
                        // Boxed: the request path's state lives on the heap only while a
                        // message is handled, not in every idle socket's task.
                        match Box::pin(self.on_message(message)).await {
                            Flow::Go => {}
                            Flow::Stop => return,
                            Flow::Run(run) => pending = Some(run),
                        }
                    }
                },
                _ = ping.tick() => {
                    if !running && self.last_seen.elapsed() >= idle {
                        tracing::debug!(connection = %self.id, "WebSocket peer silent too long; dropping it");
                        count(&self.hub, "nbs_ws_closes_total", "code", "dead");
                        self.end_counted = true;
                        return;
                    }
                    if !self.send(Message::Ping(Default::default())).await {
                        return;
                    }
                }
                _ = &mut auth_deadline, if anonymous => {
                    self.close(CloseCode::POLICY_VIOLATION, Cow::Borrowed("authentication timeout")).await;
                    return;
                }
            }
        }
    }

    async fn finish(mut self) {
        let rooms = self.hub.rooms_of(self.id);
        if self.sent_close.is_none() && !self.killed && !self.end_counted {
            count(&self.hub, "nbs_ws_closes_total", "code", "peer");
        }
        drop(self.registered.take());
        if let (true, false, Some(auth)) = (self.connected, self.killed, &self.auth) {
            let event = AfterWsDisconnect { connection: self.id, user_id: auth.user_id, code: self.sent_close, rooms };
            self.state.hooks().run_after(&self.hook_ctx(), Arc::new(event)).await;
        }
    }

    async fn send(&mut self, message: Message) -> bool {
        match tokio::time::timeout(self.write_timeout, self.socket.send(message)).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::debug!(%error, "WebSocket write failed");
                false
            }
            Err(_) => {
                tracing::debug!(connection = %self.id, "WebSocket write timed out; dropping the socket");
                count(&self.hub, "nbs_ws_closes_total", "code", "stuck");
                self.end_counted = true;
                false
            }
        }
    }

    /// Send a frame from the outbox.
    async fn send_text(&mut self, text: Utf8Bytes) -> bool {
        match WireText::try_from(Bytes::from(text)) {
            Ok(text) => self.send_wire(text).await,
            Err(_) => {
                tracing::error!("an outgoing WebSocket frame is not UTF-8");
                true
            }
        }
    }

    async fn send_wire(&mut self, text: WireText) -> bool {
        if self.hub.metrics() {
            metrics::counter!("nbs_ws_frames_out_total").increment(1);
        }
        self.send(Message::Text(text)).await
    }

    async fn send_frame<T: Serialize>(&mut self, frame: &T) -> Flow {
        let text = match serde_json::to_string(frame) {
            Ok(text) => text,
            Err(error) => {
                tracing::error!(%error, "a WebSocket frame could not be encoded");
                return Flow::Go;
            }
        };
        if self.send_wire(WireText::from(text)).await {
            Flow::Go
        } else {
            Flow::Stop
        }
    }

    /// Send the close frame (once), then wait briefly for the peer's.
    async fn close(&mut self, code: CloseCode, reason: Cow<'static, str>) {
        if self.sent_close.is_some() {
            return;
        }
        let code = super::hub::sendable(code);
        self.sent_close = Some(code);
        count(&self.hub, "nbs_ws_closes_total", "code", code.to_string());
        let frame = CloseFrame { code: WireCloseCode::from(code.get()), reason: close_reason(reason) };
        if !self.send(Message::Close(Some(frame))).await {
            return;
        }
        if self.read_failed {
            // The rest of the refused message is still unread: dropping the socket now would send
            // a TCP reset that can overtake the close frame. Give the peer a moment to read it.
            tokio::time::sleep(LINGER_AFTER_ERROR).await;
        } else {
            let socket = &mut self.socket;
            let _ = tokio::time::timeout(CLOSE_WAIT, async { while let Some(Ok(_)) = socket.next().await {} }).await;
        }
    }

    async fn on_message(&mut self, message: Message) -> Flow {
        match message {
            Message::Text(text) => self.on_text(text).await,
            Message::Binary(_) => {
                // Not part of the protocol: dropped, but it counts against the rate limit.
                count(&self.hub, "nbs_ws_dropped_frames_total", "reason", "binary");
                match self.charge().await {
                    Ok(()) | Err(Some(_)) => Flow::Go,
                    Err(None) => Flow::Stop,
                }
            }
            // tungstenite answers pings itself; a pong only proves the peer is alive. Both count
            // against the rate limit, so a ping or pong flood is closed like any other (a real
            // heartbeat sends a few per minute).
            Message::Ping(_) | Message::Pong(_) => match self.charge().await {
                Ok(()) | Err(Some(_)) => Flow::Go,
                Err(None) => Flow::Stop,
            },
            // tungstenite answers the close; the next read ends the stream. (A raw frame is never
            // read.)
            Message::Close(_) | Message::Frame(_) => Flow::Go,
        }
    }

    /// Take a token for an incoming frame: `Err(Some(ms))` = over the limit (answer
    /// `rate_limited`), `Err(None)` = flooding on after `frame_burst` refusals in a row (closed
    /// with 1008).
    async fn charge(&mut self) -> Result<(), Option<u64>> {
        match self.bucket.take() {
            Ok(()) => {
                self.refused = 0;
                Ok(())
            }
            Err(retry_after_ms) => {
                self.refused = self.refused.saturating_add(1);
                count(&self.hub, "nbs_ws_dropped_frames_total", "reason", "rate_limited");
                if self.refused > self.hub.config().frame_burst.max(1) {
                    self.close(CloseCode::POLICY_VIOLATION, Cow::Borrowed("too many messages")).await;
                    return Err(None);
                }
                Err(Some(retry_after_ms))
            }
        }
    }

    async fn on_text(&mut self, wire: WireText) -> Flow {
        // An `auth` message never passed through tungstenite: the tap took it out (`tap.rs`).
        let diverted;
        let text = if wire.as_str() == MARKER {
            diverted = self.tap.diverted.pop().unwrap_or_default();
            diverted.as_str()
        } else {
            wire.as_str()
        };
        if self.hub.metrics() {
            metrics::counter!("nbs_ws_frames_in_total").increment(1);
        }
        match self.charge().await {
            Ok(()) => {}
            Err(None) => return Flow::Stop,
            // An `auth` is still answered (exactly one answer per `auth`); it was charged.
            Err(Some(_)) if is_auth_frame(text) => {}
            Err(Some(retry_after_ms)) => {
                return match WsClientFrame::request_id(text) {
                    Some(id) => self.answer(id, "", Err(AppError::rate_limited(retry_after_ms).api_error().clone())).await,
                    None => Flow::Go,
                };
            }
        }
        match WsClientFrame::parse(text) {
            Ok(WsClientFrame::Auth(auth)) => self.on_auth(auth).await,
            Ok(WsClientFrame::Request(request)) => self.on_request(request).await,
            Ok(_) => Flow::Go,
            Err(error) => match error.answer() {
                Some(answer) => self.send_frame(&answer).await,
                None if is_auth_frame(text) => {
                    self.auth_failed(ApiError::new(codes::BAD_REQUEST, "the auth message is malformed"), CloseCode::UNAUTHORIZED).await
                }
                None => {
                    count(&self.hub, "nbs_ws_dropped_frames_total", "reason", "malformed");
                    Flow::Go
                }
            },
        }
    }

    /// `auth.failed`, then close.
    async fn auth_failed(&mut self, error: ApiError, code: CloseCode) -> Flow {
        let _ = self.send_frame(&WsServerFrame::AuthFailed(error)).await;
        self.close(code, Cow::Borrowed("authentication failed")).await;
        Flow::Stop
    }

    /// A refusal of an `auth`: a temporary failure closes with 1013 WITHOUT `auth.failed` (the
    /// client reconnects and tries again); anything else is `auth.failed` + 4003 (banned) / 4001.
    async fn auth_refused(&mut self, error: AppError) -> Flow {
        if is_transient(&error) {
            let _ = client_error(error, "auth");
            self.close(CloseCode::TRY_AGAIN_LATER, Cow::Borrowed("authentication unavailable; try again later")).await;
            return Flow::Stop;
        }
        let code = if error.code() == codes::BANNED { CloseCode::BANNED } else { CloseCode::UNAUTHORIZED };
        self.auth_failed(client_error(error, "auth"), code).await
    }

    async fn on_auth(&mut self, auth: WsAuth) -> Flow {
        let version = auth.protocol.unwrap_or(1);
        if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version) {
            let error = ApiError::new(codes::UNSUPPORTED_PROTOCOL, "this protocol version is not supported").with_details(version_details());
            return self.auth_failed(error, CloseCode::UNSUPPORTED_PROTOCOL).await;
        }
        let mut checked_at = Instant::now();
        let mut context = match authenticate_token(&self.state, &self.hub, auth.token.expose(), self.ip).await {
            Ok(context) => context,
            Err(error) => return self.auth_refused(error).await,
        };
        if let Some(current) = &self.auth {
            if current.user_id != context.user_id {
                return self.auth_failed(ApiError::new(codes::UNAUTHORIZED, "this connection belongs to another user"), CloseCode::UNAUTHORIZED).await;
            }
        } else if let Err(error) = before_connect(&self.state, Some(self.id), &context, self.ip, self.origin.clone(), self.request_id.clone()).await {
            return self.auth_refused(error).await;
        }
        if self.hub.needs_recheck(checked_at) {
            // The connect hook took longer than the hub's revocation memory: check the token again.
            checked_at = Instant::now();
            context = match authenticate_token(&self.state, &self.hub, auth.token.expose(), self.ip).await {
                Ok(again) if again.user_id == context.user_id => again,
                Ok(_) => return self.auth_failed(ApiError::new(codes::UNAUTHORIZED, "the access token is not accepted"), CloseCode::UNAUTHORIZED).await,
                Err(error) => return self.auth_refused(error).await,
            };
        }
        let user = context.user_id;
        if let Err(code) = self.become_user(context, checked_at) {
            if code == CloseCode::TRY_AGAIN_LATER {
                // Too old to judge (the revocation memory overflowed meanwhile): the client reconnects.
                self.close(CloseCode::TRY_AGAIN_LATER, Cow::Borrowed("authentication unavailable; try again later")).await;
                return Flow::Stop;
            }
            if code == CloseCode::BANNED {
                // A ban that landed while this socket authenticated answers `banned` (like the close
                // code), with the ban's end (`details.until`) read again from the account.
                if let Err(error) = authenticate_token(&self.state, &self.hub, auth.token.expose(), self.ip).await {
                    if error.code() == codes::BANNED {
                        return self.auth_refused(error).await;
                    }
                }
                return self.auth_failed(ApiError::new(codes::BANNED, "this account is banned"), code).await;
            }
            return self.auth_failed(ApiError::new(codes::UNAUTHORIZED, revoked_reason(code)), code).await;
        }
        self.send_frame(&WsServerFrame::AuthOk(Some(WsAuthOk::new(user)))).await
    }

    /// Prepare a request: answered at once when it cannot run, else its handler (with the
    /// `BeforeWsFrame` hooks) as a future the socket task drives while it keeps writing.
    async fn on_request(&mut self, request: WsRequestFrame) -> Flow {
        let WsRequestFrame { id, kind, data, .. } = request;
        let Some(mut auth) = self.auth.clone() else {
            let error = ApiError::new(codes::UNAUTHORIZED, "authenticate first (send `auth`, or connect with a token)");
            return self.answer(id, &kind, Err(error)).await;
        };
        let Some(handler) = self.hub.0.handlers.get(&kind).cloned() else {
            return self.answer(id, &kind, Err(ApiError::new(codes::UNKNOWN_TYPE, "unknown request type"))).await;
        };
        if let Some(roles) = self.hub.roles_of(self.id) {
            auth.roles = roles;
        }
        let hooks = self.state.hooks().clone();
        let hook_ctx = self.hook_ctx();
        let state = self.state.clone();
        let connection = self.id;
        let limit = Duration::from_secs(self.hub.config().request_timeout_secs.max(1));
        let task_kind = kind.clone();
        let future = Box::pin(HANDLING.scope(connection, async move {
            let kind = task_kind;
            let mut data = data;
            if hooks.before_count::<BeforeWsFrame>() > 0 {
                let event = BeforeWsFrame { connection, user_id: auth.user_id, kind: kind.clone(), data };
                match hooks.run_before(&hook_ctx, event).await {
                    Ok(event) => data = event.data,
                    Err(error) => return Err(client_error(error, &kind)),
                }
            }
            let ctx = WsCtx { state, connection, auth, request_id: id, kind: Arc::from(kind.as_str()) };
            match guarded(limit, handler(ctx, data)).await {
                Outcome::Done(Ok(value)) => Ok(value),
                Outcome::Done(Err(error)) => Err(client_error(error, &kind)),
                Outcome::TimedOut => {
                    tracing::warn!(kind = %kind, limit_secs = limit.as_secs(), "WebSocket request timed out");
                    Err(ApiError::new(codes::UNAVAILABLE, "the request took too long"))
                }
                Outcome::Panicked => {
                    tracing::error!(kind = %kind, "WebSocket handler panicked");
                    Err(ApiError::new(codes::INTERNAL, "internal server error"))
                }
            }
        }));
        Flow::Run(Pending { id, kind, future })
    }

    /// The answer to request `id` (written at once, before pushes the handler queued).
    async fn answer(&mut self, id: u64, kind: &str, result: Result<Value, ApiError>) -> Flow {
        let frame = match result {
            Ok(data) => WsResponseFrame::ok(id, data),
            Err(error) => WsResponseFrame::error(id, error),
        };
        let text = match serde_json::to_string(&frame) {
            Ok(text) => text,
            Err(error) => {
                tracing::error!(kind, %error, "a WebSocket answer could not be encoded");
                return Flow::Go;
            }
        };
        if text.len() > self.hub.config().max_message_bytes {
            tracing::warn!(kind, bytes = text.len(), "a WebSocket answer is larger than ws.max_message_bytes; answering payload_too_large");
            let error = WsResponseFrame::<Value>::error(id, ApiError::new(codes::PAYLOAD_TOO_LARGE, "the answer is too large"));
            return self.send_frame(&error).await;
        }
        if self.send_wire(WireText::from(text)).await {
            Flow::Go
        } else {
            Flow::Stop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        let mut headers = HeaderMap::new();
        assert!(version_supported(&headers));
        headers.insert(PROTOCOL_HEADER, HeaderValue::from_static("1"));
        assert!(version_supported(&headers));
        for bad in ["99", "0", "x", ""] {
            headers.insert(PROTOCOL_HEADER, HeaderValue::from_static(bad));
            assert!(!version_supported(&headers), "{bad}");
        }
        assert_eq!(query_token(&Uri::from_static("/v1/ws?token=nbsa_x")).as_deref(), Some("nbsa_x"));
        assert_eq!(query_token(&Uri::from_static("/v1/ws?access_token=a%2Bb")).as_deref(), Some("a+b"));
        assert_eq!(query_token(&Uri::from_static("/v1/ws?token=")), None);
        assert_eq!(query_token(&Uri::from_static("/v1/ws")), None);
        assert!(is_auth_frame(r#"{"type":"auth","data":5}"#) && !is_auth_frame(r#"{"id":1,"type":"auth"}"#) && !is_auth_frame("x"));
        let long = "é".repeat(100);
        let reason = close_reason(Cow::Owned(long));
        assert!(reason.as_str().len() <= MAX_CLOSE_REASON && reason.as_str().chars().all(|c| c == 'é'));
        let mut bucket = Bucket::new(1, 2);
        assert!(bucket.take().is_ok() && bucket.take().is_ok());
        assert!(bucket.take().is_err_and(|ms| (1..=1000).contains(&ms)));
        assert!(is_transient(&AppError::unavailable("x")) && is_transient(&AppError::rate_limited(5)));
        assert!(!is_transient(&AppError::unauthorized()) && !is_transient(&AppError::new(codes::BANNED, "b")));
    }

    /// The probe classifies `auth` frames exactly like a parsed document (the rule of
    /// `WsClientFrame::parse`), including duplicate keys, escapes and broken JSON.
    #[test]
    fn the_auth_probe_matches_the_document_rule() {
        fn by_document(text: &str) -> bool {
            serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|v| v.as_object().map(|o| !o.contains_key("id") && o.get("type").and_then(Value::as_str) == Some(net_backend_protocol::kinds::AUTH)))
                .unwrap_or(false)
        }
        for text in [
            r#"{"type":"auth","data":{"token":"nbsa_x"}}"#,
            r#"{"type":"auth"}"#,
            r#"{"type":"auth","data":1}"#,
            r#"{"type":"x","type":"auth"}"#,
            r#"{"type":"auth","type":"x"}"#,
            r#"{"type":"auth","id":null}"#,
            r#"{"id":1,"type":"auth"}"#,
            r#"{"type":["auth"]}"#,
            r#"{"type":{"auth":1}}"#,
            r#"{"type":"auth","data":[1,{"a":[true,null,-1.5e3]}]}"#,
            r#"{"type":"auth","x":1e400}"#,
            r#"{"type":"auth"} trailing"#,
            r#"{"type":"auth""#,
            r#"["auth"]"#,
            r#""auth""#,
            "",
            "x",
            r#"{"id":1,"type":"auth"}"#,
            r#"{"data":{"id":1,"type":"x"},"type":"auth"}"#,
        ] {
            assert_eq!(is_auth_frame(text), by_document(text), "{text}");
        }
    }

    #[test]
    fn websocket_keys_are_checked() {
        assert!(valid_key(b"dGhlIHNhbXBsZSBub25jZQ=="));
        for bad in [&b""[..], b"x", b"dGhlIHNhbXBsZSBub25jZQ=", b"dGhlIHNhbXBsZSBub25jZ===", b"dGhlIHNhbXBsZSBub25j!Q==", b"dGhlIHNhbXBsZSBub25jZQ==x"] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }
}
