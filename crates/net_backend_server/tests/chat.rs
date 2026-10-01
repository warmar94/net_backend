//! The chat module on a real loopback server with a WebSocket client (tokio-tungstenite): rooms,
//! the answer-before-echo order and the nonce, history pages (WebSocket and HTTP), member and
//! per-connection caps (and simultaneous joins at the cap), the send rate, the text rules, hooks,
//! direct messages to every connection of both users, group rooms, deletion and moderation (with
//! the audit log), presence (transitions per user, disconnects, the cap and the rate), retention,
//! the documents; the 1d fix round: DM presence never shows the peer, the DM-open rate, removed
//! group members cut off on every instance (two servers sharing a database and a broadcaster) and
//! in the join race, concurrent DM sends into one room (the MySQL deadlock: an env-gated stress
//! test for MySQL / PostgreSQL too). Bounded: every wait has a timeout; every server is stopped.
//!
//! The suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "chat", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use futures_util::{SinkExt, StreamExt};
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig, AuthService};
use net_backend_server::chat::events::{BeforeChatJoin, BeforeChatSend, BeforeDirectOpen};
use net_backend_server::chat::{Chat, ChatConfig, ChatService, RoomSpec};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::admin::AuditQuery;
use net_backend_server::protocol::{codes, routes, MessageId, PageRequest, RoomId, UnixMillis, UserId};
use net_backend_server::ws::{Broadcaster, Delivery, LocalDelivery};
use net_backend_server::{AppError, AppState, Config, Error, ManualClock, NetBackendServer, SecretString};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const PASSWORD: &str = "correct horse battery";
/// The upper bound of every wait (tests wait for frames / conditions, never a fixed time; CI
/// runners are slow and shared).
const WAIT: Duration = Duration::from_secs(30);
const T0: i64 = 1_800_000_000_000;

struct Server {
    addr: SocketAddr,
    state: AppState,
    router: axum::Router,
    clock: Arc<ManualClock>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), Error>>>,
}

fn chat_config() -> ChatConfig {
    let mut chat = ChatConfig::default();
    chat.rooms = vec![RoomSpec::new("world").with_name("World"), RoomSpec::new("small").with_max_members(3), RoomSpec::new("trade"), RoomSpec::new("quiet")];
    chat
}

/// Several "instances" (servers sharing a database) linked like a pub/sub broadcaster would link
/// them: every delivery goes through JSON (as over Redis) to every instance. `drop_controls`
/// simulates a control delivery that has not arrived yet.
#[derive(Clone, Default)]
struct SharedBroadcaster {
    sinks: Arc<Mutex<Vec<LocalDelivery>>>,
    drop_controls: Arc<AtomicBool>,
}

impl Broadcaster for SharedBroadcaster {
    fn start(&self, local: LocalDelivery) {
        self.sinks.lock().expect("sinks").push(local);
    }

    fn publish(&self, _local: &LocalDelivery, delivery: Delivery) {
        if delivery.control.is_some() && self.drop_controls.load(Ordering::SeqCst) {
            return;
        }
        let wire = serde_json::to_string(&delivery).expect("serialize");
        let back: Delivery = serde_json::from_str(&wire).expect("deserialize");
        let sinks = self.sinks.lock().expect("sinks").clone();
        for sink in sinks {
            sink.deliver(&back);
        }
    }
}

async fn start(url: &str, tweak: impl FnOnce(&mut Config, &mut ChatConfig)) -> Server {
    start_with(url, None, |server| server, tweak).await
}

async fn start_with(
    url: &str,
    broadcaster: Option<SharedBroadcaster>,
    extra: impl FnOnce(NetBackendServer) -> NetBackendServer,
    tweak: impl FnOnce(&mut Config, &mut ChatConfig),
) -> Server {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("chat-migrations");
    config.server.shutdown_grace_secs = 5;
    let mut chat = chat_config();
    tweak(&mut config, &mut chat);
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let mut server = NetBackendServer::new(config);
    if let Some(broadcaster) = broadcaster {
        server = server.broadcaster(broadcaster);
    }
    let prepared = extra(server)
        .clock(clock.clone())
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Chat::new().with_config(chat))
        .before::<BeforeChatSend, _, _>(|_ctx, mut message| async move {
            if message.text.contains("buy gold") {
                return Ok(Decision::Reject(AppError::forbidden("no advertising")));
            }
            message.text = message.text.replace("darn", "d**n");
            if message.text == "make it empty" {
                message.text = "   ".into();
            }
            Ok(Decision::Continue(message))
        })
        .before::<BeforeDirectOpen, _, _>(|_ctx, open| async move {
            if open.peer.get() % 1000 == 999 {
                return Ok(Decision::Reject(AppError::forbidden("blocked")));
            }
            Ok(Decision::Continue(open))
        })
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let state = prepared.state().clone();
    let router = prepared.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(prepared.serve_with_shutdown(listener, async move {
        let _ = stopped.await;
    }));
    let server = Server { addr, state, router, clock, stop: Some(stop), task: Some(task) };
    // The configured rooms exist once the modules started (the server serves).
    let deadline = std::time::Instant::now() + WAIT;
    while server.service().room(&server.state, RoomId(1)).await.ok().flatten().is_none() {
        assert!(std::time::Instant::now() < deadline, "the rooms were never created");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    server
}

impl Server {
    fn service(&self) -> Arc<ChatService> {
        self.state.get::<ChatService>().expect("chat service")
    }

    async fn register(&self, email: &str, name: &str) -> (UserId, String) {
        let request = Request::post(routes::auth::REGISTER)
            .header("content-type", "application/json")
            .body(Body::from(json!({"email": email, "password": PASSWORD, "display_name": name}).to_string()))
            .expect("request");
        let (status, _, body) = common::call(&self.router, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (UserId(body["account"]["id"].as_i64().expect("id")), body["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    async fn http(&self, method: Method, path: &str, body: Option<Value>, token: &str) -> (StatusCode, Value) {
        let request = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"));
        let request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request");
        let response = self.router.clone().oneshot(request).await.expect("infallible");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.expect("body");
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// The id of a public room, by key (from `GET /v1/chat/rooms`).
    async fn room_of(&self, token: &str, key: &str) -> i64 {
        let (status, rooms) = self.http(Method::GET, routes::chat::ROOMS, None, token).await;
        assert_eq!(status, StatusCode::OK, "{rooms}");
        rooms["items"].as_array().and_then(|items| items.iter().find(|r| r["key"] == key)).and_then(|r| r["id"].as_i64()).expect("the room exists")
    }

    /// Wait until `user` has `n` registered sockets (pushes reach registered sockets only).
    async fn registered(&self, user: UserId, n: usize) {
        let deadline = std::time::Instant::now() + WAIT;
        while self.state.ws().connections_of(user).len() != n {
            assert!(std::time::Instant::now() < deadline, "{user} never had {n} sockets");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn connect(&self, user: UserId, token: &str) -> Ws {
        let before = self.state.ws().connections_of(user).len();
        let mut request = format!("ws://{}{}", self.addr, routes::WS).into_client_request().expect("request");
        request.headers_mut().insert("authorization", format!("Bearer {token}").parse().expect("header"));
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request)).await.expect("handshake in time").expect("connect");
        self.registered(user, before + 1).await;
        ws
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let result = tokio::time::timeout(Duration::from_secs(60), task).await;
            assert!(matches!(result, Ok(Ok(Ok(())))), "the server did not stop cleanly: {result:?}");
        }
    }
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await.expect("a frame in time") {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(text.as_str()).expect("JSON"),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// The next frame of `kind` (other pushes skipped).
async fn push_of(ws: &mut Ws, kind: &str) -> Value {
    loop {
        let frame = recv(ws).await;
        if frame["type"] == kind {
            return frame["data"].clone();
        }
    }
}

async fn request(ws: &mut Ws, id: u64, kind: &str, data: Value) {
    ws.send(Message::text(json!({"id": id, "type": kind, "data": data}).to_string())).await.expect("send");
}

/// Send a request and read until its answer; pushes before it are returned too.
async fn call(ws: &mut Ws, id: u64, kind: &str, data: Value) -> (Value, Vec<Value>) {
    request(ws, id, kind, data).await;
    let mut pushes = Vec::new();
    loop {
        let frame = recv(ws).await;
        if frame["id"] == id {
            return (frame, pushes);
        }
        pushes.push(frame);
    }
}

async fn ok(ws: &mut Ws, id: u64, kind: &str, data: Value) -> Value {
    let (answer, _) = call(ws, id, kind, data).await;
    assert_eq!(answer["ok"], true, "{kind}: {answer}");
    answer["data"].clone()
}

async fn error_code(ws: &mut Ws, id: u64, kind: &str, data: Value) -> String {
    let (answer, _) = call(ws, id, kind, data).await;
    assert_eq!(answer["ok"], false, "{kind}: {answer}");
    answer["error"]["code"].as_str().unwrap_or_default().to_string()
}

/// No push of `kind` is queued for `ws`: a round trip on `ws` after the action answers before
/// anything of that kind (pushes made during an earlier request were queued before this answer).
async fn no_push(ws: &mut Ws, id: u64, kind: &str) {
    let (_, pushes) = call(ws, id, "chat.members", json!({"room": 999_999})).await;
    assert!(pushes.iter().all(|p| p["type"] != kind), "unexpected {kind}: {pushes:?}");
}

async fn room_id(server: &Server, key: &str, ws: &mut Ws, id: u64) -> i64 {
    let _ = server;
    ok(ws, id, "chat.join", json!({"room": key})).await["id"].as_i64().expect("room id")
}

// ---- the suite ----------------------------------------------------------------------------------

async fn rooms_send_echo_history(url: &str) {
    let server = start(url, |_, _| {}).await;
    let (ada, ada_token) = server.register("ada@example.com", "Ada").await;
    let (bo, bo_token) = server.register("bo@example.com", "Bo").await;
    let mut a = server.connect(ada, &ada_token).await;
    let mut b = server.connect(bo, &bo_token).await;
    let room = ok(&mut a, 1, "chat.join", json!({"room": "world"})).await;
    assert_eq!((room["kind"].as_str(), room["key"].as_str(), room["name"].as_str()), (Some("room"), Some("world"), Some("World")));
    assert_eq!((room["member_count"].as_u64(), room["max_members"].as_u64()), (Some(1), Some(200)));
    let world = room["id"].as_i64().expect("id");
    assert_eq!(ok(&mut b, 1, "chat.join", json!({"room": world})).await["member_count"], 2, "by id too");
    assert_eq!(ok(&mut b, 2, "chat.join", json!({"room": "world"})).await["id"], world, "joining twice is fine");
    // The answer BEFORE the sender's own echo, which carries the nonce.
    request(&mut a, 2, "chat.send", json!({"room": world, "text": "hello darn world", "nonce": "n-1"})).await;
    let answer = loop {
        let frame = recv(&mut a).await;
        if frame["type"] == "chat.presence" {
            continue;
        }
        break frame;
    };
    assert_eq!((answer["id"].as_u64(), answer["ok"].as_bool()), (Some(2), Some(true)), "the answer comes first: {answer}");
    let echo = push_of(&mut a, "chat.message").await;
    assert_eq!(echo["id"], answer["data"]["message_id"]);
    assert_eq!((echo["text"].as_str(), echo["nonce"].as_str(), echo["sender_name"].as_str()), (Some("hello d**n world"), Some("n-1"), Some("Ada")));
    assert_eq!(echo["sent_at"], T0);
    let got = push_of(&mut b, "chat.message").await;
    assert_eq!((got["sender"].as_i64(), got["text"].as_str()), (Some(ada.get()), Some("hello d**n world")));
    // History: newest first, cursor pages, over the WebSocket and HTTP.
    for n in 0..3 {
        ok(&mut a, 10 + n, "chat.send", json!({"room": world, "text": format!("m{n}")})).await;
    }
    let page = ok(&mut b, 3, "chat.history", json!({"room": world, "limit": 2})).await;
    let texts: Vec<&str> = page["items"].as_array().map(|i| i.iter().filter_map(|m| m["text"].as_str()).collect()).unwrap_or_default();
    assert_eq!(texts, ["m2", "m1"]);
    let cursor = page["next_cursor"].as_str().expect("a next page").to_string();
    let rest = ok(&mut b, 4, "chat.history", json!({"room": world, "cursor": cursor})).await;
    assert_eq!(rest["items"].as_array().map(Vec::len), Some(2));
    assert!(rest.get("next_cursor").is_none());
    let (status, body) = server.http(Method::GET, &format!("{}?limit=1", routes::chat_history_path(RoomId(world))), None, &bo_token).await;
    assert_eq!((status, body["items"][0]["text"].as_str()), (StatusCode::OK, Some("m2")));
    // Public rooms over HTTP.
    let (status, rooms) = server.http(Method::GET, routes::chat::ROOMS, None, &bo_token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rooms["items"].as_array().map(Vec::len), Some(4));
    assert_eq!(rooms["items"][0]["member_count"], 2);
    // Not joined on this connection: not_a_member; unknown rooms: not_found; bad keys: refused.
    let trade = server.room_of(&ada_token, "trade").await;
    assert_eq!(error_code(&mut a, 20, "chat.send", json!({"room": trade, "text": "hi"})).await, codes::NOT_A_MEMBER);
    assert_eq!(error_code(&mut a, 21, "chat.join", json!({"room": "nowhere"})).await, codes::NOT_FOUND);
    assert_eq!(error_code(&mut a, 22, "chat.join", json!({"room": "Not A Key"})).await, codes::VALIDATION_FAILED);
    assert_eq!(error_code(&mut a, 23, "chat.send", json!({"room": 424_242, "text": "hi"})).await, codes::NOT_FOUND);
    // Leaving: no more messages of the room.
    ok(&mut b, 5, "chat.leave", json!({"room": world})).await;
    ok(&mut b, 6, "chat.leave", json!({"room": world})).await;
    assert_eq!(error_code(&mut b, 7, "chat.send", json!({"room": world, "text": "hi"})).await, codes::NOT_A_MEMBER);
    server.stop().await;
}

async fn limits_and_rules(url: &str) {
    let server = start(url, |config, chat| {
        config.ws.max_rooms_per_connection = 2;
        chat.max_text_chars = 20;
        // 5 per hour: no token comes back while the test runs, however slow the runner.
        chat.rate_window_secs = 3600;
    })
    .await;
    let (ada, token) = server.register("limits@example.com", "Ada").await;
    let mut a = server.connect(ada, &token).await;
    let world = room_id(&server, "world", &mut a, 1).await;
    // Per-connection room cap.
    ok(&mut a, 2, "chat.join", json!({"room": "trade"})).await;
    assert_eq!(error_code(&mut a, 3, "chat.join", json!({"room": "quiet"})).await, codes::QUOTA_EXCEEDED);
    // Text rules: blank, too long, control / invisible characters, a bad nonce; a hook's
    // rewrite is checked too, a hook's refusal answers its error.
    for text in ["   ", "this text is longer than twenty", "bell\u{7}", "hidden\u{200B}"] {
        assert_eq!(error_code(&mut a, 4, "chat.send", json!({"room": world, "text": text})).await, codes::VALIDATION_FAILED, "{text:?}");
    }
    assert_eq!(error_code(&mut a, 5, "chat.send", json!({"room": world, "text": "x", "nonce": "has space"})).await, codes::VALIDATION_FAILED);
    assert_eq!(error_code(&mut a, 6, "chat.send", json!({"room": world, "text": "make it empty"})).await, codes::VALIDATION_FAILED);
    assert_eq!(error_code(&mut a, 7, "chat.send", json!({"room": world, "text": "buy gold now"})).await, codes::FORBIDDEN);
    ok(&mut a, 8, "chat.send", json!({"room": world, "text": "line one\nline two"})).await;
    // The send rate: 5 per window. Counted: every send that reached the hooks (the two refused by
    // a hook and the one sent above), so 2 more get through.
    let mut sent = 0;
    let limited = loop {
        let (answer, _) = call(&mut a, 30 + sent, "chat.send", json!({"room": world, "text": format!("r{sent}")})).await;
        if answer["ok"] == false {
            break answer;
        }
        sent += 1;
        assert!(sent <= 5, "never limited");
    };
    assert_eq!(sent, 2);
    assert_eq!(limited["error"]["code"], codes::RATE_LIMITED);
    assert!(limited["error"]["details"]["retry_after_ms"].as_u64().is_some_and(|ms| ms >= 1), "{limited}");
    server.stop().await;
}

/// Simultaneous joins at the member cap: exactly the cap get in.
async fn joins_at_the_cap(url: &str) {
    let server = start(url, |_, _| {}).await;
    let mut sockets = Vec::new();
    let mut any_token = String::new();
    for n in 0..10 {
        let (user, token) = server.register(&format!("cap{n}@example.com"), "P").await;
        sockets.push(server.connect(user, &token).await);
        any_token = token;
    }
    let small = server.room_of(&any_token, "small").await;
    let mut tasks = Vec::new();
    for mut ws in sockets {
        tasks.push(tokio::spawn(async move {
            let (answer, _) = call(&mut ws, 1, "chat.join", json!({"room": "small"})).await;
            (answer, ws)
        }));
    }
    let mut joined = 0;
    let mut keep = Vec::new();
    for task in tasks {
        let (answer, ws) = task.await.expect("task");
        if answer["ok"] == true {
            joined += 1;
        } else {
            assert_eq!(answer["error"]["code"], codes::ROOM_FULL, "{answer}");
        }
        keep.push(ws);
    }
    assert_eq!(joined, 3);
    assert_eq!(server.state.ws().room_size(&format!("chat:{small}")), 3);
    drop(keep);
    server.stop().await;
}

async fn direct_messages(url: &str) {
    let server = start(url, |_, _| {}).await;
    let (ada, ada_token) = server.register("dm-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("dm-b@example.com", "Bo").await;
    let (cy, cy_token) = server.register("dm-c@example.com", "Cy").await;
    // Open (and find again) over HTTP.
    let (status, room) = server.http(Method::POST, routes::chat::DM, Some(json!({"user": bo.get()})), &ada_token).await;
    assert_eq!(status, StatusCode::OK, "{room}");
    assert_eq!((room["kind"].as_str(), room["peer"].as_i64()), (Some("dm"), Some(bo.get())));
    let dm = room["id"].as_i64().expect("id");
    let (_, again) = server.http(Method::POST, routes::chat::DM, Some(json!({"user": ada.get()})), &bo_token).await;
    assert_eq!((again["id"].as_i64(), again["peer"].as_i64()), (Some(dm), Some(ada.get())));
    assert_eq!(server.http(Method::POST, routes::chat::DM, Some(json!({"user": ada.get()})), &ada_token).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(server.http(Method::POST, routes::chat::DM, Some(json!({"user": 77_777_777})), &ada_token).await.0, StatusCode::NOT_FOUND);
    // Sent without a join; every connection of both users gets it (Bo has two).
    let mut a = server.connect(ada, &ada_token).await;
    let mut b1 = server.connect(bo, &bo_token).await;
    let mut b2 = server.connect(bo, &bo_token).await;
    let ack = ok(&mut a, 1, "chat.send", json!({"room": dm, "text": "psst", "nonce": "d1"})).await;
    // The sender's nonce reaches the sender only.
    for (ws, nonce) in [(&mut a, Some("d1")), (&mut b1, None), (&mut b2, None)] {
        let message = push_of(ws, "chat.message").await;
        assert_eq!((message["id"].clone(), message["room"].as_i64(), message["text"].as_str()), (ack["message_id"].clone(), Some(dm), Some("psst")));
        assert_eq!(message["nonce"].as_str(), nonce, "{message}");
    }
    let history = ok(&mut a, 90, "chat.history", json!({"room": dm})).await;
    assert_eq!(history["items"][0]["nonce"], "d1", "the sender sees its nonce in the history");
    let history = ok(&mut b1, 90, "chat.history", json!({"room": dm})).await;
    assert!(history["items"][0].get("nonce").is_none(), "the peer never sees it: {history}");
    // A DM room answers a join with its info (no hub room).
    assert_eq!(ok(&mut b1, 2, "chat.join", json!({"room": dm})).await["peer"], ada.get());
    // Outsiders: no sending, no history, no join.
    let mut c = server.connect(cy, &cy_token).await;
    assert_eq!(error_code(&mut c, 1, "chat.send", json!({"room": dm, "text": "hi"})).await, codes::NOT_A_MEMBER);
    assert_eq!(error_code(&mut c, 2, "chat.history", json!({"room": dm})).await, codes::NOT_A_MEMBER);
    assert_eq!(error_code(&mut c, 3, "chat.join", json!({"room": dm})).await, codes::NOT_A_MEMBER);
    let (status, _) = server.http(Method::GET, &routes::chat_history_path(RoomId(dm)), None, &cy_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Lists: newest activity first, with the peer.
    let (_, cy_room) = server.http(Method::POST, routes::chat::DM, Some(json!({"user": cy.get()})), &ada_token).await;
    let (status, list) = server.http(Method::GET, routes::chat::DMS, None, &ada_token).await;
    assert_eq!(status, StatusCode::OK);
    let peers: Vec<i64> = list["items"].as_array().map(|i| i.iter().filter_map(|r| r["peer"].as_i64()).collect()).unwrap_or_default();
    assert_eq!(peers.len(), 2);
    server.clock.advance(1000);
    ok(&mut a, 3, "chat.send", json!({"room": cy_room["id"], "text": "hey"})).await;
    let (_, list) = server.http(Method::GET, &format!("{}?limit=1", routes::chat::DMS), None, &ada_token).await;
    assert_eq!(list["items"][0]["peer"], cy.get(), "the newest activity first");
    let cursor = list["next_cursor"].as_str().expect("next").to_string();
    let (_, rest) = server.http(Method::GET, &format!("{}?cursor={cursor}", routes::chat::DMS), None, &ada_token).await;
    assert_eq!(rest["items"][0]["peer"], bo.get());
    // A DM never tells whether the peer is online (both are): only the caller is listed. A
    // stranger who opens a DM with Ada learns nothing either.
    let members = ok(&mut a, 4, "chat.members", json!({"room": dm})).await;
    assert_eq!((members["count"].as_u64(), members["members"].clone()), (Some(1), json!([{"user": ada.get(), "name": "Ada"}])));
    let (_, probe) = server.http(Method::POST, routes::chat::DM, Some(json!({"user": ada.get()})), &cy_token).await;
    let members = ok(&mut c, 5, "chat.members", json!({"room": probe["id"]})).await;
    assert_eq!(members["members"], json!([{"user": cy.get(), "name": "Cy"}]));
    // The hook refuses a blocked pair (a peer id ending in 999 in this test).
    let blocked = server.service().open_direct(&server.state, &net_backend_server::HookCtx::new(server.state.clone(), None), ada, UserId(1999)).await;
    assert!(blocked.is_err());
    server.stop().await;
}

async fn deletion_and_moderation(url: &str) {
    let server = start(url, |_, _| {}).await;
    let (ada, ada_token) = server.register("del-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("del-b@example.com", "Bo").await;
    let (mo, mo_token) = server.register("del-m@example.com", "Mo").await;
    server.state.get::<AuthService>().expect("auth").set_user_role(&server.state, mo, "moderator", true).await.expect("role");
    let mut a = server.connect(ada, &ada_token).await;
    let mut b = server.connect(bo, &bo_token).await;
    let world = room_id(&server, "world", &mut a, 1).await;
    ok(&mut b, 1, "chat.join", json!({"room": world})).await;
    let first = ok(&mut a, 2, "chat.send", json!({"room": world, "text": "oops"})).await["message_id"].as_i64().expect("id");
    let second = ok(&mut a, 3, "chat.send", json!({"room": world, "text": "rude"})).await["message_id"].as_i64().expect("id");
    let path = |message: i64| routes::chat_message_path(RoomId(world), MessageId(message));
    // Someone else's message: 403; its sender may delete it; everyone in the room is told.
    assert_eq!(server.http(Method::DELETE, &path(first), None, &bo_token).await.0, StatusCode::FORBIDDEN);
    assert_eq!(server.http(Method::DELETE, &path(first), None, &ada_token).await.0, StatusCode::OK);
    let deleted = push_of(&mut b, "chat.deleted").await;
    assert_eq!(deleted, json!({"id": first, "room": world}));
    assert_eq!(push_of(&mut a, "chat.deleted").await["id"], first, "the sender's connection too");
    assert_eq!(server.http(Method::DELETE, &path(first), None, &ada_token).await.0, StatusCode::NOT_FOUND, "deleted already");
    // A moderator deletes anyone's message (audited).
    assert_eq!(server.http(Method::DELETE, &path(second), None, &mo_token).await.0, StatusCode::OK);
    assert_eq!(push_of(&mut a, "chat.deleted").await["id"], second);
    let history = ok(&mut b, 2, "chat.history", json!({"room": world, "limit": 100})).await;
    let ids: Vec<i64> = history["items"].as_array().map(|i| i.iter().filter_map(|m| m["id"].as_i64()).collect()).unwrap_or_default();
    assert!(!ids.contains(&first) && !ids.contains(&second), "deleted messages leave the history: {ids:?}");
    let audit =
        server.state.get::<AuthService>().expect("auth").audit_log(&server.state, &AuditQuery::new().with_action("chat.message_deleted")).await.expect("audit");
    assert_eq!(audit.items.len(), 1, "only the moderator's delete is audited");
    assert_eq!(audit.items[0].actor, Some(mo));
    // Server code: system messages and deletes.
    let system = server.service().send_as(&server.state, mo, RoomId(world), "server restart in 5 min").await.expect("send_as");
    assert_eq!(push_of(&mut b, "chat.message").await["id"], system.id.get());
    server.service().delete_message(&server.state, RoomId(world), system.id).await.expect("server delete");
    assert_eq!(push_of(&mut b, "chat.deleted").await["id"], system.id.get());
    server.stop().await;
}

async fn presence(url: &str) {
    let server = start(url, |_, chat| {
        chat.presence_max_members = 3;
        chat.presence_per_second = 100;
    })
    .await;
    let (ada, ada_token) = server.register("pr-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("pr-b@example.com", "Bo").await;
    let mut a = server.connect(ada, &ada_token).await;
    let world = room_id(&server, "world", &mut a, 1).await;
    // The joiner's own connections are members too: Ada sees her own "joined".
    let own = push_of(&mut a, "chat.presence").await;
    assert_eq!(own, json!({"room": world, "user": ada.get(), "event": "joined", "name": "Ada", "count": 1}));
    // Bo's first connection: "joined" with his name and the count; his second: nothing.
    let mut b1 = server.connect(bo, &bo_token).await;
    ok(&mut b1, 1, "chat.join", json!({"room": world})).await;
    let joined = push_of(&mut a, "chat.presence").await;
    assert_eq!(joined, json!({"room": world, "user": bo.get(), "event": "joined", "name": "Bo", "count": 2}));
    let mut b2 = server.connect(bo, &bo_token).await;
    ok(&mut b2, 1, "chat.join", json!({"room": world})).await;
    no_push(&mut a, 50, "chat.presence").await;
    let members = ok(&mut a, 51, "chat.members", json!({"room": world})).await;
    assert_eq!(members["count"], 2);
    assert_eq!(members["members"], json!([{"user": ada.get(), "name": "Ada"}, {"user": bo.get(), "name": "Bo"}]));
    // Leaving one connection: nothing; closing the last one: "left".
    ok(&mut b2, 2, "chat.leave", json!({"room": world})).await;
    no_push(&mut a, 52, "chat.presence").await;
    b1.close(None).await.expect("close");
    drop(b1);
    let left = push_of(&mut a, "chat.presence").await;
    assert_eq!((left["event"].as_str(), left["user"].as_i64(), left["count"].as_u64()), (Some("left"), Some(bo.get()), Some(1)));
    // Not joined on this connection: no member list.
    assert_eq!(error_code(&mut b2, 3, "chat.members", json!({"room": world})).await, codes::NOT_A_MEMBER);
    // Over the presence cap (3 users online): no pushes, the list still answers.
    let mut others = Vec::new();
    for n in 0..3 {
        let (user, token) = server.register(&format!("pr-{n}@example.com"), "P").await;
        let mut ws = server.connect(user, &token).await;
        ok(&mut ws, 1, "chat.join", json!({"room": world})).await;
        others.push(ws);
    }
    // Ada saw the joins up to the cap (count 2, 3), not the fourth user's (count 4).
    let mut counts = Vec::new();
    let (_, pushes) = call(&mut a, 53, "chat.members", json!({"room": world})).await;
    for push in pushes.iter().filter(|p| p["type"] == "chat.presence") {
        counts.push(push["data"]["count"].as_u64().unwrap_or_default());
    }
    assert_eq!(counts, [2, 3], "{pushes:?}");
    assert_eq!(ok(&mut a, 54, "chat.members", json!({"room": world})).await["count"], 4);
    server.stop().await;
}

async fn presence_rate(url: &str) {
    let server = start(url, |_, chat| chat.presence_per_second = 1).await;
    let (ada, ada_token) = server.register("rate-a@example.com", "Ada").await;
    let mut a = server.connect(ada, &ada_token).await;
    let world = room_id(&server, "world", &mut a, 1).await;
    let (bo, bo_token) = server.register("rate-b@example.com", "Bo").await;
    let (cy, cy_token) = server.register("rate-c@example.com", "Cy").await;
    let mut b = server.connect(bo, &bo_token).await;
    let mut c = server.connect(cy, &cy_token).await;
    ok(&mut b, 1, "chat.join", json!({"room": world})).await;
    ok(&mut c, 1, "chat.join", json!({"room": world})).await;
    // One per second per room: Bo's join is pushed, Cy's (right after) is not.
    let (_, pushes) = call(&mut a, 2, "chat.members", json!({"room": world})).await;
    let users: Vec<i64> = pushes.iter().filter(|p| p["type"] == "chat.presence").filter_map(|p| p["data"]["user"].as_i64()).collect();
    assert!(users.len() <= 1, "at most one presence push within a second: {users:?}");
    server.stop().await;
}

async fn groups_and_retention(url: &str) {
    let server = start(url, |_, chat| chat.history_retention_days = 1).await;
    let (ada, ada_token) = server.register("grp-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("grp-b@example.com", "Bo").await;
    let service = server.service();
    let group = service.create_group(&server.state, Some("Guild"), &[ada]).await.expect("group");
    assert_eq!((group.kind, group.name.as_deref()), (net_backend_server::protocol::chat::RoomKind::Group, Some("Guild")));
    let mut a = server.connect(ada, &ada_token).await;
    let mut b = server.connect(bo, &bo_token).await;
    ok(&mut a, 1, "chat.join", json!({"room": group.id})).await;
    assert_eq!(error_code(&mut b, 1, "chat.join", json!({"room": group.id})).await, codes::NOT_A_MEMBER);
    assert_eq!(server.http(Method::GET, &routes::chat_history_path(group.id), None, &bo_token).await.0, StatusCode::FORBIDDEN);
    service.add_member(&server.state, group.id, bo).await.expect("add");
    service.add_member(&server.state, group.id, bo).await.expect("add twice");
    ok(&mut b, 2, "chat.join", json!({"room": group.id})).await;
    ok(&mut a, 2, "chat.send", json!({"room": group.id, "text": "welcome"})).await;
    assert_eq!(push_of(&mut b, "chat.message").await["text"], "welcome");
    // Removing a member makes its connections leave: no more sending, receiving or rejoining.
    service.remove_member(&server.state, group.id, bo).await.expect("remove");
    assert_eq!(error_code(&mut b, 3, "chat.send", json!({"room": group.id, "text": "still here?"})).await, codes::NOT_A_MEMBER);
    assert_eq!(server.state.ws().room_size(&format!("chat:{}", group.id)), 1);
    ok(&mut a, 3, "chat.send", json!({"room": group.id, "text": "after the removal"})).await;
    no_push(&mut b, 4, "chat.message").await;
    assert_eq!(error_code(&mut b, 5, "chat.join", json!({"room": group.id})).await, codes::NOT_A_MEMBER);
    // Group rules: duplicates count once, an unknown account or a bad name creates nothing.
    let pair = service.create_group(&server.state, Some("Pair"), &[ada, bo, ada]).await.expect("dedupe");
    service.add_member(&server.state, pair.id, bo).await.expect("still fine");
    let unknown = service.create_group(&server.state, None, &[ada, UserId(987_654_321)]).await.err().map(|e| e.status());
    assert_eq!(unknown, Some(StatusCode::NOT_FOUND));
    let named = service.create_group(&server.state, Some(&"x".repeat(65)), &[ada]).await.err().map(|e| e.status());
    assert_eq!(named, Some(StatusCode::BAD_REQUEST));
    let room = service.create_room(&server.state, &RoomSpec::new("named").with_name("\u{202E}evil")).await.err().map(|e| e.status());
    assert_eq!(room, Some(StatusCode::BAD_REQUEST));
    let world = server.room_of(&ada_token, "world").await;
    assert!(service.add_member(&server.state, RoomId(world), bo).await.is_err(), "public rooms have no members");
    // Retention: a day later the message left the history; the purge deletes it.
    server.clock.advance(86_400_000 + 1);
    let page = service.history(&server.state, ada, group.id, &PageRequest::first()).await.expect("history");
    assert!(page.items.is_empty());
    // (On a shared database the earlier subtests' messages are past the retention too.)
    assert!(service.purge(&server.state).await.expect("purge") >= 1);
    assert_eq!(service.purge(&server.state).await.expect("purge again"), 0);
    server.stop().await;
}

/// `dm_open_rate`: opening (or finding) DM rooms is limited per user.
async fn dm_open_rate(url: &str) {
    let server = start(url, |_, chat| {
        chat.dm_open_rate = 2;
        chat.dm_open_window_secs = 3600;
    })
    .await;
    let (_, ada_token) = server.register("dmr-a@example.com", "Ada").await;
    let (bo, _) = server.register("dmr-b@example.com", "Bo").await;
    let (cy, _) = server.register("dmr-c@example.com", "Cy").await;
    for peer in [bo, cy] {
        assert_eq!(server.http(Method::POST, routes::chat::DM, Some(json!({"user": peer.get()})), &ada_token).await.0, StatusCode::OK);
    }
    let (status, body) = server.http(Method::POST, routes::chat::DM, Some(json!({"user": bo.get()})), &ada_token).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(codes::RATE_LIMITED)));
    assert!(body["error"]["details"]["retry_after_ms"].as_u64().is_some_and(|ms| ms > 0), "{body}");
    server.stop().await;
}

/// The join race: a removal that lands between the join's membership check and its hub join
/// (here: from the join's own before hook) still keeps the socket out.
async fn removal_during_join(url: &str) {
    let server = start_with(
        url,
        None,
        |server| {
            server.before::<BeforeChatJoin, _, _>(|ctx, join| async move {
                if join.kind == net_backend_server::protocol::chat::RoomKind::Group {
                    let service = ctx.state().get::<ChatService>().expect("chat");
                    service.remove_member(ctx.state(), join.room, join.user_id).await?;
                }
                Ok(Decision::Continue(join))
            })
        },
        |_, _| {},
    )
    .await;
    let (ada, ada_token) = server.register("race-a@example.com", "Ada").await;
    let group = server.service().create_group(&server.state, Some("Race"), &[ada]).await.expect("group");
    let mut a = server.connect(ada, &ada_token).await;
    assert_eq!(error_code(&mut a, 1, "chat.join", json!({"room": group.id})).await, codes::NOT_A_MEMBER);
    assert_eq!(server.state.ws().room_size(&format!("chat:{}", group.id)), 0, "the socket did not stay joined");
    assert_eq!(server.service().online(group.id).count, 0);
    server.stop().await;
}

/// The B1 scenario: many DM sends into ONE room at the same moment, from several sockets of both
/// users (MySQL deadlocked here before the fix). Every send is answered `ok` and every socket gets
/// every message.
async fn concurrent_dm_sends(url: &str, sockets_per_user: usize, per_socket: usize) {
    let server = start(url, |config, chat| {
        chat.rate_messages = 10_000;
        config.ws.max_connections_per_user = sockets_per_user.max(1);
        config.ws.frames_per_second = 10_000;
        config.ws.handshakes_per_ip_per_minute = 10_000;
    })
    .await;
    let (ada, ada_token) = server.register("dms-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("dms-b@example.com", "Bo").await;
    let (_, room) = server.http(Method::POST, routes::chat::DM, Some(json!({"user": bo.get()})), &ada_token).await;
    let dm = room["id"].as_i64().expect("dm");
    let mut sockets = Vec::new();
    for (user, token) in [(ada, &ada_token), (bo, &bo_token)] {
        for _ in 0..sockets_per_user {
            sockets.push(server.connect(user, token).await);
        }
    }
    let total = sockets.len() * per_socket;
    let mut tasks = Vec::new();
    for (n, mut ws) in sockets.into_iter().enumerate() {
        tasks.push(tokio::spawn(async move {
            for i in 0..per_socket {
                request(&mut ws, i as u64 + 1, "chat.send", json!({"room": dm, "text": format!("s{n} m{i}")})).await;
            }
            let (mut answers, mut messages) = (0usize, 0usize);
            while answers < per_socket || messages < total {
                let frame = recv(&mut ws).await;
                if frame.get("id").is_some_and(|id| id.is_u64()) {
                    assert_eq!(frame["ok"], true, "a send failed: {frame}");
                    answers += 1;
                } else if frame["type"] == "chat.message" {
                    messages += 1;
                }
            }
            (answers, messages)
        }));
    }
    for task in tasks {
        let (answers, messages) = tokio::time::timeout(WAIT * 2, task).await.expect("in time").expect("task");
        assert_eq!((answers, messages), (per_socket, total));
    }
    let page = server.service().history(&server.state, ada, RoomId(dm), &PageRequest::first().with_limit(100)).await.expect("history");
    let mut stored = page.items.len();
    let mut cursor = page.next_cursor;
    while let Some(next) = cursor {
        let page = server.service().history(&server.state, ada, RoomId(dm), &PageRequest::after(next).with_limit(100)).await.expect("history");
        stored += page.items.len();
        cursor = page.next_cursor;
    }
    assert_eq!(stored, total, "every message was stored");
    server.stop().await;
}

async fn suite(url: &str) {
    rooms_send_echo_history(url).await;
    limits_and_rules(url).await;
    joins_at_the_cap(url).await;
    direct_messages(url).await;
    deletion_and_moderation(url).await;
    presence(url).await;
    presence_rate(url).await;
    groups_and_retention(url).await;
    dm_open_rate(url).await;
    removal_during_join(url).await;
    concurrent_dm_sends(url, 3, 5).await;
}

// ---- runners ------------------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_suite() {
    suite("sqlite::memory:").await;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_file_suite() {
    let dir = common::temp_dir("chat-file");
    let url = format!("sqlite:{}", dir.join("chat.db").display().to_string().replace('\\', "/"));
    suite(&url).await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_chat_suite() {
    let base = common::env_url("NBS_TEST_MYSQL_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_chat_suite() {
    let base = common::env_url("NBS_TEST_POSTGRES_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
}

/// B1 stress (VPS tester): 4 sockets per user x 25 sends = 200 DM sends into one room at once.
#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server): the DM deadlock stress test"]
async fn mysql_dm_stress() {
    let base = common::env_url("NBS_TEST_MYSQL_URL");
    let (url, name) = common::fresh_database(&base).await;
    concurrent_dm_sends(&url, 4, 25).await;
    common::drop_database(&base, &name).await;
}

/// B1 stress on PostgreSQL (expected fine before and after the fix).
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server): the DM deadlock stress test"]
async fn postgres_dm_stress() {
    let base = common::env_url("NBS_TEST_POSTGRES_URL");
    let (url, name) = common::fresh_database(&base).await;
    concurrent_dm_sends(&url, 4, 25).await;
    common::drop_database(&base, &name).await;
}

/// S2 across instances: two servers on one database, linked by a broadcaster. A member removed on
/// instance A is refused at once on instance B (the table is checked on every group send), and
/// its socket on B leaves the room when the control delivery arrives (no more messages).
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removal_reaches_other_instances() {
    let dir = common::temp_dir("chat-instances");
    let url = format!("sqlite:{}", dir.join("shared.db").display().to_string().replace('\\', "/"));
    let link = SharedBroadcaster::default();
    let one = start_with(&url, Some(link.clone()), |server| server, |_, _| {}).await;
    let two = start_with(&url, Some(link.clone()), |server| server, |_, _| {}).await;
    let (ada, ada_token) = one.register("inst-a@example.com", "Ada").await;
    let (bo, bo_token) = one.register("inst-b@example.com", "Bo").await;
    let group = one.service().create_group(&one.state, Some("Guild"), &[ada, bo]).await.expect("group");
    let mut a = one.connect(ada, &ada_token).await;
    let mut b = two.connect(bo, &bo_token).await;
    ok(&mut a, 1, "chat.join", json!({"room": group.id})).await;
    ok(&mut b, 1, "chat.join", json!({"room": group.id})).await;
    ok(&mut a, 2, "chat.send", json!({"room": group.id, "text": "across"})).await;
    assert_eq!(push_of(&mut b, "chat.message").await["text"], "across", "messages cross instances");
    // The control delivery is late: B still has the socket in the room, but the send is refused.
    link.drop_controls.store(true, Ordering::SeqCst);
    one.service().remove_member(&one.state, group.id, bo).await.expect("remove");
    let hub_room = format!("chat:{}", group.id);
    assert_eq!(two.state.ws().room_size(&hub_room), 1);
    assert_eq!(error_code(&mut b, 2, "chat.send", json!({"room": group.id, "text": "still?"})).await, codes::NOT_A_MEMBER);
    // It arrives: B's socket leaves the room and gets nothing more.
    link.drop_controls.store(false, Ordering::SeqCst);
    one.service().remove_member(&one.state, group.id, bo).await.expect("remove again");
    let deadline = std::time::Instant::now() + WAIT;
    while two.state.ws().room_size(&hub_room) != 0 {
        assert!(std::time::Instant::now() < deadline, "the removal never reached the other instance");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ok(&mut a, 3, "chat.send", json!({"room": group.id, "text": "private again"})).await;
    no_push(&mut b, 3, "chat.message").await;
    drop((a, b));
    two.stop().await;
    one.stop().await;
}

/// The wiring: needs `auth` first and the hub on; the documents list the routes and kinds.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn wiring_and_documents() {
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(Chat::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");
    let mut off = config();
    off.ws.enabled = false;
    let error = NetBackendServer::new(off).module(Auth::new()).module(Chat::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("ws.enabled"), "{error}");
    let mut bad = ChatConfig::default();
    bad.rooms = vec![RoomSpec::new("Bad Key")];
    assert!(NetBackendServer::new(config()).module(Auth::new()).module(Chat::new().with_config(bad)).build().await.is_err());
    let prepared = NetBackendServer::new(config()).module(Auth::new()).module(Chat::new()).build().await.expect("build");
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for route in routes::ALL.iter().filter(|r| r.path.starts_with("/v1/chat")) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
    let asyncapi: Value = serde_json::from_str(prepared.asyncapi_json()).expect("json");
    for kind in ["chat.join", "chat.leave", "chat.send", "chat.history", "chat.members"] {
        assert!(asyncapi["components"]["messages"].get(format!("request.{kind}")).is_some(), "{kind}");
    }
    let messages = asyncapi["components"]["messages"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    for push in ["chat.message", "chat.deleted", "chat.presence"] {
        assert!(messages.iter().any(|k| *k == format!("push.{push}")), "{push} in {messages:?}");
    }
}
