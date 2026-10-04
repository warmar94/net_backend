//! The friends module: requests by id, display name and friend code (sent, received, accepted,
//! declined, cancelled, asked back), friendships ended, blocks (refused requests, a block ending a
//! friendship, unblocking), the hooks, the notifications, friend codes made and replaced, account
//! deletion, the limits (open requests, friends, blocks) with concurrent requests, and the online
//! state: `friends.presence` pushes on a loopback server with WebSocket clients, heartbeats, the
//! online window, the refresh of the stored online time, the push order when a socket closes while
//! its "online" is on its way, two instances on one database; the wiring
//! and both API documents.
//!
//! The same suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "friends", feature = "notifications", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use futures_util::StreamExt;
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::friends::events::{AfterFriendChange, BeforeFriendRequest, FriendChange};
use net_backend_server::friends::{FriendService, Friends, FriendsConfig};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::notifications::{NotificationService, Notifications};
use net_backend_server::protocol::friends::FriendState;
use net_backend_server::protocol::notifications::NotificationQuery;
use net_backend_server::protocol::{codes, routes, UnixMillis, UserId};
use net_backend_server::sea_query::{Expr, ExprTrait, Query};
use net_backend_server::ws::events::{AfterWsConnect, AfterWsDisconnect};
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
type Changes = Arc<Mutex<Vec<(UserId, UserId, FriendChange)>>>;

const T0: i64 = 1_800_000_000_000;
const PASSWORD: &str = "correct horse battery";
const WAIT: Duration = Duration::from_secs(30);

/// Delivers in this process like the default broadcaster, but can hold one player's "online"
/// presence push inside `publish` (the pushing task waits there, as with a slow bus) so a test
/// decides what happens between a presence decision and its delivery.
#[derive(Default)]
struct PresenceGate {
    /// The player whose "online" push is held, while armed.
    armed: Mutex<Option<i64>>,
    /// Set while a push is held.
    holding: std::sync::atomic::AtomicBool,
    /// "offline" pushes of the armed player published while its "online" was held.
    overtaken: std::sync::atomic::AtomicUsize,
    released: std::sync::Condvar,
}

impl PresenceGate {
    /// `(user, online)` of a `friends.presence` frame.
    fn presence(delivery: &Delivery) -> Option<(i64, bool)> {
        let frame: Value = serde_json::from_str(delivery.frame.as_str()).ok()?;
        (frame["type"] == "friends.presence").then(|| (frame["data"]["user"].as_i64().unwrap_or(0), frame["data"]["online"].as_bool().unwrap_or(false)))
    }

    fn release(&self) {
        *self.armed.lock().expect("lock") = None;
        self.released.notify_all();
    }
}

struct GateBroadcaster(Arc<PresenceGate>);

impl Broadcaster for GateBroadcaster {
    fn publish(&self, local: &LocalDelivery, delivery: Delivery) {
        let gate = &self.0;
        if let Some((user, online)) = PresenceGate::presence(&delivery) {
            let armed = gate.armed.lock().expect("lock");
            if *armed == Some(user) {
                if online {
                    // Held here until the test releases it (a test-only blocking publish). Inside
                    // `block_in_place`: the runtime moves this worker's queued tasks and its timer /
                    // I/O duties to another thread first. A plain blocking wait here could strand
                    // them (the test then never woke: a hang on a 2 vCPU machine).
                    gate.holding.store(true, std::sync::atomic::Ordering::SeqCst);
                    let released = tokio::task::block_in_place(move || {
                        let mut armed = armed;
                        while armed.is_some() {
                            armed = gate.released.wait(armed).expect("lock");
                        }
                        armed
                    });
                    drop(released);
                    gate.holding.store(false, std::sync::atomic::Ordering::SeqCst);
                } else if gate.holding.load(std::sync::atomic::Ordering::SeqCst) {
                    gate.overtaken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        local.deliver(&delivery);
    }
}

struct Server {
    addr: SocketAddr,
    gate: Arc<PresenceGate>,
    state: AppState,
    router: Router,
    clock: Arc<ManualClock>,
    changes: Changes,
    refused: Arc<Mutex<Option<UserId>>>,
    /// A pause in an app hook that runs before the friends module's (spawned) connect hook.
    connect_delay: Arc<Mutex<Option<Duration>>>,
    /// The same before the friends module's disconnect hook.
    disconnect_delay: Arc<Mutex<Option<Duration>>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), Error>>>,
}

async fn start(url: &str, tweak: impl FnOnce(&mut FriendsConfig)) -> Server {
    start_with(url, |_| {}, tweak).await
}

async fn start_with(url: &str, server_tweak: impl FnOnce(&mut Config), tweak: impl FnOnce(&mut FriendsConfig)) -> Server {
    common::watchdog(Duration::from_secs(1200));
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("friends-migrations");
    config.server.shutdown_grace_secs = 5;
    server_tweak(&mut config);
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    auth.access_token_ttl_secs = 7 * 24 * 3600;
    let mut friends = FriendsConfig::default();
    friends.request_rate = 0;
    tweak(&mut friends);
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let changes: Changes = Arc::new(Mutex::new(Vec::new()));
    let refused: Arc<Mutex<Option<UserId>>> = Arc::new(Mutex::new(None));
    let (seen, refuse) = (changes.clone(), refused.clone());
    let connect_delay: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let delay = connect_delay.clone();
    let disconnect_delay: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let off_delay = disconnect_delay.clone();
    let gate = Arc::new(PresenceGate::default());
    let prepared = NetBackendServer::new(config)
        .clock(clock.clone())
        .broadcaster(GateBroadcaster(gate.clone()))
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Notifications::new())
        .module(Friends::new().with_config(friends))
        // A game rule: one account takes no requests.
        .before::<BeforeFriendRequest, _, _>(move |_ctx, request| {
            let refuse = refuse.clone();
            async move {
                if *refuse.lock().expect("lock") == Some(request.to) {
                    return Ok(Decision::Reject(AppError::forbidden("no friends here")));
                }
                Ok(Decision::Continue(request))
            }
        })
        .after::<AfterFriendChange, _, _>(move |_ctx, event| {
            let seen = seen.clone();
            async move {
                seen.lock().expect("lock").push((event.user, event.other, event.change));
                Ok(())
            }
        })
        // App hooks run before the modules' ones: this delays the friends module's connect hook.
        .after::<AfterWsConnect, _, _>(move |_ctx, _event| {
            let pause = *delay.lock().expect("lock");
            async move {
                if let Some(pause) = pause {
                    tokio::time::sleep(pause).await;
                }
                Ok(())
            }
        })
        .after::<AfterWsDisconnect, _, _>(move |_ctx, _event| {
            let pause = *off_delay.lock().expect("lock");
            async move {
                if let Some(pause) = pause {
                    tokio::time::sleep(pause).await;
                }
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
    Server { addr, gate, state, router, clock, changes, refused, connect_delay, disconnect_delay, stop: Some(stop), task: Some(task) }
}

impl Server {
    fn service(&self) -> Arc<FriendService> {
        self.state.get::<FriendService>().expect("service")
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

    async fn ok(&self, method: Method, path: &str, body: Option<Value>, token: &str) -> Value {
        let (status, answer) = self.http(method, path, body, token).await;
        assert_eq!(status, StatusCode::OK, "{path}: {answer}");
        answer
    }

    /// The error code of a request that must fail with `status`.
    async fn fails(&self, method: Method, path: &str, body: Option<Value>, token: &str, status: StatusCode) -> String {
        let (got, answer) = self.http(method, path, body, token).await;
        assert_eq!(got, status, "{path}: {answer}");
        answer["error"]["code"].as_str().unwrap_or_default().to_string()
    }

    async fn add(&self, token: &str, body: Value) -> Value {
        self.ok(Method::POST, routes::friends::REQUESTS, Some(body), token).await
    }

    async fn users(&self, path: &str, token: &str) -> Vec<i64> {
        let page = self.ok(Method::GET, path, None, token).await;
        page["items"].as_array().map(|items| items.iter().filter_map(|e| e["user"].as_i64()).collect()).unwrap_or_default()
    }

    /// The kinds and senders of a player's notifications, oldest first.
    async fn notes(&self, user: UserId) -> Vec<(String, Option<i64>)> {
        let service = self.state.get::<NotificationService>().expect("notifications");
        let page = service.list(&self.state, user, &NotificationQuery::new()).await.expect("list");
        page.items.iter().rev().map(|n| (n.kind.clone(), n.sender.map(UserId::get))).collect()
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

/// The next frame of a kind (others are skipped: notifications arrive too).
async fn next_of(ws: &mut Ws, kind: &str) -> Value {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await.expect("a frame in time") {
            Some(Ok(Message::Text(text))) => {
                let frame: Value = serde_json::from_str(text.as_str()).expect("JSON");
                if frame["type"] == kind {
                    return frame;
                }
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

fn delete_account(user: UserId) -> net_backend_server::sea_query::DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table("auth_users").and_where(Expr::col("id").eq(user.get()));
    delete
}

// ---- the suite ----------------------------------------------------------------------------------

async fn requests_and_blocks(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("ada@example.com", "Ada").await;
    let (bo, b) = server.register("bo@example.com", "Bo").await;
    let (cy, c) = server.register("cy@example.com", "Cy").await;
    let (dee, d) = server.register("dee@example.com", "Ada").await;
    // By friend code (typed in lower case with a dash): a request, a notification, sent again = the same.
    let code = server.ok(Method::GET, routes::friends::CODE, None, &a).await["code"].as_str().expect("code").to_string();
    assert_eq!(code.len(), 8);
    assert_eq!(server.ok(Method::GET, routes::friends::CODE, None, &a).await["code"], code.as_str(), "the same code");
    let typed = format!("{}-{}", code[..4].to_lowercase(), code[4..].to_lowercase());
    let sent = server.add(&b, json!({"code": typed})).await;
    assert_eq!((sent["user"].as_i64(), sent["state"].as_str(), sent["name"].as_str()), (Some(ada.get()), Some("sent"), Some("Ada")), "{sent}");
    assert!(sent.get("online").is_none(), "no online state for a request: {sent}");
    assert_eq!(server.add(&b, json!({"user": ada.get()})).await["state"], "sent", "sending again answers the same");
    assert_eq!(server.notes(ada).await, [("friends.request".to_string(), Some(bo.get()))], "one notification");
    assert_eq!(server.users(routes::friends::REQUESTS, &a).await, [bo.get()]);
    assert_eq!(server.users("/v1/friends/requests?direction=sent", &b).await, [ada.get()]);
    assert!(server.users("/v1/friends/requests?direction=sent", &a).await.is_empty());
    // Accepting: friends on both sides, the sender is told.
    let accepted = server.ok(Method::POST, &format!("/v1/friends/requests/{bo}/accept"), None, &a).await;
    assert_eq!((accepted["state"].as_str(), accepted["online"].as_bool(), accepted["since"].as_i64()), (Some("friend"), Some(false), Some(T0)), "{accepted}");
    assert_eq!(server.notes(bo).await, [("friends.accepted".to_string(), Some(ada.get()))]);
    assert_eq!(server.users(routes::friends::LIST, &a).await, [bo.get()]);
    assert_eq!(server.users(routes::friends::LIST, &b).await, [ada.get()]);
    assert!(server.users(routes::friends::REQUESTS, &a).await.is_empty() && server.users("/v1/friends/requests?direction=sent", &b).await.is_empty());
    assert_eq!(server.ok(Method::POST, &format!("/v1/friends/requests/{bo}/accept"), None, &a).await["state"], "friend", "accepting again: the same");
    // By name: exact; two accounts named Ada are ambiguous; a missing name is 404.
    assert_eq!(server.add(&c, json!({"name": " Bo "})).await["user"], bo.get());
    assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, Some(json!({"name": "Ada"})), &c, StatusCode::CONFLICT).await, codes::CONFLICT);
    assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, Some(json!({"name": "bo"})), &c, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    // Asked back: friends at once.
    server.add(&c, json!({"user": dee.get()})).await;
    let back = server.add(&d, json!({"user": cy.get()})).await;
    assert_eq!(back["state"], "friend", "{back}");
    assert_eq!(server.users(routes::friends::LIST, &c).await, [dee.get()]);
    // Declining, cancelling: gone on both sides, idempotent.
    server.ok(Method::POST, &format!("/v1/friends/requests/{cy}/decline"), None, &b).await;
    server.ok(Method::POST, &format!("/v1/friends/requests/{cy}/decline"), None, &b).await;
    assert!(server.users("/v1/friends/requests?direction=sent", &c).await.is_empty());
    server.add(&c, json!({"user": ada.get()})).await;
    server.ok(Method::DELETE, &format!("/v1/friends/requests/{ada}"), None, &c).await;
    server.ok(Method::DELETE, &format!("/v1/friends/requests/{ada}"), None, &c).await;
    assert!(server.users(routes::friends::REQUESTS, &a).await.is_empty());
    // Removing a friend: both sides, idempotent.
    server.ok(Method::DELETE, &format!("/v1/friends/{bo}"), None, &a).await;
    server.ok(Method::DELETE, &format!("/v1/friends/{bo}"), None, &a).await;
    assert!(server.users(routes::friends::LIST, &a).await.is_empty() && server.users(routes::friends::LIST, &b).await.is_empty());
    // Blocks: the blocked player's requests are refused, the blocker must unblock first, a block ends a friendship.
    server.ok(Method::PUT, &format!("/v1/friends/blocks/{cy}"), None, &a).await;
    server.ok(Method::PUT, &format!("/v1/friends/blocks/{cy}"), None, &a).await;
    assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, Some(json!({"user": ada.get()})), &c, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, Some(json!({"user": cy.get()})), &a, StatusCode::CONFLICT).await, codes::CONFLICT);
    let blocks = server.ok(Method::GET, routes::friends::BLOCKS, None, &a).await;
    assert_eq!((blocks["items"][0]["user"].as_i64(), blocks["items"][0]["state"].as_str()), (Some(cy.get()), Some("blocked")));
    assert!(
        server.service().is_blocked(&server.state, ada, cy).await.expect("read") && !server.service().is_blocked(&server.state, cy, ada).await.expect("read")
    );
    server.ok(Method::DELETE, &format!("/v1/friends/blocks/{cy}"), None, &a).await;
    server.ok(Method::DELETE, &format!("/v1/friends/blocks/{cy}"), None, &a).await;
    assert_eq!(server.add(&c, json!({"user": ada.get()})).await["state"], "sent", "unblocked: requests work again");
    server.add(&b, json!({"user": ada.get()})).await;
    server.ok(Method::POST, &format!("/v1/friends/requests/{bo}/accept"), None, &a).await;
    server.ok(Method::PUT, &format!("/v1/friends/blocks/{bo}"), None, &a).await;
    assert!(server.users(routes::friends::LIST, &b).await.is_empty(), "a block ends the friendship");
    assert!(!server.service().are_friends(&server.state, ada, bo).await.expect("read"));
    server.ok(Method::PUT, &format!("/v1/friends/blocks/{ada}"), None, &c).await;
    assert!(server.users(routes::friends::REQUESTS, &a).await.is_empty(), "a block ends the open request");
    assert_eq!(server.service().state_of(&server.state, ada, cy).await.expect("read"), None);
    // Errors.
    assert_eq!(
        server.fails(Method::POST, routes::friends::REQUESTS, Some(json!({"user": ada.get()})), &a, StatusCode::UNPROCESSABLE_ENTITY).await,
        codes::VALIDATION_FAILED
    );
    for bad in [json!({}), json!({"user": 1, "name": "x"}), json!({"code": "nope"}), json!({"name": ""})] {
        assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, Some(bad), &a, StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    }
    assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, Some(json!({"user": 999_999})), &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, Some(json!({"code": "ZZZZZZZZ"})), &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::POST, &format!("/v1/friends/requests/{dee}/accept"), None, &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::PUT, "/v1/friends/blocks/999999", None, &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::GET, "/v1/friends?cursor=abc", None, &a, StatusCode::BAD_REQUEST).await, codes::BAD_REQUEST);
    assert_eq!(server.fails(Method::GET, "/v1/friends/requests?direction=both", None, &a, StatusCode::BAD_REQUEST).await, codes::BAD_REQUEST);
    assert_eq!(server.fails(Method::DELETE, "/v1/friends/abc", None, &a, StatusCode::BAD_REQUEST).await, codes::BAD_REQUEST);
    assert_eq!(server.http(Method::GET, routes::friends::LIST, None, "not-a-token").await.0, StatusCode::UNAUTHORIZED);
    // The hook refuses; the after hook saw every change.
    *server.refused.lock().expect("lock") = Some(dee);
    assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, Some(json!({"user": dee.get()})), &a, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    let changes = server.changes.lock().expect("lock").clone();
    assert_eq!(changes[0], (bo, ada, FriendChange::Requested));
    assert_eq!(changes[1], (ada, bo, FriendChange::Accepted));
    assert!(changes.contains(&(ada, cy, FriendChange::Blocked)) && changes.contains(&(ada, cy, FriendChange::Unblocked)));
    assert!(changes.contains(&(bo, cy, FriendChange::Declined)) && changes.contains(&(cy, ada, FriendChange::Cancelled)));
    assert!(changes.contains(&(ada, bo, FriendChange::Removed)));
    assert_eq!(changes.iter().filter(|c| c.2 == FriendChange::Requested && c.0 == bo && c.1 == ada).count(), 2, "sending again is no change");
    // A new code: the old one stops working.
    let new = server.ok(Method::POST, routes::friends::CODE, None, &a).await["code"].as_str().expect("code").to_string();
    assert_ne!(new, code);
    assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, Some(json!({"code": code})), &d, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.add(&d, json!({"code": new})).await["user"], ada.get());
    // Pages: newest first, with a cursor.
    let first = server.ok(Method::GET, "/v1/friends/requests?limit=1", None, &a).await;
    assert_eq!(first["items"][0]["user"], dee.get());
    assert!(first.get("next_cursor").is_none(), "{first}");
    // Account deletion: every row of the account goes.
    server.state.db().execute(&delete_account(dee)).await.expect("delete Dee");
    assert!(server.users(routes::friends::REQUESTS, &a).await.is_empty());
    assert!(server.users(routes::friends::LIST, &c).await.is_empty(), "Cy's friend Dee is gone");
    server.stop().await;
}

async fn limits_and_concurrency(url: &str) {
    let server = start(url, |f| {
        f.max_pending = 3;
        f.max_friends = 2;
        f.max_blocks = 1;
    })
    .await;
    let mut players = Vec::new();
    for i in 0..9 {
        players.push(server.register(&format!("p{i}@example.com"), &format!("P{i}")).await);
    }
    let (p0, t0) = players[0].clone();
    // Sent requests: 3, then quota.
    for (user, _) in &players[1..4] {
        server.add(&t0, json!({"user": user.get()})).await;
    }
    let body = Some(json!({"user": players[4].0.get()}));
    assert_eq!(server.fails(Method::POST, routes::friends::REQUESTS, body, &t0, StatusCode::FORBIDDEN).await, codes::QUOTA_EXCEEDED);
    // Friends: the third acceptance is over the limit (for the accepter); the other side's limit too.
    server.ok(Method::POST, &format!("/v1/friends/requests/{p0}/accept"), None, &players[1].1).await;
    server.ok(Method::POST, &format!("/v1/friends/requests/{p0}/accept"), None, &players[2].1).await;
    let over = server.fails(Method::POST, &format!("/v1/friends/requests/{p0}/accept"), None, &players[3].1, StatusCode::FORBIDDEN).await;
    assert_eq!(over, codes::QUOTA_EXCEEDED, "p0 has 2 friends");
    assert_eq!(server.users(routes::friends::REQUESTS, &players[3].1).await, [p0.get()], "the request stays");
    // Blocks: 1, then quota (blocking the same again is fine).
    server.ok(Method::PUT, &format!("/v1/friends/blocks/{}", players[5].0), None, &t0).await;
    server.ok(Method::PUT, &format!("/v1/friends/blocks/{}", players[5].0), None, &t0).await;
    let over = server.fails(Method::PUT, &format!("/v1/friends/blocks/{}", players[6].0), None, &t0, StatusCode::FORBIDDEN).await;
    assert_eq!(over, codes::QUOTA_EXCEEDED);
    // Received requests, concurrently: 5 players ask p8 at once, exactly 3 get through.
    let target = players[8].0;
    let mut tasks = Vec::new();
    for (user, _) in &players[3..8] {
        let (state, service, user) = (server.state.clone(), server.service(), *user);
        tasks.push(tokio::spawn(async move {
            let ctx = net_backend_server::HookCtx::new(state.clone(), None);
            service.add(&state, &ctx, user, &net_backend_server::protocol::friends::AddFriend::by_id(target)).await
        }));
    }
    let mut ok = 0;
    for task in tasks {
        match task.await.expect("task") {
            Ok(_) => ok += 1,
            Err(error) => assert!(error.code() == codes::QUOTA_EXCEEDED, "{error:?}"),
        }
    }
    assert_eq!(ok, 3, "max_pending received");
    assert_eq!(server.users(routes::friends::REQUESTS, &players[8].1).await.len(), 3);
    // Two players asking each other at the same moment end as friends, consistently on both sides.
    let (x, y) = (players[6].0, players[7].0);
    let mut tasks = Vec::new();
    for (from, to) in [(x, y), (y, x)] {
        let (state, service) = (server.state.clone(), server.service());
        tasks.push(tokio::spawn(async move {
            let ctx = net_backend_server::HookCtx::new(state.clone(), None);
            service.add(&state, &ctx, from, &net_backend_server::protocol::friends::AddFriend::by_id(to)).await.map(|e| e.state)
        }));
    }
    let mut states = Vec::new();
    for task in tasks {
        states.push(task.await.expect("task").expect("add"));
    }
    states.sort_by_key(|s| *s == FriendState::Friend);
    assert_eq!(states, [FriendState::Sent, FriendState::Friend]);
    assert_eq!(server.service().state_of(&server.state, x, y).await.expect("read"), Some(FriendState::Friend));
    assert_eq!(server.service().state_of(&server.state, y, x).await.expect("read"), Some(FriendState::Friend));
    server.stop().await;
}

async fn online_state_and_pushes(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("ada@example.com", "Ada").await;
    let (bo, b) = server.register("bo@example.com", "Bo").await;
    let (cy, c) = server.register("cy@example.com", "Cy").await;
    for (who, token) in [(bo, &b), (cy, &c)] {
        server.add(token, json!({"user": ada.get()})).await;
        server.ok(Method::POST, &format!("/v1/friends/requests/{who}/accept"), None, &a).await;
    }
    let mut ada_ws = server.connect(ada, &a).await;
    // Bo comes online: Ada gets the push; her list shows him online.
    let bo_ws = server.connect(bo, &b).await;
    let push = next_of(&mut ada_ws, "friends.presence").await;
    assert_eq!((push["data"]["user"].as_i64(), push["data"]["online"].as_bool()), (Some(bo.get()), Some(true)), "{push}");
    let list = server.ok(Method::GET, routes::friends::LIST, None, &a).await;
    let bo_entry = list["items"].as_array().and_then(|items| items.iter().find(|e| e["user"] == bo.get()).cloned()).expect("Bo listed");
    assert_eq!(bo_entry["online"], true, "{bo_entry}");
    // A second connection of Bo is no new push; closing one of two is none either.
    let bo_ws2 = server.connect(bo, &b).await;
    drop(bo_ws2);
    let deadline = std::time::Instant::now() + WAIT;
    while server.state.ws().connections_of(bo).len() != 1 {
        assert!(std::time::Instant::now() < deadline, "the second socket never closed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Bo goes offline: the push with last_seen; the list says offline.
    server.clock.advance(5_000);
    drop(bo_ws);
    let push = next_of(&mut ada_ws, "friends.presence").await;
    assert_eq!(
        (push["data"]["user"].as_i64(), push["data"]["online"].as_bool(), push["data"]["last_seen"].as_i64()),
        (Some(bo.get()), Some(false), Some(T0 + 5_000)),
        "{push}"
    );
    assert!(!server.service().is_online(&server.state, bo).await.expect("read"));
    // Two sockets of Bo closing at the same moment: ONE "offline" push (the next push Ada gets
    // below is Cy's "online", not a second "offline" of Bo).
    let bo_ws3 = server.connect(bo, &b).await;
    let push = next_of(&mut ada_ws, "friends.presence").await;
    assert_eq!((push["data"]["user"].as_i64(), push["data"]["online"].as_bool()), (Some(bo.get()), Some(true)), "{push}");
    let bo_ws4 = server.connect(bo, &b).await;
    drop((bo_ws3, bo_ws4));
    let deadline = std::time::Instant::now() + WAIT;
    while !server.state.ws().connections_of(bo).is_empty() {
        assert!(std::time::Instant::now() < deadline, "Bo's sockets never closed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let push = next_of(&mut ada_ws, "friends.presence").await;
    assert_eq!((push["data"]["user"].as_i64(), push["data"]["online"].as_bool()), (Some(bo.get()), Some(false)), "{push}");
    // Cy has no WebSocket: a heartbeat makes him online (pushed), the window ends it (read, not pushed).
    server.ok(Method::POST, routes::friends::PRESENCE, None, &c).await;
    let push = next_of(&mut ada_ws, "friends.presence").await;
    assert_eq!((push["data"]["user"].as_i64(), push["data"]["online"].as_bool()), (Some(cy.get()), Some(true)), "{push}");
    server.ok(Method::POST, routes::friends::PRESENCE, None, &c).await;
    assert!(server.service().is_online(&server.state, cy).await.expect("read"));
    server.clock.advance(91_000);
    assert!(!server.service().is_online(&server.state, cy).await.expect("read"), "past the online window");
    let list = server.ok(Method::GET, routes::friends::LIST, None, &b).await;
    assert_eq!((list["items"][0]["user"].as_i64(), list["items"][0]["online"].as_bool()), (Some(ada.get()), Some(true)), "Ada's socket: {list}");
    // The refresh moves the stored online time of connected players forward (other instances read it).
    server.clock.advance(200_000);
    server.service().refresh(&server.state).await.expect("refresh");
    let mut select = Query::select();
    select.column("online_until").from("friend_profiles").and_where(Expr::col("user_id").eq(ada.get()));
    #[derive(sqlx::FromRow)]
    struct Until {
        online_until: Option<i64>,
    }
    let row: Until = server.state.db().fetch_one(&select).await.expect("profile");
    assert!(row.online_until.is_some_and(|until| until > server.state.now().get()), "refreshed");
    // A connection that closes before its (spawned) connect hook ran: the late hook leaves the
    // player offline, with no presence row.
    *server.connect_delay.lock().expect("lock") = Some(Duration::from_millis(300));
    let cy_ws = server.connect(cy, &c).await;
    drop(cy_ws);
    let deadline = std::time::Instant::now() + WAIT;
    while !server.state.ws().connections_of(cy).is_empty() {
        assert!(std::time::Instant::now() < deadline, "Cy's socket never closed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    *server.connect_delay.lock().expect("lock") = None;
    assert!(!server.service().is_online(&server.state, cy).await.expect("read"), "online after a late connect hook");
    assert_eq!(held(&server, cy).await, 0, "a presence row after a late connect hook");
    drop(ada_ws);
    server.stop().await;
}

/// A connection that closes between its connect hook's presence decision ("online") and that
/// push's delivery: the disconnect's "offline" waits for the "online", so friends see them in
/// that order (never "offline" first and then a stale "online"). The "online" push is held inside
/// the broadcaster while the socket closes.
async fn presence_push_order(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("order-ada@example.com", "Ada").await;
    let (bo, b) = server.register("order-bo@example.com", "Bo").await;
    server.add(&b, json!({"user": ada.get()})).await;
    server.ok(Method::POST, &format!("/v1/friends/requests/{bo}/accept"), None, &a).await;
    let mut ada_ws = server.connect(ada, &a).await;
    *server.gate.armed.lock().expect("lock") = Some(bo.get());
    let bo_ws = server.connect(bo, &b).await;
    let deadline = std::time::Instant::now() + WAIT;
    while !server.gate.holding.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "the online push never came");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Bo's socket closes while his "online" is on its way.
    drop(bo_ws);
    let deadline = std::time::Instant::now() + WAIT;
    while !server.state.ws().connections_of(bo).is_empty() {
        assert!(std::time::Instant::now() < deadline, "Bo's socket never closed");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Room for the disconnect hook to overtake (it must wait for the online push instead).
    tokio::time::sleep(Duration::from_millis(500)).await;
    let overtaken = server.gate.overtaken.load(std::sync::atomic::Ordering::SeqCst);
    server.gate.release();
    assert_eq!(overtaken, 0, "the offline push went out while the online push was still held");
    let first = next_of(&mut ada_ws, "friends.presence").await;
    let second = next_of(&mut ada_ws, "friends.presence").await;
    let order: Vec<(Option<i64>, Option<bool>)> = [&first, &second].iter().map(|p| (p["data"]["user"].as_i64(), p["data"]["online"].as_bool())).collect();
    assert_eq!(order, vec![(Some(bo.get()), Some(true)), (Some(bo.get()), Some(false))], "{first} {second}");
    assert!(!server.service().is_online(&server.state, bo).await.expect("read"), "stored offline");
    drop(ada_ws);
    server.stop().await;
}

/// The presence work is not cut off by the hook time limit (`server.hook_timeout_ms`): with a
/// limit of 1 ms (a slow database in effect) the "online" and "offline" pushes and the stored
/// state still follow the sockets.
async fn presence_outlives_the_hook_limit(url: &str) {
    let server = start_with(url, |config| config.server.hook_timeout_ms = 1, |_| {}).await;
    let (ada, a) = server.register("limit-ada@example.com", "Ada").await;
    let (bo, b) = server.register("limit-bo@example.com", "Bo").await;
    server.add(&b, json!({"user": ada.get()})).await;
    server.ok(Method::POST, &format!("/v1/friends/requests/{bo}/accept"), None, &a).await;
    let mut ada_ws = server.connect(ada, &a).await;
    let bo_ws = server.connect(bo, &b).await;
    let push = next_of(&mut ada_ws, "friends.presence").await;
    assert_eq!((push["data"]["user"].as_i64(), push["data"]["online"].as_bool()), (Some(bo.get()), Some(true)), "{push}");
    assert!(server.service().is_online(&server.state, bo).await.expect("read"), "stored online");
    drop(bo_ws);
    let push = next_of(&mut ada_ws, "friends.presence").await;
    assert_eq!((push["data"]["user"].as_i64(), push["data"]["online"].as_bool()), (Some(bo.get()), Some(false)), "{push}");
    assert!(!server.service().is_online(&server.state, bo).await.expect("read"), "stored offline");
    drop(ada_ws);
    server.stop().await;
}

/// A server that stops while a player is connected stores that player offline before its
/// database pool closes (a server started again on the same database reads "offline", not the
/// online time the first one stored).
async fn offline_after_shutdown(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("stop-ada@example.com", "Ada").await;
    let (bo, b) = server.register("stop-bo@example.com", "Bo").await;
    server.add(&b, json!({"user": ada.get()})).await;
    server.ok(Method::POST, &format!("/v1/friends/requests/{bo}/accept"), None, &a).await;
    let mut ada_ws = server.connect(ada, &a).await;
    let bo_ws = server.connect(bo, &b).await;
    let push = next_of(&mut ada_ws, "friends.presence").await;
    assert_eq!((push["data"]["user"].as_i64(), push["data"]["online"].as_bool()), (Some(bo.get()), Some(true)), "{push}");
    assert!(server.service().is_online(&server.state, bo).await.expect("read"), "stored online");
    // The sockets' own disconnect hooks come too late for the database (as with a slow hook).
    *server.disconnect_delay.lock().expect("lock") = Some(Duration::from_millis(500));
    server.stop().await;
    drop((ada_ws, bo_ws));
    let again = start(url, |_| {}).await;
    assert!(!again.service().is_online(&again.state, bo).await.expect("read"), "stored offline at shutdown");
    assert_eq!(held(&again, bo).await, 0, "no presence row after a shutdown");
    again.stop().await;
}

/// The `friend_presence` rows of `user` (one per instance holding a connection).
async fn held(server: &Server, user: UserId) -> i64 {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from("friend_presence").and_where(Expr::col("user_id").eq(user.get()));
    #[derive(sqlx::FromRow)]
    struct Count {
        n: i64,
    }
    server.state.db().fetch_one::<Count, _>(&select).await.expect("count").n
}

async fn wait_held(server: &Server, user: UserId, rows: i64) {
    let deadline = std::time::Instant::now() + WAIT;
    while held(server, user).await != rows {
        assert!(std::time::Instant::now() < deadline, "never {rows} presence rows");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// No `friends.presence` frame arrives for a while.
async fn no_presence(ws: &mut Ws) {
    assert!(tokio::time::timeout(Duration::from_millis(600), next_of(ws, "friends.presence")).await.is_err(), "an unexpected friends.presence");
}

/// Two instances on one database: a player connected to both stays online (and unannounced)
/// when its connection on one of them closes; its last connection anywhere makes it offline.
async fn presence_over_two_instances(url: &str) {
    let one = start(url, |_| {}).await;
    let two = start(url, |_| {}).await;
    let (ada, a) = one.register("ada@example.com", "Ada").await;
    let (bo, b) = one.register("bo@example.com", "Bo").await;
    one.add(&b, json!({"user": ada.get()})).await;
    one.ok(Method::POST, &format!("/v1/friends/requests/{bo}/accept"), None, &a).await;
    let mut ada_one = one.connect(ada, &a).await;
    let mut ada_two = two.connect(ada, &a).await;
    wait_held(&one, ada, 2).await;

    // Bo's first connection (instance one) is announced; his second (instance two) is not.
    let bo_one = one.connect(bo, &b).await;
    let push = next_of(&mut ada_one, "friends.presence").await;
    assert_eq!((push["data"]["user"].as_i64(), push["data"]["online"].as_bool()), (Some(bo.get()), Some(true)), "{push}");
    wait_held(&one, bo, 1).await;
    let bo_two = two.connect(bo, &b).await;
    wait_held(&one, bo, 2).await;
    no_presence(&mut ada_two).await;

    // Bo's connection on instance two closes: instance one still holds him, so he stays online.
    // (Each instance has its own test clock: they move together.)
    for server in [&one, &two] {
        server.clock.advance(1_000);
    }
    drop(bo_two);
    wait_held(&one, bo, 1).await;
    no_presence(&mut ada_two).await;
    assert!(two.service().is_online(&two.state, bo).await.expect("read"), "online through instance one");
    let list = two.ok(Method::GET, routes::friends::LIST, None, &a).await;
    assert_eq!(list["items"][0]["online"], true, "{list}");

    // His last connection anywhere closes: offline, announced.
    for server in [&one, &two] {
        server.clock.advance(2_000);
    }
    drop(bo_one);
    let push = next_of(&mut ada_one, "friends.presence").await;
    assert_eq!((push["data"]["user"].as_i64(), push["data"]["online"].as_bool()), (Some(bo.get()), Some(false)), "{push}");
    wait_held(&one, bo, 0).await;
    assert!(!two.service().is_online(&two.state, bo).await.expect("read"));

    // A row left by an instance that stopped without removing it stops counting after its time
    // and is purged by the refresh.
    let mut stale = Query::insert();
    stale.into_table("friend_presence").columns(["user_id", "instance_id", "online_until"]).values_panic([bo.get().into(), 1i64.into(), (T0 + 10_000).into()]);
    one.state.db().execute(&stale).await.expect("stale row");
    one.clock.advance(400_000);
    one.service().refresh(&one.state).await.expect("refresh");
    assert_eq!(held(&one, bo).await, 0, "purged");
    drop((ada_one, ada_two));
    two.stop().await;
    one.stop().await;
}

/// A fresh database for one part of the suite. `backend`: `memory`, `file`, or a MySQL /
/// PostgreSQL base URL.
async fn database(backend: &str) -> (String, Option<(String, String)>) {
    match backend {
        "memory" => ("sqlite::memory:".into(), None),
        "file" => {
            let dir = common::temp_dir("friends-file");
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
    each!(backend, requests_and_blocks, limits_and_concurrency, online_state_and_pushes, presence_outlives_the_hook_limit);
    // Two server instances need a database both can open. The push order test needs a pool with
    // more than the one connection of `sqlite::memory:` (the held push blocks a worker thread,
    // which there can hold up the pool's only connection, and the race cannot happen).
    if backend != "memory" {
        each!(backend, presence_over_two_instances, presence_push_order, offline_after_shutdown);
    }
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
async fn mysql_friends_suite() {
    suite(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_friends_suite() {
    suite(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}

/// The module's wiring: needs `auth` first, reads `[modules.friends]`, refuses settings given twice
/// or unknown keys, lists its routes in the OpenAPI document and its push in the AsyncAPI
/// document, its name in `/v1/info`; works with the WebSocket hub off (the heartbeat included);
/// the request rate.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn wiring_documents_and_rate() {
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(Friends::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");
    let file = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.friends]\nmax_friends = 7\n").expect("config");
    let ok = NetBackendServer::new(file.clone()).module(Auth::new()).module(Friends::new()).build().await.expect("from the file");
    assert_eq!(ok.state().get::<FriendService>().expect("service").config().max_friends, 7);
    let error = NetBackendServer::new(file).module(Auth::new()).module(Friends::new().with_config(FriendsConfig::default())).build().await.err();
    assert!(error.map(|e| e.to_string()).unwrap_or_default().contains("both in code"));
    let bad = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.friends]\nmax_friend = 7\n").expect("config");
    assert!(NetBackendServer::new(bad).module(Auth::new()).module(Friends::new()).build().await.is_err(), "unknown keys are refused");

    let prepared = NetBackendServer::new(config()).module(Auth::new()).module(Friends::new()).build().await.expect("build");
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for route in routes::ALL.iter().filter(|r| r.path.starts_with("/v1/friends")) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
    let asyncapi: Value = serde_json::from_str(prepared.asyncapi_json()).expect("json");
    assert!(asyncapi["components"]["messages"].get("push.friends.presence").is_some());
    let (_, _, info) = common::call(&prepared.router(), common::get(routes::INFO)).await;
    assert_eq!(info["modules"], json!(["auth", "friends"]));

    // The hub off: requests, the heartbeat and the online state over HTTP; the rate (2 per minute).
    let mut no_ws = config();
    no_ws.ws.enabled = false;
    let mut friends = FriendsConfig::default();
    friends.request_rate = 2;
    let prepared = NetBackendServer::new(no_ws).module(Auth::new()).module(Friends::new().with_config(friends)).build().await.expect("build without ws");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let mut tokens = Vec::new();
    for i in 0..4 {
        let request = Request::post(routes::auth::REGISTER)
            .header("content-type", "application/json")
            .body(Body::from(json!({"email": format!("solo{i}@example.com"), "password": PASSWORD}).to_string()))
            .expect("request");
        let (_, _, session) = common::call(&router, request).await;
        tokens.push((UserId(session["account"]["id"].as_i64().expect("id")), session["tokens"]["access_token"].as_str().expect("token").to_string()));
    }
    let send = |token: &str, body: Value| {
        Request::post(routes::friends::REQUESTS)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    };
    for other in [1, 2] {
        let (status, _, body) = common::call(&router, send(&tokens[0].1, json!({"user": tokens[other].0.get()}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, _, body) = common::call(&router, send(&tokens[0].1, json!({"user": tokens[3].0.get()}))).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(codes::RATE_LIMITED)), "{body}");
    let accept = Request::post(format!("/v1/friends/requests/{}/accept", tokens[0].0))
        .header("authorization", format!("Bearer {}", tokens[1].1))
        .body(Body::empty())
        .expect("request");
    assert_eq!(common::call(&router, accept).await.0, StatusCode::OK);
    let heartbeat = Request::post(routes::friends::PRESENCE).header("authorization", format!("Bearer {}", tokens[1].1)).body(Body::empty()).expect("request");
    assert_eq!(common::call(&router, heartbeat).await.0, StatusCode::OK);
    let list = Request::get(routes::friends::LIST).header("authorization", format!("Bearer {}", tokens[0].1)).body(Body::empty()).expect("request");
    let (_, _, page) = common::call(&router, list).await;
    assert_eq!((page["items"][0]["user"].as_i64(), page["items"][0]["online"].as_bool()), (Some(tokens[1].0.get()), Some(true)), "{page}");
    assert!(page["items"][0]["last_seen"].as_i64().is_some(), "{page}");

    // The update rate: heartbeats, code resets and settings changes share one bucket (3 here).
    let mut friends = FriendsConfig::default();
    friends.update_rate = 3;
    let mut no_ws = config();
    no_ws.ws.enabled = false;
    let prepared = NetBackendServer::new(no_ws).module(Auth::new()).module(Friends::new().with_config(friends)).build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let request = Request::post(routes::auth::REGISTER)
        .header("content-type", "application/json")
        .body(Body::from(json!({"email": "busy@example.com", "password": PASSWORD}).to_string()))
        .expect("request");
    let (_, _, session) = common::call(&router, request).await;
    let token = session["tokens"]["access_token"].as_str().expect("token").to_string();
    let call = |method: Method, path: &str, body: Option<Value>| {
        let request = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"));
        match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request")
    };
    assert_eq!(common::call(&router, call(Method::POST, routes::friends::PRESENCE, None)).await.0, StatusCode::OK);
    assert_eq!(common::call(&router, call(Method::POST, routes::friends::CODE, None)).await.0, StatusCode::OK);
    let (status, _, body) = common::call(&router, call(Method::PUT, routes::friends::SETTINGS, Some(json!({"steam_findable": false})))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = common::call(&router, call(Method::POST, routes::friends::PRESENCE, None)).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(codes::RATE_LIMITED)), "{body}");
    assert_eq!(common::call(&router, call(Method::GET, routes::friends::CODE, None)).await.0, StatusCode::OK, "reads are not counted");
}
