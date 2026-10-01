//! Serving on a real loopback socket: graceful shutdown (in-flight requests drain, modules and
//! hooks shut down in order, the listener closes), the grace deadline, a failing start, and
//! migrations on start. Bounded: every wait has a timeout, nothing is left running.
#![cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::routing::get;
use common::http_config;
use futures_util::future::BoxFuture;
use net_backend_server::{AppState, Error, Module, NetBackendServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, line: impl Into<String>) {
    if let Ok(mut log) = log.lock() {
        log.push(line.into());
    }
}

fn lines(log: &Log) -> Vec<String> {
    log.lock().map(|l| l.clone()).unwrap_or_default()
}

/// A module logging its start and shutdown; `fail_start` makes its start fail.
struct Lifecycle {
    name: &'static str,
    log: Log,
    fail_start: bool,
}

impl Module for Lifecycle {
    fn name(&self) -> &'static str {
        self.name
    }

    fn start<'a>(&'a self, _state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if self.fail_start {
                return Err(Error::Startup("cannot start".into()));
            }
            push(&self.log, format!("start {}", self.name));
            Ok(())
        })
    }

    fn shutdown<'a>(&'a self, _state: &'a AppState) -> BoxFuture<'a, ()> {
        Box::pin(async move { push(&self.log, format!("shutdown {}", self.name)) })
    }
}

/// A minimal HTTP/1.1 GET over a fresh connection: (status, body).
async fn http_get(addr: SocketAddr, path: &str) -> std::io::Result<(u16, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    Ok((status, body))
}

async fn wait_until_up(addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok((200, _)) = http_get(addr, "/healthz").await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the server did not come up");
}

#[tokio::test]
async fn graceful_shutdown_drains_and_runs_callbacks_in_order() {
    let log: Log = Arc::default();
    let (log_start, log_stop) = (log.clone(), log.clone());
    let server = NetBackendServer::new(http_config())
        .module(Lifecycle { name: "first", log: log.clone(), fail_start: false })
        .module(Lifecycle { name: "second", log: log.clone(), fail_start: false })
        .on_start(move |_ctx| {
            let log = log_start.clone();
            async move {
                push(&log, "start hook");
                Ok(())
            }
        })
        .on_shutdown(move |_ctx| {
            let log = log_stop.clone();
            async move { push(&log, "shutdown hook") }
        })
        .route(
            "/v1/game/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(600)).await;
                "done"
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(server.serve_with_shutdown(listener, async move {
        let _ = stopped.await;
    }));
    wait_until_up(addr).await;

    let in_flight = tokio::spawn(http_get(addr, "/v1/game/slow"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = stop.send(());
    let answer = tokio::time::timeout(Duration::from_secs(5), in_flight).await.expect("in time").expect("task").expect("io");
    assert_eq!(answer, (200, "done".to_string()), "the in-flight request finished");
    let result = tokio::time::timeout(Duration::from_secs(5), running).await.expect("server stopped in time").expect("task");
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(lines(&log), ["start first", "start second", "start hook", "shutdown second", "shutdown first", "shutdown hook"]);
    // The listener is closed.
    assert!(http_get(addr, "/healthz").await.is_err());
}

#[tokio::test]
async fn grace_deadline_bounds_the_shutdown() {
    let mut config = http_config();
    config.server.shutdown_grace_secs = 1;
    let finished = Arc::new(AtomicBool::new(false));
    let flag = finished.clone();
    let log: Log = Arc::default();
    let server = NetBackendServer::new(config).module(Lifecycle { name: "pool_user", log: log.clone(), fail_start: false }).route(
        "/v1/game/stuck",
        get(move || {
            let flag = flag.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                flag.store(true, Ordering::SeqCst);
                "late"
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(server.serve_with_shutdown(listener, async move {
        let _ = stopped.await;
    }));
    wait_until_up(addr).await;
    let stuck = tokio::spawn(http_get(addr, "/v1/game/stuck"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let started = Instant::now();
    let _ = stop.send(());
    let result = tokio::time::timeout(Duration::from_secs(8), running).await.expect("stopped within the deadline").expect("task");
    assert!(result.is_ok(), "{result:?}");
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(900) && took < Duration::from_secs(3), "{took:?}");
    assert_eq!(lines(&log), ["start pool_user", "shutdown pool_user"]);
    // The handler was dropped at the deadline: it never finishes after the modules and the pool
    // shut down, and the client gets no answer.
    let answer = tokio::time::timeout(Duration::from_secs(5), stuck).await.expect("client returns").expect("task");
    assert!(!matches!(answer, Ok((200, _))), "{answer:?}");
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(!finished.load(Ordering::SeqCst), "the handler completed after shutdown");
}

#[tokio::test]
async fn slow_headers_are_cut_off() {
    let mut config = http_config();
    config.server.header_read_timeout_secs = 1;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(NetBackendServer::new(config).serve_with_shutdown(listener, async move {
        let _ = stopped.await;
    }));
    wait_until_up(addr).await;
    // A half-sent request: the request line and one header, never the blank line.
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n").await.expect("write");
    let started = Instant::now();
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(6), stream.read_to_end(&mut rest)).await;
    assert!(closed.is_ok(), "the connection is still open after 6 s");
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    // An idle keep-alive connection is closed after the same time.
    let mut idle = TcpStream::connect(addr).await.expect("connect");
    let mut buf = [0u8; 16];
    let closed = tokio::time::timeout(Duration::from_secs(6), idle.read(&mut buf)).await;
    assert!(matches!(closed, Ok(Ok(0)) | Ok(Err(_))), "{closed:?}");
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

/// A module whose start or shutdown misbehaves.
struct Misbehaving {
    start: &'static str,
    stop: &'static str,
    log: Log,
}

impl Module for Misbehaving {
    fn name(&self) -> &'static str {
        "misbehaving"
    }

    fn start<'a>(&'a self, _state: &'a AppState) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            match self.start {
                "panic" => panic!("start bug"),
                "hang" => std::future::pending().await,
                _ => Ok(()),
            }
        })
    }

    fn shutdown<'a>(&'a self, _state: &'a AppState) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match self.stop {
                "panic" => panic!("shutdown bug"),
                "hang" => std::future::pending().await,
                _ => push(&self.log, "misbehaving down"),
            }
        })
    }
}

#[tokio::test]
async fn module_start_and_shutdown_are_bounded_and_contained() {
    let mut config = http_config();
    config.server.module_start_timeout_secs = 1;
    config.server.module_shutdown_timeout_secs = 1;
    let log: Log = Arc::default();
    for (start, expected) in [("panic", "start panicked"), ("hang", "did not finish within 1 s")] {
        let server = NetBackendServer::new(config.clone()).module(Lifecycle { name: "before", log: log.clone(), fail_start: false }).module(Misbehaving {
            start,
            stop: "ok",
            log: log.clone(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let result = tokio::time::timeout(Duration::from_secs(10), server.serve_with_shutdown(listener, std::future::pending())).await.expect("returns");
        assert!(matches!(&result, Err(Error::Startup(m)) if m.contains(expected)), "{result:?}");
    }
    assert_eq!(lines(&log), ["start before", "shutdown before", "start before", "shutdown before"]);

    // A shutdown that panics or hangs does not stop the others or the stop itself.
    for stop_kind in ["panic", "hang"] {
        let log: Log = Arc::default();
        let server = NetBackendServer::new(config.clone()).module(Lifecycle { name: "first", log: log.clone(), fail_start: false }).module(Misbehaving {
            start: "ok",
            stop: stop_kind,
            log: log.clone(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let (stop, stopped) = oneshot::channel::<()>();
        let running = tokio::spawn(server.serve_with_shutdown(listener, async move {
            let _ = stopped.await;
        }));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = stop.send(());
        let result = tokio::time::timeout(Duration::from_secs(8), running).await.expect("stopped").expect("task");
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(lines(&log), ["start first", "shutdown first"], "{stop_kind}");
    }
}

#[tokio::test]
async fn a_failing_start_shuts_down_what_started() {
    let log: Log = Arc::default();
    let server = NetBackendServer::new(http_config())
        .module(Lifecycle { name: "good", log: log.clone(), fail_start: false })
        .module(Lifecycle { name: "bad", log: log.clone(), fail_start: true })
        .module(Lifecycle { name: "never", log: log.clone(), fail_start: false });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let result = tokio::time::timeout(Duration::from_secs(10), server.serve_with_shutdown(listener, std::future::pending())).await.expect("returns");
    assert!(matches!(&result, Err(Error::Startup(m)) if m.contains("module `bad`")), "{result:?}");
    assert_eq!(lines(&log), ["start good", "shutdown good"]);

    // A failing start hook too.
    let server = NetBackendServer::new(http_config()).on_start(|_ctx| async { Err(net_backend_server::AppError::conflict("no")) });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let result = tokio::time::timeout(Duration::from_secs(10), server.serve_with_shutdown(listener, std::future::pending())).await.expect("returns");
    assert!(matches!(&result, Err(Error::Startup(m)) if m.contains("start hook 0 failed")), "{result:?}");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn migrate_on_start() {
    struct Notes;
    impl Module for Notes {
        fn name(&self) -> &'static str {
            "notes"
        }
        fn migrations(&self, _dialect: net_backend_server::Dialect) -> Vec<net_backend_server::Migration> {
            vec![net_backend_server::Migration::new(1, "create_notes", "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)")]
        }
    }
    let dir = common::temp_dir("serve-migrate");
    let url = format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"));
    let mut config = http_config();
    config.database.url = net_backend_server::SecretString::new(url.clone());
    config.database.migrate_on_start = true;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(NetBackendServer::new(config.clone()).module(Notes).serve_with_shutdown(listener, async move {
        let _ = stopped.await;
    }));
    wait_until_up(addr).await;
    let _ = stop.send(());
    let result = tokio::time::timeout(Duration::from_secs(5), running).await.expect("stopped").expect("task");
    assert!(result.is_ok(), "{result:?}");
    let db = net_backend_server::Db::connect(&config.database).await.expect("connect");
    db.execute_script("INSERT INTO notes (id, body) VALUES (1, 'x')").await.expect("the table exists");
    db.close().await;
}
