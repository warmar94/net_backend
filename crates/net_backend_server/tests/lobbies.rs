//! The lobbies module: create (rules, metadata, the hook, the join code in both forms), join by id
//! and by code (visibility, a full lobby, a lobby not open, the per-player limit, the hook, a new
//! code), ready flags, the host's changes (metadata, size, state, closing), kicks, handing over,
//! leaving (the host passes on, the last member removes the lobby), the search by metadata and the
//! friends' lobbies, the chat room, the pushes on a loopback server with WebSocket clients, the
//! disconnect rule, the purge, concurrent joins, and the wiring (rates included). The 0.2.0 review
//! fixes: an oversized metadata change refused before anything, the change rate, strangers get 404,
//! the hook after the rights check, the chat room deleted with its lobby (and orphans by the purge).
//!
//! The same suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "lobbies", feature = "chat", feature = "friends", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use futures_util::StreamExt;
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::chat::Chat;
use net_backend_server::friends::Friends;
use net_backend_server::hooks::Decision;
use net_backend_server::lobbies::events::{AfterLobbyChange, BeforeLobbyCreate, BeforeLobbyJoin, BeforeLobbyUpdate, LobbyEvent};
use net_backend_server::lobbies::{Lobbies, LobbiesConfig, LobbyActor, LobbyService};
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::lobbies::LobbyCode;
use net_backend_server::protocol::{codes, routes, LobbyId, UnixMillis, UserId};
use net_backend_server::sea_query::{Expr, ExprTrait, Query};
use net_backend_server::{AppError, AppState, Config, Error, HookCtx, ManualClock, NetBackendServer, SecretString};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Events = Arc<Mutex<Vec<(LobbyId, Option<UserId>, Option<UserId>, LobbyEvent)>>>;

const T0: i64 = 1_800_000_000_000;
const PASSWORD: &str = "correct horse battery";
const WAIT: Duration = Duration::from_secs(30);

struct Server {
    addr: SocketAddr,
    state: AppState,
    router: Router,
    events: Events,
    refused: Arc<Mutex<Option<UserId>>>,
    clock: Arc<ManualClock>,
    /// How many `BeforeLobbyUpdate` hooks ran.
    updates: Arc<AtomicUsize>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), Error>>>,
}

async fn start(url: &str, tweak: impl FnOnce(&mut LobbiesConfig)) -> Server {
    common::watchdog(Duration::from_secs(1200));
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("lobbies-migrations");
    config.server.shutdown_grace_secs = 5;
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    auth.access_token_ttl_secs = 7 * 24 * 3600;
    let mut lobbies = LobbiesConfig::default();
    lobbies.create_rate = 0;
    lobbies.join_rate = 0;
    lobbies.bad_code_rate = 0;
    lobbies.purge_interval_secs = 0;
    lobbies.disconnect_grace_secs = 0;
    tweak(&mut lobbies);
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let refused: Arc<Mutex<Option<UserId>>> = Arc::new(Mutex::new(None));
    let (seen, refuse) = (events.clone(), refused.clone());
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let updates = Arc::new(AtomicUsize::new(0));
    let counted = updates.clone();
    let prepared = NetBackendServer::new(config)
        .clock(clock.clone())
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Chat::new())
        .module(Friends::new())
        .module(Lobbies::new().with_config(lobbies))
        // The game's rule: no lobby without a mode.
        .before::<BeforeLobbyCreate, _, _>(|_ctx, create| async move {
            if create.request.metadata.get("mode").is_some_and(|m| m == "forbidden") {
                return Ok(Decision::Reject(AppError::forbidden("this mode is not allowed")));
            }
            Ok(Decision::Continue(create))
        })
        .before::<BeforeLobbyJoin, _, _>(move |_ctx, join| {
            let refuse = refuse.clone();
            async move {
                if *refuse.lock().expect("lock") == Some(join.user) {
                    return Ok(Decision::Reject(AppError::forbidden("banned from lobbies")));
                }
                Ok(Decision::Continue(join))
            }
        })
        .before::<BeforeLobbyUpdate, _, _>(move |_ctx, update| {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(Decision::Continue(update))
            }
        })
        .after::<AfterLobbyChange, _, _>(move |_ctx, event| {
            let seen = seen.clone();
            async move {
                seen.lock().expect("lock").push((event.lobby, event.actor, event.user, event.event.clone()));
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
    Server { addr, state, router, events, refused, clock, updates, stop: Some(stop), task: Some(task) }
}

impl Server {
    fn service(&self) -> Arc<LobbyService> {
        self.state.get::<LobbyService>().expect("service")
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
        let (status, answer) = self.http(method.clone(), path, body, token).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {answer}");
        answer
    }

    /// The error code of a request that must fail with `status`.
    async fn fails(&self, method: Method, path: &str, body: Option<Value>, token: &str, status: StatusCode) -> String {
        let (got, answer) = self.http(method.clone(), path, body, token).await;
        assert_eq!(got, status, "{method} {path}: {answer}");
        answer["error"]["code"].as_str().unwrap_or_default().to_string()
    }

    async fn create(&self, token: &str, body: Value) -> Value {
        self.ok(Method::POST, routes::lobbies::LIST, Some(body), token).await
    }

    /// Whether the player may read the chat room.
    async fn in_chat(&self, room: i64, token: &str) -> bool {
        let (status, body) = self.http(Method::GET, &format!("/v1/chat/rooms/{room}/messages"), None, token).await;
        match status {
            StatusCode::OK => true,
            StatusCode::FORBIDDEN => {
                assert_eq!(body["error"]["code"], codes::NOT_A_MEMBER, "{body}");
                false
            }
            other => panic!("chat history: {other} {body}"),
        }
    }

    async fn befriend(&self, a: (UserId, &str), b: (UserId, &str)) {
        self.ok(Method::POST, routes::friends::REQUESTS, Some(json!({"user": b.0.get()})), a.1).await;
        self.ok(Method::POST, &format!("/v1/friends/requests/{}/accept", a.0), None, b.1).await;
    }

    fn events(&self) -> Vec<(LobbyId, Option<UserId>, Option<UserId>, LobbyEvent)> {
        std::mem::take(&mut *self.events.lock().expect("lock"))
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

/// The next frame of a kind (others are skipped).
async fn next_of(ws: &mut Ws, kind: &str) -> Value {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await.expect("a frame in time") {
            Some(Ok(Message::Text(text))) => {
                let frame: Value = serde_json::from_str(text.as_str()).expect("JSON");
                if frame["type"] == kind {
                    return frame["data"].clone();
                }
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// The next `lobby.changed` whose changes are exactly `[change]` (earlier ones are skipped).
async fn next_change(ws: &mut Ws, change: &str) -> Value {
    loop {
        let push = next_of(ws, "lobby.changed").await;
        if push["changes"] == json!([change]) {
            return push;
        }
    }
}

/// Whether the chat room's row exists.
async fn room_exists(server: &Server, room: i64) -> bool {
    #[derive(sqlx::FromRow)]
    struct N {
        n: i64,
    }
    let query = Query::select().expr_as(Expr::col("id").count(), "n").from("chat_rooms").and_where(Expr::col("id").eq(room)).to_owned();
    server.state.db().fetch_one::<N, _>(&query).await.expect("count").n == 1
}

fn delete_account(user: UserId) -> net_backend_server::sea_query::DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table("auth_users").and_where(Expr::col("id").eq(user.get()));
    delete
}

fn players(lobby: &Value) -> Vec<i64> {
    lobby["players"].as_array().map(|p| p.iter().filter_map(|m| m["user"].as_i64()).collect()).unwrap_or_default()
}

fn ids(page: &Value) -> Vec<i64> {
    page["items"].as_array().map(|items| items.iter().filter_map(|l| l["id"].as_i64()).collect()).unwrap_or_default()
}

/// Create, read, join by id and by code, visibility, the join code, the limits and the hooks.
async fn create_and_join(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("ada@example.com", "Ada").await;
    let (bo, b) = server.register("bo@example.com", "Bo").await;
    let (cy, c) = server.register("cy@example.com", "Cy").await;
    let (dee, d) = server.register("dee@example.com", "Dee").await;

    // Rules: size, visibility, metadata, the server's largest lobby, the hook.
    for bad in [
        json!({"max_players": 0}),
        json!({"max_players": 65}),
        json!({"max_players": 2, "visibility": "secret"}),
        json!({"max_players": 2, "metadata": {"bad key": "x"}}),
    ] {
        assert_eq!(server.fails(Method::POST, routes::lobbies::LIST, Some(bad), &a, StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    }
    let forbidden = json!({"max_players": 2, "metadata": {"mode": "forbidden"}});
    assert_eq!(server.fails(Method::POST, routes::lobbies::LIST, Some(forbidden), &a, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);

    let lobby = server.create(&a, json!({"max_players": 3, "metadata": {"mode": "ranked", "map": "dust"}})).await;
    let id = lobby["id"].as_i64().expect("id");
    assert_eq!(
        (lobby["visibility"].as_str(), lobby["state"].as_str(), lobby["host"].as_i64(), lobby["members"].as_i64()),
        (Some("public"), Some("open"), Some(ada.get()), Some(1))
    );
    assert_eq!(lobby["metadata"], json!({"map": "dust", "mode": "ranked"}));
    let code = LobbyCode::parse(lobby["code"].as_str().expect("code")).expect("a valid code");
    assert_eq!(lobby["code_number"].as_u64(), Some(code.to_u64()), "both forms");
    assert!(code.to_u64() > 0 && code.to_u64() < 1 << 40);
    assert!(lobby["chat_room"].is_i64(), "a chat room with the chat module");
    assert_eq!(players(&lobby), [ada.get()]);
    assert_eq!(lobby["players"][0]["name"], "Ada");
    assert_eq!(server.events().last().map(|e| e.3.clone()), Some(LobbyEvent::Created));

    // A stranger sees a public lobby without its code and room.
    let seen = server.ok(Method::GET, &format!("/v1/lobbies/{id}"), None, &b).await;
    assert!(seen.get("code").is_none() && seen.get("code_number").is_none() && seen.get("chat_room").is_none(), "{seen}");
    assert_eq!(players(&seen), [ada.get()]);

    // By id; again answers the lobby; by code (typed loosely); the chat room follows.
    let joined = server.ok(Method::POST, &format!("/v1/lobbies/{id}/join"), None, &b).await;
    assert_eq!((players(&joined), joined["code"].as_str()), (vec![ada.get(), bo.get()], Some(code.as_str())));
    assert_eq!(server.ok(Method::POST, &format!("/v1/lobbies/{id}/join"), None, &b).await["members"], 2, "a member already");
    let typed = code.grouped().to_lowercase();
    let joined = server.ok(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": typed})), &c).await;
    assert_eq!(players(&joined), [ada.get(), bo.get(), cy.get()]);
    let room = joined["chat_room"].as_i64().expect("room");
    assert!(server.in_chat(room, &b).await && server.in_chat(room, &c).await && !server.in_chat(room, &d).await);
    // Full (3 players), the per-player limit (1 lobby), no such code, not a code.
    let full = server.fails(Method::POST, &format!("/v1/lobbies/{id}/join"), None, &d, StatusCode::FORBIDDEN).await;
    assert_eq!(full, codes::QUOTA_EXCEEDED);
    assert_eq!(server.fails(Method::POST, routes::lobbies::LIST, Some(json!({"max_players": 2})), &b, StatusCode::FORBIDDEN).await, codes::QUOTA_EXCEEDED);
    let other = LobbyCode::from_u64(code.to_u64() ^ 1).expect("code");
    assert_eq!(
        server.fails(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": other.as_str()})), &d, StatusCode::NOT_FOUND).await,
        codes::NOT_FOUND
    );
    for bad in ["K7M2", "K7M2Q9X0"] {
        assert_eq!(
            server.fails(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": bad})), &d, StatusCode::UNPROCESSABLE_ENTITY).await,
            codes::VALIDATION_FAILED
        );
    }
    assert_eq!(server.fails(Method::POST, "/v1/lobbies/999999/join", None, &d, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::POST, "/v1/lobbies/x/join", None, &d, StatusCode::BAD_REQUEST).await, codes::BAD_REQUEST);
    assert_eq!(server.http(Method::GET, routes::lobbies::MINE, None, "nope").await.0, StatusCode::UNAUTHORIZED);

    // Private: not seen or joined by id, joined by code; the hook refuses; a new code.
    server.ok(Method::POST, &format!("/v1/lobbies/{id}/leave"), None, &c).await;
    let private = server.create(&c, json!({"max_players": 4, "visibility": "private"})).await;
    let pid = private["id"].as_i64().expect("id");
    assert_eq!(server.fails(Method::GET, &format!("/v1/lobbies/{pid}"), None, &d, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::POST, &format!("/v1/lobbies/{pid}/join"), None, &d, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    *server.refused.lock().expect("lock") = Some(dee);
    let old = private["code"].as_str().expect("code").to_string();
    assert_eq!(server.fails(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": old})), &d, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    *server.refused.lock().expect("lock") = None;
    assert_eq!(server.fails(Method::POST, &format!("/v1/lobbies/{pid}/code"), None, &d, StatusCode::NOT_FOUND).await, codes::NOT_FOUND, "a stranger");
    let renewed = server.ok(Method::POST, &format!("/v1/lobbies/{pid}/code"), None, &c).await;
    let new = renewed["code"].as_str().expect("code").to_string();
    assert_ne!(new, old);
    assert_eq!(
        server.fails(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": old})), &d, StatusCode::NOT_FOUND).await,
        codes::NOT_FOUND,
        "the old code stops working"
    );
    let joined = server.ok(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": new})), &d).await;
    assert_eq!(players(&joined), [cy.get(), dee.get()]);
    assert_eq!(server.ok(Method::GET, &format!("/v1/lobbies/{pid}"), None, &d).await["code"], new.as_str(), "members see the code");
    // Not open: no one joins.
    server.ok(Method::POST, &format!("/v1/lobbies/{pid}/leave"), None, &d).await;
    server.ok(Method::PATCH, &format!("/v1/lobbies/{pid}"), Some(json!({"state": "in_game"})), &c).await;
    assert_eq!(server.fails(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": new})), &d, StatusCode::CONFLICT).await, codes::CONFLICT);
    // Mine.
    let mine = server.ok(Method::GET, routes::lobbies::MINE, None, &a).await;
    assert_eq!((mine["lobbies"][0]["id"].as_i64(), players(&mine["lobbies"][0])), (Some(id), vec![ada.get(), bo.get()]));
    assert_eq!(server.ok(Method::GET, routes::lobbies::MINE, None, &d).await["lobbies"], json!([]));
    server.stop().await;
}

/// Ready flags, the host's changes, kicks, handing over, leaving and closing, with the pushes.
async fn host_actions_and_pushes(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("ada@example.com", "Ada").await;
    let (bo, b) = server.register("bo@example.com", "Bo").await;
    let (cy, c) = server.register("cy@example.com", "Cy").await;
    let mut ws_a = server.connect(ada, &a).await;
    let mut ws_b = server.connect(bo, &b).await;
    let mut ws_c = server.connect(cy, &c).await;
    let lobby = server.create(&a, json!({"max_players": 4})).await;
    let id = lobby["id"].as_i64().expect("id");
    let at = |tail: &str| format!("/v1/lobbies/{id}{tail}");
    server.ok(Method::POST, &at("/join"), None, &b).await;
    let push = next_of(&mut ws_a, "lobby.member").await;
    assert_eq!((push["lobby"].as_i64(), push["change"].as_str(), push["member"]["user"].as_i64()), (Some(id), Some("joined"), Some(bo.get())));
    assert_eq!(next_of(&mut ws_b, "lobby.member").await["member"]["name"], "Bo", "the new member too");
    server.ok(Method::POST, &at("/join"), None, &c).await;
    next_of(&mut ws_a, "lobby.member").await;
    next_of(&mut ws_b, "lobby.member").await;
    next_of(&mut ws_c, "lobby.member").await;
    server.events();

    // Ready: pushed to everyone; again changes nothing; a stranger is no member.
    server.ok(Method::PUT, &at("/ready"), Some(json!({"ready": true})), &b).await;
    for ws in [&mut ws_a, &mut ws_c] {
        let push = next_of(ws, "lobby.member").await;
        assert_eq!((push["change"].as_str(), push["member"]["user"].as_i64(), push["member"]["ready"].as_bool()), (Some("ready"), Some(bo.get()), Some(true)));
    }
    server.ok(Method::PUT, &at("/ready"), Some(json!({"ready": true})), &b).await;
    assert_eq!(server.events(), [(LobbyId(id), Some(bo), Some(bo), LobbyEvent::Ready(true))], "one change");
    let (_dee, d) = server.register("dee@example.com", "Dee").await;
    assert_eq!(server.fails(Method::PUT, &at("/ready"), Some(json!({"ready": true})), &d, StatusCode::FORBIDDEN).await, codes::NOT_A_MEMBER);

    // The host's changes: only the host; metadata set and removed; size not below the members; limits.
    let change = json!({"metadata": {"mode": "ranked"}});
    assert_eq!(server.fails(Method::PATCH, &at(""), Some(change.clone()), &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    let changed = server.ok(Method::PATCH, &at(""), Some(json!({"metadata": {"mode": "ranked", "map": "dust"}, "max_players": 5})), &a).await;
    assert_eq!((changed["metadata"].clone(), changed["max_players"].as_i64()), (json!({"map": "dust", "mode": "ranked"}), Some(5)));
    let push = next_of(&mut ws_b, "lobby.changed").await;
    assert_eq!(push["changes"], json!(["settings", "metadata"]));
    assert_eq!((push["lobby"]["metadata"]["map"].as_str(), push["lobby"].get("players")), (Some("dust"), None));
    let changed = server.ok(Method::PATCH, &at(""), Some(json!({"metadata": {"map": null, "nothing": null}})), &a).await;
    assert_eq!(changed["metadata"], json!({"mode": "ranked"}));
    assert_eq!(next_of(&mut ws_c, "lobby.changed").await["changes"], json!(["settings", "metadata"]), "the earlier push first");
    assert_eq!(next_of(&mut ws_c, "lobby.changed").await["changes"], json!(["metadata"]));
    let too_small = server.fails(Method::PATCH, &at(""), Some(json!({"max_players": 2})), &a, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(too_small, codes::VALIDATION_FAILED);
    let many: serde_json::Map<String, Value> = (0..33).map(|n| (format!("k{n}"), json!("v"))).collect();
    let over = server.fails(Method::PATCH, &at(""), Some(json!({"metadata": many})), &a, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(over, codes::VALIDATION_FAILED);
    assert_eq!(server.ok(Method::GET, &at(""), None, &a).await["metadata"], json!({"mode": "ranked"}), "a refused change changes nothing");
    let friends_only = server.http(Method::PATCH, &at(""), Some(json!({"visibility": "friends"})), &a).await;
    assert_eq!(friends_only.0, StatusCode::OK, "friends module registered: {}", friends_only.1);

    // In game and back: every ready flag resets.
    server.ok(Method::PATCH, &at(""), Some(json!({"state": "in_game"})), &a).await;
    let back = server.ok(Method::PATCH, &at(""), Some(json!({"state": "open"})), &a).await;
    assert!(back["players"].as_array().expect("players").iter().all(|m| m["ready"] == false), "{back}");

    // Kick: only the host, not oneself; the kicked player gets the push and leaves the chat room.
    let room = back["chat_room"].as_i64().expect("room");
    assert_eq!(server.fails(Method::DELETE, &at(&format!("/members/{}", ada.get())), None, &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    let own = server.fails(Method::DELETE, &at(&format!("/members/{}", ada.get())), None, &a, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(own, codes::VALIDATION_FAILED);
    server.events();
    server.ok(Method::DELETE, &at(&format!("/members/{}", cy.get())), None, &a).await;
    let push = next_of(&mut ws_c, "lobby.member").await;
    assert_eq!((push["change"].as_str(), push["member"]["user"].as_i64()), (Some("kicked"), Some(cy.get())));
    assert!(!server.in_chat(room, &c).await && server.in_chat(room, &b).await);
    assert_eq!(server.events(), [(LobbyId(id), Some(ada), Some(cy), LobbyEvent::Kicked)]);
    server.ok(Method::DELETE, &at(&format!("/members/{}", cy.get())), None, &a).await;
    assert!(server.events().is_empty(), "kicking a non-member changes nothing");

    // Handing over: to members only; then the host's rights move.
    assert_eq!(server.fails(Method::POST, &at("/host"), Some(json!({"user": cy.get()})), &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    server.ok(Method::POST, &at("/host"), Some(json!({"user": bo.get()})), &a).await;
    let push = next_change(&mut ws_a, "host").await;
    assert_eq!(push["lobby"]["host"].as_i64(), Some(bo.get()));
    assert_eq!(server.fails(Method::PATCH, &at(""), Some(json!({"max_players": 3})), &a, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);

    // A leaving host passes the lobby to the member who joined first; the last one removes it.
    server.ok(Method::POST, &at("/leave"), None, &b).await;
    assert_eq!(server.ok(Method::GET, &at(""), None, &a).await["host"].as_i64(), Some(ada.get()));
    server.ok(Method::POST, &at("/leave"), None, &b).await;
    server.events();
    server.ok(Method::POST, &at("/leave"), None, &a).await;
    assert_eq!(server.events().last().map(|e| e.3.clone()), Some(LobbyEvent::Closed));
    assert_eq!(server.fails(Method::GET, &at(""), None, &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::POST, &at("/leave"), None, &a, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);

    // Closing: every member gets a last lobby.changed with state closed; the code stops working.
    let lobby = server.create(&a, json!({"max_players": 4, "visibility": "private"})).await;
    let id = lobby["id"].as_i64().expect("id");
    server.ok(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": lobby["code"]})), &b).await;
    let closed = server.ok(Method::PATCH, &format!("/v1/lobbies/{id}"), Some(json!({"state": "closed"})), &a).await;
    assert_eq!(closed["state"], "closed");
    let push = loop {
        let push = next_change(&mut ws_b, "state").await;
        if push["lobby"]["state"] == "closed" {
            break push;
        }
    };
    assert_eq!(push["lobby"]["id"].as_i64(), Some(id));
    assert_eq!(server.fails(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": lobby["code"]})), &c, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.ok(Method::GET, routes::lobbies::MINE, None, &b).await["lobbies"], json!([]));

    // Server code: a host-only action as the server, and adding a player.
    let lobby = server.create(&a, json!({"max_players": 4, "visibility": "private"})).await;
    let id = LobbyId(lobby["id"].as_i64().expect("id"));
    let ctx = HookCtx::new(server.state.clone(), None);
    let info = server.service().add_player(&server.state, &ctx, id, cy).await.expect("added");
    assert_eq!(info.players.len(), 2);
    assert!(server.service().kick(&server.state, &ctx, LobbyActor::Player(bo), id, cy).await.is_err_and(|e| e.code() == codes::NOT_FOUND), "a stranger");
    assert!(server.service().kick(&server.state, &ctx, LobbyActor::Server, id, cy).await.expect("kick"));
    assert_eq!(server.service().members_of(&server.state, id).await.expect("members"), [ada]);
    server.service().close(&server.state, &ctx, LobbyActor::Server, id).await.expect("close");
    assert!(server.service().lobby(&server.state, id).await.expect("read").is_none());

    let _ = ws_a.close(None).await;
    let _ = ws_b.close(None).await;
    let _ = ws_c.close(None).await;
    server.stop().await;
}

/// The search by metadata and its pages; friends-only lobbies and blocks.
async fn search_and_friends(url: &str) {
    let server = start(url, |config| config.max_lobbies_per_user = 10).await;
    let (ada, a) = server.register("ada@example.com", "Ada").await;
    let (bo, b) = server.register("bo@example.com", "Bo").await;
    let (cy, c) = server.register("cy@example.com", "Cy").await;
    let mut made = Vec::new();
    for n in 0..5 {
        let mode = if n % 2 == 0 { "ranked" } else { "casual" };
        let lobby = server.create(&a, json!({"max_players": 2, "metadata": {"mode": mode, "map": format!("m{n}")}})).await;
        made.push(lobby["id"].as_i64().expect("id"));
    }
    let private = server.create(&a, json!({"max_players": 2, "visibility": "private", "metadata": {"mode": "ranked"}})).await["id"].as_i64().expect("id");
    let search = |body: Value| server.ok(Method::POST, routes::lobbies::SEARCH, Some(body), &c);
    // Newest first, public only, without codes.
    let all = search(json!({})).await;
    assert_eq!(ids(&all), made.iter().rev().copied().collect::<Vec<_>>());
    assert!(!ids(&all).contains(&private));
    assert!(all["items"][0].get("code").is_none() && all["items"][0].get("players").is_none() && all["items"][0]["members"] == 1);
    assert_eq!(ids(&search(json!({"filters": [{"key": "mode", "value": "ranked"}]})).await), [made[4], made[2], made[0]]);
    let both = json!({"filters": [{"key": "mode", "value": "ranked"}, {"key": "map", "value": "m2"}]});
    assert_eq!(ids(&search(both).await), [made[2]]);
    assert!(ids(&search(json!({"filters": [{"key": "mode", "value": "RANKED"}]})).await).is_empty(), "values match exactly");
    // Pages.
    let first = search(json!({"limit": 2})).await;
    assert_eq!(ids(&first), [made[4], made[3]]);
    let second = search(json!({"limit": 2, "cursor": first["next_cursor"]})).await;
    assert_eq!(ids(&second), [made[2], made[1]]);
    let third = search(json!({"limit": 2, "cursor": second["next_cursor"]})).await;
    assert_eq!((ids(&third), third.get("next_cursor").is_none()), (vec![made[0]], true));
    assert_eq!(server.fails(Method::POST, routes::lobbies::SEARCH, Some(json!({"cursor": "x"})), &c, StatusCode::BAD_REQUEST).await, codes::BAD_REQUEST);
    let nine: Vec<Value> = (0..9).map(|n| json!({"key": format!("k{n}"), "value": "v"})).collect();
    assert_eq!(
        server.fails(Method::POST, routes::lobbies::SEARCH, Some(json!({"filters": nine})), &c, StatusCode::UNPROCESSABLE_ENTITY).await,
        codes::VALIDATION_FAILED
    );
    // Full lobbies are left out unless asked; lobbies not open are never listed.
    server.ok(Method::POST, &format!("/v1/lobbies/{}/join", made[4]), None, &b).await;
    assert!(!ids(&search(json!({})).await).contains(&made[4]));
    assert!(ids(&search(json!({"include_full": true})).await).contains(&made[4]));
    server.ok(Method::PATCH, &format!("/v1/lobbies/{}", made[3]), Some(json!({"state": "in_game"})), &a).await;
    assert!(!ids(&search(json!({"include_full": true})).await).contains(&made[3]));

    // Friends-only: seen and joined by id by a friend of the host only; the friends' search.
    let friends = server.create(&a, json!({"max_players": 4, "visibility": "friends"})).await["id"].as_i64().expect("id");
    assert_eq!(server.fails(Method::GET, &format!("/v1/lobbies/{friends}"), None, &c, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::POST, &format!("/v1/lobbies/{friends}/join"), None, &c, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert!(!ids(&search(json!({})).await).contains(&friends));
    assert!(ids(&search(json!({"friends": true})).await).is_empty(), "no friends: nothing");
    server.befriend((ada, &a), (cy, &c)).await;
    let mine = search(json!({"friends": true})).await;
    assert!(ids(&mine).contains(&friends) && ids(&mine).contains(&made[2]) && !ids(&mine).contains(&private), "{mine}");
    server.ok(Method::POST, &format!("/v1/lobbies/{friends}/join"), None, &c).await;
    // A player the host blocked is refused, by id and by code.
    let code = server.ok(Method::GET, &format!("/v1/lobbies/{private}"), None, &a).await["code"].clone();
    server.ok(Method::PUT, &format!("/v1/friends/blocks/{}", bo.get()), None, &a).await;
    assert_eq!(server.fails(Method::POST, routes::lobbies::JOIN_CODE, Some(json!({"code": code})), &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(server.fails(Method::POST, &format!("/v1/lobbies/{}/join", made[2]), None, &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    server.stop().await;
}

/// Concurrent joins into the last places; the disconnect rule; the purge; account deletion.
async fn limits_and_upkeep(url: &str) {
    let server = start(url, |_| {}).await;
    let (ada, a) = server.register("ada@example.com", "Ada").await;
    let mut players = Vec::new();
    for n in 0..8 {
        players.push(server.register(&format!("p{n}@example.com"), &format!("P{n}")).await);
    }
    let lobby = server.create(&a, json!({"max_players": 4})).await;
    let id = LobbyId(lobby["id"].as_i64().expect("id"));
    // 8 players at once into 3 free places: exactly 3 get in.
    let mut tasks = Vec::new();
    for (user, _) in &players {
        let (state, service, user) = (server.state.clone(), server.service(), *user);
        tasks.push(tokio::spawn(async move {
            let ctx = HookCtx::new(state.clone(), None);
            service.join(&state, &ctx, user, id).await.map(|_| ())
        }));
    }
    let mut ok = 0;
    for task in tasks {
        match task.await.expect("task") {
            Ok(()) => ok += 1,
            Err(error) => assert_eq!(error.code(), codes::QUOTA_EXCEEDED, "{error:?}"),
        }
    }
    assert_eq!(ok, 3);
    let members = server.service().members_of(&server.state, id).await.expect("members");
    assert_eq!(members.len(), 4);

    // The disconnect rule (grace 0): the last socket closing leaves the lobby.
    let (user, token) = players.iter().find(|(u, _)| members.contains(u)).cloned().expect("a member");
    let mut ws = server.connect(user, &token).await;
    ws.close(None).await.expect("close");
    let deadline = std::time::Instant::now() + WAIT;
    while server.service().members_of(&server.state, id).await.expect("members").contains(&user) {
        assert!(std::time::Instant::now() < deadline, "the disconnected player never left");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Account deletion: a deleted host leaves a host-less lobby the purge repairs; a lobby
    // without members goes in the purge.
    server.state.db().execute(&delete_account(ada)).await.expect("delete");
    assert_eq!(server.service().lobby(&server.state, id).await.expect("read").and_then(|l| l.host), None);
    server.service().purge(&server.state).await.expect("purge");
    let host = server.service().lobby(&server.state, id).await.expect("read").and_then(|l| l.host);
    let first = server.service().members_of(&server.state, id).await.expect("members").first().copied();
    assert!(host.is_some() && host == first, "{host:?} {first:?}");
    let ctx = HookCtx::new(server.state.clone(), None);
    let (solo, s) = players[7].clone();
    server.service().leave_all(&server.state, &ctx, solo).await.expect("leave");
    let lone = server.create(&s, json!({"max_players": 2})).await["id"].as_i64().expect("id");
    server.ok(Method::POST, &format!("/v1/lobbies/{lone}/leave"), None, &s).await;
    assert!(server.service().lobby(&server.state, LobbyId(lone)).await.expect("read").is_none(), "removed when its last member left");
    let (gone, g) = players[6].clone();
    server.service().leave_all(&server.state, &ctx, gone).await.expect("leave");
    let other = server.create(&g, json!({"max_players": 2})).await["id"].as_i64().expect("id");
    server.state.db().execute(&delete_account(gone)).await.expect("delete");
    assert_eq!(server.service().purge(&server.state).await.expect("purge"), 1, "the lobby whose only member's account went");
    assert!(server.service().lobby(&server.state, LobbyId(other)).await.expect("read").is_none());
    server.stop().await;
}

/// A fresh database for one part of the suite. `backend`: `memory`, `file`, or a MySQL /
/// PostgreSQL base URL.
async fn database(backend: &str) -> (String, Option<(String, String)>) {
    match backend {
        "memory" => ("sqlite::memory:".into(), None),
        "file" => {
            let dir = common::temp_dir("lobbies-file");
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

/// The 0.2.0 review fixes: an oversized metadata change is refused before the hook and the
/// database, the metadata merge writes only changes, the change rate, strangers get 404 (members
/// 403) and the hook runs after the rights check, the chat room goes with its lobby (close, last
/// leave, purge) and the purge deletes chat rooms no lobby names.
async fn review_fixes(url: &str) {
    let server = start(url, |l| l.update_rate = 6).await;
    let (_ada, a) = server.register("ada@example.com", "Ada").await;
    let (_bo, b) = server.register("bo@example.com", "Bo").await;
    let (_cy, c) = server.register("cy@example.com", "Cy").await;
    let lobby = server.create(&a, json!({"max_players": 4, "metadata": {"mode": "ranked"}})).await;
    let id = lobby["id"].as_i64().expect("id");
    let at = |tail: &str| format!("/v1/lobbies/{id}{tail}");
    server.ok(Method::POST, &at("/join"), None, &b).await;

    // SF-1: 1000 removals: 422 before anything (also for a lobby that does not exist), no hook ran.
    let many: serde_json::Map<String, Value> = (0..1000).map(|n| (format!("k{n}"), Value::Null)).collect();
    let body = json!({"metadata": many});
    assert_eq!(server.fails(Method::PATCH, &at(""), Some(body.clone()), &a, StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    assert_eq!(server.fails(Method::PATCH, "/v1/lobbies/999999", Some(body), &a, StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    assert_eq!(server.updates.load(Ordering::SeqCst), 0, "refused before the hook");
    // 64 changes (2 x 32 keys) are fine; the merged map is checked against the limits.
    let mut fine: serde_json::Map<String, Value> = (0..32).map(|n| (format!("k{n}"), json!("v"))).collect();
    fine.extend((32..63).map(|n| (format!("k{n}"), Value::Null)));
    fine.insert("mode".into(), Value::Null);
    let changed = server.ok(Method::PATCH, &at(""), Some(json!({"metadata": fine})), &a).await;
    assert_eq!(changed["metadata"].as_object().map(|m| m.len()), Some(32), "{changed}");
    // Setting a value it has already is no change (no metadata push, no event).
    server.events();
    server.ok(Method::PATCH, &at(""), Some(json!({"metadata": {"k1": "v"}})), &a).await;
    assert!(server.events().is_empty(), "nothing changed");

    // NIT 12 + NIT 2: a stranger gets 404 everywhere a host acts, a member 403; no hook runs for either.
    let before = server.updates.load(Ordering::SeqCst);
    assert_eq!(server.fails(Method::PATCH, &at(""), Some(json!({"max_players": 3})), &c, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::PATCH, &at(""), Some(json!({"max_players": 3})), &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(server.updates.load(Ordering::SeqCst), before, "the hook never saw a refused change");
    assert_eq!(server.fails(Method::POST, &at("/code"), None, &c, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::POST, &at("/code"), None, &b, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(server.fails(Method::POST, &at("/host"), Some(json!({"user": 1})), &c, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.fails(Method::DELETE, &at("/members/1"), None, &c, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);

    // The change rate (6 per minute per player here; the host used 4 above): its fifth and sixth
    // change pass, the seventh is 429.
    let mut limited = false;
    for _ in 0..4 {
        let (status, body) = server.http(Method::POST, &at("/code"), None, &a).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(body["error"]["code"], codes::RATE_LIMITED);
            limited = true;
            break;
        }
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert!(limited, "the host's changes are rate limited");

    // SF-2: closing deletes the chat room; so does the last member leaving, and the purge.
    let room = lobby["chat_room"].as_i64().expect("room");
    assert!(room_exists(&server, room).await);
    let ctx = HookCtx::new(server.state.clone(), None);
    server.service().close(&server.state, &ctx, LobbyActor::Server, LobbyId(id)).await.expect("close");
    assert!(!room_exists(&server, room).await, "closed: the room is gone");
    let lone = server.create(&c, json!({"max_players": 2})).await;
    let lone_room = lone["chat_room"].as_i64().expect("room");
    server.ok(Method::POST, &format!("/v1/lobbies/{}/leave", lone["id"]), None, &c).await;
    assert!(!room_exists(&server, lone_room).await, "the last member left: the room is gone");
    let (gone, g) = server.register("gone@example.com", "Gone").await;
    let empty = server.create(&g, json!({"max_players": 2})).await;
    let empty_room = empty["chat_room"].as_i64().expect("room");
    server.state.db().execute(&delete_account(gone)).await.expect("delete");
    assert_eq!(server.service().purge(&server.state).await.expect("purge"), 1);
    assert!(!room_exists(&server, empty_room).await, "purged with its lobby");

    // An orphan (the lobby row went without its room): the purge deletes it after an hour, and
    // never the room of a living lobby.
    let (_dee, d) = server.register("dee@example.com", "Dee").await;
    let living = server.create(&d, json!({"max_players": 2})).await;
    let living_room = living["chat_room"].as_i64().expect("room");
    let (_eve, e) = server.register("eve@example.com", "Eve").await;
    let orphan = server.create(&e, json!({"max_players": 2})).await;
    let orphan_room = orphan["chat_room"].as_i64().expect("room");
    let mut delete = Query::delete();
    delete.from_table("lobbies").and_where(Expr::col("id").eq(orphan["id"].as_i64().expect("id")));
    server.state.db().execute(&delete).await.expect("delete the lobby row");
    server.service().purge(&server.state).await.expect("purge");
    assert!(room_exists(&server, orphan_room).await, "younger than an hour: kept");
    server.clock.advance(3_600_001);
    server.service().purge(&server.state).await.expect("purge");
    assert!(!room_exists(&server, orphan_room).await, "the orphan is gone");
    assert!(room_exists(&server, living_room).await, "a living lobby keeps its room");
    server.stop().await;
}

async fn suite(backend: &str) {
    each!(backend, create_and_join, host_actions_and_pushes, search_and_friends, limits_and_upkeep, review_fixes);
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
async fn mysql_lobbies_suite() {
    suite(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_lobbies_suite() {
    suite(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}

/// The module's wiring: needs `auth` first, reads `[modules.lobbies]`, refuses settings given
/// twice or unknown keys, lists its routes in the OpenAPI document and its pushes in the AsyncAPI
/// document, its name in `/v1/info`; works without the chat and friends modules and without the
/// hub; the create rate, the join rate and the bad-code rate.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn wiring_documents_and_rates() {
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(Lobbies::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");
    let file = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.lobbies]\nmax_players = 7\n").expect("config");
    let ok = NetBackendServer::new(file.clone()).module(Auth::new()).module(Lobbies::new()).build().await.expect("from the file");
    assert_eq!(ok.state().get::<LobbyService>().expect("service").config().max_players, 7);
    let error = NetBackendServer::new(file).module(Auth::new()).module(Lobbies::new().with_config(LobbiesConfig::default())).build().await.err();
    assert!(error.map(|e| e.to_string()).unwrap_or_default().contains("both in code"));
    let bad = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.lobbies]\nmax_player = 7\n").expect("config");
    assert!(NetBackendServer::new(bad).module(Auth::new()).module(Lobbies::new()).build().await.is_err(), "unknown keys are refused");

    let prepared = NetBackendServer::new(config()).module(Auth::new()).module(Lobbies::new()).build().await.expect("build");
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for route in routes::ALL.iter().filter(|r| r.path.starts_with("/v1/lobbies")) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
    let asyncapi: Value = serde_json::from_str(prepared.asyncapi_json()).expect("json");
    for push in ["push.lobby.member", "push.lobby.changed"] {
        assert!(asyncapi["components"]["messages"].get(push).is_some(), "{push}");
    }
    let (_, _, info) = common::call(&prepared.router(), common::get(routes::INFO)).await;
    assert_eq!(info["modules"], json!(["auth", "lobbies"]));

    // No chat, no friends, no hub: lobbies work without a room; friends-only is refused; the rates.
    let mut no_ws = config();
    no_ws.ws.enabled = false;
    let mut lobbies = LobbiesConfig::default();
    lobbies.create_rate = 1;
    lobbies.join_rate = 3;
    lobbies.bad_code_rate = 2;
    let prepared = NetBackendServer::new(no_ws).module(Auth::new()).module(Lobbies::new().with_config(lobbies)).build().await.expect("build without ws");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let register = |email: &str| {
        Request::post(routes::auth::REGISTER)
            .header("content-type", "application/json")
            .body(Body::from(json!({"email": email, "password": PASSWORD}).to_string()))
            .expect("request")
    };
    let (_, _, one) = common::call(&router, register("one@example.com")).await;
    let (_, _, two) = common::call(&router, register("two@example.com")).await;
    let token = |s: &Value| s["tokens"]["access_token"].as_str().expect("token").to_string();
    let (_, _, three) = common::call(&router, register("three@example.com")).await;
    let (t1, t2, t3) = (token(&one), token(&two), token(&three));
    let post = |path: &str, token: &str, body: Value| {
        Request::post(path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    };
    let (status, _, body) = common::call(&router, post(routes::lobbies::LIST, &t3, json!({"max_players": 2, "visibility": "friends"}))).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some(codes::VALIDATION_FAILED)), "{body}");
    let (status, _, lobby) = common::call(&router, post(routes::lobbies::LIST, &t1, json!({"max_players": 2, "visibility": "private"}))).await;
    assert_eq!(status, StatusCode::OK, "{lobby}");
    assert!(lobby.get("chat_room").is_none(), "no chat module: no room");
    let (status, _, body) = common::call(&router, post(routes::lobbies::LIST, &t2, json!({"max_players": 2}))).await;
    assert_eq!(status, StatusCode::OK, "another player's own bucket: {body}");
    let (status, _, body) = common::call(&router, post(routes::lobbies::LIST, &t2, json!({"max_players": 2}))).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(codes::RATE_LIMITED)), "{body}");
    // Two codes that match nothing, then even the right code answers 429.
    let code = LobbyCode::parse(lobby["code"].as_str().expect("code")).expect("code");
    let wrong = LobbyCode::from_u64(code.to_u64() ^ 1).expect("code");
    for _ in 0..2 {
        let (status, _, _) = common::call(&router, post(routes::lobbies::JOIN_CODE, &t3, json!({"code": wrong.as_str()}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _, body) = common::call(&router, post(routes::lobbies::JOIN_CODE, &t3, json!({"code": code.as_str()}))).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(codes::RATE_LIMITED)), "{body}");
    // The join rate (3 attempts) is now used up as well.
    let (status, _, body) = common::call(&router, post(routes::lobbies::JOIN_CODE, &t3, json!({"code": code.as_str()}))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}
