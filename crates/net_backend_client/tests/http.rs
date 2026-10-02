//! The async client against the real server on loopback: typed calls, the session (register,
//! login, refresh before expiry, single-flight refresh, `token_expired` → refresh → one retry,
//! rotation + reuse, logout), errors (404 / 409 / 422 / 403 / 429), limits, and the honest
//! "never sent" answers.

mod common;

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use common::{email, Server, PASSWORD};
use net_backend_client::protocol::admin::GetUser;
use net_backend_client::protocol::auth::{GetAccount, LoginRequest, RefreshRequest, RegisterRequest, TokenPair};
use net_backend_client::protocol::chat::ListRooms;
use net_backend_client::protocol::storage::{
    BatchGet, GetObject, ListObjects, ObjectRef, ObjectVersion, PutObject, RemoveObject, VersionConflict, WriteObject,
};
use net_backend_client::protocol::{codes, UnixMillis, UserId, PROTOCOL_VERSION};
use net_backend_client::{Client, Error};
use serde_json::json;

async fn registered(server: &Server, name: &str) -> Client {
    let client = server.client();
    client.register(RegisterRequest::new(email(name), PASSWORD).with_display_name(name)).await.expect("register");
    client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_calls_and_api_errors() {
    let server = Server::start();
    let client = server.client();
    let info = client.info().await.expect("info");
    assert!(info.supports(PROTOCOL_VERSION) && info.has_module("storage") && info.has_module("chat"));
    assert!(matches!(client.call(&GetAccount::new()).await, Err(Error::NotLoggedIn)), "an authed call without a session is never sent");

    let mut updates = client.token_updates();
    let session = client.register(RegisterRequest::new(email("ada"), PASSWORD).with_display_name("Ada")).await.expect("register");
    assert!(matches!(updates.try_changed(), Some(Some(_))), "the new pair is reported");
    let me = client.call(&GetAccount::new()).await.expect("me");
    assert_eq!((me.id, me.display_name.as_deref()), (session.account.id, Some("Ada")));

    // Storage: create, read, a stale conditional write (409 with the protocol's details), list, batch, delete.
    let ack = client.call(&WriteObject::new("saves", "slot-1", PutObject::new(json!({"level": 1})).if_version(ObjectVersion::ABSENT))).await.expect("create");
    assert_eq!(ack.version, ObjectVersion::new(1));
    client.call(&WriteObject::new("saves", "slot-1", PutObject::new(json!({"level": 2})))).await.expect("overwrite");
    let save = client.call(&GetObject::new("saves", "slot-1")).await.expect("read");
    assert_eq!((save.value["level"].as_i64(), save.version), (Some(2), ObjectVersion::new(2)));
    let stale = client.call(&WriteObject::new("saves", "slot-1", PutObject::new(json!({})).if_version(ObjectVersion::new(1)))).await.expect_err("stale");
    assert_eq!((stale.status(), stale.code()), (Some(409), Some(codes::VERSION_CONFLICT)));
    let details = stale.api_error().and_then(|e| e.details_as::<VersionConflict>());
    assert_eq!(details.and_then(|d| d.current_version), Some(ObjectVersion::new(2)));
    assert_eq!(stale.was_sent(), Some(true));
    let page = client.call(&ListObjects::new("saves")).await.expect("list");
    assert_eq!(page.items.len(), 1);
    let batch = client.call(&BatchGet::new(vec![ObjectRef::new("saves", "slot-1")])).await.expect("batch");
    assert_eq!(batch.objects.len(), 1);
    client.call(&RemoveObject::new("saves", "slot-1").if_version(ObjectVersion::new(2))).await.expect("delete");
    let missing = client.call(&GetObject::new("saves", "slot-1")).await.expect_err("gone");
    assert_eq!((missing.status(), missing.code()), (Some(404), Some(codes::NOT_FOUND)));

    // 422 with field messages; a path that would need escaping is never sent; 403 for an admin route.
    let invalid = client.call(&WriteObject::new("saves", "slot-2", PutObject::new(json!("x".repeat(300 * 1024))))).await.expect_err("too big a value");
    assert!(invalid.status() == Some(422) || invalid.status() == Some(413), "{invalid:?}");
    let unsafe_path = client.call(&GetObject::new("saves", "../etc")).await.expect_err("refused");
    assert!(matches!(unsafe_path, Error::InvalidRequest(_)) && unsafe_path.was_sent() == Some(false));
    let forbidden = client.call(&GetUser::new(UserId(1))).await.expect_err("not an admin");
    assert_eq!((forbidden.status(), forbidden.code()), (Some(403), Some(codes::FORBIDDEN)));
    let rooms = client.call(&ListRooms::new()).await.expect("rooms");
    assert!(rooms.items.iter().any(|room| room.key.as_deref() == Some("world")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_access_token_is_refreshed_before_it_expires_single_flight() {
    let server = Server::start();
    let client = server.client();
    client.login(LoginRequest::new(email("bob"), PASSWORD)).await.expect_err("no account yet");
    client.register(RegisterRequest::new(email("bob"), PASSWORD)).await.expect("register");
    let first: TokenPair = client.tokens().expect("tokens");
    // The same pair, but stored as if its access token had just expired: every caller wants a
    // refresh first.
    let mut stale = first.clone();
    stale.access_expires_at = UnixMillis(UnixMillis::now().get() - 1_000);
    client.resume(stale);
    // Twenty calls at once: ONE refresh for all of them.
    let calls: Vec<_> = (0..20)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.call(&GetAccount::new()).await })
        })
        .collect();
    for call in calls {
        call.await.expect("join").expect("me");
    }
    let second = client.tokens().expect("tokens");
    assert_ne!(second.refresh_token.expose(), first.refresh_token.expose(), "rotated");
    // The first refresh token again, inside the 30 s grace window: the server answers the SAME
    // pair it gave the client, so there was exactly one refresh.
    let plain = server.client();
    let grace = plain.call(&RefreshRequest::new(first.refresh_token.clone())).await.expect("grace answer");
    assert_eq!(grace.access_token.expose(), second.access_token.expose(), "exactly one refresh happened");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_expired_is_refreshed_and_retried_once() {
    let server = Server::start();
    let client = Client::builder(&server.base).refresh_margin(Duration::ZERO).build().expect("client");
    client.register(RegisterRequest::new(email("cyd"), PASSWORD)).await.expect("register");
    let before = client.tokens().expect("tokens");
    // The server's clock passes the access token's expiry; this machine's clock does not. With a zero
    // margin the client would not refresh for ~1 h by itself, so the refresh below can ONLY come from
    // the server's 401 `token_expired` (the live test could not reach this path: proactive refresh
    // always came first there).
    server.advance(Duration::from_secs(3601));
    let mut updates = client.token_updates();
    client.call(&GetAccount::new()).await.expect("401 token_expired -> refresh -> retry");
    let after = client.tokens().expect("tokens");
    assert_ne!(after.access_token.expose(), before.access_token.expose());
    assert!(matches!(updates.try_changed(), Some(Some(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_refresh_token_ends_the_session() {
    let server = Server::start();
    let client = registered(&server, "dee").await;
    let old = client.tokens().expect("tokens");
    client.refresh().await.expect("rotate");
    // Within the grace window the old token answers the same pair (a retry is harmless) ...
    let other = server.client();
    other.resume(old.clone());
    let same = other.refresh().await.expect("grace");
    assert_eq!(same.access_token.expose(), client.tokens().expect("tokens").access_token.expose());
    // ... after it, a reuse revokes the whole session.
    server.advance(Duration::from_secs(31));
    let stale = server.client();
    stale.resume(old);
    let mut updates = stale.token_updates();
    let ended = stale.refresh().await.expect_err("reused");
    assert!(matches!(&ended, Error::SessionEnded { code, .. } if code == codes::REFRESH_TOKEN_REUSED), "{ended:?}");
    assert!(ended.needs_login() && stale.tokens().is_none());
    assert!(matches!(updates.try_changed(), Some(None)), "the end is reported");
    // The family is revoked: the first client's session is over too.
    client.forget_session();
    client.resume(same);
    let gone = client.call(&GetAccount::new()).await.expect_err("revoked");
    assert!(gone.needs_login() || gone.status() == Some(401), "{gone:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_and_logout_everywhere() {
    let server = Server::start();
    let a = registered(&server, "eve").await;
    let b = server.client();
    b.login(LoginRequest::new(email("eve"), PASSWORD)).await.expect("second device");
    let old = a.tokens().expect("tokens");
    let mut updates = a.token_updates();
    a.logout().await.expect("logout");
    assert!(a.tokens().is_none() && matches!(updates.try_changed(), Some(None)));
    assert!(matches!(a.logout().await, Err(Error::NotLoggedIn)));
    // The old tokens no longer work.
    a.resume(old);
    assert!(a.call(&GetAccount::new()).await.expect_err("revoked").needs_login());
    // Everywhere: the other device is logged out too.
    let c = server.client();
    c.login(LoginRequest::new(email("eve"), PASSWORD)).await.expect("third device");
    c.logout_everywhere().await.expect("everywhere");
    let ended = b.call(&GetAccount::new()).await.expect_err("logged out elsewhere");
    assert!(ended.needs_login(), "{ended:?}");
    assert!(b.tokens().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limits_answer_429_with_retry_after() {
    let server = Server::start_with(|setup| setup.auth.rate_limits = true);
    let client = registered(&server, "fay").await;
    let mut limited = None;
    for _ in 0..20 {
        match client.login(LoginRequest::new(email("fay"), "wrong password!")).await {
            Err(error) if error.status() == Some(429) => {
                limited = Some(error);
                break;
            }
            Err(error) => assert_eq!(error.code(), Some(codes::INVALID_CREDENTIALS)),
            Ok(_) => panic!("a wrong password logged in"),
        }
    }
    let limited = limited.expect("a 429 after repeated failures");
    assert_eq!(limited.code(), Some(codes::RATE_LIMITED));
    assert!(limited.retry_after().is_some_and(|wait| wait > Duration::ZERO), "{limited:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_and_body_limits() {
    let server = Server::start_with(|setup| setup.storage.max_objects_per_user = 1);
    let client = Client::builder(&server.base).max_response_bytes(4096).build().expect("client");
    client.register(RegisterRequest::new(email("gus"), PASSWORD)).await.expect("register");
    client.call(&WriteObject::new("saves", "a", PutObject::new(json!({"blob": "x".repeat(20_000)})))).await.expect("one object");
    let quota = client.call(&WriteObject::new("saves", "b", PutObject::new(json!(1)))).await.expect_err("quota");
    assert_eq!((quota.status(), quota.code()), (Some(403), Some(codes::QUOTA_EXCEEDED)));
    let big = client.call(&GetObject::new("saves", "a")).await.expect_err("too large");
    assert!(matches!(big, Error::BodyTooLarge { limit: 4096, .. }) && big.was_sent() == Some(true), "{big:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_and_silent_servers() {
    // Nothing listens: never sent.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);
    let client = Client::new(&format!("http://127.0.0.1:{port}")).expect("client");
    let error = client.info().await.expect_err("refused");
    assert!(matches!(error, Error::Network { sent: Some(false), .. }), "{error:?}");
    // A server that accepts and never answers: the one deadline ends the call ("maybe sent").
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = silent.local_addr().expect("addr");
    let keep = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let held = Arc::clone(&keep);
    tokio::spawn(async move {
        while let Ok((socket, _)) = silent.accept().await {
            held.lock().await.push(socket);
        }
    });
    let client = Client::builder(&format!("http://{addr}")).timeout(Duration::from_millis(300)).build().expect("client");
    let error = client.info().await.expect_err("timeout");
    assert!(matches!(error, Error::Timeout { sent: None, .. }), "{error:?}");
}

#[test]
fn urls_and_runtimes_are_checked_without_panics() {
    assert!(matches!(Client::new("http://game.example.com"), Err(Error::InvalidRequest(_))), "plain http to a public host");
    assert!(Client::builder("http://game.example.com").allow_insecure_http(true).build().is_ok());
    assert!(Client::new("https://game.example.com").is_ok() && Client::new("http://localhost:8080").is_ok());
    assert!(matches!(Client::new("game.example.com"), Err(Error::InvalidRequest(_))));
    // An async call polled outside any tokio runtime: an error at the first poll, never a panic.
    let client = Client::new("https://game.example.com").expect("client");
    let mut call = Box::pin(client.info());
    let mut context = Context::from_waker(Waker::noop());
    match call.as_mut().poll(&mut context) {
        Poll::Ready(Err(Error::InvalidRequest(why))) => assert!(why.contains("tokio runtime"), "{why}"),
        other => panic!("expected InvalidRequest, got {:?}", other.map(|r| r.map(|_| ()))),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_never_show_in_debug_output() {
    let server = Server::start();
    let client = registered(&server, "hal").await;
    let tokens = client.tokens().expect("tokens");
    let text = format!("{client:?} {:?} {tokens:?}", client.token_updates());
    assert!(!text.contains(tokens.access_token.expose()) && !text.contains(tokens.refresh_token.expose()), "{text}");
}
