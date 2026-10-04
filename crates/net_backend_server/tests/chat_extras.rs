//! The chat extras on a real loopback server with WebSocket clients (tokio-tungstenite): message
//! editing (the sender's window, the hooks, moderators with `chat.moderate`, the audit, pushes, the
//! history), read markers (forward only, the coalesced `chat.read` pushes, receipts, unread counts,
//! the rate), typing indicators (the throttle, stop, the room cap, a message clearing it), rooms
//! created by players (create / limits / hooks, public and private, invitations, joins over HTTP and
//! `chat.join`, roles, kicks as bans, hand-on, leave, delete, the `chat.room` pushes, staff with
//! `chat.moderate`, concurrent joins at the cap, the upkeep after an owner's account is deleted),
//! and the documents. Groups and lobbies keep their rooms (their own suites). The 0.2.0 review
//! fixes: `chat.moderate` reads, opens and joins private player rooms; a kick checks the actor
//! first; a removed sender no longer deletes; the invitation rate and blocks; deleting a group room.
//!
//! The suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "chat", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use futures_util::{SinkExt, StreamExt};
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig, AuthService};
use net_backend_server::chat::events::{BeforeChatSend, BeforeChatTyping, BeforeRoomCreate, BeforeRoomInvite};
use net_backend_server::chat::{Chat, ChatConfig, ChatService, RoomSpec};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
#[cfg(feature = "sqlite")]
use net_backend_server::permissions::Permissions;
use net_backend_server::protocol::admin::AuditQuery;
use net_backend_server::protocol::{codes, routes, MessageId, RoomId, UnixMillis, UserId};
use net_backend_server::sea_query::{Expr, ExprTrait, Query};
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
/// The upper bound of every wait.
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

async fn start(url: &str, tweak: impl FnOnce(&mut ChatConfig)) -> Server {
    common::watchdog(Duration::from_secs(1200));
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("chat-extras-migrations");
    config.server.shutdown_grace_secs = 5;
    let mut chat = ChatConfig::default();
    chat.rooms = vec![RoomSpec::new("world").with_name("World"), RoomSpec::new("hall").with_name("Hall")];
    chat.purge_interval_secs = 0;
    tweak(&mut chat);
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let server = NetBackendServer::new(config)
        .clock(clock.clone())
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Chat::new().with_config(chat));
    #[cfg(feature = "notifications")]
    let server = server.module(net_backend_server::notifications::Notifications::new());
    #[cfg(feature = "friends")]
    let server = server.module(net_backend_server::friends::Friends::new());
    let prepared = server
        .before::<BeforeChatSend, _, _>(|_ctx, mut message| async move {
            if message.text.contains("buy gold") {
                return Ok(Decision::Reject(AppError::forbidden("no advertising")));
            }
            message.text = message.text.replace("darn", "d**n");
            Ok(Decision::Continue(message))
        })
        .before::<BeforeChatTyping, _, _>(|_ctx, typing| async move {
            if typing.user_id.get() % 1000 == 999 {
                return Ok(Decision::Reject(AppError::forbidden("muted")));
            }
            Ok(Decision::Continue(typing))
        })
        .before::<BeforeRoomCreate, _, _>(|_ctx, mut room| async move {
            if room.name.contains("forbidden") {
                return Ok(Decision::Reject(AppError::forbidden("not that name")));
            }
            room.name = room.name.replace("darn", "d**n");
            Ok(Decision::Continue(room))
        })
        .before::<BeforeRoomInvite, _, _>(|_ctx, invite| async move {
            if invite.invitee.get() % 1000 == 998 {
                return Ok(Decision::Reject(AppError::forbidden("blocked")));
            }
            Ok(Decision::Continue(invite))
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
    let deadline = std::time::Instant::now() + WAIT;
    while server.service().room(&server.state, RoomId(2)).await.ok().flatten().is_none() {
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

    async fn grant(&self, user: UserId, role: &str) {
        self.state.get::<AuthService>().expect("auth").set_user_role(&self.state, user, role, true).await.expect("role");
    }

    async fn audit(&self, action: &str) -> Vec<Option<UserId>> {
        let auth = self.state.get::<AuthService>().expect("auth");
        auth.audit_log(&self.state, &AuditQuery::new().with_action(action)).await.expect("audit").items.iter().map(|r| r.actor).collect()
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

/// The next push of `kind` (other frames skipped).
async fn push_of(ws: &mut Ws, kind: &str) -> Value {
    loop {
        let frame = recv(ws).await;
        if frame["type"] == kind && frame.get("id").is_none() {
            return frame["data"].clone();
        }
    }
}

/// Send a request and read until its answer; frames before it are returned too.
async fn call(ws: &mut Ws, id: u64, kind: &str, data: Value) -> (Value, Vec<Value>) {
    ws.send(Message::text(json!({"id": id, "type": kind, "data": data}).to_string())).await.expect("send");
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

/// The next `chat.room` push with this `change` (other frames skipped).
async fn room_event(ws: &mut Ws, change: &str) -> Value {
    loop {
        let push = push_of(ws, "chat.room").await;
        if push["change"] == change {
            return push;
        }
    }
}

/// No push of `kind` is queued for `ws` (a round trip after the action answers first).
async fn no_push(ws: &mut Ws, id: u64, kind: &str) {
    let (_, pushes) = call(ws, id, "chat.members", json!({"room": 999_999})).await;
    assert!(pushes.iter().all(|p| p["type"] != kind), "unexpected {kind}: {pushes:?}");
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

fn room_path(room: i64, rest: &str) -> String {
    format!("/v1/chat/rooms/{room}{rest}")
}

// ---- editing ------------------------------------------------------------------------------------

async fn editing(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, ada_token) = server.register("ed-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("ed-b@example.com", "Bo").await;
    let (mo, mo_token) = server.register("ed-m@example.com", "Mo").await;
    server.grant(mo, "moderator").await;
    let mut a = server.connect(ada, &ada_token).await;
    let mut b = server.connect(bo, &bo_token).await;
    let world = ok(&mut a, 1, "chat.join", json!({"room": "world"})).await["id"].as_i64().expect("id");
    ok(&mut b, 1, "chat.join", json!({"room": world})).await;
    let sent = ok(&mut a, 2, "chat.send", json!({"room": world, "text": "helo", "nonce": "n1"})).await["message_id"].as_i64().expect("id");
    push_of(&mut b, "chat.message").await;
    // The sender edits within the window: the answer is the message, the room gets chat.edited.
    server.clock.advance(1000);
    let edited = ok(&mut a, 3, "chat.edit", json!({"room": world, "message": sent, "text": "hello darn"})).await;
    assert_eq!((edited["text"].as_str(), edited["edited_at"].as_i64(), edited["sent_at"].as_i64()), (Some("hello d**n"), Some(T0 + 1000), Some(T0)));
    assert_eq!((edited["nonce"].as_str(), edited["sender_name"].as_str()), (Some("n1"), Some("Ada")), "the sender's own nonce");
    let push = push_of(&mut b, "chat.edited").await;
    assert_eq!(push, json!({"id": sent, "room": world, "text": "hello d**n", "edited_at": T0 + 1000, "edited_by": ada.get()}));
    // The history shows the latest text with edited_at; the send hook's refusal holds for edits.
    let history = ok(&mut b, 2, "chat.history", json!({"room": world})).await;
    assert_eq!((history["items"][0]["text"].as_str(), history["items"][0]["edited_at"].as_i64()), (Some("hello d**n"), Some(T0 + 1000)));
    assert_eq!(error_code(&mut a, 4, "chat.edit", json!({"room": world, "message": sent, "text": "buy gold"})).await, codes::FORBIDDEN);
    assert_eq!(error_code(&mut a, 5, "chat.edit", json!({"room": world, "message": sent, "text": "  "})).await, codes::VALIDATION_FAILED);
    // Someone else: refused; over HTTP the same.
    assert_eq!(error_code(&mut b, 3, "chat.edit", json!({"room": world, "message": sent, "text": "mine now"})).await, codes::FORBIDDEN);
    let path = routes::chat_message_path(RoomId(world), MessageId(sent));
    assert_eq!(server.http(Method::PATCH, &path, Some(json!({"text": "x"})), &bo_token).await.0, StatusCode::FORBIDDEN);
    // The window closes (900 s by default); a moderator (chat.moderate) still edits, audited.
    server.clock.advance(900_000);
    assert_eq!(error_code(&mut a, 6, "chat.edit", json!({"room": world, "message": sent, "text": "late"})).await, codes::FORBIDDEN);
    let (status, body) = server.http(Method::PATCH, &path, Some(json!({"text": "[removed by a moderator]"})), &mo_token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("nonce").is_none(), "the nonce is the sender's: {body}");
    assert_eq!(push_of(&mut a, "chat.edited").await["edited_by"], mo.get());
    assert_eq!(push_of(&mut b, "chat.edited").await["text"], "[removed by a moderator]");
    assert_eq!(server.audit("chat.message_edited").await, vec![Some(mo)], "only the moderator's edit is audited");
    // Deleted messages cannot be edited; a missing one is 404 too.
    assert_eq!(server.http(Method::DELETE, &path, None, &ada_token).await.0, StatusCode::OK);
    assert_eq!(server.http(Method::PATCH, &path, Some(json!({"text": "back"})), &mo_token).await.0, StatusCode::NOT_FOUND);
    // A DM edit reaches both users; server code edits too.
    let (_, dm) = server.http(Method::POST, routes::chat::DM, Some(json!({"user": bo.get()})), &ada_token).await;
    let dm = dm["id"].as_i64().expect("dm");
    let in_dm = ok(&mut a, 7, "chat.send", json!({"room": dm, "text": "psst"})).await["message_id"].as_i64().expect("id");
    ok(&mut a, 8, "chat.edit", json!({"room": dm, "message": in_dm, "text": "psst!"})).await;
    assert_eq!(push_of(&mut b, "chat.edited").await["text"], "psst!");
    let system = server.service().edit_message(&server.state, RoomId(dm), MessageId(in_dm), "edited by the server").await.expect("server edit");
    assert_eq!(system.text, "edited by the server");
    let push = push_of(&mut b, "chat.edited").await;
    assert!(push.get("edited_by").is_none(), "{push}");
    // Turned off: senders cannot edit (moderators can).
    server.stop().await;
    let server = start(url, |chat| chat.allow_edit = false).await;
    let (cy, cy_token) = server.register("ed-c@example.com", "Cy").await;
    let mut c = server.connect(cy, &cy_token).await;
    let world = ok(&mut c, 1, "chat.join", json!({"room": "world"})).await["id"].as_i64().expect("id");
    let sent = ok(&mut c, 2, "chat.send", json!({"room": world, "text": "hi"})).await["message_id"].as_i64().expect("id");
    assert_eq!(error_code(&mut c, 3, "chat.edit", json!({"room": world, "message": sent, "text": "hey"})).await, codes::FORBIDDEN);
    server.stop().await;
}

// ---- read markers -------------------------------------------------------------------------------

async fn read_markers(url: &str) {
    let server = start(url, |chat| chat.read_push_interval_ms = 1500).await;
    let (ada, ada_token) = server.register("rd-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("rd-b@example.com", "Bo").await;
    let (cy, cy_token) = server.register("rd-c@example.com", "Cy").await;
    let mut a = server.connect(ada, &ada_token).await;
    let mut b = server.connect(bo, &bo_token).await;
    let (_, dm) = server.http(Method::POST, routes::chat::DM, Some(json!({"user": bo.get()})), &ada_token).await;
    let dm = dm["id"].as_i64().expect("dm");
    let mut ids = Vec::new();
    for (n, text) in ["one", "two", "three", "four"].iter().enumerate() {
        ids.push(ok(&mut a, 10 + n as u64, "chat.send", json!({"room": dm, "text": text})).await["message_id"].as_i64().expect("id"));
    }
    // Unread: all four for Bo, none for Ada (her own).
    let counts = ok(&mut b, 1, "chat.unread", json!({"rooms": [dm]})).await;
    assert_eq!(counts, json!({"rooms": [{"room": dm, "unread": 4}]}));
    assert_eq!(ok(&mut a, 20, "chat.unread", json!({"rooms": [dm]})).await["rooms"][0]["unread"], 0);
    // Bo reads up to the first: Ada gets chat.read at once.
    ok(&mut b, 2, "chat.mark_read", json!({"room": dm, "message": ids[0]})).await;
    let receipt = push_of(&mut a, "chat.read").await;
    assert_eq!(receipt, json!({"room": dm, "user": bo.get(), "message": ids[0], "read_at": T0}));
    // Two more within the interval: one push, the newest marker.
    ok(&mut b, 3, "chat.mark_read", json!({"room": dm, "message": ids[1]})).await;
    ok(&mut b, 4, "chat.mark_read", json!({"room": dm, "message": ids[2]})).await;
    assert_eq!(push_of(&mut a, "chat.read").await["message"], ids[2], "coalesced: the newest wins");
    no_push(&mut a, 21, "chat.read").await;
    // Backwards: the marker stays (no push).
    ok(&mut b, 5, "chat.mark_read", json!({"room": dm, "message": ids[0]})).await;
    let counts = ok(&mut b, 6, "chat.unread", json!({"rooms": [dm]})).await;
    assert_eq!(counts["rooms"][0], json!({"room": dm, "unread": 1, "last_read": ids[2]}));
    // Over HTTP: the marker, the receipts, the counts.
    assert_eq!(server.http(Method::PUT, &room_path(dm, "/read"), Some(json!({"message": ids[3]})), &bo_token).await.0, StatusCode::OK);
    assert_eq!(push_of(&mut a, "chat.read").await["message"], ids[3], "pushed (now or at the end of the interval)");
    let (status, receipts) = server.http(Method::GET, &room_path(dm, "/receipts"), None, &ada_token).await;
    assert_eq!(status, StatusCode::OK, "{receipts}");
    assert_eq!((receipts["receipts"][0]["user"].as_i64(), receipts["receipts"][0]["message"].as_i64()), (Some(bo.get()), Some(ids[3])));
    let (_, counts) = server.http(Method::POST, routes::chat::UNREAD, Some(json!({"rooms": [dm, 999_999]})), &bo_token).await;
    assert_eq!(counts, json!({"rooms": [{"room": dm, "unread": 0, "last_read": ids[3]}]}), "unknown rooms are left out");
    // A message of another room, an outsider, a deleted message (not counted).
    // ("hall": a public room no other part of the suite writes to; the file database is shared.)
    let world = ok(&mut a, 22, "chat.join", json!({"room": "hall"})).await["id"].as_i64().expect("hall");
    let in_world = ok(&mut a, 23, "chat.send", json!({"room": world, "text": "hi all"})).await["message_id"].as_i64().expect("id");
    assert_eq!(error_code(&mut b, 7, "chat.mark_read", json!({"room": dm, "message": in_world})).await, codes::NOT_FOUND);
    let mut c = server.connect(cy, &cy_token).await;
    assert_eq!(error_code(&mut c, 1, "chat.mark_read", json!({"room": dm, "message": ids[0]})).await, codes::NOT_A_MEMBER);
    assert_eq!(error_code(&mut c, 2, "chat.receipts", json!({"room": dm})).await, codes::NOT_A_MEMBER);
    assert_eq!(ok(&mut c, 3, "chat.unread", json!({"rooms": [dm, world]})).await["rooms"], json!([{"room": world, "unread": 1}]), "only rooms Cy may read");
    assert_eq!(error_code(&mut c, 4, "chat.unread", json!({"rooms": []})).await, codes::VALIDATION_FAILED);
    let gone = ok(&mut a, 24, "chat.send", json!({"room": world, "text": "oops"})).await["message_id"].as_i64().expect("id");
    server.service().delete_message(&server.state, RoomId(world), MessageId(gone)).await.expect("delete");
    assert_eq!(ok(&mut c, 5, "chat.unread", json!({"rooms": [world]})).await["rooms"][0]["unread"], 1, "deleted messages do not count");
    // A public room stores the marker without a push and shares no receipts.
    ok(&mut c, 6, "chat.join", json!({"room": world})).await;
    ok(&mut c, 7, "chat.mark_read", json!({"room": world, "message": in_world})).await;
    no_push(&mut a, 25, "chat.read").await;
    assert_eq!(error_code(&mut c, 8, "chat.receipts", json!({"room": world})).await, codes::BAD_REQUEST);
    assert_eq!(ok(&mut c, 9, "chat.unread", json!({"rooms": [world]})).await["rooms"][0]["unread"], 0);
    server.stop().await;
    // The rate: a burst of `read_rate`, then 429.
    let server = start(url, |chat| chat.read_rate = 2).await;
    let (dee, dee_token) = server.register("rd-d@example.com", "Dee").await;
    let mut d = server.connect(dee, &dee_token).await;
    let world = ok(&mut d, 1, "chat.join", json!({"room": "world"})).await["id"].as_i64().expect("world");
    let sent = ok(&mut d, 2, "chat.send", json!({"room": world, "text": "x"})).await["message_id"].as_i64().expect("id");
    ok(&mut d, 3, "chat.mark_read", json!({"room": world, "message": sent})).await;
    ok(&mut d, 4, "chat.mark_read", json!({"room": world, "message": sent})).await;
    assert_eq!(error_code(&mut d, 5, "chat.mark_read", json!({"room": world, "message": sent})).await, codes::RATE_LIMITED);
    server.stop().await;
}

// ---- typing -------------------------------------------------------------------------------------

async fn typing(url: &str) {
    let server = start(url, |chat| chat.typing_max_members = 2).await;
    let (ada, ada_token) = server.register("ty-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("ty-b@example.com", "Bo").await;
    let (cy, cy_token) = server.register("ty-c@example.com", "Cy").await;
    let mut a = server.connect(ada, &ada_token).await;
    let mut b = server.connect(bo, &bo_token).await;
    let world = ok(&mut a, 1, "chat.join", json!({"room": "world"})).await["id"].as_i64().expect("id");
    ok(&mut b, 1, "chat.join", json!({"room": world})).await;
    // Typing: pushed once per interval; a stop after a start; a second stop is quiet.
    ok(&mut a, 2, "chat.set_typing", json!({"room": world, "typing": true})).await;
    assert_eq!(push_of(&mut b, "chat.typing").await, json!({"room": world, "user": ada.get(), "typing": true, "expires_in_ms": 6000}));
    ok(&mut a, 3, "chat.set_typing", json!({"room": world, "typing": true})).await;
    no_push(&mut b, 2, "chat.typing").await;
    ok(&mut a, 4, "chat.set_typing", json!({"room": world, "typing": false})).await;
    assert_eq!(push_of(&mut b, "chat.typing").await, json!({"room": world, "user": ada.get(), "typing": false, "expires_in_ms": 0}));
    ok(&mut a, 5, "chat.set_typing", json!({"room": world, "typing": false})).await;
    no_push(&mut b, 3, "chat.typing").await;
    // A start after a stop and after a message pushes at once.
    ok(&mut a, 6, "chat.set_typing", json!({"room": world, "typing": true})).await;
    assert_eq!(push_of(&mut b, "chat.typing").await["typing"], true);
    ok(&mut a, 7, "chat.send", json!({"room": world, "text": "done"})).await;
    ok(&mut a, 8, "chat.set_typing", json!({"room": world, "typing": true})).await;
    assert_eq!(push_of(&mut b, "chat.typing").await["typing"], true, "the message cleared the throttle");
    // Not joined: refused. DMs need no join.
    let mut c = server.connect(cy, &cy_token).await;
    assert_eq!(error_code(&mut c, 1, "chat.set_typing", json!({"room": world, "typing": true})).await, codes::NOT_A_MEMBER);
    let (_, dm) = server.http(Method::POST, routes::chat::DM, Some(json!({"user": bo.get()})), &cy_token).await;
    ok(&mut c, 2, "chat.set_typing", json!({"room": dm["id"], "typing": true})).await;
    assert_eq!(push_of(&mut b, "chat.typing").await["user"], cy.get());
    // A room over the cap (3 online > 2): quiet.
    ok(&mut c, 3, "chat.join", json!({"room": world})).await;
    ok(&mut b, 4, "chat.set_typing", json!({"room": world, "typing": true})).await;
    let (_, pushes) = call(&mut c, 4, "chat.members", json!({"room": 999_999})).await;
    assert!(!pushes.iter().any(|p| p["type"] == "chat.typing" && p["data"]["user"] == bo.get()), "{pushes:?}");
    server.stop().await;
}

// ---- rooms created by players -------------------------------------------------------------------

async fn player_rooms(url: &str) {
    let server = start(url, |chat| {
        chat.max_rooms_per_player = 2;
        chat.max_player_room_members = 3;
    })
    .await;
    let (ada, ada_token) = server.register("pr-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("pr-b@example.com", "Bo").await;
    let (cy, cy_token) = server.register("pr-c@example.com", "Cy").await;
    let (dee, dee_token) = server.register("pr-d@example.com", "Dee").await;
    let mut a = server.connect(ada, &ada_token).await;
    let mut b = server.connect(bo, &bo_token).await;
    // Create: public and private; the owner's limit; the rules; the hook.
    let (status, den) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "darn Den", "visibility": "public"})), &ada_token).await;
    assert_eq!(status, StatusCode::OK, "{den}");
    assert_eq!((den["kind"].as_str(), den["name"].as_str(), den["visibility"].as_str()), (Some("player"), Some("d**n Den"), Some("public")));
    assert_eq!((den["owner"].as_i64(), den["role"].as_str()), (Some(ada.get()), Some("owner")));
    let den = den["id"].as_i64().expect("id");
    let (_, vault) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "Vault"})), &ada_token).await;
    assert_eq!(vault["visibility"], "private");
    let vault = vault["id"].as_i64().expect("id");
    let (status, body) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "Third"})), &ada_token).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::QUOTA_EXCEEDED));
    assert_eq!(server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": " "})), &bo_token).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "forbidden"})), &bo_token).await.0, StatusCode::FORBIDDEN);
    // Lists: the public rooms; the server's rooms stay apart.
    let (_, public) = server.http(Method::GET, routes::chat::ROOMS_PUBLIC, None, &bo_token).await;
    let listed: Vec<i64> = public["items"].as_array().map(|i| i.iter().filter_map(|r| r["id"].as_i64()).collect()).unwrap_or_default();
    assert_eq!(listed, vec![den]);
    assert!(public["items"][0].get("role").is_none(), "Bo has no role there");
    let (_, configured) = server.http(Method::GET, routes::chat::ROOMS, None, &bo_token).await;
    assert!(configured["items"].as_array().is_some_and(|i| i.iter().all(|r| r["kind"] == "room")));
    // Join the public room by chat.join: a member (stored); the owner is told.
    let joined = ok(&mut b, 1, "chat.join", json!({"room": den})).await;
    assert_eq!(joined["role"], "member");
    assert_eq!(room_event(&mut a, "joined").await, json!({"room": den, "change": "joined", "user": bo.get(), "by": bo.get()}));
    // The private room: not without an invitation.
    assert_eq!(error_code(&mut b, 2, "chat.join", json!({"room": vault})).await, codes::NOT_A_MEMBER);
    assert_eq!(server.http(Method::GET, &room_path(vault, ""), None, &bo_token).await.0, StatusCode::FORBIDDEN);
    assert_eq!(server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": cy.get()})), &bo_token).await.0, StatusCode::FORBIDDEN);
    // An invitation: pushed to the invited player, listed in "mine", accepted by joining.
    assert_eq!(server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": bo.get()})), &ada_token).await.0, StatusCode::OK);
    assert_eq!(room_event(&mut b, "invited").await, json!({"room": vault, "change": "invited", "user": bo.get(), "by": ada.get()}));
    assert_eq!(server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": bo.get()})), &ada_token).await.0, StatusCode::OK);
    no_push(&mut b, 3, "chat.room").await;
    let (_, mine) = server.http(Method::GET, routes::chat::ROOMS_MINE, None, &bo_token).await;
    let roles: Vec<(i64, String)> = mine["items"]
        .as_array()
        .map(|i| i.iter().map(|r| (r["id"].as_i64().unwrap_or(0), r["role"].as_str().unwrap_or("").to_string())).collect())
        .unwrap_or_default();
    assert_eq!(roles, vec![(den, "member".to_string()), (vault, "invited".to_string())]);
    #[cfg(feature = "notifications")]
    {
        use net_backend_server::notifications::NotificationService;
        use net_backend_server::protocol::notifications::NotificationQuery;
        let notes = server.state.get::<NotificationService>().expect("notifications").list(&server.state, bo, &NotificationQuery::new()).await.expect("list");
        let invited = notes.items.iter().any(|n| n.kind == "chat.invite" && n.data.as_ref().is_some_and(|d| d["room"] == vault));
        assert!(invited, "the invitation is a notification");
    }
    let (status, accepted) = server.http(Method::POST, &room_path(vault, "/join"), None, &bo_token).await;
    assert_eq!((status, accepted["role"].as_str()), (StatusCode::OK, Some("member")));
    assert_eq!(room_event(&mut a, "joined").await["user"], bo.get());
    // Roles: the owner names a moderator, who invites; the room fills up (3 places).
    assert_eq!(
        server.http(Method::PUT, &room_path(vault, &format!("/members/{bo}/role")), Some(json!({"role": "moderator"})), &bo_token).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server.http(Method::PUT, &room_path(vault, &format!("/members/{bo}/role")), Some(json!({"role": "owner"})), &ada_token).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        server.http(Method::PUT, &room_path(vault, &format!("/members/{bo}/role")), Some(json!({"role": "moderator"})), &ada_token).await.0,
        StatusCode::OK
    );
    assert_eq!(room_event(&mut b, "role").await, json!({"room": vault, "change": "role", "user": bo.get(), "by": ada.get(), "role": "moderator"}));
    assert_eq!(server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": cy.get()})), &bo_token).await.0, StatusCode::OK);
    let mut c = server.connect(cy, &cy_token).await;
    ok(&mut c, 1, "chat.join", json!({"room": vault})).await;
    let (status, body) = server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": dee.get()})), &ada_token).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::QUOTA_EXCEEDED), "the room is full");
    assert_eq!(server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": 1998})), &ada_token).await.0, StatusCode::FORBIDDEN, "the hook");
    // Kicks: the moderator kicks a member (banned: no rejoin, the socket leaves the room).
    assert_eq!(server.http(Method::DELETE, &room_path(vault, &format!("/members/{ada}")), None, &bo_token).await.0, StatusCode::FORBIDDEN, "not the owner");
    assert_eq!(server.http(Method::DELETE, &room_path(vault, &format!("/members/{cy}")), None, &bo_token).await.0, StatusCode::OK);
    assert_eq!(room_event(&mut c, "kicked").await, json!({"room": vault, "change": "kicked", "user": cy.get(), "by": bo.get()}));
    empty_hub_room(&server, vault).await;
    assert_eq!(error_code(&mut c, 2, "chat.join", json!({"room": vault})).await, codes::FORBIDDEN);
    assert_eq!(error_code(&mut c, 3, "chat.history", json!({"room": vault})).await, codes::NOT_A_MEMBER);
    let (_, rows) = server.http(Method::GET, &room_path(vault, "/members"), None, &bo_token).await;
    let listed: Vec<(i64, &str)> =
        rows["items"].as_array().map(|i| i.iter().map(|r| (r["user"].as_i64().unwrap_or(0), r["role"].as_str().unwrap_or(""))).collect()).unwrap_or_default();
    assert_eq!(listed, vec![(ada.get(), "owner"), (bo.get(), "moderator"), (cy.get(), "banned")], "staff see bans");
    assert_eq!(server.http(Method::GET, &room_path(vault, "/members"), None, &cy_token).await.0, StatusCode::FORBIDDEN);
    // An invitation lifts the ban.
    assert_eq!(server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": cy.get()})), &ada_token).await.0, StatusCode::OK);
    assert_eq!(server.http(Method::POST, &room_path(vault, "/join"), None, &cy_token).await.0, StatusCode::OK);
    // Changes: moderators rename, only the owner changes the visibility; everyone is told.
    let (status, renamed) = server.http(Method::PATCH, &room_path(vault, ""), Some(json!({"name": "Big Vault"})), &bo_token).await;
    assert_eq!((status, renamed["name"].as_str(), renamed["role"].as_str()), (StatusCode::OK, Some("Big Vault"), Some("moderator")));
    let update = room_event(&mut a, "updated").await;
    assert_eq!(update["info"]["name"], "Big Vault");
    assert!(update["info"].get("role").is_none(), "a shared push has no role: {update}");
    assert_eq!(server.http(Method::PATCH, &room_path(vault, ""), Some(json!({"visibility": "public"})), &bo_token).await.0, StatusCode::FORBIDDEN);
    assert_eq!(server.http(Method::PATCH, &room_path(vault, ""), Some(json!({})), &ada_token).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, opened) = server.http(Method::PATCH, &room_path(vault, ""), Some(json!({"visibility": "public"})), &ada_token).await;
    assert_eq!(opened["visibility"], "public");
    // Room staff moderate messages in their room (audited); members do not.
    let said = ok(&mut b, 5, "chat.send", json!({"room": den, "text": "spam"})).await["message_id"].as_i64().expect("id");
    let path = routes::chat_message_path(RoomId(den), MessageId(said));
    assert_eq!(server.http(Method::DELETE, &path, None, &ada_token).await.0, StatusCode::OK, "the owner of the room");
    // Hand on: the old owner becomes a moderator; the owner's limit counts for the new one.
    assert_eq!(server.http(Method::POST, &room_path(vault, "/owner"), Some(json!({"user": dee.get()})), &ada_token).await.0, StatusCode::NOT_FOUND);
    assert_eq!(server.http(Method::POST, &room_path(vault, "/owner"), Some(json!({"user": bo.get()})), &ada_token).await.0, StatusCode::OK);
    let (_, info) = server.http(Method::GET, &room_path(vault, ""), None, &ada_token).await;
    assert_eq!((info["owner"].as_i64(), info["role"].as_str()), (Some(bo.get()), Some("moderator")));
    // The owner leaves: the oldest moderator owns the room.
    assert_eq!(server.http(Method::POST, &room_path(vault, "/leave"), None, &bo_token).await.0, StatusCode::OK);
    let (_, info) = server.http(Method::GET, &room_path(vault, ""), None, &ada_token).await;
    assert_eq!((info["owner"].as_i64(), info["role"].as_str()), (Some(ada.get()), Some("owner")));
    // Delete: members are told, sockets leave, the messages go.
    ok(&mut a, 30, "chat.join", json!({"room": den})).await;
    assert_eq!(server.http(Method::DELETE, &room_path(den, ""), None, &bo_token).await.0, StatusCode::FORBIDDEN, "a member");
    assert_eq!(server.http(Method::DELETE, &room_path(den, ""), None, &ada_token).await.0, StatusCode::OK);
    assert_eq!(room_event(&mut b, "deleted").await, json!({"room": den, "change": "deleted", "by": ada.get()}));
    empty_hub_room(&server, den).await;
    assert_eq!(error_code(&mut a, 31, "chat.history", json!({"room": den})).await, codes::NOT_FOUND);
    // The last member leaves: the room is deleted.
    let (_, solo) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "Solo"})), &dee_token).await;
    let solo = solo["id"].as_i64().expect("id");
    assert_eq!(server.http(Method::POST, &room_path(solo, "/leave"), None, &dee_token).await.0, StatusCode::OK);
    assert_eq!(server.http(Method::GET, &room_path(solo, ""), None, &dee_token).await.0, StatusCode::NOT_FOUND);
    // Staff with chat.moderate act as the owner anywhere (a room deletion is audited).
    let (mo, mo_token) = server.register("pr-m@example.com", "Mo").await;
    server.grant(mo, "moderator").await;
    assert_eq!(server.http(Method::DELETE, &room_path(vault, &format!("/members/{ada}")), None, &mo_token).await.0, StatusCode::OK);
    assert_eq!(server.http(Method::DELETE, &room_path(vault, ""), None, &mo_token).await.0, StatusCode::OK);
    assert_eq!(server.audit("chat.room_deleted").await, vec![Some(mo)]);
    // Other kinds: the membership routes answer 400.
    let world = ok(&mut a, 32, "chat.join", json!({"room": "world"})).await["id"].as_i64().expect("world");
    assert_eq!(server.http(Method::POST, &room_path(world, "/join"), None, &ada_token).await.0, StatusCode::BAD_REQUEST);
    let (_, world_info) = server.http(Method::GET, &room_path(world, ""), None, &dee_token).await;
    assert_eq!(world_info["key"], "world");
    drop((a, b, c));
    server.stop().await;
}

/// Wait until no socket is in the hub room of `room`.
async fn empty_hub_room(server: &Server, room: i64) {
    let hub_room = format!("chat:{room}");
    let deadline = std::time::Instant::now() + WAIT;
    while server.state.ws().room_size(&hub_room) != 0 {
        assert!(std::time::Instant::now() < deadline, "a socket stayed in {hub_room}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn delete_account(user: UserId) -> net_backend_server::sea_query::DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table("auth_users").and_where(Expr::col("id").eq(user.get()));
    delete
}

/// Concurrent joins into the last places; the create rate; the upkeep after an owner's account
/// is deleted; turned off.
async fn room_limits_and_upkeep(url: &str) {
    let server = start(url, |chat| {
        chat.max_player_room_members = 3;
        chat.room_create_rate = 2;
    })
    .await;
    let (owner, owner_token) = server.register("lim-o@example.com", "Owner").await;
    let (_, hall) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "Hall", "visibility": "public"})), &owner_token).await;
    let hall = hall["id"].as_i64().expect("id");
    let mut players = Vec::new();
    for n in 0..6 {
        players.push(server.register(&format!("lim-{n}@example.com"), &format!("P{n}")).await);
    }
    let joins = players.iter().map(|(_, token)| {
        let (router, token) = (server.router.clone(), token.clone());
        async move {
            let request = Request::post(room_path(hall, "/join")).header("authorization", format!("Bearer {token}")).body(Body::empty()).expect("request");
            router.oneshot(request).await.expect("infallible").status()
        }
    });
    let statuses = futures_util::future::join_all(joins).await;
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::OK).count(), 2, "exactly the free places: {statuses:?}");
    assert!(statuses.iter().all(|s| *s == StatusCode::OK || *s == StatusCode::FORBIDDEN), "{statuses:?}");
    // The create rate (a burst of 2).
    let (_, second) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "Second"})), &owner_token).await;
    assert!(second["id"].is_i64(), "{second}");
    let (status, body) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "Third"})), &owner_token).await;
    assert_eq!((status, code(&body)), (StatusCode::TOO_MANY_REQUESTS, codes::RATE_LIMITED));
    // The owner's account is deleted: the upkeep makes the oldest member the owner.
    let first_member = {
        let (_, rows) = server.http(Method::GET, &room_path(hall, "/members"), None, &owner_token).await;
        UserId(rows["items"][1]["user"].as_i64().expect("member"))
    };
    server.state.db().execute(&delete_account(owner)).await.expect("delete");
    assert_eq!(server.service().room_upkeep(&server.state).await.expect("upkeep"), 2, "Hall and Second");
    let token = &players.iter().find(|(id, _)| *id == first_member).expect("member").1;
    let (_, info) = server.http(Method::GET, &room_path(hall, ""), None, token).await;
    assert_eq!((info["owner"].as_i64(), info["role"].as_str()), (Some(first_member.get()), Some("owner")));
    assert_eq!(server.service().room_upkeep(&server.state).await.expect("upkeep"), 0, "nothing left to fix (Second had no member: deleted)");
    server.stop().await;
    // Turned off: creating is refused.
    let server = start(url, |chat| chat.player_rooms = false).await;
    let (_, token) = server.register("off@example.com", "Off").await;
    let (status, body) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "Nope"})), &token).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::FORBIDDEN));
    server.stop().await;
}

/// Group rooms keep working with the new member roles (server code, as the groups and lobbies
/// modules use them): members join, read markers and typing work, the room stays a group room.
async fn group_rooms_still_work(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, ada_token) = server.register("gr-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("gr-b@example.com", "Bo").await;
    let group = server.service().create_group(&server.state, Some("Guild"), &[ada]).await.expect("group");
    server.service().add_member(&server.state, group.id, bo).await.expect("add");
    let mut a = server.connect(ada, &ada_token).await;
    let mut b = server.connect(bo, &bo_token).await;
    assert_eq!(ok(&mut a, 1, "chat.join", json!({"room": group.id})).await["kind"], "group");
    ok(&mut b, 1, "chat.join", json!({"room": group.id})).await;
    let sent = ok(&mut a, 2, "chat.send", json!({"room": group.id, "text": "guild news"})).await["message_id"].as_i64().expect("id");
    ok(&mut b, 2, "chat.mark_read", json!({"room": group.id, "message": sent})).await;
    assert_eq!(push_of(&mut a, "chat.read").await["user"], bo.get());
    assert_eq!(server.http(Method::POST, &room_path(group.id.get(), "/join"), None, &bo_token).await.0, StatusCode::BAD_REQUEST);
    server.service().remove_member(&server.state, group.id, bo).await.expect("remove");
    assert_eq!(error_code(&mut b, 3, "chat.history", json!({"room": group.id})).await, codes::NOT_A_MEMBER);
    server.stop().await;
}

/// The 0.2.0 review fixes: a holder of `chat.moderate` opens, reads and joins a private player
/// room (also after a ban); a kick answers by the actor's rights before it reveals a ban; a sender
/// removed from a room no longer deletes its messages there; the invitation rate; a block refuses
/// an invitation; an unknown invitee is 404; deleting a group room (server code).
async fn review_fixes(url: &str) {
    let server = start(url, |chat| chat.invite_rate = 3).await;
    let (ada, ada_token) = server.register("rf-a@example.com", "Ada").await;
    let (bo, bo_token) = server.register("rf-b@example.com", "Bo").await;
    let (cy, cy_token) = server.register("rf-c@example.com", "Cy").await;
    let (mo, mo_token) = server.register("rf-m@example.com", "Mo").await;
    server.grant(mo, "moderator").await;
    let (_, vault) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "Vault"})), &ada_token).await;
    let vault = vault["id"].as_i64().expect("id");
    let mut a = server.connect(ada, &ada_token).await;
    ok(&mut a, 1, "chat.join", json!({"room": vault})).await;
    let said = ok(&mut a, 2, "chat.send", json!({"room": vault, "text": "secret"})).await["message_id"].as_i64().expect("id");

    // SF-5: the moderator opens the private room and reads its history (HTTP and WebSocket); a
    // plain player still cannot.
    let (status, info) = server.http(Method::GET, &room_path(vault, ""), None, &mo_token).await;
    assert_eq!((status, info["visibility"].as_str()), (StatusCode::OK, Some("private")), "{info}");
    let (status, history) = server.http(Method::GET, &room_path(vault, "/messages"), None, &mo_token).await;
    assert_eq!((status, history["items"][0]["id"].as_i64()), (StatusCode::OK, Some(said)), "{history}");
    assert_eq!(server.http(Method::GET, &room_path(vault, "/messages"), None, &cy_token).await.0, StatusCode::FORBIDDEN);
    let mut m = server.connect(mo, &mo_token).await;
    assert_eq!(ok(&mut m, 1, "chat.history", json!({"room": vault})).await["items"][0]["id"].as_i64(), Some(said));
    // ... joins it (a member, stored), also after the owner banned it.
    assert_eq!(ok(&mut m, 2, "chat.join", json!({"room": vault})).await["role"], "member");
    assert_eq!(server.http(Method::DELETE, &room_path(vault, &format!("/members/{mo}")), None, &ada_token).await.0, StatusCode::OK);
    let (status, joined) = server.http(Method::POST, &room_path(vault, "/join"), None, &mo_token).await;
    assert_eq!((status, joined["role"].as_str()), (StatusCode::OK, Some("member")), "{joined}");

    // NIT 1: a member kicking a banned player gets 403 (not 200: nobody learns who is banned).
    assert_eq!(server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": bo.get()})), &ada_token).await.0, StatusCode::OK);
    assert_eq!(server.http(Method::POST, &room_path(vault, "/join"), None, &bo_token).await.0, StatusCode::OK);
    assert_eq!(server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": cy.get()})), &ada_token).await.0, StatusCode::OK);
    assert_eq!(server.http(Method::DELETE, &room_path(vault, &format!("/members/{cy}")), None, &ada_token).await.0, StatusCode::OK);
    let (status, body) = server.http(Method::DELETE, &room_path(vault, &format!("/members/{cy}")), None, &bo_token).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::FORBIDDEN), "a member, a banned target");
    let (status, body) = server.http(Method::DELETE, &room_path(vault, &format!("/members/{bo}")), None, &cy_token).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::NOT_A_MEMBER), "the banned player itself");

    // NIT 10: an unknown invitee is 404 (its account is locked first).
    let (status, body) = server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": 999_999})), &ada_token).await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, codes::NOT_FOUND));

    // SF-3: a player who blocked the inviter is not invited (with the friends module).
    #[cfg(feature = "friends")]
    {
        let (eve, eve_token) = server.register("rf-e@example.com", "Eve").await;
        let (status, _) = server.http(Method::PUT, &format!("/v1/friends/blocks/{bo}"), None, &eve_token).await;
        assert_eq!(status, StatusCode::OK);
        let (_, hall) = server.http(Method::POST, routes::chat::ROOMS, Some(json!({"name": "Hall"})), &bo_token).await;
        let hall = hall["id"].as_i64().expect("id");
        let (status, body) = server.http(Method::POST, &room_path(hall, "/invites"), Some(json!({"user": eve.get()})), &bo_token).await;
        assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::FORBIDDEN), "blocked");
    }
    // ... and the invitation rate (3 here): Ada sent 3 (one refused), the fourth is 429.
    let (status, body) = server.http(Method::POST, &room_path(vault, "/invites"), Some(json!({"user": cy.get()})), &ada_token).await;
    assert_eq!((status, code(&body)), (StatusCode::TOO_MANY_REQUESTS, codes::RATE_LIMITED));

    // NIT 4: a sender removed from a group room no longer deletes its message there.
    let group = server.service().create_group(&server.state, Some("Party"), &[ada, bo]).await.expect("group");
    let mut b = server.connect(bo, &bo_token).await;
    ok(&mut b, 1, "chat.join", json!({"room": group.id})).await;
    let mine = ok(&mut b, 2, "chat.send", json!({"room": group.id, "text": "hi"})).await["message_id"].as_i64().expect("id");
    server.service().remove_member(&server.state, group.id, bo).await.expect("remove");
    let (status, body) = server.http(Method::DELETE, &routes::chat_message_path(group.id, MessageId(mine)), None, &bo_token).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::NOT_A_MEMBER));

    // SF-2: deleting a group room (server code): its sockets leave, it is gone; again: false; a
    // player room is not deleted this way.
    ok(&mut a, 3, "chat.join", json!({"room": group.id})).await;
    assert!(server.service().delete_group_room(&server.state, group.id).await.expect("delete"));
    empty_hub_room(&server, group.id.get()).await;
    assert_eq!(error_code(&mut a, 4, "chat.history", json!({"room": group.id})).await, codes::NOT_FOUND);
    assert!(!server.service().delete_group_room(&server.state, group.id).await.expect("again"));
    let refused = server.service().delete_group_room(&server.state, RoomId(vault)).await;
    assert!(refused.is_err_and(|e| e.code() == codes::BAD_REQUEST));
    drop((a, b, m));
    server.stop().await;
}

async fn suite(url: &str) {
    editing(url).await;
    read_markers(url).await;
    typing(url).await;
    player_rooms(url).await;
    room_limits_and_upkeep(url).await;
    group_rooms_still_work(url).await;
    review_fixes(url).await;
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
    let dir = common::temp_dir("chat-extras-file");
    let url = format!("sqlite:{}", dir.join("chat.db").display().to_string().replace('\\', "/"));
    suite(&url).await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_chat_extras_suite() {
    let base = common::env_url("NBS_TEST_MYSQL_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_chat_extras_suite() {
    let base = common::env_url("NBS_TEST_POSTGRES_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
}

/// The documents list the new kinds; the module declares `chat.moderate`.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn documents_and_permission() {
    let mut config = Config::default();
    config.database.url = SecretString::new("sqlite::memory:");
    let prepared = NetBackendServer::new(config).module(Auth::new()).module(Chat::new()).build().await.expect("build");
    let asyncapi: Value = serde_json::from_str(prepared.asyncapi_json()).expect("json");
    let messages = asyncapi["components"]["messages"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    for kind in ["chat.edit", "chat.mark_read", "chat.receipts", "chat.unread", "chat.set_typing"] {
        assert!(messages.iter().any(|k| *k == format!("request.{kind}")), "{kind} in {messages:?}");
    }
    for push in ["chat.edited", "chat.read", "chat.typing", "chat.room"] {
        assert!(messages.iter().any(|k| *k == format!("push.{push}")), "{push} in {messages:?}");
    }
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for route in routes::ALL.iter().filter(|r| r.path.starts_with("/v1/chat")) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
    let permissions = prepared.state().get::<Permissions>().expect("permissions");
    assert!(permissions.declared().any(|p| p.name() == "chat.moderate"));
    assert_eq!(permissions.roles_with("chat.moderate"), ["moderator"]);
}
