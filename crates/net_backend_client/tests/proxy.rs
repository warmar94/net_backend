//! Proxies: HTTP calls and the WebSocket through a local HTTP CONNECT proxy (with and without
//! `Proxy-Authorization`), a loopback server reached directly, refused proxy settings, and a proxy
//! that is not there. The proxy is a small CONNECT forwarder on 127.0.0.1 that sends every tunnel
//! to the test server, whatever host the client asked for (the client never resolves that host).

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use common::{email, Server, PASSWORD};
use net_backend_client::protocol::auth::{GetAccount, RegisterRequest};
use net_backend_client::{Client, Error};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What the proxy saw.
#[derive(Default)]
struct Seen {
    tunnels: AtomicUsize,
    refused: AtomicUsize,
    requests: Mutex<Vec<String>>,
}

/// A CONNECT proxy on 127.0.0.1 that forwards every tunnel to `upstream`. With `credentials`
/// (`user:password`), a CONNECT without the matching `Proxy-Authorization: Basic` gets 407.
async fn connect_proxy(upstream: SocketAddr, credentials: Option<&'static str>) -> (SocketAddr, Arc<Seen>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Seen::default());
    let stats = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let stats = Arc::clone(&stats);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 8192 {
                    match socket.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                stats.requests.lock().unwrap_or_else(PoisonError::into_inner).push(text.lines().next().unwrap_or_default().to_string());
                if let Some(credentials) = credentials {
                    let expected = format!("proxy-authorization: basic {}", base64(credentials.as_bytes())).to_ascii_lowercase();
                    if !text.to_ascii_lowercase().lines().any(|line| line.trim() == expected) {
                        stats.refused.fetch_add(1, Ordering::SeqCst);
                        let _ = socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n").await;
                        return;
                    }
                }
                if !text.starts_with("CONNECT ") {
                    let _ = socket.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n").await;
                    return;
                }
                let Ok(mut server) = tokio::net::TcpStream::connect(upstream).await else { return };
                stats.tunnels.fetch_add(1, Ordering::SeqCst);
                if socket.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.is_err() {
                    return;
                }
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut server).await;
            });
        }
    });
    (addr, seen)
}

/// Standard base64 with padding (for the test proxy's expected header).
fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(TABLE[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The test server's address, and a base URL with a host that is NOT loopback (only the proxy
/// knows where it goes).
fn addresses(server: &Server) -> (SocketAddr, String) {
    let addr: SocketAddr = server.base.trim_start_matches("http://").parse().expect("server address");
    (addr, format!("http://game.test:{}", addr.port()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_calls_go_through_the_connect_proxy() {
    let server = Server::start();
    let (addr, base) = addresses(&server);
    let (proxy, seen) = connect_proxy(addr, None).await;
    let client = Client::builder(&base).allow_insecure_http(true).proxy(&format!("http://{proxy}")).build().expect("client");
    assert!(client.info().await.is_ok());
    client.register(RegisterRequest::new(email("pia"), PASSWORD)).await.expect("register through the proxy");
    client.call(&GetAccount::new()).await.expect("authed call through the proxy");
    assert!(seen.tunnels.load(Ordering::SeqCst) >= 1);
    let requests = seen.requests.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert!(requests.iter().all(|line| *line == format!("CONNECT game.test:{} HTTP/1.1", addr.port())), "{requests:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_credentials_are_sent_and_a_refusal_is_never_sent() {
    let server = Server::start();
    let (addr, base) = addresses(&server);
    let (proxy, seen) = connect_proxy(addr, Some("tester:fake-proxy-pass")).await;
    let client = Client::builder(&base).allow_insecure_http(true).proxy(&format!("http://tester:fake-proxy-pass@{proxy}")).build().expect("client");
    assert!(client.info().await.is_ok(), "Proxy-Authorization: Basic accepted");
    assert!(!format!("{client:?}").contains("fake-proxy-pass"));
    let without = Client::builder(&base).allow_insecure_http(true).proxy(&format!("http://{proxy}")).build().expect("client");
    let error = without.info().await.expect_err("407");
    assert!(matches!(&error, Error::Network { message, .. } if message.contains("proxy authorization required")), "{error:?}");
    assert_eq!(error.was_sent(), Some(false));
    assert!(seen.refused.load(Ordering::SeqCst) >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_is_direct_and_bad_proxies_are_refused() {
    let server = Server::start();
    // A loopback server never uses the proxy (here one that is not even there).
    let direct = Client::builder(&server.base).proxy("http://127.0.0.1:9").build().expect("client");
    assert!(direct.info().await.is_ok());
    // Only http:// proxies (HTTP CONNECT).
    for bad in ["socks5://127.0.0.1:1080", "https://proxy.example.com:443", "not a url"] {
        let refused = Client::builder("https://api.example.com").proxy(bad).build();
        assert!(matches!(refused, Err(Error::InvalidRequest(_))), "{bad}");
    }
    assert!(Client::builder("https://api.example.com").no_proxy().build().is_ok());
    // A proxy that is not there: never sent.
    let (_, base) = addresses(&server);
    let unreachable = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr")
    };
    let client = Client::builder(&base).allow_insecure_http(true).proxy(&format!("http://{unreachable}")).build().expect("client");
    let error = client.info().await.expect_err("no proxy there");
    assert!(matches!(error, Error::Network { sent: Some(false), .. }), "{error:?}");
}

#[cfg(feature = "ws")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_websocket_goes_through_the_connect_proxy() {
    use common::Echo;
    use net_backend_client::ws::{WsAuthMode, WsSettings};

    let server = Server::start();
    let (addr, base) = addresses(&server);
    let (proxy, seen) = connect_proxy(addr, Some("tester:fake-proxy-pass")).await;
    let client = Client::builder(&base).allow_insecure_http(true).proxy(&format!("http://tester:fake-proxy-pass@{proxy}")).build().expect("client");
    let session = client.register(RegisterRequest::new(email("wes"), PASSWORD)).await.expect("register");
    let before = seen.tunnels.load(Ordering::SeqCst);
    for mode in [WsAuthMode::Header, WsAuthMode::FirstMessage] {
        let ws = client.connect_ws(WsSettings::default().with_auth(mode)).await.expect("connect through the proxy");
        let echoed = ws.request(&Echo { text: "via proxy".into() }).await.expect("echo");
        assert_eq!(echoed.user, session.account.id.get());
        ws.close();
    }
    assert!(seen.tunnels.load(Ordering::SeqCst) >= before + 2, "each WebSocket opened its own tunnel");
    // A proxy that refuses: the connection is never made.
    let (refusing, _) = connect_proxy(addr, Some("tester:other")).await;
    let other = Client::builder(&base).allow_insecure_http(true).proxy(&format!("http://{refusing}")).build().expect("client");
    other.resume(client.tokens().expect("tokens"));
    let error = other.connect_ws(WsSettings::default()).await.expect_err("407");
    assert!(matches!(&error, Error::Network { sent: Some(false), message, .. } if message.contains("proxy")), "{error:?}");
}
