//! No access token in the server's `log` records or `tracing` events, with every level on, while
//! sockets authenticate by first message (in one frame and in several) and by the
//! `Authorization` header. tungstenite logs every frame and message it receives at TRACE through
//! the `log` crate, with the content; the server keeps `auth` messages away from it
//! (`src/ws/tap.rs`). Its own test binary: it installs the process's logger and subscriber. Bounded:
//! every wait has a timeout; the server is stopped.
#![cfg(feature = "sqlite")]

mod common;

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http::StatusCode;
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::routes;
use net_backend_server::{Config, NetBackendServer, SecretString};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The server runtime's threads; the test client runs on the test's own thread, so its records
/// (tungstenite's client side logs the request headers and every frame it sends) are left out.
const SERVER_THREADS: &str = "nbs-server";

const WAIT: Duration = Duration::from_secs(30);

/// Every `log` record made on a server thread.
struct LogCapture(Arc<Mutex<Vec<String>>>);

impl log::Log for LogCapture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if std::thread::current().name().is_some_and(|name| name.starts_with(SERVER_THREADS)) {
            let line = format!("{} {}: {}", record.level(), record.target(), record.args());
            self.0.lock().unwrap_or_else(PoisonError::into_inner).push(line);
        }
    }

    fn flush(&self) {}
}

/// Every `tracing` event of the process, as text.
#[derive(Clone)]
struct Events(Arc<Mutex<Vec<u8>>>);

impl io::Write for Events {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await.expect("a frame in time") {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(text.as_str()).expect("JSON"),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

async fn send(ws: &mut Ws, value: &Value) {
    ws.send(Message::text(value.to_string())).await.expect("send");
}

async fn connect(addr: std::net::SocketAddr, token: Option<&str>) -> Ws {
    let mut request = format!("ws://{addr}{}", routes::WS).into_client_request().expect("request");
    if let Some(token) = token {
        request.headers_mut().insert("authorization", format!("Bearer {token}").parse().expect("header"));
    }
    tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request)).await.expect("handshake in time").expect("handshake").0
}

async fn echo(ws: &mut Ws, id: u64, text: &str) {
    send(ws, &json!({"id": id, "type": "test.echo", "data": text})).await;
    assert_eq!(recv(ws).await, json!({"id": id, "ok": true, "data": text}));
}

#[test]
fn no_token_in_the_servers_trace_logs() {
    common::watchdog(Duration::from_secs(600));
    let records = Arc::new(Mutex::new(Vec::new()));
    log::set_logger(Box::leak(Box::new(LogCapture(records.clone())))).expect("logger");
    log::set_max_level(log::LevelFilter::Trace);
    let events = Events(Arc::new(Mutex::new(Vec::new())));
    let writer = events.clone();
    let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_ansi(false).with_writer(move || writer.clone()).finish();
    tracing::subscriber::set_global_default(subscriber).expect("subscriber");

    let server_runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name(SERVER_THREADS).enable_all().build().expect("runtime");
    let (addr, token, stop, task) = server_runtime.block_on(async {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config.database.migrations_dir = common::temp_dir("ws-log-migrations");
        config.server.shutdown_grace_secs = 5;
        let mut auth = AuthConfig::default();
        auth.argon2_memory_kib = 64;
        auth.argon2_iterations = 1;
        auth.purge_interval_secs = 0;
        let server = NetBackendServer::new(config)
            .module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
            .ws_handler("test.echo", |_ctx, data| async move { Ok(data) });
        let prepared = server.build().await.expect("build");
        prepared.migrate().await.expect("migrate");
        let router = prepared.router();
        let (status, _, body) = common::call(
            &router,
            common::post_json(
                routes::auth::REGISTER,
                json!({"email": "log@example.com", "password": "correct horse battery", "display_name": "Player"}).to_string(),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let token = body["tokens"]["access_token"].as_str().expect("token").to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(prepared.serve_with_shutdown(listener, async move {
            let _ = stopped.await;
        }));
        (addr, token, stop, task)
    });

    let client = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    client.block_on(async {
        let auth = json!({"type": "auth", "data": {"token": token, "protocol": 1}});
        // First-message auth, one frame.
        let mut ws = connect(addr, None).await;
        send(&mut ws, &auth).await;
        assert_eq!(recv(&mut ws).await["type"], "auth.ok");
        echo(&mut ws, 1, "a visible request").await;
        ws.close(None).await.expect("close");
        // First-message auth in two frames, a ping between them.
        let mut ws = connect(addr, None).await;
        let text = auth.to_string();
        let (first, rest) = text.split_at(text.len() / 2);
        ws.send(Message::Frame(Frame::message(first.as_bytes().to_vec(), OpCode::Data(Data::Text), false))).await.expect("send");
        ws.send(Message::Ping(Vec::new().into())).await.expect("ping");
        ws.send(Message::Frame(Frame::message(rest.as_bytes().to_vec(), OpCode::Data(Data::Continue), true))).await.expect("send");
        assert_eq!(recv(&mut ws).await["type"], "auth.ok");
        echo(&mut ws, 2, "after a fragmented auth").await;
        ws.close(None).await.expect("close");
        // The header, then also an `auth` message on the same socket.
        let mut ws = connect(addr, Some(&token)).await;
        echo(&mut ws, 3, "header auth").await;
        send(&mut ws, &auth).await;
        assert_eq!(recv(&mut ws).await["type"], "auth.ok");
        ws.close(None).await.expect("close");
    });

    server_runtime.block_on(async {
        let _ = stop.send(());
        let result = tokio::time::timeout(WAIT, task).await;
        assert!(matches!(result, Ok(Ok(Ok(())))), "the server did not stop cleanly: {result:?}");
    });
    drop(server_runtime);

    let records = records.lock().unwrap_or_else(PoisonError::into_inner).join("\n");
    let events = String::from_utf8_lossy(&events.0.lock().unwrap_or_else(PoisonError::into_inner)).to_string();
    let hex: String = token.bytes().map(|b| format!("{b:02x}")).collect();
    for (what, text) in [("a `log` record of the server", &records), ("a `tracing` event", &events)] {
        assert!(!text.contains(&token), "the token is in {what}");
        assert!(!text.contains(&hex), "the token (as hex) is in {what}");
    }
    let lower = records.to_lowercase();
    assert!(!lower.contains("authorization") && !lower.contains("bearer "), "a credential header is in a `log` record of the server");
    // The capture works: the server's tungstenite logged the requests it received, and the
    // stand-in for each `auth` message.
    assert!(records.contains("tungstenite") && records.contains("Received message") && records.contains("a visible request"), "no tungstenite record captured");
    assert!(records.contains("after a fragmented auth") && records.contains("header auth"));
    assert!(records.matches("not by tungstenite").count() >= 3, "the stand-in for each `auth` message");
}
