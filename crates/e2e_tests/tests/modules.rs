//! The storage and chat modules driven by the published `bevy_net_backend` 0.1.0 client in
//! headless Bevy apps (one app per player), over loopback:
//! - storage over HTTP, every call built from the protocol's `HttpCall` (method, path, payload):
//!   create, read with the `ETag`, a stale conditional write answered 409 with the protocol's
//!   `VersionConflict`, a batch, a delete;
//! - chat over the WebSocket with the protocol's own types as the client's typed requests and
//!   pushes (its `bevy_net_backend` feature): join, presence, send with a nonce (the answer before
//!   the echo), the push to the other player, history, members, a direct message without a join,
//!   a deletion.

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_net_backend::http::Method;
use bevy_net_backend::{
    BackendAppExt, BackendCredentials, BackendError, BackendPlugin, BearerToken, HttpClient, HttpConfig, HttpResponse, OutgoingRequest, RawResponse, RequestId,
    WsClient, WsConnections, WsPush, WsResponse, WsSettings, WsState,
};
use net_backend_protocol::chat::{
    ChatHistory, ChatMessage, DeleteMessage, JoinRoom, ListMembers, MessageDeleted, OpenDirect, Presence, PresenceEvent, RoomInfo, RoomKind, RoomMembers,
    SendAck, SendMessage,
};
use net_backend_protocol::routes::HttpMethod;
use net_backend_protocol::storage::{
    BatchGet, BatchObjects, GetObject, ObjectAck, ObjectRef, ObjectVersion, PutObject, StorageObject, VersionConflict, WriteObject,
};
use net_backend_protocol::{codes, ErrorBody, HttpCall, Page, PayloadKind, RoomId, UserId};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::axum::body::{to_bytes, Body};
use net_backend_server::chat::{Chat, ChatConfig, RoomSpec};
use net_backend_server::mail::MemoryMailer;
use net_backend_server::storage::Storage;
use net_backend_server::{AppState, Config, NetBackendServer, SecretString};
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tower::ServiceExt;

const MAIN: &str = "main";
const PASSWORD: &str = "correct horse battery";
/// The upper bound of every wait (a condition, never a fixed time: CI runners are slow).
const WAIT: Duration = Duration::from_secs(60);

struct Server {
    runtime: Runtime,
    state: AppState,
    router: net_backend_server::axum::Router,
    base: String,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), net_backend_server::Error>>>,
}

impl Server {
    fn start() -> Server {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
        let (state, router, base, stop, task) = runtime.block_on(async {
            let mut config = Config::default();
            config.database.url = SecretString::new("sqlite::memory:");
            config.database.migrations_dir =
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp").join(format!("e2e-m-{}", std::process::id()));
            config.server.shutdown_grace_secs = 5;
            let mut auth = AuthConfig::default();
            auth.argon2_memory_kib = 64;
            auth.argon2_iterations = 1;
            auth.purge_interval_secs = 0;
            auth.rate_limits = false;
            let mut chat = ChatConfig::default();
            chat.rooms = vec![RoomSpec::new("world").with_name("World")];
            let prepared = NetBackendServer::new(config)
                .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
                .module(Storage::new())
                .module(Chat::new().with_config(chat))
                .build()
                .await
                .expect("build");
            prepared.migrate().await.expect("migrate");
            let state = prepared.state().clone();
            let router = prepared.router();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let base = format!("http://{}", listener.local_addr().expect("addr"));
            let (stop, stopped) = oneshot::channel::<()>();
            let task = tokio::spawn(prepared.serve_with_shutdown(listener, async move {
                let _ = stopped.await;
            }));
            (state, router, base, stop, task)
        });
        Server { runtime, state, router, base, stop: Some(stop), task: Some(task) }
    }

    fn register(&self, email: &str, name: &str) -> (UserId, String) {
        self.runtime.block_on(async {
            let request = http::Request::post(net_backend_protocol::routes::auth::REGISTER)
                .header("content-type", "application/json")
                .body(Body::from(json!({"email": email, "password": PASSWORD, "display_name": name}).to_string()))
                .expect("request");
            let response = self.router.clone().oneshot(request).await.expect("infallible");
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
            let body: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            assert_eq!(status, 200, "{body}");
            (UserId(body["account"]["id"].as_i64().expect("id")), body["tokens"]["access_token"].as_str().expect("token").to_string())
        })
    }

    fn ws_url(&self) -> String {
        format!("{}{}", self.base.replacen("http://", "ws://", 1), net_backend_protocol::routes::WS)
    }

    fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let result = self.runtime.block_on(async { tokio::time::timeout(Duration::from_secs(30), task).await });
            assert!(matches!(result, Ok(Ok(Ok(())))), "the server did not stop cleanly");
        }
        drop(self.state);
    }
}

/// Everything one app received.
#[derive(Resource, Default)]
struct Seen {
    http: Vec<HttpResponse>,
    rooms: Vec<WsResponse<RoomInfo>>,
    acks: Vec<WsResponse<SendAck>>,
    pages: Vec<WsResponse<Page<ChatMessage>>>,
    members: Vec<WsResponse<RoomMembers>>,
    messages: Vec<WsPush<ChatMessage>>,
    presence: Vec<WsPush<Presence>>,
    deleted: Vec<WsPush<MessageDeleted>>,
    /// The order frames arrived in, for the answer-before-echo check.
    order: Vec<&'static str>,
}

#[allow(clippy::too_many_arguments)]
fn collect(
    mut seen: ResMut<Seen>,
    mut http: MessageReader<HttpResponse>,
    mut rooms: MessageReader<WsResponse<RoomInfo>>,
    mut acks: MessageReader<WsResponse<SendAck>>,
    mut pages: MessageReader<WsResponse<Page<ChatMessage>>>,
    mut members: MessageReader<WsResponse<RoomMembers>>,
    mut messages: MessageReader<WsPush<ChatMessage>>,
    mut presence: MessageReader<WsPush<Presence>>,
    mut deleted: MessageReader<WsPush<MessageDeleted>>,
) {
    seen.http.extend(http.read().cloned());
    seen.rooms.extend(rooms.read().cloned());
    for ack in acks.read() {
        seen.order.push("ack");
        seen.acks.push(ack.clone());
    }
    seen.pages.extend(pages.read().cloned());
    seen.members.extend(members.read().cloned());
    for message in messages.read() {
        seen.order.push("message");
        seen.messages.push(message.clone());
    }
    seen.presence.extend(presence.read().cloned());
    seen.deleted.extend(deleted.read().cloned());
}

fn app(server: &Server, token: &str) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, BackendPlugin::new(HttpConfig::new(server.base.clone()))))
        .add_ws_request::<JoinRoom>()
        .add_ws_request::<SendMessage>()
        .add_ws_request::<ChatHistory>()
        .add_ws_request::<ListMembers>()
        .add_ws_push::<ChatMessage>()
        .add_ws_push::<Presence>()
        .add_ws_push::<MessageDeleted>()
        .init_resource::<Seen>()
        .add_systems(Update, collect);
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new(token.to_string()));
    app
}

fn step_until(apps: &mut [&mut App], what: &str, mut done: impl FnMut(&[&mut App]) -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done(apps) {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        for app in apps.iter_mut() {
            app.update();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn seen(app: &App) -> &Seen {
    app.world().resource::<Seen>()
}

/// Send a protocol call over HTTP the way any Rust client can: method, path and payload all come
/// from its `HttpCall` impl.
fn send_call<C: HttpCall>(app: &App, call: &C) -> RequestId {
    let path = call.path().expect("a path-safe call");
    let method = match C::ROUTE.method {
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
        _ => Method::GET,
    };
    let mut request = OutgoingRequest::new(method, path);
    match C::PAYLOAD {
        PayloadKind::Json => request = request.with_json(call.payload()),
        PayloadKind::Query => {
            // The protocol's helper: absent fields are left out, never sent as `null`.
            for (name, text) in net_backend_protocol::http_call::query_pairs(call.payload()).expect("a flat query payload") {
                request = request.with_query(name, text);
            }
        }
        _ => {}
    }
    app.world().resource::<HttpClient>().send(request)
}

fn http_answer(app: &App, id: RequestId) -> Option<Result<RawResponse, BackendError>> {
    seen(app).http.iter().find(|r| r.id == id).map(|r| r.result.clone())
}

/// The call's answer: the typed response, or the protocol's error body (with the status).
fn call_http<C: HttpCall>(server_app: &mut App, call: &C) -> (u16, Result<(C::Response, RawResponse), ErrorBody>) {
    let id = send_call(server_app, call);
    step_until(&mut [server_app], "an HTTP answer", |apps| http_answer(apps[0], id).is_some());
    match http_answer(server_app, id).expect("answered") {
        Ok(raw) => (raw.status.as_u16(), Ok((raw.json::<C::Response>().expect("the call's response type"), raw))),
        Err(BackendError::Status(raw)) => (raw.status.as_u16(), Err(raw.json::<ErrorBody>().expect("the protocol's error body"))),
        Err(other) => panic!("not an HTTP answer: {other}"),
    }
}

#[test]
fn storage_over_http_with_typed_calls() {
    let server = Server::start();
    let (ada, token) = server.register("e2e-store@example.com", "Ada");
    let mut app = app(&server, &token);
    // Create: version 1 and its ETag.
    let put = WriteObject::new("saves", "slot-1", PutObject::new(json!({"level": 3})).if_absent());
    let (status, answer) = call_http(&mut app, &put);
    let (ack, raw): (ObjectAck, RawResponse) = answer.expect("created");
    assert_eq!((status, ack.version), (200, ObjectVersion(1)));
    assert_eq!(raw.headers.get("etag").and_then(|v| v.to_str().ok()), Some("\"1\""));
    // Read it back as the protocol's StorageObject.
    let (_, answer) = call_http(&mut app, &GetObject::new("saves", "slot-1"));
    let (object, _): (StorageObject, RawResponse) = answer.expect("read");
    assert_eq!((object.owner, object.value, object.version), (ada, json!({"level": 3}), ObjectVersion(1)));
    // Update with the right version, then a stale one: 409 with the protocol's VersionConflict.
    let (status, _) = call_http(&mut app, &WriteObject::new("saves", "slot-1", PutObject::new(json!({"level": 4})).if_version(ObjectVersion(1))));
    assert_eq!(status, 200);
    let (status, answer) = call_http(&mut app, &WriteObject::new("saves", "slot-1", PutObject::new(json!({"level": 9})).if_version(ObjectVersion(1))));
    let error = answer.expect_err("a conflict").error;
    assert_eq!((status, error.code.as_str()), (409, codes::VERSION_CONFLICT));
    assert_eq!(error.details_as::<VersionConflict>().and_then(|c| c.current_version), Some(ObjectVersion(2)));
    // A batch read.
    let (_, answer) = call_http(&mut app, &BatchGet::new(vec![ObjectRef::new("saves", "slot-1"), ObjectRef::new("saves", "none")]));
    let (batch, _): (BatchObjects, RawResponse) = answer.expect("batch");
    assert_eq!(batch.objects.len(), 1);
    assert_eq!(batch.objects[0].value, json!({"level": 4}));
    // Delete; then 404.
    let (status, _) = call_http(&mut app, &net_backend_protocol::storage::RemoveObject::new("saves", "slot-1"));
    assert_eq!(status, 200);
    let (status, answer) = call_http(&mut app, &GetObject::new("saves", "slot-1"));
    assert_eq!((status, answer.err().map(|e| e.error.code)), (404, Some(codes::NOT_FOUND.to_string())));
    server.stop();
}

fn connect(app: &mut App, server: &Server) {
    app.world().resource::<WsClient>().connect(MAIN, WsSettings::new(server.ws_url()));
}

fn connected(app: &App) -> bool {
    app.world().resource::<WsConnections>().state(MAIN) == Some(WsState::Connected)
}

fn ws<R: bevy_net_backend::WsRequest>(app: &App, request: &R) -> RequestId {
    app.world().resource::<WsClient>().request(MAIN, request)
}

#[test]
fn chat_over_the_websocket_with_the_protocol_types() {
    let server = Server::start();
    let (ada, ada_token) = server.register("e2e-chat-a@example.com", "Ada");
    let (bo, bo_token) = server.register("e2e-chat-b@example.com", "Bo");
    let mut a = app(&server, &ada_token);
    let mut b = app(&server, &bo_token);
    connect(&mut a, &server);
    connect(&mut b, &server);
    step_until(&mut [&mut a, &mut b], "both connected", |apps| apps.iter().all(|app| connected(app)));
    // Wait until the server registered both sockets (pushes reach registered sockets only).
    let deadline = Instant::now() + WAIT;
    while !(server.state.ws().is_online(ada) && server.state.ws().is_online(bo)) {
        assert!(Instant::now() < deadline, "the sockets were never registered");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Join by key: the protocol's RoomInfo comes back typed.
    let join_a = ws(&a, &JoinRoom::new("world"));
    step_until(&mut [&mut a], "Ada joined", |apps| seen(apps[0]).rooms.iter().any(|r| r.id == join_a));
    let room = seen(&a).rooms.iter().find(|r| r.id == join_a).and_then(|r| r.result.clone().ok()).expect("RoomInfo");
    assert_eq!((room.kind, room.key.as_deref(), room.name.as_deref()), (RoomKind::Room, Some("world"), Some("World")));
    let world = room.id;
    let join_b = ws(&b, &JoinRoom::new(world));
    step_until(&mut [&mut a, &mut b], "Bo joined and Ada saw it", |apps| {
        seen(apps[1]).rooms.iter().any(|r| r.id == join_b) && seen(apps[0]).presence.iter().any(|p| p.data.user == bo && p.data.event == PresenceEvent::Joined)
    });
    let presence = seen(&a).presence.iter().find(|p| p.data.user == bo).map(|p| p.data.clone()).expect("presence");
    assert_eq!((presence.room, presence.name.as_deref(), presence.count), (world, Some("Bo"), Some(2)));
    // Send with a nonce: the answer arrives before the echo; Bo gets the push.
    let send = ws(&a, &SendMessage::new(world, "hello from bevy").with_nonce("n-42"));
    step_until(&mut [&mut a, &mut b], "the answer, the echo and the push", |apps| {
        seen(apps[0]).acks.iter().any(|r| r.id == send) && !seen(apps[0]).messages.is_empty() && !seen(apps[1]).messages.is_empty()
    });
    assert_eq!(seen(&a).order.first().copied(), Some("ack"), "the answer comes before the echo: {:?}", seen(&a).order);
    let ack = seen(&a).acks.iter().find(|r| r.id == send).and_then(|r| r.result.clone().ok()).expect("SendAck");
    let echo = seen(&a).messages[0].data.clone();
    assert_eq!((echo.id, echo.nonce.as_deref(), echo.sender), (ack.message_id, Some("n-42"), ada));
    let pushed = seen(&b).messages[0].data.clone();
    assert_eq!((pushed.text.as_str(), pushed.sender_name.as_deref(), pushed.room), ("hello from bevy", Some("Ada"), world));
    // History and members, typed.
    let history = ws(&b, &ChatHistory::new(world));
    let members = ws(&b, &ListMembers::new(world));
    step_until(&mut [&mut b], "history and members", |apps| {
        seen(apps[0]).pages.iter().any(|r| r.id == history) && seen(apps[0]).members.iter().any(|r| r.id == members)
    });
    let page = seen(&b).pages.iter().find(|r| r.id == history).and_then(|r| r.result.clone().ok()).expect("a page");
    assert_eq!(page.items.first().map(|m| m.id), Some(ack.message_id));
    let online = seen(&b).members.iter().find(|r| r.id == members).and_then(|r| r.result.clone().ok()).expect("members");
    assert_eq!(online.count, 2);
    // A direct message: opened over HTTP (typed call), sent without a join, pushed to Bo.
    let (status, answer) = call_http(&mut a, &OpenDirect::new(bo));
    let (dm, _): (RoomInfo, RawResponse) = answer.expect("a DM room");
    assert_eq!((status, dm.kind, dm.peer), (200, RoomKind::Dm, Some(bo)));
    let before = seen(&b).messages.len();
    ws(&a, &SendMessage::new(dm.id, "just for you"));
    step_until(&mut [&mut a, &mut b], "the DM push", |apps| seen(apps[1]).messages.len() > before);
    assert_eq!(seen(&b).messages.last().map(|m| (m.data.room, m.data.text.clone())), Some((dm.id, "just for you".to_string())));
    // Ada deletes her first message: both get the typed chat.deleted push.
    let (status, _) = call_http(&mut a, &DeleteMessage::new(world, ack.message_id));
    assert_eq!(status, 200);
    step_until(&mut [&mut a, &mut b], "the deletion push", |apps| !seen(apps[1]).deleted.is_empty());
    assert_eq!(seen(&b).deleted[0].data, MessageDeleted::new(ack.message_id, world));
    assert_eq!(RoomId(world.get()), world);
    server.stop();
}
