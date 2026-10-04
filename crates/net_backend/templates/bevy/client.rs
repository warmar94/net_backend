//! The {{name}} client: a headless Bevy app (no window) on bevy_net_backend. It asks the server for
//! its protocol and modules, logs in to a development account (registering it on the first run),
//! connects the WebSocket, joins the public chat room `world`, sends a message and prints the
//! room's messages for 10 seconds.
//!
//! ```text
//! cargo run -p client
//! ```
//!
//! The server is `http://127.0.0.1:8080` (the one `cargo run` starts); the variable
//! `NET_BACKEND_URL` names another. On this machine the client uses a development account unless
//! the variables `NET_BACKEND_EMAIL` and `NET_BACKEND_PASSWORD` name one; any other server needs
//! these two variables (the development password is in this file). Guides:
//! <https://docs.rs/bevy_net_backend>, <https://docs.rs/net_backend_protocol>.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use bevy_net_backend::http::Method;
use bevy_net_backend::prelude::*;
use net_backend_protocol::auth::{AuthSession, LoginRequest, RegisterRequest};
use net_backend_protocol::chat::{ChatMessage, JoinRoom, RoomInfo, SendAck, SendMessage};
use net_backend_protocol::routes::{self, HttpMethod};
use net_backend_protocol::version::GetServerInfo;
use net_backend_protocol::{
    ErrorBody, HttpCall, PROTOCOL_HEADER, PROTOCOL_VERSION, PayloadKind, ServerInfo, codes,
};

/// The project's name.
const NAME: &str = "{{name}}";
/// The development account (registered on the first run; used on this machine only).
const DEV_EMAIL: &str = "player@example.com";
const DEV_PASSWORD: &str = "dev password 1234";
/// How long the client prints the chat room's messages, in seconds.
const LISTEN_SECS: f64 = 10.0;
/// The WebSocket connection's name.
const MAIN: &str = "main";

/// What the client waits for.
#[derive(Resource, Default, PartialEq)]
enum Step {
    #[default]
    Info,
    Login,
    Register,
    Ws,
    /// Listening until this time (seconds since the start).
    Listening(f64),
}

/// The HTTP request in flight.
#[derive(Resource, Default)]
struct Pending(Option<RequestId>);

#[derive(Resource)]
struct ServerUrl(String);

/// The account the client logs in to.
#[derive(Resource)]
struct Account {
    email: String,
    password: String,
}

fn main() -> AppExit {
    let url = std::env::var("NET_BACKEND_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let account = match (
        std::env::var("NET_BACKEND_EMAIL"),
        std::env::var("NET_BACKEND_PASSWORD"),
    ) {
        (Ok(email), Ok(password)) => Account { email, password },
        _ if is_loopback(&url) => Account {
            email: DEV_EMAIL.into(),
            password: DEV_PASSWORD.into(),
        },
        _ => {
            eprintln!(
                "error: set NET_BACKEND_EMAIL and NET_BACKEND_PASSWORD for a server on another machine"
            );
            return AppExit::error();
        }
    };
    let http = HttpConfig::new(url.clone())
        .with_timeout(Duration::from_secs(15))
        .with_header(PROTOCOL_HEADER, &PROTOCOL_VERSION.to_string());
    App::new()
        .add_plugins(
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
                1.0 / 60.0,
            ))),
        )
        .add_plugins(bevy::log::LogPlugin::default())
        .add_plugins(BackendPlugin::new(http))
        // The protocol's chat types are the client's typed WebSocket requests and pushes.
        .add_ws_request::<JoinRoom>()
        .add_ws_request::<SendMessage>()
        .add_ws_push::<ChatMessage>()
        .insert_resource(ServerUrl(url))
        .insert_resource(account)
        .init_resource::<Step>()
        .init_resource::<Pending>()
        .add_systems(Startup, ask_info)
        .add_systems(
            Update,
            (on_http, on_ws_state, on_join, on_send, on_message, stop),
        )
        .run()
}

/// Sends a protocol call over HTTP: method, path and payload come from its `HttpCall` impl.
fn send_call<C: HttpCall>(http: &HttpClient, call: &C) -> RequestId {
    let method = match C::ROUTE.method {
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
        _ => Method::GET,
    };
    let mut request = OutgoingRequest::new(method, call.path().unwrap_or_default());
    match C::PAYLOAD {
        PayloadKind::Json => request = request.with_json(call.payload()),
        PayloadKind::Query => {
            let pairs = net_backend_protocol::http_call::query_pairs(call.payload());
            for (name, value) in pairs.unwrap_or_default() {
                request = request.with_query(name, value);
            }
        }
        _ => {}
    }
    http.send(request)
}

/// The protocol's error code of a refused request, if any.
fn error_code(error: &BackendError) -> Option<String> {
    match error {
        BackendError::Status(raw) => raw.json::<ErrorBody>().ok().map(|body| body.error.code),
        _ => None,
    }
}

fn ask_info(http: Res<HttpClient>, url: Res<ServerUrl>, mut pending: ResMut<Pending>) {
    info!("server {}", url.0);
    pending.0 = Some(send_call(&http, &GetServerInfo::new()));
}

#[allow(clippy::too_many_arguments)]
fn on_http(
    mut answers: MessageReader<HttpResponse>,
    http: Res<HttpClient>,
    ws: Res<WsClient>,
    url: Res<ServerUrl>,
    account: Res<Account>,
    mut step: ResMut<Step>,
    mut pending: ResMut<Pending>,
    mut credentials: ResMut<BackendCredentials>,
    mut exit: MessageWriter<AppExit>,
) {
    for answer in answers.read() {
        if pending.0 != Some(answer.id) {
            continue;
        }
        pending.0 = None;
        let result = &answer.result;
        match (&*step, result) {
            (Step::Info, Ok(raw)) => {
                let Ok(info) = raw.json::<ServerInfo>() else {
                    continue;
                };
                info!("protocol {}, modules {:?}", info.protocol, info.modules);
                let usable = info.supports(PROTOCOL_VERSION)
                    && info.has_module("auth")
                    && info.has_module("chat");
                if !usable {
                    info!(
                        "this client needs protocol {PROTOCOL_VERSION} and the modules auth and chat"
                    );
                    exit.write(AppExit::Success);
                    continue;
                }
                *step = Step::Login;
                let login = LoginRequest::new(account.email.as_str(), account.password.as_str());
                pending.0 = Some(send_call(&http, &login));
            }
            (Step::Login | Step::Register, Ok(raw)) => {
                let Ok(session) = raw.json::<AuthSession>() else {
                    continue;
                };
                let what = if *step == Step::Login {
                    "logged in as"
                } else {
                    "registered"
                };
                info!("{what} {} (account {})", account.email, session.account.id);
                // The access token on every request and on the WebSocket handshake.
                credentials.set(session.tokens.access_token.clone());
                let ws_url = format!("{}{}", url.0.replacen("http", "ws", 1), routes::WS);
                let settings = WsSettings::new(ws_url)
                    .with_header(PROTOCOL_HEADER, &PROTOCOL_VERSION.to_string());
                ws.connect(MAIN, settings);
                *step = Step::Ws;
            }
            (Step::Login, Err(error))
                if error_code(error).as_deref() == Some(codes::INVALID_CREDENTIALS) =>
            {
                *step = Step::Register;
                let register =
                    RegisterRequest::new(account.email.as_str(), account.password.as_str());
                pending.0 = Some(send_call(&http, &register));
            }
            (_, Err(error)) => {
                error!("the request failed: {error}");
                exit.write(AppExit::error());
            }
            _ => {}
        }
    }
}

fn on_ws_state(
    mut changes: MessageReader<WsStateChanged>,
    ws: Res<WsClient>,
    mut exit: MessageWriter<AppExit>,
) {
    for change in changes.read() {
        match &change.state {
            // Room membership ends with each connection: join (again) on every connect.
            WsState::Connected => {
                ws.request(MAIN, &JoinRoom::new("world"));
            }
            WsState::Disconnected => {
                if let Some(error) = &change.error {
                    error!("the WebSocket closed: {error}");
                    exit.write(AppExit::error());
                }
            }
            _ => {}
        }
    }
}

fn on_join(
    mut answers: MessageReader<WsResponse<RoomInfo>>,
    ws: Res<WsClient>,
    time: Res<Time<Real>>,
    mut step: ResMut<Step>,
) {
    for answer in answers.read() {
        match &answer.result {
            Ok(room) => {
                info!("joined `world`; listening for {LISTEN_SECS} s");
                let text = format!("hello from the {NAME} Bevy client");
                ws.request(MAIN, &SendMessage::new(room.id, text));
                if !matches!(*step, Step::Listening(_)) {
                    *step = Step::Listening(time.elapsed_secs_f64() + LISTEN_SECS);
                }
            }
            Err(error) => error!("join failed: {error}"),
        }
    }
}

fn on_send(mut answers: MessageReader<WsResponse<SendAck>>) {
    for answer in answers.read() {
        if let Err(error) = &answer.result {
            error!("send failed: {error}");
        }
    }
}

fn on_message(mut pushes: MessageReader<WsPush<ChatMessage>>) {
    for push in pushes.read() {
        let message = &push.data;
        let sender = match &message.sender_name {
            Some(name) => name.clone(),
            None => format!("player {}", message.sender),
        };
        info!("[world] {sender}: {}", message.text);
    }
}

fn stop(
    step: Res<Step>,
    time: Res<Time<Real>>,
    ws: Res<WsClient>,
    mut exit: MessageWriter<AppExit>,
) {
    if let Step::Listening(until) = *step
        && time.elapsed_secs_f64() >= until
    {
        ws.disconnect(MAIN);
        info!("done");
        exit.write(AppExit::Success);
    }
}

/// Whether the URL's host is this machine (127.0.0.1, ::1, localhost): the development account is
/// used there only.
fn is_loopback(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
