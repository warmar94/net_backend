//! What the friends module's online state costs the database: a player's WebSocket connection
//! opening or closing runs a fixed number of statements on the module's tables, however many
//! friends the player has; a second connection of a player who is online already, and closing it,
//! run none. The statements are counted through sqlx's `sqlx::query` events. Its own test binary:
//! it installs the process's `tracing` subscriber, and it runs one server at a time (SQLite here;
//! MySQL / PostgreSQL too with `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL`). Bounded: every
//! wait has a timeout; the server is stopped.
#![cfg(all(feature = "friends", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use futures_util::StreamExt;
use http::{Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::friends::{Friends, FriendsConfig};
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::{routes, UserId};
use net_backend_server::{AppState, Config, NetBackendServer, SecretString};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const PASSWORD: &str = "correct horse battery";
const WAIT: Duration = Duration::from_secs(30);

/// The most statements one player's connection may cost when it comes online or goes offline
/// (online time, presence row, its friends, other instances).
const PER_CHANGE: u64 = 4;

/// Every statement on the friends module's tables (`friend_*`) that sqlx runs in this process.
/// (The socket's sign-in reads the token and the roles as on every server; not counted.)
static STATEMENTS: AtomicU64 = AtomicU64::new(0);

/// The text of those statements (for the failure messages).
static TEXTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct CountStatements;

struct Text(String);

impl tracing::field::Visit for Text {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "db.statement" || field.name() == "summary" {
            self.0.push_str(&format!("{value:?} "));
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for CountStatements {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            let mut text = Text(String::new());
            event.record(&mut text);
            if text.0.contains("friend_") {
                let mut texts = TEXTS.lock().unwrap_or_else(PoisonError::into_inner);
                texts.push(text.0.split_whitespace().collect::<Vec<_>>().join(" "));
                STATEMENTS.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

/// The statements counted since `since`.
fn texts(since: u64) -> String {
    let texts = TEXTS.lock().unwrap_or_else(PoisonError::into_inner);
    texts.iter().skip(usize::try_from(since).unwrap_or(usize::MAX)).cloned().collect::<Vec<_>>().join("\n")
}

struct Server {
    addr: std::net::SocketAddr,
    state: AppState,
    router: Router,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), net_backend_server::Error>>,
}

async fn start(url: &str) -> Server {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("presence-cost-migrations");
    config.server.shutdown_grace_secs = 5;
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    // No background statements while the test counts.
    auth.revocation_poll_secs = 3600;
    let mut friends = FriendsConfig::default();
    friends.request_rate = 0;
    friends.online_window_secs = 3600;
    let prepared = NetBackendServer::new(config)
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Friends::new().with_config(friends))
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let state = prepared.state().clone();
    let router = prepared.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(prepared.serve_with_shutdown(listener, async move {
        let _ = stopped.await;
    }));
    Server { addr, state, router, stop, task }
}

impl Server {
    async fn register(&self, email: &str) -> (UserId, String) {
        let body = json!({"email": email, "password": PASSWORD, "display_name": email.split('@').next().unwrap_or("p")});
        let (status, _, answer) = common::call(&self.router, common::post_json(routes::auth::REGISTER, body.to_string())).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        (UserId(answer["account"]["id"].as_i64().expect("id")), answer["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    async fn post(&self, path: &str, body: Option<Value>, token: &str) {
        let request = Request::post(path).header("authorization", format!("Bearer {token}"));
        let request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request");
        let (status, _, answer) = common::call(&self.router, request).await;
        assert_eq!(status, StatusCode::OK, "{path}: {answer}");
    }

    /// `a` and `b` become friends.
    async fn befriend(&self, (a, ta): &(UserId, String), (b, tb): &(UserId, String)) {
        self.post(routes::friends::REQUESTS, Some(json!({"user": b.get()})), ta).await;
        self.post(&format!("/v1/friends/requests/{a}/accept"), None, tb).await;
    }

    async fn connect(&self, user: UserId, token: &str) -> Ws {
        let before = self.state.ws().connections_of(user).len();
        let mut request = format!("ws://{}{}", self.addr, routes::WS).into_client_request().expect("request");
        request.headers_mut().insert("authorization", format!("Bearer {token}").parse().expect("header"));
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request)).await.expect("handshake in time").expect("connect");
        self.sockets(user, before + 1).await;
        ws
    }

    /// Wait until the hub holds `n` connections of `user`.
    async fn sockets(&self, user: UserId, n: usize) {
        let deadline = std::time::Instant::now() + WAIT;
        while self.state.ws().connections_of(user).len() != n {
            assert!(std::time::Instant::now() < deadline, "never {n} sockets");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        let result = tokio::time::timeout(Duration::from_secs(60), self.task).await;
        assert!(matches!(result, Ok(Ok(Ok(())))), "the server did not stop cleanly: {result:?}");
    }
}

/// The statements run from now until the count stays still for a while.
async fn settled(since: u64) -> u64 {
    let deadline = std::time::Instant::now() + WAIT;
    let mut last = STATEMENTS.load(Ordering::SeqCst);
    loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let now = STATEMENTS.load(Ordering::SeqCst);
        if now == last {
            return now - since;
        }
        assert!(std::time::Instant::now() < deadline, "the statements never stopped");
        last = now;
    }
}

/// The next `friends.presence` of `ws`: (user, online).
async fn presence(ws: &mut Ws) -> (i64, bool) {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await.expect("a frame in time") {
            Some(Ok(Message::Text(text))) => {
                let frame: Value = serde_json::from_str(text.as_str()).expect("JSON");
                if frame["type"] == "friends.presence" {
                    return (frame["data"]["user"].as_i64().unwrap_or(0), frame["data"]["online"].as_bool().unwrap_or(false));
                }
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

async fn costs(url: &str) {
    let server = start(url).await;
    let ada = server.register("ada@example.com").await;
    let one = server.register("one@example.com").await;
    let many = server.register("many@example.com").await;
    let alone = server.register("alone@example.com").await;
    server.befriend(&one, &ada).await;
    server.befriend(&many, &ada).await;
    for n in 0..24 {
        let friend = server.register(&format!("friend{n}@example.com")).await;
        server.befriend(&friend, &many).await;
    }
    // Everybody's first connection once (profiles made), then Ada stays.
    for (user, token) in [&one, &many, &alone] {
        // Each step's presence work done before the next one (a socket that closes before the
        // presence task looked at it changes nothing; on a slow database an "online" decided
        // while the socket was open would still reach Ada below).
        let ws = server.connect(*user, token).await;
        settled(0).await;
        drop(ws);
        server.sockets(*user, 0).await;
        settled(0).await;
    }
    let mut ada_ws = server.connect(ada.0, &ada.1).await;
    settled(0).await;

    // Coming online: one friend or 25, the same statements.
    let mut cost = Vec::new();
    let mut sockets = Vec::new();
    for (user, token) in [&one, &many] {
        let since = STATEMENTS.load(Ordering::SeqCst);
        sockets.push(server.connect(*user, token).await);
        assert_eq!(presence(&mut ada_ws).await, (user.get(), true));
        cost.push(settled(since).await);
        assert!(cost[cost.len() - 1] <= PER_CHANGE, "online: {cost:?} statements:\n{}", texts(since));
    }
    assert_eq!(cost[0], cost[1], "online with 1 friend vs 25 friends: {cost:?}");

    // A second connection, and closing it again: nothing.
    let since = STATEMENTS.load(Ordering::SeqCst);
    let second = server.connect(many.0, &many.1).await;
    assert_eq!(settled(since).await, 0, "a second connection:\n{}", texts(since));
    let since = STATEMENTS.load(Ordering::SeqCst);
    drop(second);
    server.sockets(many.0, 1).await;
    assert_eq!(settled(since).await, 0, "closing a second connection:\n{}", texts(since));

    // Going offline: the same, whatever the number of friends.
    let mut cost = Vec::new();
    for ((user, _), ws) in [&one, &many].into_iter().zip(sockets) {
        let since = STATEMENTS.load(Ordering::SeqCst);
        drop(ws);
        assert_eq!(presence(&mut ada_ws).await, (user.get(), false));
        cost.push(settled(since).await);
        assert!(cost[cost.len() - 1] <= PER_CHANGE, "offline: {cost:?} statements:\n{}", texts(since));
    }
    assert_eq!(cost[0], cost[1], "offline with 1 friend vs 25 friends: {cost:?}");

    // No friends: fewer still (nobody to tell).
    let since = STATEMENTS.load(Ordering::SeqCst);
    let ws = server.connect(alone.0, &alone.1).await;
    let online = settled(since).await;
    assert!(online < PER_CHANGE, "online without friends: {online} statements:\n{}", texts(since));
    drop(ws);
    drop(ada_ws);
    server.stop().await;
}

#[test]
fn presence_costs_a_fixed_number_of_statements() {
    common::watchdog(Duration::from_secs(600));
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(CountStatements)).expect("subscriber");
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("runtime");
    runtime.block_on(async {
        #[cfg(feature = "sqlite")]
        {
            let dir = common::temp_dir("presence-cost");
            costs(&format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"))).await;
        }
        #[cfg(any(feature = "mysql", feature = "postgres"))]
        for var in ["NBS_TEST_MYSQL_URL", "NBS_TEST_POSTGRES_URL"] {
            let base = std::env::var(var).ok().filter(|url| net_backend_server::Dialect::from_url(url).is_some_and(|d| d.is_enabled()));
            if let Some(base) = base {
                let (url, name) = common::fresh_database(&base).await;
                costs(&url).await;
                common::drop_database(&base, &name).await;
            }
        }
    });
}
