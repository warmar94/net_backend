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
