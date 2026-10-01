//! The WebSocket hub on a real loopback socket with a WebSocket client (tokio-tungstenite): auth at
//! the handshake and by first message, the auth deadline, failures, protocol versions, token
//! expiry, revocations and bans (also from another process), malformed frames, handlers, rooms,
//! caps, backpressure, heartbeats, the rate and size limits, hooks, shutdown. Bounded: every wait
//! has a timeout; every server is stopped.
#![cfg(feature = "sqlite")]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use http::StatusCode;
use net_backend_server::auth::{Auth, AuthConfig, AuthService};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::admin::BanRequest;
use net_backend_server::protocol::{codes, routes, ServerPush, UnixMillis, UserId, WsCall, PROTOCOL_HEADER};
use net_backend_server::ws::events::{AfterWsConnect, AfterWsDisconnect, BeforeWsConnect, BeforeWsFrame};
use net_backend_server::ws::{Broadcaster, Delivery, LocalDelivery, PushError, Target, WsCtx};
use net_backend_server::{AppError, AppState, AuthContext, Authenticator, Config, Error, ManualClock, NetBackendServer, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const PASSWORD: &str = "correct horse battery";
/// The upper bound of every wait. Generous on purpose: tests wait for a condition (a frame, a
/// count), never for a fixed time, and CI runners are slow and shared; a passing run never waits
/// this long.
const WAIT: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
struct Echo {
    text: String,
}

#[derive(Serialize, Deserialize)]
struct Echoed {
    text: String,
    user: i64,
}

impl WsCall for Echo {
    type Response = Echoed;
    const KIND: &'static str = "test.typed";
}

#[derive(Serialize, Deserialize)]
struct Note {
    n: u32,
}

impl ServerPush for Note {
    const KIND: &'static str = "test.note";
}

/// What the hooks saw.
#[derive(Default)]
struct Probe {
    connects: AtomicUsize,
    disconnects: Mutex<Vec<AfterWsDisconnect>>,
    refuse_connect: AtomicBool,
    /// While set, `BeforeWsConnect` signals `entered` and waits for `release` (a deterministic
    /// "the socket is between its token check and its registration").
    slow_connect: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    published: AtomicUsize,
}

/// `Bearer flaky`: a temporary failure (the database is down); every other token: not mine.
struct FlakyAuth;

impl Authenticator for FlakyAuth {
    fn authenticate<'a>(&'a self, parts: &'a http::request::Parts, _state: &'a AppState) -> BoxFuture<'a, Result<Option<AuthContext>, AppError>> {
        Box::pin(async move {
            match parts.headers.get("authorization").and_then(|v| v.to_str().ok()) {
                Some("Bearer flaky") => Err(AppError::unavailable("the database is restarting")),
                _ => Ok(None),
            }
        })
    }
}

/// Counts what goes through the broadcaster, then delivers locally.
struct Counting(Arc<Probe>);

impl Broadcaster for Counting {
    fn publish(&self, local: &LocalDelivery, delivery: Delivery) {
        self.0.published.fetch_add(1, Ordering::SeqCst);
        local.deliver(&delivery);
    }
}

struct Server {
    addr: SocketAddr,
    state: AppState,
    router: axum::Router,
    clock: Arc<ManualClock>,
    probe: Arc<Probe>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), Error>>>,
}

fn cheap_auth() -> AuthConfig {
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.hash_concurrency = 4;
    auth.purge_interval_secs = 0;
    auth
}

fn handlers(server: NetBackendServer) -> NetBackendServer {
    server
        .ws_call::<Echo, _, _>(|ctx: WsCtx, echo: Echo| async move { Ok(Echoed { text: echo.text, user: ctx.auth.user_id.get() }) })
        .ws_handler("test.echo", |_ctx, data| async move { Ok(data) })
        .ws_handler("test.whoami", |ctx, _| async move {
            Ok(json!({ "user": ctx.auth.user_id.get(), "session": ctx.auth.session_id, "connection": ctx.connection.get() }))
        })
        .ws_handler("test.join", |ctx, data| async move {
            let room = data["room"].as_str().unwrap_or_default().to_string();
            ctx.hub().join(ctx.connection, room.as_str())?;
            Ok(json!({ "size": ctx.hub().room_size(&room) }))
        })
        .ws_handler("test.leave", |ctx, data| async move { Ok(json!({ "left": ctx.hub().leave(ctx.connection, data["room"].as_str().unwrap_or_default()) })) })
        .ws_handler("test.say", |ctx, data| async move {
            let room = data["room"].as_str().unwrap_or_default();
            ctx.hub().push_raw(Target::Room(room.into()), "test.said", &json!({ "from": ctx.auth.user_id.get(), "text": data["text"] }))?;
            Ok(json!({}))
        })
        .ws_handler("test.panic", |_ctx, _| async move {
            if std::hint::black_box(true) {
                panic!("handler panic with secret 10.0.0.9");
            }
            Ok(Value::Null)
        })
        .ws_handler("test.slow", |_ctx, _| async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(Value::Null)
        })
        .ws_handler("test.big_answer", |_ctx, _| async move { Ok(json!("x".repeat(100 * 1024))) })
        .ws_handler("test.internal", |_ctx, _| async move { Err::<Value, _>(AppError::internal(std::io::Error::other("db password=hunter2"))) })
        .ws_handler("test.blocked", |_ctx, _| async move { Ok(Value::Null) })
        .ws_handler("test.sleep", |_ctx, data| async move {
            tokio::time::sleep(Duration::from_millis(data.as_u64().unwrap_or(0))).await;
            Ok(json!("slept"))
        })
        .ws_handler("test.roles", |ctx, _| async move { Ok(json!(ctx.auth.roles)) })
        .ws_handler("test.push_then_sleep", |ctx, _| async move {
            ctx.hub().push_connection(ctx.connection, &Note { n: 77 })?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(json!("done"))
        })
}

async fn start(tweak: impl FnOnce(&mut Config)) -> Server {
    start_with(tweak, "sqlite::memory:", cheap_auth()).await
}

async fn start_with(tweak: impl FnOnce(&mut Config), url: &str, auth: AuthConfig) -> Server {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("ws-migrations");
    config.server.shutdown_grace_secs = 5;
    config.ws.query_token = true;
    tweak(&mut config);
    let clock = Arc::new(ManualClock::new(UnixMillis::now()));
    let probe = Arc::new(Probe::default());
    let (p1, p2, p3, p4) = (probe.clone(), probe.clone(), probe.clone(), probe.clone());
    let server = NetBackendServer::new(config)
        .clock(clock.clone())
        .authenticator(FlakyAuth)
        .broadcaster(Counting(probe.clone()))
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .after::<AfterWsConnect, _, _>(move |_ctx, _event| {
            let probe = p1.clone();
            async move {
                probe.connects.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .after::<AfterWsDisconnect, _, _>(move |_ctx, event| {
            let probe = p2.clone();
            async move {
                if let Ok(mut list) = probe.disconnects.lock() {
                    list.push((*event).clone());
                }
                Ok(())
            }
        })
        .before::<BeforeWsConnect, _, _>(move |_ctx, event| {
            let probe = p3.clone();
            async move {
                if probe.refuse_connect.load(Ordering::SeqCst) {
                    return Ok(Decision::Reject(AppError::forbidden("maintenance")));
                }
                if probe.slow_connect.load(Ordering::SeqCst) {
                    probe.entered.notify_one();
                    probe.release.notified().await;
                }
                Ok(Decision::Continue(event))
            }
        })
        .before::<BeforeWsFrame, _, _>(move |_ctx, mut event| {
            let _probe = p4.clone();
            async move {
                if event.kind == "test.blocked" {
                    return Ok(Decision::Reject(AppError::forbidden("blocked by a hook")));
                }
                if event.kind == "test.echo" && event.data.get("rewrite").is_some() {
                    event.data = json!({"rewritten": true});
                }
                Ok(Decision::Continue(event))
            }
        });
    let prepared = handlers(server).build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    let state = prepared.state().clone();
    let router = prepared.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(prepared.serve_with_shutdown(listener, async move {
        let _ = stopped.await;
    }));
    Server { addr, state, router, clock, probe, stop: Some(stop), task: Some(task) }
}

impl Server {
    async fn register(&self, email: &str) -> (UserId, String) {
        let (status, _, body) = common::call(
            &self.router,
            common::post_json(routes::auth::REGISTER, json!({"email": email, "password": PASSWORD, "display_name": "Player"}).to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (UserId(body["account"]["id"].as_i64().expect("id")), body["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    fn service(&self) -> Arc<AuthService> {
        self.state.get::<AuthService>().expect("auth service")
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let result = tokio::time::timeout(Duration::from_secs(30), task).await;
            assert!(matches!(result, Ok(Ok(Ok(())))), "the server did not stop cleanly: {result:?}");
        }
    }

    async fn wait_connections(&self, expected: usize) {
        let deadline = Instant::now() + WAIT;
        while self.state.ws().stats().connections != expected {
            assert!(Instant::now() < deadline, "connections stayed {:?}, expected {expected}", self.state.ws().stats());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until `user` has `expected` registered sockets. The client's handshake can finish
    /// before the server's socket task registered the user: pushes from the server side before
    /// that reach nobody (on a slow runner this is a real window).
    async fn wait_registered(&self, user: UserId, expected: usize) {
        let deadline = Instant::now() + WAIT;
        while self.state.ws().connections_of(user).len() != expected {
            assert!(Instant::now() < deadline, "{user} has {} sockets, expected {expected}", self.state.ws().connections_of(user).len());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait until the disconnect hook ran `expected` times (it runs after the socket is
    /// unregistered, so the connection count can reach 0 first).
    async fn wait_disconnects(&self, expected: usize) {
        let deadline = Instant::now() + WAIT;
        while self.probe.disconnects.lock().map(|l| l.len()).unwrap_or_default() < expected {
            assert!(Instant::now() < deadline, "the disconnect hook did not run {expected} times");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn connect_with(addr: SocketAddr, token: Option<&str>, headers: &[(&str, &str)], query: &str) -> Result<Ws, Box<tokio_tungstenite::tungstenite::Error>> {
    let mut request = format!("ws://{addr}{}{query}", routes::WS).into_client_request().map_err(Box::new)?;
    if let Some(token) = token {
        request.headers_mut().insert("authorization", format!("Bearer {token}").parse().expect("header"));
    }
    for (name, value) in headers {
        request.headers_mut().insert(http::HeaderName::from_bytes(name.as_bytes()).expect("name"), value.parse().expect("header"));
    }
    let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request)).await.expect("handshake in time").map_err(Box::new)?;
    Ok(ws)
}

async fn connect(addr: SocketAddr, token: Option<&str>) -> Ws {
    connect_with(addr, token, &[], "").await.expect("connect")
}

/// The HTTP status (and JSON body) of a refused handshake.
async fn refused(addr: SocketAddr, token: Option<&str>, headers: &[(&str, &str)]) -> (u16, Value) {
    match connect_with(addr, token, headers, "").await {
        Ok(_) => panic!("the handshake was accepted"),
        Err(error) => match *error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                let body = response.body().as_ref().and_then(|b| serde_json::from_slice(b).ok()).unwrap_or(Value::Null);
                (response.status().as_u16(), body)
            }
            other => panic!("not an HTTP refusal: {other}"),
        },
    }
}

async fn send(ws: &mut Ws, value: Value) {
    ws.send(Message::text(value.to_string())).await.expect("send");
}

/// The next data or close frame (pings / pongs skipped).
async fn next(ws: &mut Ws) -> Option<Message> {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await.expect("a frame in time") {
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(message)) => return Some(message),
            Some(Err(_)) | None => return None,
        }
    }
}

async fn recv(ws: &mut Ws) -> Value {
    match next(ws).await {
        Some(Message::Text(text)) => serde_json::from_str(text.as_str()).expect("JSON"),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

/// Read until the close frame; its code (`None`: the connection ended without one).
async fn close_code(ws: &mut Ws) -> Option<u16> {
    loop {
        match next(ws).await {
            Some(Message::Close(frame)) => return frame.map(|f| u16::from(f.code)),
            Some(_) => continue,
            None => return None,
        }
    }
}

async fn call(ws: &mut Ws, id: u64, kind: &str, data: Value) -> Value {
    send(ws, json!({"id": id, "type": kind, "data": data})).await;
    let answer = recv(ws).await;
    assert_eq!(answer["id"], id, "{answer}");
    answer
}

fn auth_frame(token: &str) -> Value {
    json!({"type": "auth", "data": {"token": token, "protocol": 1}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handshake_auth_requests_and_pushes() {
    let server = start(|_| {}).await;
    let (user, token) = server.register("ada@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    let answer = call(&mut ws, 1, "test.echo", json!({"hello": [1, 2]})).await;
    assert_eq!(answer, json!({"id": 1, "ok": true, "data": {"hello": [1, 2]}}));
    let answer = call(&mut ws, 2, "test.typed", json!({"text": "hi"})).await;
    assert_eq!(answer["data"], json!({"text": "hi", "user": user.get()}));
    let who = call(&mut ws, u64::MAX, "test.whoami", Value::Null).await;
    assert_eq!(who["data"]["user"], user.get());
    assert!(who["data"]["session"].is_i64());
    // Pushes: typed to the user, raw to everyone; never an `id` or `ok`.
    server.state.ws().push_user(user, &Note { n: 7 }).expect("push");
    assert_eq!(recv(&mut ws).await, json!({"type": "test.note", "data": {"n": 7}}));
    server.state.ws().push_raw(Target::All, "test.broadcast", &json!([1])).expect("push");
    assert_eq!(recv(&mut ws).await, json!({"type": "test.broadcast", "data": [1]}));
    assert!(matches!(server.state.ws().push_raw(Target::All, "auth.ok", &json!({})), Err(PushError::ReservedKind)));
    assert!(server.state.ws().is_online(user));
    assert_eq!(server.probe.connects.load(Ordering::SeqCst), 1);
    // `?token=` works too (query never logged).
    let mut by_query = connect_with(server.addr, None, &[], &format!("?token={token}")).await.expect("query token");
    assert_eq!(call(&mut by_query, 1, "test.echo", json!(1)).await["ok"], true);
    // The client closes normally: the disconnect hook sees no server code.
    ws.close(None).await.expect("close");
    drop(ws);
    drop(by_query);
    server.wait_connections(0).await;
    server.wait_disconnects(2).await;
    let disconnects = server.probe.disconnects.lock().map(|l| l.len()).unwrap_or_default();
    assert_eq!(disconnects, 2);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_message_auth_and_exactly_one_answer_per_auth() {
    let server = start(|_| {}).await;
    let (user, token) = server.register("bo@example.com").await;
    let mut ws = connect(server.addr, None).await;
    // Before authenticating, requests are refused (and answered).
    let answer = call(&mut ws, 1, "test.echo", json!(1)).await;
    assert_eq!(answer["ok"], false);
    assert_eq!(answer["error"]["code"], codes::UNAUTHORIZED);
    send(&mut ws, auth_frame(&token)).await;
    assert_eq!(recv(&mut ws).await, json!({"type": "auth.ok", "data": {"user_id": user.get(), "protocol": 1}}));
    // A request right behind the auth frame (no ack awaited) is processed after it.
    send(&mut ws, auth_frame(&token)).await;
    send(&mut ws, json!({"id": 2, "type": "test.echo", "data": "after"})).await;
    assert_eq!(recv(&mut ws).await["type"], "auth.ok");
    assert_eq!(recv(&mut ws).await, json!({"id": 2, "ok": true, "data": "after"}));
    // A header-authenticated socket also answers an `auth` frame (with_auth_ack clients).
    let mut both = connect(server.addr, Some(&token)).await;
    send(&mut both, auth_frame(&token)).await;
    assert_eq!(recv(&mut both).await["type"], "auth.ok");
    assert_eq!(call(&mut both, 3, "test.echo", json!(3)).await["data"], 3);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_deadline_closes_with_1008() {
    let server = start(|config| config.ws.auth_timeout_secs = 1).await;
    let started = Instant::now();
    let mut ws = connect(server.addr, None).await;
    assert_eq!(close_code(&mut ws).await, Some(1008));
    assert!(started.elapsed() >= Duration::from_millis(900));
    server.wait_connections(0).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_failures() {
    let server = start(|_| {}).await;
    let (_, ada) = server.register("ada2@example.com").await;
    let (_, bo) = server.register("bo2@example.com").await;
    // A bad token by first message: auth.failed, then 4001.
    let mut ws = connect(server.addr, None).await;
    send(&mut ws, auth_frame("nbsa_0000000000000000000000000000000000000000000000000000000000000000")).await;
    let failed = recv(&mut ws).await;
    assert_eq!(failed["type"], "auth.failed");
    assert_eq!(failed["error"]["code"], codes::UNAUTHORIZED);
    assert_eq!(close_code(&mut ws).await, Some(4001));
    // A malformed auth frame still gets its one answer.
    let mut ws = connect(server.addr, None).await;
    send(&mut ws, json!({"type": "auth", "data": {"nope": 1}})).await;
    let failed = recv(&mut ws).await;
    assert_eq!((failed["type"].as_str(), failed["error"]["code"].as_str()), (Some("auth.failed"), Some(codes::BAD_REQUEST)));
    assert_eq!(close_code(&mut ws).await, Some(4001));
    // Another user's token on an authenticated socket.
    let mut ws = connect(server.addr, Some(&ada)).await;
    send(&mut ws, auth_frame(&bo)).await;
    assert_eq!(recv(&mut ws).await["type"], "auth.failed");
    assert_eq!(close_code(&mut ws).await, Some(4001));
    // An unsupported protocol in `auth`: auth.failed, then 4010.
    let mut ws = connect(server.addr, None).await;
    send(&mut ws, json!({"type": "auth", "data": {"token": ada, "protocol": 99}})).await;
    let failed = recv(&mut ws).await;
    assert_eq!(failed["error"]["code"], codes::UNSUPPORTED_PROTOCOL);
    assert_eq!(failed["error"]["details"], json!({"supported_min": 1, "supported_max": 1}));
    assert_eq!(close_code(&mut ws).await, Some(4010));
    // Bad credentials at the handshake: 401 before the upgrade (never 400).
    assert_eq!(refused(server.addr, Some("nbsa_bad"), &[]).await.0, 401);
    assert_eq!(refused(server.addr, None, &[("authorization", "Basic dTpw")]).await.0, 401);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_mismatch_upgrades_then_closes_4010() {
    let server = start(|_| {}).await;
    let (_, token) = server.register("v@example.com").await;
    let mut ws = connect_with(server.addr, Some(&token), &[(PROTOCOL_HEADER, "99")], "").await.expect("upgraded");
    assert_eq!(close_code(&mut ws).await, Some(4010));
    let mut ok = connect_with(server.addr, Some(&token), &[(PROTOCOL_HEADER, "1")], "").await.expect("version 1");
    assert_eq!(call(&mut ok, 1, "test.echo", json!(1)).await["ok"], true);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_expiry_keeps_open_sockets_but_refuses_new_handshakes() {
    let server = start(|_| {}).await;
    let (_, token) = server.register("exp@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    server.clock.advance(3_601_000);
    // The open socket keeps working past the access token's expiry.
    assert_eq!(call(&mut ws, 1, "test.echo", json!("still here")).await["data"], "still here");
    // A new handshake with the expired token: 401 token_expired (the client refreshes).
    let (status, body) = refused(server.addr, Some(&token), &[]).await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], codes::TOKEN_EXPIRED);
    // First-message auth with it: auth.failed token_expired + 4001.
    let mut late = connect(server.addr, None).await;
    send(&mut late, auth_frame(&token)).await;
    assert_eq!(recv(&mut late).await["error"]["code"], codes::TOKEN_EXPIRED);
    assert_eq!(close_code(&mut late).await, Some(4001));
    assert_eq!(call(&mut ws, 2, "test.echo", json!(2)).await["ok"], true);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logout_closes_4001_and_ban_closes_4003() {
    let server = start(|_| {}).await;
    let (_, token) = server.register("out@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    let mut other_session = {
        let (status, _, body) =
            common::call(&server.router, common::post_json(routes::auth::LOGIN, json!({"email": "out@example.com", "password": PASSWORD}).to_string())).await;
        assert_eq!(status, StatusCode::OK);
        connect(server.addr, Some(body["tokens"]["access_token"].as_str().expect("token"))).await
    };
    let logout = http::Request::post(routes::auth::LOGOUT)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{}"))
        .expect("request");
    let (status, _, body) = common::call(&server.router, logout).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(close_code(&mut ws).await, Some(4001));
    // The other session's socket stays open.
    assert_eq!(call(&mut other_session, 1, "test.echo", json!(1)).await["ok"], true);
    // A ban closes every socket of the user with 4003, and the handshake is then 403.
    let (banned, banned_token) = server.register("ban@example.com").await;
    let mut a = connect(server.addr, Some(&banned_token)).await;
    let mut b = connect(server.addr, None).await;
    send(&mut b, auth_frame(&banned_token)).await;
    assert_eq!(recv(&mut b).await["type"], "auth.ok");
    server.service().ban_user(&server.state, banned, BanRequest::new()).await.expect("ban");
    assert_eq!(close_code(&mut a).await, Some(4003));
    assert_eq!(close_code(&mut b).await, Some(4003));
    let (status, body) = refused(server.addr, Some(&banned_token), &[]).await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], codes::BANNED);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revocations_from_another_process_close_sockets() {
    let dir = common::temp_dir("ws-cross");
    let url = format!("sqlite:{}", dir.join("cross.db").display().to_string().replace('\\', "/"));
    let mut auth = cheap_auth();
    auth.revocation_poll_secs = 1;
    let server = start_with(|_| {}, &url, auth).await;
    let (user, token) = server.register("cross@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    assert_eq!(call(&mut ws, 1, "test.echo", json!(1)).await["ok"], true);
    // The "other process" (the command line, another instance): its own server on the same file.
    let mut other_auth = cheap_auth();
    other_auth.revocation_poll_secs = 0;
    let mut config = Config::default();
    config.database.url = SecretString::new(&url);
    config.database.migrations_dir = common::temp_dir("ws-cross-migrations");
    let other = NetBackendServer::new(config).module(Auth::new().with_config(other_auth).mailer(MemoryMailer::new())).build().await.expect("build other");
    let service = other.state().get::<AuthService>().expect("service");
    service.ban_user(other.state(), user, BanRequest::new()).await.expect("ban from the other process");
    // Within the WAIT bound (the poll runs every second; a slow runner may take longer).
    assert_eq!(close_code(&mut ws).await, Some(4003));
    other.state().db().close().await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_frames_and_handler_failures() {
    let server = start(|config| {
        config.ws.request_timeout_secs = 1;
        config.ws.max_message_bytes = 64 * 1024;
    })
    .await;
    let (_, token) = server.register("mal@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    // With a usable id: answered bad_request (lenient id extraction).
    send(&mut ws, json!({"id": 5, "type": 7})).await;
    assert_eq!(recv(&mut ws).await, json!({"id": 5, "ok": false, "error": {"code": "bad_request", "message": "the request is malformed"}}));
    // Without one: dropped (the next answer is for the next request).
    ws.send(Message::text("not json")).await.expect("send");
    send(&mut ws, json!({"type": "test.echo", "data": 1})).await;
    send(&mut ws, json!({"id": -1, "type": "test.echo"})).await;
    ws.send(Message::binary(vec![1u8, 2, 3])).await.expect("send");
    assert_eq!(call(&mut ws, 6, "test.echo", json!("next")).await["data"], "next");
    // Unknown kind, bad typed data, a panic, an internal error, a timeout, a too-large answer.
    assert_eq!(call(&mut ws, 7, "test.nope", Value::Null).await["error"]["code"], codes::UNKNOWN_TYPE);
    assert_eq!(call(&mut ws, 8, "test.typed", json!({"wrong": 1})).await["error"]["code"], codes::BAD_REQUEST);
    let panicked = call(&mut ws, 9, "test.panic", Value::Null).await;
    assert_eq!(panicked["error"], json!({"code": "internal", "message": "internal server error"}));
    let internal = call(&mut ws, 10, "test.internal", Value::Null).await;
    assert_eq!(internal["error"], json!({"code": "internal", "message": "internal server error"}));
    assert!(!internal.to_string().contains("hunter2"));
    assert_eq!(call(&mut ws, 11, "test.slow", Value::Null).await["error"]["code"], codes::UNAVAILABLE);
    assert_eq!(call(&mut ws, 12, "test.big_answer", Value::Null).await["error"]["code"], codes::PAYLOAD_TOO_LARGE);
    // Hooks: refuse a kind, rewrite data.
    assert_eq!(call(&mut ws, 13, "test.blocked", Value::Null).await["error"]["code"], codes::FORBIDDEN);
    assert_eq!(call(&mut ws, 14, "test.echo", json!({"rewrite": 1})).await["data"], json!({"rewritten": true}));
    // The socket survived all of it.
    assert_eq!(call(&mut ws, 15, "test.echo", json!(15)).await["data"], 15);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rooms_and_room_caps() {
    let server = start(|config| {
        config.ws.max_room_members = 2;
        config.ws.max_rooms_per_connection = 2;
    })
    .await;
    let (ada, ada_token) = server.register("room-a@example.com").await;
    let (_, bo_token) = server.register("room-b@example.com").await;
    let (_, cy_token) = server.register("room-c@example.com").await;
    let mut a = connect(server.addr, Some(&ada_token)).await;
    let mut b = connect(server.addr, Some(&bo_token)).await;
    let mut c = connect(server.addr, Some(&cy_token)).await;
    assert_eq!(call(&mut a, 1, "test.join", json!({"room": "lobby"})).await["data"]["size"], 1);
    assert_eq!(call(&mut b, 1, "test.join", json!({"room": "lobby"})).await["data"]["size"], 2);
    assert_eq!(call(&mut c, 1, "test.join", json!({"room": "lobby"})).await["error"]["code"], codes::ROOM_FULL);
    assert_eq!(call(&mut c, 2, "test.join", json!({"room": ""})).await["error"]["code"], codes::BAD_REQUEST);
    call(&mut a, 2, "test.say", json!({"room": "lobby", "text": "hi"})).await;
    let expected = json!({"type": "test.said", "data": {"from": ada.get(), "text": "hi"}});
    assert_eq!(recv(&mut a).await, expected);
    assert_eq!(recv(&mut b).await, expected);
    // Per-connection room cap.
    assert_eq!(call(&mut a, 3, "test.join", json!({"room": "second"})).await["ok"], true);
    assert_eq!(call(&mut a, 4, "test.join", json!({"room": "third"})).await["error"]["code"], codes::QUOTA_EXCEEDED);
    assert_eq!(call(&mut a, 5, "test.leave", json!({"room": "second"})).await["data"]["left"], true);
    assert_eq!(server.state.ws().room_size("lobby"), 2);
    // Membership ends with the socket; the disconnect hook lists the rooms it left.
    drop(b);
    let deadline = Instant::now() + WAIT;
    while server.state.ws().room_size("lobby") != 1 {
        assert!(Instant::now() < deadline, "b never left the room");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(call(&mut c, 3, "test.join", json!({"room": "lobby"})).await["data"]["size"], 2);
    let deadline = Instant::now() + WAIT;
    loop {
        let rooms = server.probe.disconnects.lock().map(|l| l.iter().map(|d| d.rooms.clone()).collect::<Vec<_>>()).unwrap_or_default();
        if rooms.iter().any(|r| r.iter().any(|room| &**room == "lobby")) {
            break;
        }
        assert!(Instant::now() < deadline, "no disconnect hook with the room");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_caps() {
    let server = start(|config| {
        config.ws.max_connections = 3;
        config.ws.max_connections_per_user = 2;
    })
    .await;
    let (user, token) = server.register("caps@example.com").await;
    let (_, other) = server.register("caps2@example.com").await;
    let mut first = connect(server.addr, Some(&token)).await;
    // "Oldest" is registration order: register the first before the second.
    server.wait_registered(user, 1).await;
    let mut second = connect(server.addr, Some(&token)).await;
    assert_eq!(call(&mut second, 1, "test.echo", json!(1)).await["ok"], true);
    // A third socket of the same user replaces the oldest (4009).
    let mut third = connect(server.addr, Some(&token)).await;
    assert_eq!(close_code(&mut first).await, Some(4009));
    assert_eq!(call(&mut third, 1, "test.echo", json!(1)).await["ok"], true);
    server.wait_connections(2).await;
    let _x = connect(server.addr, Some(&other)).await;
    // The global cap: 503 (retried by clients) once full.
    let (status, body) = refused(server.addr, Some(&other), &[]).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], codes::UNAVAILABLE);
    drop(second);
    server.wait_connections(2).await;
    let _y = connect(server.addr, Some(&other)).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_consumers_are_closed() {
    let server = start(|config| {
        config.ws.outbox_frames = 4;
        config.ws.write_timeout_secs = 2;
    })
    .await;
    let (slow, slow_token) = server.register("slow@example.com").await;
    // A reader that starts late: its outbox overflows, it gets 1013 after what was already sent.
    let mut late = connect(server.addr, Some(&slow_token)).await;
    // Pushes reach registered sockets only: wait for the server side first (CI flake, 2026-10-01:
    // on a slow runner the 2000 pushes went out before the socket was registered).
    server.wait_registered(slow, 1).await;
    let chunk = "y".repeat(1024);
    for n in 0..2000 {
        server.state.ws().push_raw(Target::User(slow), "test.flood", &json!({"n": n, "pad": chunk})).expect("push");
    }
    let mut got = 0;
    let code = loop {
        match next(&mut late).await {
            Some(Message::Text(_)) => got += 1,
            Some(Message::Close(frame)) => break frame.map(|f| u16::from(f.code)),
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(code, Some(1013));
    assert!(got < 2000, "{got}");
    server.wait_connections(0).await;
    // A peer that never reads at all: the write deadline drops it.
    let (stuck, stuck_token) = server.register("stuck@example.com").await;
    let _stuck = connect(server.addr, Some(&stuck_token)).await;
    server.wait_registered(stuck, 1).await;
    let big = "z".repeat(512 * 1024);
    for _ in 0..200 {
        server.state.ws().push_raw(Target::User(stuck), "test.flood", &big).expect("push");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    server.wait_connections(0).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeats_and_dead_peers() {
    let server = start(|config| {
        config.ws.ping_interval_secs = 1;
        config.ws.idle_timeout_secs = 3;
        config.ws.request_timeout_secs = 1;
    })
    .await;
    let (_, token) = server.register("beat@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    // The server pings.
    let deadline = Instant::now() + WAIT;
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await.expect("a frame") {
            Some(Ok(Message::Ping(_))) => break,
            Some(Ok(_)) => {}
            other => panic!("{other:?}"),
        }
        assert!(Instant::now() < deadline);
    }
    // The server answers our ping.
    ws.send(Message::Ping(b"hb".to_vec().into())).await.expect("ping");
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await.expect("a frame") {
            Some(Ok(Message::Pong(data))) => {
                assert_eq!(&data[..], b"hb");
                break;
            }
            Some(Ok(_)) => {}
            other => panic!("{other:?}"),
        }
    }
    // A live client (it answers pings while we read) stays for longer than the idle timeout.
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    }
    assert_eq!(call(&mut ws, 1, "test.echo", json!(1)).await["ok"], true);
    // A peer that goes silent (never polled: no pongs) is dropped after the idle timeout.
    let _silent = connect(server.addr, Some(&token)).await;
    server.wait_connections(2).await;
    let deadline = Instant::now() + WAIT;
    while server.state.ws().stats().connections != 1 {
        assert!(Instant::now() < deadline, "the silent peer was not dropped");
        // Keep the first one alive.
        let _ = tokio::time::timeout(Duration::from_millis(100), ws.next()).await;
    }
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_and_size_limits() {
    let server = start(|config| {
        config.ws.frames_per_second = 1;
        config.ws.frame_burst = 3;
        config.ws.max_message_bytes = 64 * 1024;
    })
    .await;
    let (_, token) = server.register("rate@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    for id in 1..=3 {
        send(&mut ws, json!({"id": id, "type": "test.echo", "data": id})).await;
    }
    send(&mut ws, json!({"id": 4, "type": "test.echo", "data": 4})).await;
    for id in 1..=3 {
        assert_eq!(recv(&mut ws).await["data"], id);
    }
    let limited = recv(&mut ws).await;
    assert_eq!(limited["id"], 4);
    assert_eq!(limited["error"]["code"], codes::RATE_LIMITED);
    assert!(limited["error"]["details"]["retry_after_ms"].as_u64().is_some_and(|ms| ms >= 1));
    // Flooding on: closed with 1008.
    for id in 5..40 {
        if ws.send(Message::text(json!({"id": id, "type": "test.echo"}).to_string())).await.is_err() {
            break;
        }
    }
    assert_eq!(close_code(&mut ws).await, Some(1008));
    // A message over the limit: 1009.
    let mut big = connect(server.addr, Some(&token)).await;
    let _ = big.send(Message::text("x".repeat(65 * 1024))).await;
    assert_eq!(close_code(&mut big).await, Some(1009));
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hooks_can_refuse_connections() {
    let server = start(|_| {}).await;
    let (_, token) = server.register("hook@example.com").await;
    server.probe.refuse_connect.store(true, Ordering::SeqCst);
    let (status, body) = refused(server.addr, Some(&token), &[]).await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["message"], "maintenance");
    let mut ws = connect(server.addr, None).await;
    send(&mut ws, auth_frame(&token)).await;
    assert_eq!(recv(&mut ws).await["error"]["code"], codes::FORBIDDEN);
    assert_eq!(close_code(&mut ws).await, Some(4001));
    server.probe.refuse_connect.store(false, Ordering::SeqCst);
    let mut ok = connect(server.addr, Some(&token)).await;
    assert_eq!(call(&mut ok, 1, "test.echo", json!(1)).await["ok"], true);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_closes_sockets_with_1001() {
    // A long grace: the shutdown must not wait for it (the sockets close at once with 1001).
    let server = start(|config| config.server.shutdown_grace_secs = 120).await;
    let (_, token) = server.register("bye@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    let mut anonymous = connect(server.addr, None).await;
    assert_eq!(call(&mut ws, 1, "test.echo", json!(1)).await["ok"], true);
    let addr = server.addr;
    let started = Instant::now();
    let stopping = tokio::spawn(server.stop());
    assert_eq!(close_code(&mut ws).await, Some(1001));
    assert_eq!(close_code(&mut anonymous).await, Some(1001));
    tokio::time::timeout(Duration::from_secs(60), stopping).await.expect("stopped in time").expect("no panic");
    assert!(started.elapsed() < Duration::from_secs(60), "waited for the grace: {:?}", started.elapsed());
    assert!(connect_with(addr, Some(&token), &[], "").await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn asyncapi_document_lists_the_kinds() {
    let server = start(|_| {}).await;
    let (status, _, doc) = common::call(&server.router, common::get(net_backend_server::ASYNCAPI_PATH)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["asyncapi"], "3.0.0");
    for kind in ["test.echo", "test.typed", "test.join"] {
        assert!(doc["components"]["messages"].get(format!("request.{kind}")).is_some(), "{kind}");
    }
    server.stop().await;
}

#[tokio::test]
async fn duplicate_or_reserved_kinds_stop_the_build() {
    let mut config = Config::default();
    config.database.url = SecretString::new("sqlite::memory:");
    let error = NetBackendServer::new(config)
        .ws_handler("game.x", |_, d| async move { Ok(d) })
        .ws_handler("game.x", |_, d| async move { Ok(d) })
        .ws_handler("auth.ok", |_, d| async move { Ok(d) })
        .build()
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(error.contains("`game.x` is registered twice") && error.contains("`auth.ok` is reserved"), "{error}");
}

/// S1: a running handler does not stop the socket's writer; pushes queued meanwhile follow its
/// answer; a fast reader never gets 1013.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_flow_while_a_handler_runs() {
    let server = start(|config| config.ws.outbox_frames = 8).await;
    let (user, token) = server.register("busy@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    server.wait_registered(user, 1).await;
    send(&mut ws, json!({"id": 1, "type": "test.sleep", "data": 1500})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for n in 0..20u32 {
        server.state.ws().push_user(user, &Note { n }).expect("push");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // 20 pushes into an outbox of 8 while the handler runs: written as they come (they are not
    // the handler's own), nothing lost, no 1013; the answer when the handler is done.
    let mut notes = Vec::new();
    let mut answered = false;
    while notes.len() < 20 || !answered {
        let frame = recv(&mut ws).await;
        if frame["id"] == 1 {
            assert_eq!(frame["data"], "slept");
            answered = true;
        } else {
            assert_eq!(frame["type"], "test.note", "{frame}");
            notes.push(frame["data"]["n"].as_u64().unwrap_or(99));
        }
    }
    assert_eq!(notes, (0..20).collect::<Vec<u64>>());
    // The handler's own push to its socket follows its answer, even when made long before.
    assert_eq!(call(&mut ws, 3, "test.push_then_sleep", Value::Null).await["data"], "done");
    assert_eq!(recv(&mut ws).await, json!({"type": "test.note", "data": {"n": 77}}));
    assert_eq!(call(&mut ws, 2, "test.echo", json!(2)).await["data"], 2);
    server.stop().await;
}

/// S2: a push over the message limit is refused to the caller, nobody is disconnected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_pushes_are_refused() {
    let server = start(|_| {}).await;
    let (user, token) = server.register("big@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    server.wait_registered(user, 1).await;
    for size in [1_080_000usize, 2 * 1024 * 1024] {
        let result = server.state.ws().push_raw(Target::User(user), "test.big", &"x".repeat(size));
        assert!(matches!(result, Err(PushError::TooLarge { .. })), "{size}: {result:?}");
        let result = server.state.ws().push_raw(Target::All, "test.big", &"x".repeat(size));
        assert!(matches!(result, Err(PushError::TooLarge { .. })), "{size}");
    }
    server.state.ws().push_user(user, &Note { n: 1 }).expect("a small push still works");
    assert_eq!(recv(&mut ws).await["type"], "test.note");
    assert_eq!(call(&mut ws, 1, "test.echo", json!(1)).await["ok"], true);
    server.stop().await;
}

/// S3: a ban landing while the socket authenticates (slow hook, no revocation poll) still closes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bans_during_authentication_are_applied() {
    let mut auth = cheap_auth();
    auth.revocation_poll_secs = 0;
    // The connect hook waits for the test (not a fixed time): no hook timeout in between.
    let server = start_with(|config| config.server.hook_timeout_ms = 120_000, "sqlite::memory:", auth).await;
    let (user, token) = server.register("race@example.com").await;
    let (other, other_token) = server.register("race2@example.com").await;
    server.probe.slow_connect.store(true, Ordering::SeqCst);
    // Handshake path: the hook holds the socket after the token check; the ban lands meanwhile.
    let addr = server.addr;
    let header = tokio::spawn(async move { connect_with(addr, Some(&token), &[], "").await });
    tokio::time::timeout(WAIT, server.probe.entered.notified()).await.expect("the connect hook ran");
    server.service().ban_user(&server.state, user, BanRequest::new()).await.expect("ban");
    server.probe.release.notify_one();
    match header.await.expect("task") {
        Ok(mut ws) => assert_eq!(close_code(&mut ws).await, Some(4003)),
        Err(error) => panic!("{error}"),
    }
    // First-message path.
    let mut ws = connect(server.addr, None).await;
    send(&mut ws, auth_frame(&other_token)).await;
    tokio::time::timeout(WAIT, server.probe.entered.notified()).await.expect("the connect hook ran");
    server.service().ban_user(&server.state, other, BanRequest::new()).await.expect("ban");
    server.probe.release.notify_one();
    // The ban reaches the hub through the revocation stream (a task): when it lands before the
    // registration the `auth` is refused, when it lands right after, the fresh socket is closed.
    // Either way the banned player ends with 4003 (the old fixed 200 ms / 600 ms timing always
    // took the first branch).
    let answer = recv(&mut ws).await;
    assert!(answer["type"] == "auth.failed" || answer["type"] == "auth.ok", "{answer}");
    assert_eq!(close_code(&mut ws).await, Some(4003));
    server.wait_connections(0).await;
    server.stop().await;
}

/// S4: a temporary failure while authenticating is retryable (503 / close 1013, no auth.failed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temporary_auth_failures_are_retryable() {
    let server = start(|_| {}).await;
    assert_eq!(refused(server.addr, Some("flaky"), &[]).await.0, 503);
    let mut ws = connect(server.addr, None).await;
    send(&mut ws, auth_frame("flaky")).await;
    match next(&mut ws).await {
        Some(Message::Close(frame)) => assert_eq!(frame.map(|f| u16::from(f.code)), Some(1013)),
        other => panic!("expected close 1013 without auth.failed, got {other:?}"),
    }
    server.stop().await;
}

/// S5: a role change reaches an open socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn role_changes_reach_open_sockets() {
    let server = start(|_| {}).await;
    let (user, token) = server.register("mod@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    assert_eq!(call(&mut ws, 1, "test.roles", Value::Null).await["data"], json!([]));
    server.service().set_user_role(&server.state, user, "moderator", true).await.expect("grant");
    let deadline = Instant::now() + WAIT;
    let mut id = 2;
    while call(&mut ws, id, "test.roles", Value::Null).await["data"] != json!(["moderator"]) {
        assert!(Instant::now() < deadline, "the role never arrived");
        id += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.service().set_user_role(&server.state, user, "moderator", false).await.expect("revoke");
    let deadline = Instant::now() + WAIT;
    while call(&mut ws, id + 100, "test.roles", Value::Null).await["data"] != json!([]) {
        assert!(Instant::now() < deadline, "the role was never removed");
        id += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.stop().await;
}

/// S6: connection pushes never go through the broadcaster (ids are per instance).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_pushes_stay_local() {
    let server = start(|_| {}).await;
    let (user, token) = server.register("local@example.com").await;
    let mut ws = connect(server.addr, Some(&token)).await;
    let who = call(&mut ws, 1, "test.whoami", Value::Null).await;
    let id = server.state.ws().connections_of(user)[0];
    assert_eq!(who["data"]["connection"], id.get());
    let before = server.probe.published.load(Ordering::SeqCst);
    server.state.ws().push_connection(id, &Note { n: 1 }).expect("push");
    assert_eq!(recv(&mut ws).await["data"]["n"], 1);
    assert_eq!(server.probe.published.load(Ordering::SeqCst), before);
    server.state.ws().push_user(user, &Note { n: 2 }).expect("push");
    assert_eq!(recv(&mut ws).await["data"]["n"], 2);
    assert_eq!(server.probe.published.load(Ordering::SeqCst), before + 1);
    server.stop().await;
}

/// S7: per-address and pending caps; the decision on 4009: the same session goes first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn address_pending_and_session_caps() {
    let server = start(|config| {
        config.ws.max_pending_connections = 1;
        config.ws.max_connections_per_ip = 4;
        config.ws.max_connections_per_user = 2;
    })
    .await;
    let (user, token) = server.register("cap@example.com").await;
    let _pending = connect(server.addr, None).await;
    let (status, body) = refused(server.addr, None, &[]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (503, Some(codes::UNAVAILABLE)));
    // Authenticated sockets are not held back by the pending cap.
    let (login, _, body) =
        common::call(&server.router, common::post_json(routes::auth::LOGIN, json!({"email": "cap@example.com", "password": PASSWORD}).to_string())).await;
    assert_eq!(login, StatusCode::OK);
    let second_session = body["tokens"]["access_token"].as_str().expect("token").to_string();
    let mut a = connect(server.addr, Some(&second_session)).await;
    server.wait_registered(user, 1).await;
    let mut b = connect(server.addr, Some(&token)).await;
    assert_eq!(call(&mut b, 1, "test.echo", json!(1)).await["ok"], true);
    // A third socket of the user: the oldest of ITS session (b) goes, not the older a.
    let mut c = connect(server.addr, Some(&token)).await;
    assert_eq!(close_code(&mut b).await, Some(4009));
    assert_eq!(call(&mut a, 1, "test.echo", json!(1)).await["ok"], true);
    assert_eq!(call(&mut c, 1, "test.echo", json!(1)).await["ok"], true);
    server.wait_connections(3).await;
    // The per-address cap: 4 sockets from 127.0.0.1 (pending + a + c + this one), then 429.
    let _d = connect(server.addr, Some(&second_session)).await;
    assert_eq!(refused(server.addr, Some(&token), &[]).await.0, 429);
    server.stop().await;
}

/// N1: an `auth` over the frame rate limit is still answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limited_auth_is_answered() {
    let server = start(|config| {
        config.ws.frames_per_second = 1;
        config.ws.frame_burst = 1;
    })
    .await;
    let (_, token) = server.register("burst@example.com").await;
    let mut ws = connect(server.addr, None).await;
    send(&mut ws, auth_frame(&token)).await;
    send(&mut ws, auth_frame(&token)).await;
    assert_eq!(recv(&mut ws).await["type"], "auth.ok");
    assert_eq!(recv(&mut ws).await["type"], "auth.ok");
    server.stop().await;
}
