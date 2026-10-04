//! The notifications module: sending from server code (validation, unknown accounts, the hooks),
//! the player's routes (lists newest first with cursors and `unread_only`, counts, marking read /
//! unread by ids or all, other players' ids skipped, deletes), the per-player cap (the oldest go),
//! the retention purge, account deletion (recipient: cascade; sender: kept, sender cleared),
//! concurrent sends, and the live `notify.new` push plus the `notify.*` requests on a loopback
//! server with a WebSocket client; the wiring and both API documents.
//!
//! The same suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "notifications", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::notifications::events::{AfterNotify, BeforeNotify};
use net_backend_server::notifications::{NewNotification, NotificationService, Notifications, NotificationsConfig};
use net_backend_server::protocol::{codes, routes, UnixMillis, UserId};
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

const T0: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;
const PASSWORD: &str = "correct horse battery";
const WAIT: Duration = Duration::from_secs(30);

struct Server {
    addr: SocketAddr,
    state: AppState,
    router: Router,
    clock: Arc<ManualClock>,
    after: Arc<std::sync::Mutex<Vec<(UserId, String)>>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), Error>>>,
}

async fn start(url: &str, tweak: impl FnOnce(&mut NotificationsConfig)) -> Server {
    common::watchdog(Duration::from_secs(1200));
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("notifications-migrations");
    config.server.shutdown_grace_secs = 5;
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    auth.access_token_ttl_secs = 7 * 24 * 3600;
    let mut notifications = NotificationsConfig::default();
    notifications.purge_interval_secs = 0;
    tweak(&mut notifications);
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let after = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = after.clone();
    let prepared = NetBackendServer::new(config)
        .clock(clock.clone())
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Notifications::new().with_config(notifications))
        // A game rule: "promo" is muted for everyone; "shout" texts are lower-cased.
        .before::<BeforeNotify, _, _>(|_ctx, mut notify| async move {
            if notify.notification.kind == "promo" {
                return Ok(Decision::Reject(AppError::forbidden("muted")));
            }
            if notify.notification.kind == "shout" {
                notify.notification.text = notify.notification.text.map(|t| t.to_lowercase());
            }
            Ok(Decision::Continue(notify))
        })
        .after::<AfterNotify, _, _>(move |_ctx, event| {
            let seen = seen.clone();
            async move {
                seen.lock().expect("lock").push((event.user_id, event.notification.kind.clone()));
                Ok(())
            }
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
    Server { addr, state, router, clock, after, stop: Some(stop), task: Some(task) }
}

impl Server {
    fn service(&self) -> Arc<NotificationService> {
        self.state.get::<NotificationService>().expect("service")
    }

    async fn register(&self, email: &str) -> (UserId, String) {
        let request = Request::post(routes::auth::REGISTER)
            .header("content-type", "application/json")
            .body(Body::from(json!({"email": email, "password": PASSWORD}).to_string()))
            .expect("request");
        let (status, _, body) = common::call(&self.router, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (UserId(body["account"]["id"].as_i64().expect("id")), body["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    async fn send_to(&self, user: UserId, kind: &str) -> i64 {
        self.service().send(&self.state, user, NewNotification::new(kind)).await.expect("send").id.get()
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

    async fn ok(&self, method: Method, path: &str, body: Option<Value>, token: &str) -> Value {
        let (status, answer) = self.http(method, path, body, token).await;
        assert_eq!(status, StatusCode::OK, "{path}: {answer}");
        answer
    }

    async fn connect(&self, user: UserId, token: &str) -> Ws {
        let before = self.state.ws().connections_of(user).len();
        let mut request = format!("ws://{}{}", self.addr, routes::WS).into_client_request().expect("request");
        request.headers_mut().insert("authorization", format!("Bearer {token}").parse().expect("header"));
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request)).await.expect("handshake in time").expect("connect");
        let deadline = std::time::Instant::now() + WAIT;
        while self.state.ws().connections_of(user).len() != before + 1 {
            assert!(std::time::Instant::now() < deadline, "the socket was never registered");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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

/// A request and its answer; the pushes that came before it.
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

fn ids_of(page: &Value) -> Vec<i64> {
    page["items"].as_array().map(|items| items.iter().filter_map(|n| n["id"].as_i64()).collect()).unwrap_or_default()
}

// ---- the suite ----------------------------------------------------------------------------------

async fn send_list_mark_delete(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("ada@example.com").await;
    let (bo, b) = server.register("bo@example.com").await;
    // Sending: the full shape, the hooks, the rules, unknown accounts.
    let gift = NewNotification::new("gift").with_text("Bo sent you 50 gold").with_data(json!({"gold": 50})).with_sender(bo);
    let sent = server.service().send(&server.state, ada, gift).await.expect("send");
    assert_eq!((sent.kind.as_str(), sent.sender, sent.read, sent.created_at), ("gift", Some(bo), false, UnixMillis(T0)));
    let shout = server.service().send(&server.state, ada, NewNotification::new("shout").with_text("HELLO")).await.expect("send");
    assert_eq!(shout.text.as_deref(), Some("hello"), "a hook may change it");
    let muted = server.service().send(&server.state, ada, NewNotification::new("promo")).await.expect_err("muted");
    assert_eq!(muted.code(), codes::FORBIDDEN);
    assert_eq!(server.service().send(&server.state, ada, NewNotification::new("Bad Kind")).await.expect_err("kind").code(), codes::VALIDATION_FAILED);
    let big = NewNotification::new("x").with_data(json!("d".repeat(5000)));
    assert_eq!(server.service().send(&server.state, ada, big).await.expect_err("data").code(), codes::VALIDATION_FAILED);
    assert_eq!(
        server.service().send(&server.state, ada, NewNotification::new("x").with_text("a\u{202E}b")).await.expect_err("text").code(),
        codes::VALIDATION_FAILED
    );
    assert_eq!(server.service().send(&server.state, UserId(999_999), NewNotification::new("x")).await.expect_err("nobody").code(), codes::NOT_FOUND);
    let ghost = NewNotification::new("x").with_sender(UserId(999_998));
    assert_eq!(server.service().send(&server.state, ada, ghost).await.expect_err("no sender").code(), codes::NOT_FOUND);
    assert_eq!(server.after.lock().expect("lock").as_slice(), [(ada, "gift".to_string()), (ada, "shout".to_string())]);
    let third = server.send_to(ada, "quest.done").await;
    server.send_to(bo, "welcome").await;
    // Lists: newest first, cursor pages, unread only, each player's own.
    let all = server.ok(Method::GET, routes::notifications::LIST, None, &a).await;
    assert_eq!(ids_of(&all), [third, shout.id.get(), sent.id.get()]);
    assert_eq!((all["items"][2]["data"]["gold"].as_i64(), all["items"][2]["sender"].as_i64()), (Some(50), Some(bo.get())));
    let first = server.ok(Method::GET, "/v1/notifications?limit=2", None, &a).await;
    let cursor = first["next_cursor"].as_str().expect("more").to_string();
    let rest = server.ok(Method::GET, &format!("/v1/notifications?limit=2&cursor={cursor}"), None, &a).await;
    assert_eq!((ids_of(&rest), rest.get("next_cursor")), (vec![sent.id.get()], None));
    assert_eq!(server.http(Method::GET, "/v1/notifications?cursor=abc", None, &a).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(ids_of(&server.ok(Method::GET, routes::notifications::LIST, None, &b).await).len(), 1, "only Bo's own");
    let count = server.ok(Method::GET, routes::notifications::COUNT, None, &a).await;
    assert_eq!((count["unread"].as_u64(), count["total"].as_u64()), (Some(3), Some(3)));
    // Marking: by ids (others' skipped), unread again, all.
    let bos = ids_of(&server.ok(Method::GET, routes::notifications::LIST, None, &b).await)[0];
    let ack = server.ok(Method::POST, routes::notifications::MARK, Some(json!({"ids": [sent.id.get(), bos], "read": true})), &a).await;
    assert_eq!((ack["changed"].as_u64(), ack["unread"].as_u64()), (Some(1), Some(2)));
    assert_eq!(server.ok(Method::GET, routes::notifications::COUNT, None, &b).await["unread"], 1, "Bo's stays unread");
    let unread = server.ok(Method::GET, "/v1/notifications?unread_only=true", None, &a).await;
    assert_eq!(ids_of(&unread), [third, shout.id.get()]);
    let read = server.ok(Method::GET, routes::notifications::LIST, None, &a).await;
    assert_eq!(read["items"][2]["read"], true);
    let again = server.ok(Method::POST, routes::notifications::MARK, Some(json!({"ids": [sent.id.get()], "read": true})), &a).await;
    assert_eq!(again["changed"], 0, "already read");
    let back = server.ok(Method::POST, routes::notifications::MARK, Some(json!({"ids": [sent.id.get()], "read": false})), &a).await;
    assert_eq!((back["changed"].as_u64(), back["unread"].as_u64()), (Some(1), Some(3)));
    let all_read = server.ok(Method::POST, routes::notifications::MARK, Some(json!({"all": true, "read": true})), &a).await;
    assert_eq!((all_read["changed"].as_u64(), all_read["unread"].as_u64()), (Some(3), Some(0)));
    for bad in [json!({"read": true}), json!({"ids": [], "read": true}), json!({"ids": [1, 1], "read": true}), json!({"all": true, "ids": [1], "read": true})] {
        let (status, body) = server.http(Method::POST, routes::notifications::MARK, Some(bad.clone()), &a).await;
        assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some(codes::VALIDATION_FAILED)), "{bad}");
    }
    // Deleting: idempotent, never another player's.
    let path = format!("/v1/notifications/{third}");
    assert_eq!(server.http(Method::DELETE, &path, None, &b).await.0, StatusCode::OK);
    assert_eq!(server.ok(Method::GET, routes::notifications::COUNT, None, &a).await["total"], 3, "Bo cannot delete Ada's");
    server.ok(Method::DELETE, &path, None, &a).await;
    server.ok(Method::DELETE, &path, None, &a).await;
    assert_eq!(server.ok(Method::GET, routes::notifications::COUNT, None, &a).await["total"], 2);
    assert_eq!(server.http(Method::DELETE, "/v1/notifications/abc", None, &a).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(server.http(Method::GET, routes::notifications::LIST, None, "not-a-token").await.0, StatusCode::UNAUTHORIZED);
    // Account deletion: the sender's notifications stay without a sender; the recipient's go.
    let delete_account = |user: UserId| {
        let mut delete = Query::delete();
        delete.from_table("auth_users").and_where(Expr::col("id").eq(user.get()));
        delete
    };
    server.state.db().execute(&delete_account(bo)).await.expect("delete Bo");
    let list = server.ok(Method::GET, routes::notifications::LIST, None, &a).await;
    assert!(list["items"].as_array().is_some_and(|items| items.iter().all(|n| n.get("sender").is_none())), "{list}");
    server.state.db().execute(&delete_account(ada)).await.expect("delete Ada");
    let (cy, _) = server.register("cy@example.com").await;
    assert_eq!(server.service().count(&server.state, ada).await.expect("count").total, 0);
    assert_eq!(server.service().count(&server.state, cy).await.expect("count").total, 0);
    server.stop().await;
}

async fn cap_retention_and_concurrency(url: &str) {
    let server = start(url, |n| {
        n.max_per_user = 5;
        n.retention_days = 3;
    })
    .await;
    let (ada, a) = server.register("ada@example.com").await;
    let mut ids = Vec::new();
    for i in 0..7 {
        ids.push(server.send_to(ada, &format!("n{i}")).await);
    }
    let list = server.ok(Method::GET, routes::notifications::LIST, None, &a).await;
    assert_eq!(ids_of(&list), ids.iter().rev().take(5).copied().collect::<Vec<_>>(), "the oldest two went");
    // Retention: 4 days later the old ones go, a new one stays.
    server.clock.advance(4 * DAY);
    let fresh = server.send_to(ada, "fresh").await;
    assert_eq!(server.service().purge(&server.state).await.expect("purge"), 4, "the five old, minus the one the cap took");
    assert_eq!(ids_of(&server.ok(Method::GET, routes::notifications::LIST, None, &a).await), [fresh]);
    // Concurrent sends: none lost or failed; the next send trims to the cap.
    let mut tasks = Vec::new();
    for i in 0..12 {
        let (state, service) = (server.state.clone(), server.service());
        tasks.push(tokio::spawn(async move { service.send(&state, ada, NewNotification::new(format!("c{i}"))).await.map(|n| n.id) }));
    }
    for task in tasks {
        task.await.expect("task").expect("send");
    }
    let last = server.send_to(ada, "last").await;
    let count = server.service().count(&server.state, ada).await.expect("count");
    assert_eq!(count.total, 5);
    assert_eq!(ids_of(&server.ok(Method::GET, routes::notifications::LIST, None, &a).await)[0], last);
    server.stop().await;
}

async fn live_pushes_and_ws_requests(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("ada@example.com").await;
    let (bo, b) = server.register("bo@example.com").await;
    let mut phone = server.connect(ada, &a).await;
    let mut pc = server.connect(ada, &a).await;
    let mut other = server.connect(bo, &b).await;
    let sent =
        server.service().send(&server.state, ada, NewNotification::new("reward").with_text("50 gold").with_data(json!({"gold": 50}))).await.expect("send");
    for ws in [&mut phone, &mut pc] {
        let push = recv(ws).await;
        assert_eq!(push["type"], "notify.new", "{push}");
        assert_eq!(
            (push["data"]["id"].as_i64(), push["data"]["kind"].as_str(), push["data"]["read"].as_bool()),
            (Some(sent.id.get()), Some("reward"), Some(false))
        );
        assert_eq!(push["data"]["data"]["gold"], 50);
    }
    // Bo got nothing: his next answer comes with no push before it.
    let (answer, pushes) = call(&mut other, 1, "notify.count", json!({})).await;
    assert_eq!((answer["ok"].as_bool(), answer["data"]["unread"].as_u64(), pushes.len()), (Some(true), Some(0), 0), "{answer}");
    // The requests over the WebSocket answer like the routes.
    let (answer, _) = call(&mut phone, 1, "notify.list", json!({"unread_only": true})).await;
    assert_eq!(ids_of(&answer["data"]), [sent.id.get()]);
    let (answer, _) = call(&mut phone, 2, "notify.mark", json!({"ids": [sent.id.get()], "read": true})).await;
    assert_eq!((answer["data"]["changed"].as_u64(), answer["data"]["unread"].as_u64()), (Some(1), Some(0)));
    let (answer, _) = call(&mut phone, 3, "notify.mark", json!({"ids": [], "read": true})).await;
    assert_eq!((answer["ok"].as_bool(), answer["error"]["code"].as_str()), (Some(false), Some(codes::VALIDATION_FAILED)));
    let (answer, _) = call(&mut phone, 4, "notify.delete", json!({"id": sent.id.get()})).await;
    assert_eq!(answer["ok"], true, "{answer}");
    let (answer, _) = call(&mut phone, 5, "notify.count", json!({})).await;
    assert_eq!((answer["data"]["unread"].as_u64(), answer["data"]["total"].as_u64()), (Some(0), Some(0)));
    let (answer, _) = call(&mut phone, 6, "notify.list", json!({"limit": "ten"})).await;
    assert_eq!(answer["error"]["code"], codes::BAD_REQUEST);
    for ws in [phone, pc, other] {
        drop(ws);
    }
    server.stop().await;
}

/// A fresh database for one part of the suite. `backend`: `memory`, `file`, or a MySQL /
/// PostgreSQL base URL.
async fn database(backend: &str) -> (String, Option<(String, String)>) {
    match backend {
        "memory" => ("sqlite::memory:".into(), None),
        "file" => {
            let dir = common::temp_dir("notifications-file");
            (format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/")), None)
        }
        #[cfg(any(feature = "mysql", feature = "postgres"))]
        base => {
            let (url, name) = common::fresh_database(base).await;
            (url, Some((base.to_string(), name)))
        }
        #[cfg(not(any(feature = "mysql", feature = "postgres")))]
        other => panic!("no backend for {other}"),
    }
}

macro_rules! each {
    ($backend:expr, $($part:ident),* $(,)?) => {
        $(
            let (url, cleanup) = database($backend).await;
            $part(&url).await;
            #[cfg(any(feature = "mysql", feature = "postgres"))]
            if let Some((base, name)) = cleanup {
                common::drop_database(&base, &name).await;
            }
            #[cfg(not(any(feature = "mysql", feature = "postgres")))]
            let _ = cleanup;
        )*
    };
}

async fn suite(backend: &str) {
    each!(backend, send_list_mark_delete, cap_retention_and_concurrency, live_pushes_and_ws_requests);
}

// ---- runners ------------------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_suite() {
    suite("memory").await;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_file_suite() {
    suite("file").await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_notifications_suite() {
    suite(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_notifications_suite() {
    suite(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}

/// The module's wiring: needs `auth` first, reads `[modules.notifications]`, refuses settings given
/// twice or unknown keys, lists its routes in the OpenAPI document and its kinds in the AsyncAPI
/// document, its name in `/v1/info`; the HTTP routes work with the WebSocket hub off.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn wiring_and_documents() {
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(Notifications::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");
    let file = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.notifications]\nmax_per_user = 7\n").expect("config");
    let ok = NetBackendServer::new(file.clone()).module(Auth::new()).module(Notifications::new()).build().await.expect("from the file");
    assert_eq!(ok.state().get::<NotificationService>().expect("service").config().max_per_user, 7);
    let error = NetBackendServer::new(file).module(Auth::new()).module(Notifications::new().with_config(NotificationsConfig::default())).build().await.err();
    assert!(error.map(|e| e.to_string()).unwrap_or_default().contains("both in code"));
    let bad = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.notifications]\nmax_per_player = 7\n").expect("config");
    assert!(NetBackendServer::new(bad).module(Auth::new()).module(Notifications::new()).build().await.is_err(), "unknown keys are refused");

    let prepared = NetBackendServer::new(config()).module(Auth::new()).module(Notifications::new()).build().await.expect("build");
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for route in routes::ALL.iter().filter(|r| r.path.starts_with("/v1/notifications")) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
    let asyncapi: Value = serde_json::from_str(prepared.asyncapi_json()).expect("json");
    for kind in ["notify.list", "notify.count", "notify.mark", "notify.delete"] {
        assert!(asyncapi["components"]["messages"].get(format!("request.{kind}")).is_some(), "{kind}");
    }
    assert!(asyncapi["components"]["messages"].get("push.notify.new").is_some());
    let (_, _, info) = common::call(&prepared.router(), common::get(routes::INFO)).await;
    assert_eq!(info["modules"], json!(["auth", "notifications"]));

    // The HTTP routes with the hub off: sending stores, nothing to push to.
    let mut no_ws = config();
    no_ws.ws.enabled = false;
    let prepared = NetBackendServer::new(no_ws).module(Auth::new()).module(Notifications::new()).build().await.expect("build without ws");
    prepared.migrate().await.expect("migrate");
    let request = Request::post(routes::auth::REGISTER)
        .header("content-type", "application/json")
        .body(Body::from(json!({"email": "solo@example.com", "password": PASSWORD}).to_string()))
        .expect("request");
    let (_, _, session) = common::call(&prepared.router(), request).await;
    let user = UserId(session["account"]["id"].as_i64().expect("id"));
    let service = prepared.state().get::<NotificationService>().expect("service");
    service.send(prepared.state(), user, NewNotification::new("hello")).await.expect("stored without a hub");
    assert_eq!(service.count(prepared.state(), user).await.expect("count").unread, 1);
}
