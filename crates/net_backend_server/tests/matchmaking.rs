//! The matchmaking module: tickets (rules, one per player, the hook, cancel, read), the default
//! rule (first come, first matched), the game's rules through the round hook (its matches, data,
//! dropped proposals), timeouts with `match.expired`, matched tickets read over HTTP, the pushes on
//! a loopback server with WebSocket clients, the disconnect rule, the after hook, and the wiring.
//! Tickets live in memory: no database suite per backend.
#![cfg(all(feature = "matchmaking", feature = "sqlite"))]

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use futures_util::StreamExt;
use http::{Method, Request, StatusCode};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::matchmaking::events::{AfterMatchFound, BeforeTicketCreate, MatchmakingRound, ProposedMatch};
use net_backend_server::matchmaking::{Matchmaking, MatchmakingConfig, MatchmakingService, QueueSpec};
use net_backend_server::protocol::{codes, routes, UnixMillis, UserId};
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
type Matches = Arc<Mutex<Vec<(String, Vec<UserId>)>>>;

const T0: i64 = 1_800_000_000_000;
const PASSWORD: &str = "correct horse battery";
const WAIT: Duration = Duration::from_secs(30);

/// How the test's round hook behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rules {
    /// Leave the default proposal.
    Default,
    /// Pair tickets with the same `region`, with data; also propose a match naming a ticket twice.
    Regions,
    /// Refuse every round.
    Refuse,
}

struct Server {
    addr: SocketAddr,
    state: AppState,
    router: Router,
    clock: Arc<ManualClock>,
    rules: Arc<Mutex<Rules>>,
    matches: Matches,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), Error>>>,
}

async fn start() -> Server {
    common::watchdog(Duration::from_secs(600));
    let mut config = Config::default();
    config.database.url = SecretString::new("sqlite::memory:");
    config.database.migrations_dir = common::temp_dir("matchmaking-migrations");
    config.server.shutdown_grace_secs = 5;
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    auth.access_token_ttl_secs = 7 * 24 * 3600;
    let mut matchmaking =
        MatchmakingConfig::default().with_queue(QueueSpec::new("duel", 2).with_timeout_secs(60)).with_queue(QueueSpec::new("squad", 3).with_timeout_secs(60));
    matchmaking.interval_ms = 0;
    matchmaking.ticket_rate = 0;
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let rules = Arc::new(Mutex::new(Rules::Default));
    let matches: Matches = Arc::new(Mutex::new(Vec::new()));
    let (chosen, seen) = (rules.clone(), matches.clone());
    let prepared = NetBackendServer::new(config)
        .clock(clock.clone())
        .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
        .module(Matchmaking::new().with_config(matchmaking))
        // The game puts its stored rating into the attributes, and refuses a queue it closed.
        .before::<BeforeTicketCreate, _, _>(|_ctx, mut ticket| async move {
            if ticket.request.attributes.as_ref().is_some_and(|a| a["closed"] == true) {
                return Ok(Decision::Reject(AppError::forbidden("this queue is closed for you")));
            }
            let mut attributes = ticket.request.attributes.take().unwrap_or_else(|| json!({}));
            attributes["rating"] = json!(1500);
            ticket.request.attributes = Some(attributes);
            Ok(Decision::Continue(ticket))
        })
        .before::<MatchmakingRound, _, _>(move |_ctx, mut round| {
            let chosen = *chosen.lock().expect("lock");
            async move {
                match chosen {
                    Rules::Default => Ok(Decision::Continue(round)),
                    Rules::Refuse => Ok(Decision::Reject(AppError::unavailable("no matches now"))),
                    Rules::Regions => {
                        round.matches.clear();
                        let region = |t: &net_backend_server::matchmaking::events::QueuedTicket| {
                            t.attributes.as_ref().and_then(|a| a["region"].as_str().map(str::to_string)).unwrap_or_default()
                        };
                        let mut waiting = round.tickets.clone();
                        while let Some(first) = waiting.first().cloned() {
                            waiting.remove(0);
                            if let Some(at) = waiting.iter().position(|t| region(t) == region(&first)) {
                                let second = waiting.remove(at);
                                round.matches.push(ProposedMatch::new(vec![first.ticket, second.ticket]).with_data(json!({"region": region(&first)})));
                            }
                        }
                        // A broken proposal (a ticket twice) is dropped.
                        if let Some(first) = round.tickets.first() {
                            round.matches.push(ProposedMatch::new(vec![first.ticket, first.ticket]));
                        }
                        Ok(Decision::Continue(round))
                    }
                }
            }
        })
        .after::<AfterMatchFound, _, _>(move |_ctx, found| {
            let seen = seen.clone();
            async move {
                seen.lock().expect("lock").push((found.queue.clone(), found.players.clone()));
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
    Server { addr, state, router, clock, rules, matches, stop: Some(stop), task: Some(task) }
}

impl Server {
    fn service(&self) -> Arc<MatchmakingService> {
        self.state.get::<MatchmakingService>().expect("service")
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

    async fn fails(&self, method: Method, path: &str, body: Option<Value>, token: &str, status: StatusCode) -> String {
        let (got, answer) = self.http(method.clone(), path, body, token).await;
        assert_eq!(got, status, "{method} {path}: {answer}");
        answer["error"]["code"].as_str().unwrap_or_default().to_string()
    }

    async fn queue(&self, token: &str, body: Value) -> Value {
        self.ok(Method::POST, routes::matchmaking::TICKET, Some(body), token).await
    }

    async fn round(&self) -> usize {
        self.service().run_round(&self.state).await.expect("round")
    }

    fn matches(&self) -> Vec<(String, Vec<UserId>)> {
        std::mem::take(&mut *self.matches.lock().expect("lock"))
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

/// Tickets, the default rule, matched tickets, cancel, the queue list and the hooks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tickets_and_the_default_rule() {
    let server = start().await;
    let mut players = Vec::new();
    for n in 0..5 {
        players.push(server.register(&format!("p{n}@example.com")).await);
    }
    let (p0, t0) = players[0].clone();
    // Rules: an unknown queue, a bad key, too large attributes, the hook's refusal.
    assert_eq!(server.fails(Method::POST, routes::matchmaking::TICKET, Some(json!({"queue": "solo"})), &t0, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(
        server.fails(Method::POST, routes::matchmaking::TICKET, Some(json!({"queue": "Duel"})), &t0, StatusCode::UNPROCESSABLE_ENTITY).await,
        codes::VALIDATION_FAILED
    );
    let big = json!({"queue": "duel", "attributes": "x".repeat(2000)});
    assert_eq!(server.fails(Method::POST, routes::matchmaking::TICKET, Some(big), &t0, StatusCode::UNPROCESSABLE_ENTITY).await, codes::VALIDATION_FAILED);
    let closed = json!({"queue": "duel", "attributes": {"closed": true}});
    assert_eq!(server.fails(Method::POST, routes::matchmaking::TICKET, Some(closed), &t0, StatusCode::FORBIDDEN).await, codes::FORBIDDEN);
    assert_eq!(server.fails(Method::GET, routes::matchmaking::TICKET, None, &t0, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);

    // One ticket per player; the queue list counts the waiting ones.
    let ticket = server.queue(&t0, json!({"queue": "duel", "attributes": {"region": "eu"}})).await;
    assert_eq!(
        (ticket["queue"].as_str(), ticket["status"].as_str(), ticket["created_at"].as_i64(), ticket["expires_at"].as_i64()),
        (Some("duel"), Some("waiting"), Some(T0), Some(T0 + 60_000))
    );
    assert!(ticket["id"].as_i64().is_some_and(|id| id > 0));
    assert_eq!(server.fails(Method::POST, routes::matchmaking::TICKET, Some(json!({"queue": "squad"})), &t0, StatusCode::CONFLICT).await, codes::CONFLICT);
    assert_eq!(server.ok(Method::GET, routes::matchmaking::TICKET, None, &t0).await["id"], ticket["id"]);
    for (_, token) in &players[1..4] {
        server.queue(token, json!({"queue": "duel"})).await;
    }
    server.queue(&players[4].1, json!({"queue": "squad"})).await;
    let queues = server.ok(Method::GET, routes::matchmaking::QUEUES, None, &t0).await;
    assert_eq!(queues, json!({"queues": [{"key": "duel", "players": 2, "waiting": 4}, {"key": "squad", "players": 3, "waiting": 1}]}));

    // The default rule: first come, first matched, 2 per duel; the squad waits for 3.
    assert_eq!(server.round().await, 2);
    let mut found = server.matches();
    found.sort();
    assert_eq!(found, [("duel".to_string(), vec![p0, players[1].0]), ("duel".to_string(), vec![players[2].0, players[3].0])]);
    let matched = server.ok(Method::GET, routes::matchmaking::TICKET, None, &t0).await;
    assert_eq!((matched["status"].as_str(), matched["found"]["players"].clone()), (Some("matched"), json!([p0.get(), players[1].0.get()])));
    assert_eq!(matched["found"]["ticket"], ticket["id"]);
    assert_eq!(server.ok(Method::GET, routes::matchmaking::QUEUES, None, &t0).await["queues"][0]["waiting"], 0);
    // A matched ticket is replaced by a new one; it is forgotten after matched_keep_secs.
    let again = server.queue(&t0, json!({"queue": "duel"})).await;
    assert_eq!(again["status"], "waiting");
    server.ok(Method::DELETE, routes::matchmaking::TICKET, None, &t0).await;
    server.ok(Method::DELETE, routes::matchmaking::TICKET, None, &t0).await;
    assert_eq!(server.fails(Method::GET, routes::matchmaking::TICKET, None, &t0, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    server.clock.advance(61_000);
    assert_eq!(server.fails(Method::GET, routes::matchmaking::TICKET, None, &players[1].1, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);
    assert_eq!(server.round().await, 0, "the squad ticket ran out");
    assert_eq!(server.ok(Method::GET, routes::matchmaking::QUEUES, None, &t0).await["queues"][1]["waiting"], 0);
    // A refusing round hook: the ticket keeps waiting.
    server.queue(&t0, json!({"queue": "duel"})).await;
    *server.rules.lock().expect("lock") = Rules::Refuse;
    assert_eq!(server.round().await, 0, "a refusing hook skips the round");
    assert_eq!(server.ok(Method::GET, routes::matchmaking::QUEUES, None, &t0).await["queues"][0]["waiting"], 1);
    assert_eq!(server.http(Method::GET, routes::matchmaking::QUEUES, None, "nope").await.0, StatusCode::UNAUTHORIZED);
    server.stop().await;
}

/// The game's rules (regions), the pushes, timeouts with `match.expired`, the disconnect rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn game_rules_pushes_and_timeouts() {
    let server = start().await;
    *server.rules.lock().expect("lock") = Rules::Regions;
    let (a, ta) = server.register("a@example.com").await;
    let (b, tb) = server.register("b@example.com").await;
    let (c, tc) = server.register("c@example.com").await;
    let (d, td) = server.register("d@example.com").await;
    let mut ws_a = server.connect(a, &ta).await;
    let mut ws_c = server.connect(c, &tc).await;
    let mut ws_d = server.connect(d, &td).await;
    let ticket_a = server.queue(&ta, json!({"queue": "duel", "attributes": {"region": "eu"}})).await;
    server.queue(&tb, json!({"queue": "duel", "attributes": {"region": "us"}})).await;
    server.queue(&tc, json!({"queue": "duel", "attributes": {"region": "eu"}})).await;
    assert_eq!(server.round().await, 1, "eu pairs; us waits; the broken proposal is dropped");
    let push = next_of(&mut ws_a, "match.found").await;
    assert_eq!((push["ticket"].clone(), push["queue"].as_str(), push["players"].clone()), (ticket_a["id"].clone(), Some("duel"), json!([a.get(), c.get()])));
    assert_eq!(push["data"], json!({"region": "eu"}));
    assert_eq!(next_of(&mut ws_c, "match.found").await["players"], json!([a.get(), c.get()]));
    assert_eq!(server.ok(Method::GET, routes::matchmaking::TICKET, None, &tb).await["status"], "waiting");
    assert_eq!(server.matches(), [("duel".to_string(), vec![a, c])]);

    // Timeout: the waiting ticket runs out with match.expired (b has no socket: read over HTTP).
    server.queue(&td, json!({"queue": "squad"})).await;
    server.clock.advance(60_000);
    assert_eq!(server.round().await, 0);
    let expired = next_of(&mut ws_d, "match.expired").await;
    assert_eq!(expired["queue"], "squad");
    assert_eq!(server.fails(Method::GET, routes::matchmaking::TICKET, None, &tb, StatusCode::NOT_FOUND).await, codes::NOT_FOUND);

    // The disconnect rule: the last socket closing cancels a waiting ticket.
    server.queue(&td, json!({"queue": "duel"})).await;
    ws_d.close(None).await.expect("close");
    let deadline = std::time::Instant::now() + WAIT;
    while server.service().ticket_of(&server.state, d).is_some() {
        assert!(std::time::Instant::now() < deadline, "the ticket was never cancelled");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = b;
    let _ = ws_a.close(None).await;
    let _ = ws_c.close(None).await;
    server.stop().await;
}

/// The wiring: needs `auth` first, reads `[modules.matchmaking]` (queues included), refuses
/// settings given twice or unknown keys; no tables; its routes in OpenAPI, its pushes in
/// AsyncAPI, its name in `/v1/info`; the ticket rate.
#[tokio::test]
async fn wiring_documents_and_rate() {
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(Matchmaking::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");
    let toml = "[database]\nurl = \"sqlite::memory:\"\n[modules.matchmaking]\ninterval_ms = 0\nticket_rate = 1\n[[modules.matchmaking.queues]]\nkey = \"duel\"\nplayers = 2\n";
    let file = Config::from_toml_str(toml).expect("config");
    let prepared = NetBackendServer::new(file.clone()).module(Auth::new()).module(Matchmaking::new()).build().await.expect("from the file");
    assert_eq!(prepared.state().get::<MatchmakingService>().expect("service").config().queues, vec![QueueSpec::new("duel", 2)]);
    let error = NetBackendServer::new(file).module(Auth::new()).module(Matchmaking::new().with_config(MatchmakingConfig::default())).build().await.err();
    assert!(error.map(|e| e.to_string()).unwrap_or_default().contains("both in code"));
    let bad = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.matchmaking]\ninterval = 7\n").expect("config");
    assert!(NetBackendServer::new(bad).module(Auth::new()).module(Matchmaking::new()).build().await.is_err(), "unknown keys are refused");
    assert!(Matchmaking::new().migrations_are_empty(), "no tables");

    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    for route in routes::ALL.iter().filter(|r| r.path.starts_with("/v1/matchmaking")) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
    let asyncapi: Value = serde_json::from_str(prepared.asyncapi_json()).expect("json");
    for push in ["push.match.found", "push.match.expired"] {
        assert!(asyncapi["components"]["messages"].get(push).is_some(), "{push}");
    }
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let (_, _, info) = common::call(&router, common::get(routes::INFO)).await;
    assert_eq!(info["modules"], json!(["auth", "matchmaking"]));
    let request = Request::post(routes::auth::REGISTER)
        .header("content-type", "application/json")
        .body(Body::from(json!({"email": "solo@example.com", "password": PASSWORD}).to_string()))
        .expect("request");
    let (_, _, session) = common::call(&router, request).await;
    let token = session["tokens"]["access_token"].as_str().expect("token").to_string();
    let queue = || {
        Request::post(routes::matchmaking::TICKET)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(json!({"queue": "duel"}).to_string()))
            .expect("request")
    };
    assert_eq!(common::call(&router, queue()).await.0, StatusCode::OK);
    let (status, _, body) = common::call(&router, queue()).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(codes::RATE_LIMITED)), "{body}");
}

/// `Module::migrations` of the matchmaking module, for the "no tables" check.
trait NoTables {
    fn migrations_are_empty(&self) -> bool;
}

impl NoTables for Matchmaking {
    fn migrations_are_empty(&self) -> bool {
        use net_backend_server::{Dialect, Module};
        Dialect::ALL.iter().all(|d| self.migrations(*d).is_empty())
    }
}
