//! A measurement, not a check (ignored by default): the server's heap per idle, authenticated
//! WebSocket with the shipped `[ws]` defaults. The server runs in a child process (this test
//! binary again) with a counting allocator, so the client sockets do not count. Loopback only,
//! bounded (the child exits after 120 s at most).
//!
//! ```text
//! cargo test -p net_backend_server --release --no-default-features --features sqlite --test ws_memory -- --ignored --nocapture
//! ```
//!
//! It counts requested heap bytes (not RSS, not kernel socket buffers). It needs ~2 descriptors
//! per socket in the test process plus one in the child: with a low limit (`ulimit -n 1024`,
//! common on Linux) run `ulimit -n 8192` first, or set `NBS_WS_MEMORY_SOCKETS` lower; the test
//! skips with a message when it runs out of descriptors.
#![cfg(feature = "sqlite")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use http::request::Parts;
use net_backend_server::protocol::UserId;
use net_backend_server::{AppError, AppState, AuthContext, Authenticator, Config, NetBackendServer, SecretString};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

struct Counting;

static HEAP: AtomicIsize = AtomicIsize::new(0);

// SAFETY: forwards every call to the system allocator unchanged; only counts sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP.fetch_add(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: the caller's contract is passed on unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HEAP.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: the caller's contract is passed on unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        HEAP.fetch_add(new_size as isize - layout.size() as isize, Ordering::Relaxed);
        // SAFETY: the caller's contract is passed on unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const CHILD_ENV: &str = "NBS_WS_MEMORY_CHILD";
const SOCKETS: usize = 2000;

/// `Bearer <n>` = user n (no database: the measurement is about the hub).
struct NumberAuth;

impl Authenticator for NumberAuth {
    fn authenticate<'a>(&'a self, parts: &'a Parts, _state: &'a AppState) -> BoxFuture<'a, Result<Option<AuthContext>, AppError>> {
        Box::pin(async move {
            let user = parts.headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).and_then(|n| n.parse().ok());
            Ok(user.map(|n| AuthContext::new(UserId(n))))
        })
    }
}

/// The child: serve, print `PORT`, then `STAT <sockets> <heap bytes>` twice a second.
#[ignore = "helper process of ws_heap_per_idle_socket"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_memory_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let mut config = Config::default();
    config.database.url = SecretString::new("sqlite::memory:");
    config.ws.handshakes_per_ip_per_minute = 0;
    config.ws.max_connections_per_ip = 100_000;
    let prepared =
        NetBackendServer::new(config).authenticator(NumberAuth).ws_handler("probe.echo", |_ctx, data| async move { Ok(data) }).build().await.expect("build");
    let state = prepared.state().clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    println!("PORT {}", listener.local_addr().expect("addr").port());
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            println!("STAT {} {}", state.ws().stats().authenticated, HEAP.load(Ordering::Relaxed));
        }
    });
    let _ = tokio::time::timeout(Duration::from_secs(120), prepared.serve(listener)).await;
}

fn parse_stat(line: &str) -> Option<(usize, i64)> {
    let mut parts = line.split_once("STAT ")?.1.split(' ');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

#[ignore = "a measurement: run in release with --nocapture"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_heap_per_idle_socket() {
    let exe = std::env::current_exe().expect("exe");
    let mut child = tokio::process::Command::new(exe)
        .args(["--exact", "ws_memory_child", "--ignored", "--nocapture", "--test-threads", "1"])
        .env(CHILD_ENV, "1")
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the child");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    let port: u16 = loop {
        let line = lines.next_line().await.expect("read").expect("the child ended");
        // libtest prints "test ws_memory_child ... " on the same line first.
        if let Some((_, port)) = line.split_once("PORT ") {
            break port.trim().parse().expect("port");
        }
    };
    // The baseline: the server is up, no socket yet.
    let baseline = loop {
        let line = lines.next_line().await.expect("read").expect("the child ended");
        if let Some((0, heap)) = parse_stat(&line) {
            break heap;
        }
    };
    let sockets_wanted: usize = std::env::var("NBS_WS_MEMORY_SOCKETS").ok().and_then(|v| v.parse().ok()).unwrap_or(SOCKETS);
    let mut sockets = Vec::with_capacity(sockets_wanted);
    for n in 0..sockets_wanted {
        let mut request = format!("ws://127.0.0.1:{port}/v1/ws").into_client_request().expect("request");
        request.headers_mut().insert("authorization", format!("Bearer {}", n + 1).parse().expect("header"));
        let mut ws = match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _)) => ws,
            Err(error) if error.to_string().contains("Too many open files") || error.to_string().contains("os error 24") => {
                println!("ws_heap_per_idle_socket SKIPPED: out of file descriptors after {n} sockets; run `ulimit -n 8192` (or set NBS_WS_MEMORY_SOCKETS lower) and retry");
                let _ = child.kill().await;
                return;
            }
            Err(error) => panic!("connect: {error}"),
        };
        // One request and its answer, so the socket went through a read and a write.
        ws.send(Message::text(r#"{"id":1,"type":"probe.echo","data":{"text":"hi"}}"#)).await.expect("send");
        let _ = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("answer");
        sockets.push(ws);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = (0usize, 0i64);
    while Instant::now() < deadline {
        let Some(line) = lines.next_line().await.expect("read") else { break };
        if let Some(stat) = parse_stat(&line) {
            last = stat;
            if stat.0 == sockets_wanted {
                break;
            }
        }
    }
    assert_eq!(last.0, sockets_wanted, "not every socket is registered");
    let per_socket = (last.1 - baseline) as f64 / sockets_wanted as f64 / 1024.0;
    println!(
        "ws heap: {sockets_wanted} idle authenticated sockets, server heap {:.1} MiB (baseline {:.1} MiB), {per_socket:.1} KiB per socket",
        last.1 as f64 / 1_048_576.0,
        baseline as f64 / 1_048_576.0
    );
    drop(sockets);
    let _ = child.kill().await;
    assert!(per_socket < 64.0, "more than 64 KiB per idle socket: {per_socket:.1}");
}
