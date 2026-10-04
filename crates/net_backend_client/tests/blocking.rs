//! The blocking interface: blocking calls, `send` + `Reply::try_take` polled like a game loop,
//! `TokenUpdates::try_changed`, and blocking calls from tokio's `spawn_blocking` (and inside a runtime).

mod common;

use std::time::{Duration, Instant};

use common::{email, Server, PASSWORD, WAIT};
use net_backend_client::blocking::Client;
use net_backend_client::protocol::auth::{GetAccount, RegisterRequest};
use net_backend_client::protocol::storage::{GetObject, PutObject, WriteObject};
use net_backend_client::Error;
use serde_json::json;

#[test]
fn blocking_calls_and_a_polled_game_loop() {
    let server = Server::start();
    let client = Client::new(&server.base).expect("client");
    let mut updates = client.token_updates();
    let session = client.register(RegisterRequest::new(email("ivy"), PASSWORD)).expect("register");
    assert!(matches!(updates.try_changed(), Some(Some(_))));
    assert_eq!(client.call(&GetAccount::new()).expect("me").id, session.account.id);
    client.call(&WriteObject::new("saves", "slot-1", PutObject::new(json!({"hp": 7})))).expect("save");
    // A frame loop: the call never blocks; the answer is taken exactly once.
    let mut reply = client.send(GetObject::new("saves", "slot-1"));
    let deadline = Instant::now() + WAIT;
    let save = loop {
        if let Some(answer) = reply.try_take() {
            break answer.expect("load");
        }
        assert!(Instant::now() < deadline, "no answer");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(save.value["hp"].as_i64(), Some(7));
    assert!(reply.try_take().is_none() && reply.is_taken());
    assert_eq!(client.info().expect("info").protocol, net_backend_client::protocol::PROTOCOL_VERSION);
    client.logout().expect("logout");
    assert!(matches!(updates.try_changed(), Some(None)));
    assert!(matches!(client.call(&GetAccount::new()), Err(Error::NotLoggedIn)));
}

#[test]
fn blocking_from_spawn_blocking_and_inside_a_runtime_works() {
    let server = Server::start();
    let client = Client::new(&server.base).expect("client");
    client.register(RegisterRequest::new(email("kai"), PASSWORD)).expect("register");
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    let base = server.base.clone();
    runtime.block_on(async {
        // tokio's place for blocking work: everything works there (a new client, calls, Reply::wait).
        let inside = client.clone();
        let answers = tokio::task::spawn_blocking(move || {
            let fresh = Client::new(&base)?;
            let info = fresh.info()?;
            let me = inside.call(&GetAccount::new())?;
            let waited = inside.send(GetAccount::new()).wait()?;
            Ok::<_, Error>((info.protocol, me.id, waited.id))
        })
        .await
        .expect("join")
        .expect("blocking calls from spawn_blocking");
        assert_eq!(answers.1, answers.2);
        // Directly in async code it is not refused either (it stalls this worker until the answer,
        // never deadlocks: the client's work runs on its own thread); the async client is the right tool.
        assert!(client.info().is_ok());
        assert!(client.async_client().info().await.is_ok());
    });
}

#[cfg(feature = "ws")]
#[test]
fn a_blocking_websocket_polled_from_a_loop() {
    use common::{Echo, Note};
    use net_backend_client::ws::WsSettings;

    let server = Server::start();
    let client = Client::new(&server.base).expect("client");
    let session = client.register(RegisterRequest::new(email("jon"), PASSWORD)).expect("register");
    let ws = client.connect_ws(WsSettings::default()).expect("connect");
    let mut notes = ws.subscribe::<Note>();
    let mut reply = ws.request(&Echo { text: "hi".into() });
    server.push(session.account.id, Note { n: 5 });
    let deadline = Instant::now() + WAIT;
    let (mut echoed, mut note) = (None, None);
    while echoed.is_none() || note.is_none() {
        assert!(Instant::now() < deadline, "no answer / push");
        if let Some(answer) = reply.try_take() {
            echoed = Some(answer.expect("echo"));
        }
        if let Some(push) = notes.try_next() {
            note = Some(push.expect("note"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(echoed.map(|e| e.user), Some(session.account.id.get()));
    assert_eq!(note, Some(Note { n: 5 }));
    ws.close();
    let deadline = Instant::now() + WAIT;
    while !ws.is_closed() {
        assert!(Instant::now() < deadline, "not closed");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A server that accepts connections, reads, and never answers; counts accepted connections.
fn silent_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let accepted = std::sync::Arc::new(AtomicUsize::new(0));
    let count = std::sync::Arc::clone(&accepted);
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for socket in listener.incoming().flatten() {
            let mut socket = socket;
            let mut buffer = [0u8; 4096];
            let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = socket.read(&mut buffer);
            count.fetch_add(1, Ordering::SeqCst);
            held.push(socket);
        }
    });
    (base, accepted)
}

/// Poll `reply` like a game loop until its answer arrives (one overall deadline).
fn take<T>(reply: &mut net_backend_client::Reply<T>) -> Result<T, Error> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(answer) = reply.try_take() {
            return answer;
        }
        assert!(Instant::now() < deadline, "no answer");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_cancelled_send_is_answered_cancelled_with_an_honest_sent() {
    use net_backend_client::protocol::auth::{AccessToken, RefreshToken, TokenPair};
    use net_backend_client::protocol::UnixMillis;
    use std::sync::atomic::Ordering;

    let (base, accepted) = silent_server();
    let later = UnixMillis(UnixMillis::now().get() + 3_600_000);
    // Handed to the connection (the request went out; the server never answers): `sent: None`.
    let client = Client::new(&base).expect("client");
    client.resume(TokenPair::new(AccessToken::new("nbsa_fake_cancel"), later, RefreshToken::new("nbsr_fake_cancel"), later));
    let mut reply = client.send(GetAccount::new());
    let deadline = Instant::now() + WAIT;
    while accepted.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the request never reached the server");
        std::thread::sleep(Duration::from_millis(5));
    }
    reply.cancel();
    let error = take(&mut reply).expect_err("cancelled");
    assert!(matches!(error, Error::Cancelled { sent: None, .. }), "{error:?}");
    assert_eq!(error.was_sent(), None);
    // Still waiting for a token refresh (which itself hangs): never sent, `sent: Some(false)`.
    let waiting = Client::new(&base).expect("client");
    let expired = UnixMillis(UnixMillis::now().get() - 1_000);
    waiting.resume(TokenPair::new(AccessToken::new("nbsa_fake_cancel_2"), expired, RefreshToken::new("nbsr_fake_cancel_2"), later));
    let before = accepted.load(Ordering::SeqCst);
    let mut reply = waiting.send(GetAccount::new());
    let handle = reply.cancel_handle();
    while accepted.load(Ordering::SeqCst) == before {
        assert!(Instant::now() < deadline, "the refresh never reached the server");
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.cancel();
    let error = take(&mut reply).expect_err("cancelled");
    assert!(matches!(error, Error::Cancelled { sent: Some(false), .. }), "{error:?}");
    // An answered request: cancel changes nothing.
    let server = Server::start();
    let client = Client::new(&server.base).expect("client");
    let mut reply = client.send(net_backend_client::protocol::GetServerInfo::new());
    let info = take(&mut reply).expect("info");
    reply.cancel();
    assert_eq!(info.protocol, net_backend_client::protocol::PROTOCOL_VERSION);
}

#[test]
fn blocking_calls_with_their_own_deadline() {
    use net_backend_client::protocol::GetServerInfo;

    let (base, _) = silent_server();
    // The builder's 100 ms does not cut a call that has its own longer deadline.
    let client = Client::from_builder(net_backend_client::Client::builder(&base).timeout(Duration::from_millis(100))).expect("client");
    let started = Instant::now();
    let error = client.call_with_timeout(&GetServerInfo::new(), Duration::from_millis(600)).expect_err("timeout");
    assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
    assert!(started.elapsed() >= Duration::from_millis(550), "{:?}", started.elapsed());
    // Polled from a loop: a short own deadline answers `Timeout` without blocking the frame.
    let slow = Client::new(&base).expect("client");
    let started = Instant::now();
    let mut reply = slow.send_with_timeout(GetServerInfo::new(), Duration::from_millis(200));
    let error = take(&mut reply).expect_err("timeout");
    assert!(matches!(error, Error::Timeout { sent: None, .. }), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    // Against the real server both answer normally.
    let server = Server::start();
    let client = Client::new(&server.base).expect("client");
    let session = client.register(RegisterRequest::new(email("ida"), PASSWORD)).expect("register");
    assert_eq!(client.call_with_timeout(&GetAccount::new(), Duration::from_secs(30)).expect("me").id, session.account.id);
    let mut reply = client.send_with_timeout(GetAccount::new(), Duration::from_secs(30));
    assert_eq!(take(&mut reply).expect("me").id, session.account.id);
}
