//! `net_backend_server`'s WebSocket hub driven by the published `bevy_net_backend` client in
//! a headless Bevy app (`MinimalPlugins`, no window), over `ws://127.0.0.1` (loopback: plain
//! `ws://` is allowed there without opting in). The server runs on a tokio runtime in this
//! process; the app is stepped on the test thread. Bounded: every wait has a deadline.

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_net_backend::{
    BackendAppExt, BackendCredentials, BackendError, BackendPlugin, BearerToken, Credentials, OutgoingRequest, RequestId, WsClient, WsConnections, WsPush,
    WsPushMessage, WsRequest, WsResponse, WsSettings, WsState, WsStateChanged,
};
use net_backend_server::auth::{Auth, AuthConfig, AuthService};
use net_backend_server::axum::body::{to_bytes, Body};
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::admin::BanRequest;
use net_backend_server::protocol::{routes, CloseCode, ServerPush, UserId, WsAuth, WsCall};
use net_backend_server::{AppError, AppState, AuthContext, Authenticator, Config, NetBackendServer, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tower::ServiceExt;

const MAIN: &str = "main";
const PASSWORD: &str = "correct horse battery";

/// One request kind, shared by both sides: the server implements it through the protocol's
/// `WsCall`, the client through its `WsRequest`.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Echo {
    text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Echoed {
    text: String,
    user: i64,
}

impl WsCall for Echo {
    type Response = Echoed;
    const KIND: &'static str = "e2e.echo";
}

impl WsRequest for Echo {
    type Response = Echoed;
    const KIND: &'static str = "e2e.echo";
}

/// A server push.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Note {
    n: u32,
}

impl ServerPush for Note {
    const KIND: &'static str = "e2e.note";
}

impl WsPushMessage for Note {
    const KIND: &'static str = "e2e.note";
}

/// `Bearer flaky` fails temporarily (a database restart); every other token is not this one's.
struct Flaky;

impl Authenticator for Flaky {
    fn authenticate<'a>(&'a self, parts: &'a http::request::Parts, _state: &'a AppState) -> boxed::BoxFuture<'a, Result<Option<AuthContext>, AppError>> {
        Box::pin(async move {
            match parts.headers.get("authorization").and_then(|v| v.to_str().ok()) {
                Some("Bearer flaky") => Err(AppError::unavailable("the database is restarting")),
                _ => Ok(None),
            }
        })
    }
}

mod boxed {
    pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
}

/// First-message authentication only (no handshake header).
struct FirstMessage(String);

impl Credentials for FirstMessage {
    fn apply(&self, _request: &mut OutgoingRequest) {}

    fn ws_auth_message(&self) -> Option<String> {
        Some(WsAuth::new(self.0.clone()).to_message())
    }
}

/// Both: the Bearer header on the handshake and the first-message `auth`.
struct Both(String);

impl Credentials for Both {
    fn apply(&self, request: &mut OutgoingRequest) {
        BearerToken::new(self.0.clone()).apply(request);
    }

    fn ws_auth_message(&self) -> Option<String> {
        Some(WsAuth::new(self.0.clone()).to_message())
    }
}

struct Server {
    runtime: Runtime,
    state: AppState,
    router: net_backend_server::axum::Router,
    url: String,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), net_backend_server::Error>>>,
}

impl Server {
    fn start() -> Server {
        Self::start_with(|_| {})
    }

    fn start_with(tweak: impl FnOnce(&mut Config)) -> Server {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
        let (state, router, url, stop, task) = runtime.block_on(async {
            let mut config = Config::default();
            config.database.url = SecretString::new("sqlite::memory:");
            let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp").join(format!("e2e-{}", std::process::id()));
            config.database.migrations_dir = dir;
            config.server.shutdown_grace_secs = 5;
            tweak(&mut config);
            let mut auth = AuthConfig::default();
            auth.argon2_memory_kib = 64;
            auth.argon2_iterations = 1;
            auth.purge_interval_secs = 0;
            let prepared = NetBackendServer::new(config)
                .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
                .authenticator(Flaky)
                .ws_call::<Echo, _, _>(|ctx, echo: Echo| async move { Ok(Echoed { text: echo.text, user: ctx.auth.user_id.get() }) })
                .build()
                .await
                .expect("build");
            prepared.migrate().await.expect("migrate");
            let state = prepared.state().clone();
            let router = prepared.router();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let url = format!("ws://{}{}", listener.local_addr().expect("addr"), routes::WS);
            let (stop, stopped) = oneshot::channel::<()>();
            let task = tokio::spawn(prepared.serve_with_shutdown(listener, async move {
                let _ = stopped.await;
            }));
            (state, router, url, stop, task)
        });
        Server { runtime, state, router, url, stop: Some(stop), task: Some(task) }
    }

    fn register(&self, email: &str) -> (UserId, String) {
        self.runtime.block_on(async {
            let request = http::Request::post(routes::auth::REGISTER)
                .header("content-type", "application/json")
                .body(Body::from(json!({"email": email, "password": PASSWORD}).to_string()))
                .expect("request");
            let response = self.router.clone().oneshot(request).await.expect("infallible");
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            assert_eq!(status, 200, "{body}");
            (UserId(body["account"]["id"].as_i64().expect("id")), body["tokens"]["access_token"].as_str().expect("token").to_string())
        })
    }

    fn logout(&self, token: &str) {
        self.runtime.block_on(async {
            let request = http::Request::post(routes::auth::LOGOUT)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("request");
            let response = self.router.clone().oneshot(request).await.expect("infallible");
            assert_eq!(response.status(), 200);
        });
    }

    fn ban(&self, user: UserId) {
        let service = self.state.get::<AuthService>().expect("auth");
        self.runtime.block_on(service.ban_user(&self.state, user, BanRequest::new())).expect("ban");
    }

    fn connections_of(&self, user: UserId) -> usize {
        self.state.ws().connections_of(user).len()
    }

    fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let result = self.runtime.block_on(async { tokio::time::timeout(Duration::from_secs(30), task).await });
            assert!(matches!(result, Ok(Ok(Ok(())))), "the server did not stop cleanly");
        }
    }
}

/// Everything the app received, collected by a system.
#[derive(Resource, Default)]
struct Seen {
    states: Vec<WsStateChanged>,
    echoes: Vec<WsResponse<Echoed>>,
    notes: Vec<WsPush<Note>>,
}

fn collect(
    mut seen: ResMut<Seen>,
    mut states: MessageReader<WsStateChanged>,
    mut echoes: MessageReader<WsResponse<Echoed>>,
    mut notes: MessageReader<WsPush<Note>>,
) {
    seen.states.extend(states.read().cloned());
    seen.echoes.extend(echoes.read().cloned());
    seen.notes.extend(notes.read().cloned());
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, BackendPlugin::default()))
        .add_ws_request::<Echo>()
        .add_ws_push::<Note>()
        .init_resource::<Seen>()
        .add_systems(Update, collect);
    app
}

/// Step the app until `done`, bounded by a generous deadline (a condition, never a fixed time: CI
/// runners are slow and shared; a passing run never comes close).
fn step_until(app: &mut App, what: &str, mut done: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done(app) {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        app.update();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn step_for(app: &mut App, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        app.update();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn state(app: &App) -> Option<WsState> {
    app.world().resource::<WsConnections>().state(MAIN)
}

fn connected(app: &App) -> bool {
    state(app) == Some(WsState::Connected)
}

fn request(app: &App, text: &str) -> RequestId {
    app.world().resource::<WsClient>().request(MAIN, &Echo { text: text.into() })
}

fn answer(app: &App, id: RequestId) -> Option<Result<Echoed, BackendError>> {
    app.world().resource::<Seen>().echoes.iter().find(|a| a.id == id).map(|a| a.result.clone())
}

fn last_close_code(app: &App) -> Option<u16> {
    app.world().resource::<Seen>().states.iter().rev().find_map(|s| s.error.as_ref().and_then(BackendError::close_code))
}

fn connects(app: &App) -> usize {
    app.world().resource::<Seen>().states.iter().filter(|s| s.state == WsState::Connected).count()
}

fn set_credentials(app: &mut App, credentials: impl Credentials) {
    app.world_mut().resource_mut::<BackendCredentials>().set(credentials);
}

fn connect(app: &mut App, settings: WsSettings) {
    app.world().resource::<WsClient>().connect(MAIN, settings);
}

/// Bearer on the handshake: connect, request / answer, a typed push, reconnect after the server
/// closes with 1001, then no reconnect after a ban (4003).
#[test]
fn header_auth_requests_pushes_reconnect_and_ban() {
    let server = Server::start();
    let (user, token) = server.register("e2e-header@example.com");
    let mut app = app();
    set_credentials(&mut app, BearerToken::new(token));
    connect(&mut app, WsSettings::new(server.url.clone()));
    step_until(&mut app, "connected", connected);
    let id = request(&app, "hello");
    step_until(&mut app, "the answer", |app| answer(app, id).is_some());
    assert_eq!(answer(&app, id).and_then(Result::ok), Some(Echoed { text: "hello".into(), user: user.get() }));
    // A push to the user.
    server.state.ws().push_user(user, &Note { n: 42 }).expect("push");
    step_until(&mut app, "the push", |app| !app.world().resource::<Seen>().notes.is_empty());
    assert_eq!(app.world().resource::<Seen>().notes[0].data, Note { n: 42 });
    // A server-side close the client may come back from (1001): it reconnects by itself.
    assert_eq!(server.state.ws().close_user(user, CloseCode::GOING_AWAY, "redeploy"), 1);
    step_until(&mut app, "the reconnect", |app| connects(app) >= 2 && connected(app));
    assert_eq!(last_close_code(&app), Some(1001));
    let id = request(&app, "again");
    step_until(&mut app, "the answer after the reconnect", |app| answer(app, id).is_some());
    assert!(answer(&app, id).is_some_and(|a| a.is_ok()));
    // A ban: 4003, and the client stays away.
    server.ban(user);
    step_until(&mut app, "disconnected by the ban", |app| state(app) == Some(WsState::Disconnected));
    assert_eq!(last_close_code(&app), Some(4003));
    step_for(&mut app, Duration::from_secs(2));
    assert_eq!(state(&app), Some(WsState::Disconnected));
    assert_eq!(server.connections_of(user), 0, "the client came back after 4003");
    server.stop();
}

/// First-message `auth` only, with the client's `with_auth_ack`: requests wait for `auth.ok`.
#[test]
fn first_message_auth_with_ack() {
    let server = Server::start();
    let (user, token) = server.register("e2e-first@example.com");
    let mut app = app();
    set_credentials(&mut app, FirstMessage(token));
    connect(&mut app, WsSettings::new(server.url.clone()).with_auth_ack(Duration::from_secs(5)));
    let id = request(&app, "after auth");
    step_until(&mut app, "the answer", |app| answer(app, id).is_some());
    assert_eq!(answer(&app, id).and_then(Result::ok).map(|e| e.user), Some(user.get()));
    assert_eq!(server.connections_of(user), 1);
    server.stop();
}

/// Header AND first-message auth (the server answers the `auth` of a header-authenticated socket).
#[test]
fn header_and_first_message_auth_with_ack() {
    let server = Server::start();
    let (user, token) = server.register("e2e-both@example.com");
    let mut app = app();
    set_credentials(&mut app, Both(token));
    connect(&mut app, WsSettings::new(server.url.clone()).with_auth_ack(Duration::from_secs(5)));
    let id = request(&app, "both");
    step_until(&mut app, "the answer", |app| answer(app, id).is_some());
    assert_eq!(answer(&app, id).and_then(Result::ok).map(|e| e.user), Some(user.get()));
    server.stop();
}

/// An unsupported protocol version: the server upgrades and closes with 4010; no reconnect.
#[test]
fn unsupported_version_is_final() {
    let server = Server::start();
    let (user, token) = server.register("e2e-version@example.com");
    let mut app = app();
    set_credentials(&mut app, BearerToken::new(token));
    connect(&mut app, WsSettings::new(server.url.clone()).with_header(net_backend_server::protocol::PROTOCOL_HEADER, "99"));
    step_until(&mut app, "disconnected", |app| state(app) == Some(WsState::Disconnected));
    assert_eq!(last_close_code(&app), Some(4010));
    step_for(&mut app, Duration::from_secs(2));
    assert_eq!(state(&app), Some(WsState::Disconnected));
    // Upgraded once (the server refuses after the upgrade), never again.
    assert_eq!(connects(&app), 1);
    assert_eq!(server.connections_of(user), 0);
    server.stop();
}

/// A bad token at the handshake: 401, final for the client (no retries).
#[test]
fn bad_token_is_final() {
    let server = Server::start();
    let mut app = app();
    set_credentials(&mut app, BearerToken::new("nbsa_not-a-token"));
    connect(&mut app, WsSettings::new(server.url.clone()));
    step_until(&mut app, "disconnected", |app| state(app) == Some(WsState::Disconnected));
    assert_eq!(connects(&app), 0);
    let error = app.world().resource::<Seen>().states.iter().rev().find_map(|s| s.error.clone());
    assert!(matches!(error, Some(BackendError::Status(ref response)) if response.status.as_u16() == 401), "{error:?}");
    server.stop();
}

/// Heartbeats both ways with the real client: the server answers the client's pings (a client
/// with a 2.5 s dead-peer limit stays up), and the client answers the server's pings (the server
/// drops sockets silent for 4 s; a client that itself pings only every 15 s stays up).
#[test]
fn heartbeats_keep_both_sides_alive() {
    let server = Server::start_with(|config| {
        config.ws.ping_interval_secs = 1;
        config.ws.idle_timeout_secs = 4;
        config.ws.request_timeout_secs = 1;
    });
    let (user, token) = server.register("e2e-heartbeat@example.com");
    let mut app = app();
    set_credentials(&mut app, BearerToken::new(token));
    let client = app.world().resource::<WsClient>();
    // The client pings every 250 ms and gives the server 2.5 s to answer (tight enough to notice a
    // server that never answers, loose enough for a slow runner).
    client.connect("fast", WsSettings::new(server.url.clone()).with_heartbeat(Duration::from_millis(250), Duration::from_millis(2500)));
    client.connect("slow", WsSettings::new(server.url.clone()));
    let both = |app: &App| {
        let connections = app.world().resource::<WsConnections>();
        connections.is_connected("fast") && connections.is_connected("slow")
    };
    step_until(&mut app, "both connected", both);
    // Longer than the server's idle timeout (4 s) and the client's dead-peer limit.
    step_for(&mut app, Duration::from_secs(6));
    assert!(both(&app), "a connection dropped: {:?}", app.world().resource::<Seen>().states);
    assert_eq!(connects(&app), 2, "a connection was re-established: {:?}", app.world().resource::<Seen>().states);
    assert_eq!(server.connections_of(user), 2);
    server.stop();
}

/// A temporary server failure during first-message auth (close 1013, no `auth.failed`): the client
/// keeps reconnecting instead of giving up.
#[test]
fn temporary_auth_failure_is_retried() {
    let server = Server::start();
    let mut app = app();
    set_credentials(&mut app, FirstMessage("flaky".into()));
    connect(&mut app, WsSettings::new(server.url.clone()).with_auth_ack(Duration::from_secs(5)));
    step_until(&mut app, "a reconnect attempt", |app| {
        app.world()
            .resource::<Seen>()
            .states
            .iter()
            .any(|s| matches!(s.state, WsState::Reconnecting { .. }) && s.error.as_ref().and_then(BackendError::close_code) == Some(1013))
    });
    assert_ne!(state(&app), Some(WsState::Disconnected));
    server.stop();
}

/// A logout (4001) and a replacement by the per-user cap (4009) are final for the client.
#[test]
fn logout_and_replacement_are_final() {
    let server = Server::start_with(|config| config.ws.max_connections_per_user = 1);
    let (user, token) = server.register("e2e-final@example.com");
    let mut app = app();
    set_credentials(&mut app, BearerToken::new(token.clone()));
    let client = app.world().resource::<WsClient>();
    client.connect("first", WsSettings::new(server.url.clone()));
    let is = |app: &App, name: &str, wanted: WsState| app.world().resource::<WsConnections>().state(name) == Some(wanted);
    step_until(&mut app, "first connected", |app| is(app, "first", WsState::Connected));
    app.world().resource::<WsClient>().connect("second", WsSettings::new(server.url.clone()));
    step_until(&mut app, "first replaced", |app| is(app, "first", WsState::Disconnected) && is(app, "second", WsState::Connected));
    step_for(&mut app, Duration::from_secs(1));
    assert!(is(&app, "first", WsState::Disconnected), "4009 was retried");
    server.logout(&token);
    step_until(&mut app, "second logged out", |app| is(app, "second", WsState::Disconnected));
    step_for(&mut app, Duration::from_secs(1));
    assert!(is(&app, "second", WsState::Disconnected), "4001 was retried");
    let codes: Vec<u16> = app.world().resource::<Seen>().states.iter().filter_map(|s| s.error.as_ref().and_then(BackendError::close_code)).collect();
    assert!(codes.contains(&4009) && codes.contains(&4001), "{codes:?}");
    assert_eq!(server.connections_of(user), 0);
    server.stop();
}
