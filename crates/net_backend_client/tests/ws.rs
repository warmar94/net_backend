//! The WebSocket against the real server on loopback (header and first-message auth, typed
//! requests, pushes, chat with the protocol's own types, close codes and the reconnect rules, a
//! refused handshake token, exactly one answer per request when the link dies), plus two scripted
//! peers for what the real server never does (silence, a busy handshake).

mod common;

use std::time::Duration;

use std::sync::atomic::Ordering;

use common::{email, until, Echo, Note, Server, Slow, PASSWORD, SLOW_STARTED, WAIT};
use net_backend_client::protocol::auth::{AccessToken, LoginRequest, RefreshToken, RegisterRequest, TokenPair};
use net_backend_client::protocol::chat::{ChatMessage, JoinRoom, SendMessage};
use net_backend_client::protocol::{CloseCode, UnixMillis, UserId};
use net_backend_client::ws::{Reconnect, WsAuthMode, WsConnection, WsEvent, WsEvents, WsSettings, WsState};
use net_backend_client::{Client, Error};

async fn registered(server: &Server, name: &str) -> (Client, UserId) {
    let client = server.client();
    let session = client.register(RegisterRequest::new(email(name), PASSWORD).with_display_name(name)).await.expect("register");
    (client, session.account.id)
}

/// Fast reconnects for the tests (jitter off, small delays).
fn fast() -> WsSettings {
    WsSettings::default().with_reconnect(Reconnect::default().with_base(Duration::from_millis(20)).with_cap(Duration::from_millis(200)).with_jitter(false))
}

/// The next event matching `wanted`, with ONE overall deadline.
async fn event(events: &mut WsEvents, what: &str, wanted: impl Fn(&WsEvent) -> bool) -> WsEvent {
    let found = tokio::time::timeout(WAIT, async {
        while let Some(event) = events.next().await {
            if wanted(&event) {
                return Some(event);
            }
        }
        None
    })
    .await;
    match found {
        Ok(Some(event)) => event,
        Ok(None) => panic!("the events ended before {what}"),
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

async fn closed_with(events: &mut WsEvents) -> Option<Error> {
    match event(events, "Closed", |e| matches!(e, WsEvent::Closed { .. })).await {
        WsEvent::Closed { error } => error,
        _ => None,
    }
}

async fn echo(ws: &WsConnection, text: &str) -> Result<i64, Error> {
    ws.request(&Echo { text: text.into() }).await.map(|e| e.user)
}

/// Notifications over the WebSocket: the `notify.new` push as the protocol's type, and the
/// `notify.*` requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notifications_push_and_requests() {
    use net_backend_client::protocol::notifications::{CountNotifications, MarkNotifications, Notification, NotificationQuery};
    let server = Server::start();
    let (client, user) = registered(&server, "nt-kim").await;
    let ws = client.connect_ws(fast()).await.expect("connect");
    let mut notifications = ws.subscribe::<Notification>();
    let id = server.notify(user, "reward");
    let pushed = tokio::time::timeout(WAIT, notifications.next()).await.expect("push").expect("open").expect("notification");
    assert_eq!((pushed.id.get(), pushed.kind.as_str(), pushed.read), (id, "reward", false));
    let page = ws.request(&NotificationQuery::new().unread_only()).await.expect("list");
    assert_eq!(page.items.iter().map(|n| n.id.get()).collect::<Vec<_>>(), [id]);
    let ack = ws.request(&MarkNotifications::all_read()).await.expect("mark");
    assert_eq!((ack.changed, ack.unread), (1, 0));
    let count = ws.request(&CountNotifications::new()).await.expect("count");
    assert_eq!((count.unread, count.total), (0, 1));
    ws.close();
}

/// `friends.presence` through `ws.subscribe`: a friend's socket opens and closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn friend_presence_pushes() {
    use net_backend_client::protocol::friends::{AcceptFriend, AddFriend, FriendPresence};
    let server = Server::start();
    let (ada, ada_id) = registered(&server, "fp-ada").await;
    let (bo, bo_id) = registered(&server, "fp-bo").await;
    bo.call(&AddFriend::by_id(ada_id)).await.expect("request");
    ada.call(&AcceptFriend::new(bo_id)).await.expect("accept");
    let ws = ada.connect_ws(fast()).await.expect("connect");
    let mut presence = ws.subscribe::<FriendPresence>();
    let bo_ws = bo.connect_ws(fast()).await.expect("connect Bo");
    let online = tokio::time::timeout(WAIT, presence.next()).await.expect("push").expect("open").expect("presence");
    assert_eq!((online.user, online.online), (bo_id, true));
    bo_ws.close();
    let offline = tokio::time::timeout(WAIT, presence.next()).await.expect("push").expect("open").expect("presence");
    assert_eq!((offline.user, offline.online, offline.last_seen.is_some()), (bo_id, false, true));
    ws.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lobby_and_match_pushes() {
    use net_backend_client::protocol::lobbies::{CreateLobby, EditLobby, JoinLobby, LobbyChange, LobbyMemberUpdate, LobbyUpdate, MemberChange, UpdateLobby};
    use net_backend_client::protocol::matchmaking::{CreateTicket, MatchFound};
    let server = Server::start();
    let (ada, _ada_id) = registered(&server, "lp-ada").await;
    let (bo, bo_id) = registered(&server, "lp-bo").await;
    let ws = ada.connect_ws(fast()).await.expect("connect");
    let mut members = ws.subscribe::<LobbyMemberUpdate>();
    let mut changes = ws.subscribe::<LobbyUpdate>();
    let mut matches = ws.subscribe::<MatchFound>();
    let lobby = ada.call(&CreateLobby::new(2)).await.expect("create");
    bo.call(&JoinLobby::new(lobby.id)).await.expect("join");
    let joined = tokio::time::timeout(WAIT, members.next()).await.expect("push").expect("open").expect("member");
    assert_eq!((joined.lobby, joined.change, joined.member.user), (lobby.id, MemberChange::Joined, bo_id));
    ada.call(&EditLobby::new(lobby.id, UpdateLobby::new().set_meta("map", "dust"))).await.expect("edit");
    let changed = tokio::time::timeout(WAIT, changes.next()).await.expect("push").expect("open").expect("change");
    assert_eq!((changed.changes, changed.lobby.metadata.get("map").cloned()), (vec![LobbyChange::Metadata], Some("dust".to_string())));
    // Two tickets in the `duel` queue: the server's round matches them.
    let bo_ws = bo.connect_ws(fast()).await.expect("connect Bo");
    bo.call(&CreateTicket::new("duel")).await.expect("queue Bo");
    ada.call(&CreateTicket::new("duel")).await.expect("queue Ada");
    let found = tokio::time::timeout(WAIT, matches.next()).await.expect("push").expect("open").expect("match");
    assert_eq!((found.queue.as_str(), found.players.len()), ("duel", 2));
    assert!(found.players.contains(&bo_id));
    bo_ws.close();
    ws.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_extras_over_the_socket() {
    use net_backend_client::protocol::chat::{
        CreateRoom, EditMessage, InviteToRoom, MarkRead, MessageEdited, OpenDirect, ReadReceipt, RoomChange, RoomUpdate, RoomUser, SetTyping, TypingUpdate,
        UnreadQuery,
    };
    let server = Server::start();
    let (ada, _ada_id) = registered(&server, "cx-ada").await;
    let (bo, bo_id) = registered(&server, "cx-bo").await;
    let a = ada.connect_ws(fast()).await.expect("connect Ada");
    let b = bo.connect_ws(fast()).await.expect("connect Bo");
    let mut edits = b.subscribe::<MessageEdited>();
    let mut typing = b.subscribe::<TypingUpdate>();
    let mut reads = a.subscribe::<ReadReceipt>();
    let mut rooms = b.subscribe::<RoomUpdate>();
    let dm = ada.call(&OpenDirect::new(bo_id)).await.expect("dm");
    let ack = a.request(&SendMessage::new(dm.id, "helo")).await.expect("send");
    // Edit: the answer is the message, Bo gets chat.edited.
    let edited = a.request(&EditMessage::new(dm.id, ack.message_id, "hello")).await.expect("edit");
    assert_eq!((edited.text.as_str(), edited.edited_at.is_some()), ("hello", true));
    let push = tokio::time::timeout(WAIT, edits.next()).await.expect("push").expect("open").expect("edited");
    assert_eq!((push.id, push.text.as_str()), (ack.message_id, "hello"));
    // Typing (no join in a DM) and the read marker.
    a.request(&SetTyping::started(dm.id)).await.expect("typing");
    let push = tokio::time::timeout(WAIT, typing.next()).await.expect("push").expect("open").expect("typing");
    assert!(push.typing && push.expires_in_ms > 0);
    let unread = b.request(&UnreadQuery::new(vec![dm.id])).await.expect("unread");
    assert_eq!(unread.rooms[0].unread, 1);
    b.request(&MarkRead::new(dm.id, ack.message_id)).await.expect("read");
    let push = tokio::time::timeout(WAIT, reads.next()).await.expect("push").expect("open").expect("read");
    assert_eq!((push.user, push.message), (bo_id, ack.message_id));
    // A player room's invitation reaches the invited player.
    let room = ada.call(&CreateRoom::new("Client Den")).await.expect("create");
    ada.call(&InviteToRoom::new(room.id, RoomUser::new(bo_id))).await.expect("invite");
    let push = tokio::time::timeout(WAIT, rooms.next()).await.expect("push").expect("open").expect("room");
    assert_eq!((push.room, push.change, push.user), (room.id, RoomChange::Invited, Some(bo_id)));
    a.close();
    b.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_auth_requests_pushes_and_a_reconnect() {
    let server = Server::start();
    let (client, user) = registered(&server, "kim").await;
    let ws = client.connect_ws(fast()).await.expect("connect");
    assert_eq!(ws.state(), WsState::Connected);
    assert_eq!(echo(&ws, "hello").await.expect("echo"), user.get());
    let mut notes = ws.subscribe::<Note>();
    let mut all = ws.pushes();
    server.push(user, Note { n: 1 });
    assert_eq!(tokio::time::timeout(WAIT, notes.next()).await.expect("push").expect("open").expect("note"), Note { n: 1 });
    assert_eq!(all.next().await.expect("open").expect("raw").kind, "test.note");
    // 1001 (a redeploy): the client comes back by itself and tells the app to resync.
    let mut events = ws.events();
    assert_eq!(server.close_user(user, CloseCode::GOING_AWAY), 1);
    event(&mut events, "a reconnect", |e| matches!(e, WsEvent::Connected { reconnected: true })).await;
    assert_eq!(echo(&ws, "again").await.expect("echo after the reconnect"), user.get());
    // A request over the message limit is refused before anything is sent.
    let big = ws.request(&Echo { text: "x".repeat(2 * 1024 * 1024) }).await.expect_err("too large");
    assert!(matches!(big, Error::RequestTooLarge { .. }) && big.was_sent() == Some(false));
    ws.close();
    assert!(closed_with(&mut events).await.is_none(), "closed by the app");
    assert_eq!(ws.state(), WsState::Closed);
    let after = echo(&ws, "late").await.expect_err("closed");
    assert!(matches!(after, Error::Disconnected { sent: Some(false), .. }), "{after:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_message_auth_and_both() {
    let server = Server::start();
    let (client, user) = registered(&server, "lee").await;
    for mode in [WsAuthMode::FirstMessage, WsAuthMode::Both] {
        let ws = client.connect_ws(WsSettings::default().with_auth(mode)).await.expect("connect");
        assert_eq!(echo(&ws, "hi").await.expect("echo"), user.get(), "{mode:?}");
        ws.close();
        ws.closed().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_with_the_protocol_types() {
    let server = Server::start();
    let (client, user) = registered(&server, "max").await;
    let ws = client.connect_ws(WsSettings::default()).await.expect("connect");
    let mut messages = ws.subscribe::<ChatMessage>();
    let room = ws.request(&JoinRoom::new("world")).await.expect("join");
    let ack = ws.request(&SendMessage::new(room.id, "hello world")).await.expect("send");
    let message = tokio::time::timeout(WAIT, messages.next()).await.expect("echo push").expect("open").expect("decoded");
    assert_eq!((message.id, message.sender, message.text.as_str()), (ack.message_id, user, "hello world"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_4001_gets_one_refresh_and_one_reconnect() {
    let server = Server::start();
    let (client, user) = registered(&server, "ned").await;
    let ws = client.connect_ws(fast()).await.expect("connect");
    let mut events = ws.events();
    let before = client.tokens().expect("tokens");
    // The client counts as connected before the server's socket task registered it: a close sent
    // in that window reaches nobody (CI failure, 2026-10-02). Wait for the server side first.
    until("the socket to be registered", || server.connections_of(user) == 1).await;
    // A revoked-token close while the session is still valid: refresh, connect again at once.
    assert_eq!(server.close_user(user, CloseCode::UNAUTHORIZED), 1);
    event(&mut events, "the reconnect after 4001", |e| matches!(e, WsEvent::Connected { reconnected: true })).await;
    assert_ne!(client.tokens().expect("tokens").access_token.expose(), before.access_token.expose(), "refreshed");
    assert_eq!(echo(&ws, "back").await.expect("echo"), user.get());
    // A second 4001 right away: final, no more tries.
    server.close_user(user, CloseCode::UNAUTHORIZED);
    let error = closed_with(&mut events).await.expect("why");
    assert_eq!(error.close_code(), Some(CloseCode::UNAUTHORIZED), "{error:?}");
    assert_eq!(server.connections_of(user), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_4001_without_a_reconnect_policy_ends_at_once() {
    let server = Server::start();
    let (client, user) = registered(&server, "nell").await;
    let ws = client.connect_ws(WsSettings::default().without_reconnect()).await.expect("connect");
    let mut events = ws.events();
    let before = client.tokens().expect("tokens");
    until("the socket to be registered", || server.connections_of(user) == 1).await;
    assert_eq!(server.close_user(user, CloseCode::UNAUTHORIZED), 1);
    let error = closed_with(&mut events).await.expect("why");
    assert_eq!(error.close_code(), Some(CloseCode::UNAUTHORIZED), "{error:?}");
    assert_eq!(client.tokens().expect("tokens").access_token.expose(), before.access_token.expose(), "no refresh, no reconnect");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.connections_of(user), 0);
}

/// On a current-thread runtime the connection's task runs on this thread: a blocking `wait` for
/// one of its replies is refused at once (it would never end); `.await` works.
#[tokio::test(flavor = "current_thread")]
async fn waiting_for_a_reply_on_a_current_thread_runtime_is_refused() {
    let server = Server::start();
    let (client, user) = registered(&server, "cora").await;
    let ws = client.connect_ws(WsSettings::default()).await.expect("connect");
    let refused = ws.request(&Echo { text: "wait".into() }).wait();
    assert!(matches!(refused, Err(Error::InvalidRequest(_))), "{refused:?}");
    assert_eq!(echo(&ws, "await").await.expect("echo"), user.get());
    ws.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_ends_the_socket_for_good() {
    let server = Server::start();
    let (client, user) = registered(&server, "ola").await;
    let ws = client.connect_ws(fast()).await.expect("connect");
    let mut events = ws.events();
    let other = server.client();
    other.login(LoginRequest::new(email("ola"), PASSWORD)).await.expect("login");
    other.logout_everywhere().await.expect("logout everywhere");
    let error = closed_with(&mut events).await.expect("why");
    assert!(error.needs_login(), "{error:?}");
    assert!(client.tokens().is_none(), "the refused refresh ended the session");
    // Nothing comes back.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.connections_of(user), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ban_and_replacement_are_final() {
    let server = Server::start_with(|setup| setup.config.ws.max_connections_per_user = 1);
    let (client, user) = registered(&server, "pia").await;
    let first = client.connect_ws(fast()).await.expect("connect");
    let mut first_events = first.events();
    let second = client.connect_ws(fast()).await.expect("connect again");
    let error = closed_with(&mut first_events).await.expect("replaced");
    assert_eq!(error.close_code(), Some(CloseCode::REPLACED));
    assert_eq!(first.state(), WsState::Closed, "4009 is final");
    let mut events = second.events();
    server.ban(user);
    let error = closed_with(&mut events).await.expect("banned");
    assert!(error.close_code() == Some(CloseCode::BANNED) || error.is(net_backend_client::protocol::codes::BANNED), "{error:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.connections_of(user), 0, "never reconnected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_token_handshake_is_refreshed_once() {
    let server = Server::start();
    let client = Client::builder(&server.base).refresh_margin(Duration::ZERO).build().expect("client");
    let session = client.register(RegisterRequest::new(email("quin"), PASSWORD)).await.expect("register");
    let before = client.tokens().expect("tokens");
    server.advance(Duration::from_secs(3601));
    for mode in [WsAuthMode::Header, WsAuthMode::FirstMessage] {
        let ws = client.connect_ws(WsSettings::default().with_auth(mode)).await.expect("401 token_expired -> refresh -> connect");
        assert_eq!(echo(&ws, "ok").await.expect("echo"), session.account.id.get());
        ws.close();
        server.advance(Duration::from_secs(3601));
    }
    assert_ne!(client.tokens().expect("tokens").access_token.expose(), before.access_token.expose());
    // A token nobody knows (and no way to refresh it): final, with the server's code.
    let stranger = server.client();
    stranger.resume(fake_pair());
    let error = stranger.connect_ws(WsSettings::default()).await.expect_err("refused");
    assert!(error.needs_login() || error.status() == Some(401), "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_request_gets_exactly_one_answer_when_the_link_dies() {
    let server = Server::start();
    let (client, user) = registered(&server, "rex").await;
    let settings = WsSettings::default()
        .with_reconnect(Reconnect::default().with_base(Duration::from_millis(400)).with_cap(Duration::from_millis(400)).with_jitter(false));
    let ws = client.connect_ws(settings).await.expect("connect");
    let mut events = ws.events();
    // In flight when the link dies: answered Disconnected, sent (never resent).
    let started = SLOW_STARTED.load(Ordering::SeqCst);
    let running = ws.request(&Slow { millis: 3_000 });
    until("the slow request to reach the server", || SLOW_STARTED.load(Ordering::SeqCst) > started).await;
    server.close_user(user, CloseCode::INTERNAL_ERROR);
    let lost = running.await.expect_err("lost");
    assert!(matches!(lost, Error::Disconnected { sent: Some(true), .. }), "{lost:?}");
    // Made while reconnecting: waits and goes out on the new link.
    event(&mut events, "reconnecting", |e| matches!(e, WsEvent::Reconnecting { .. })).await;
    let waiting = ws.request(&Echo { text: "queued".into() });
    assert_eq!(waiting.await.expect("sent after the reconnect").user, user.get());
    // A request the server does not answer in time.
    let late = ws.request_with_timeout(&Slow { millis: 2_000 }, Duration::from_millis(100)).await.expect_err("timeout");
    assert!(matches!(late, Error::Timeout { sent: Some(true), .. }), "{late:?}");
}

/// A token pair the server never issued (far from expiry, so no refresh is tried first).
fn fake_pair() -> TokenPair {
    let later = UnixMillis(UnixMillis::now().get() + 3_600_000);
    TokenPair::new(AccessToken::new("nbsa_not_a_real_token"), later, RefreshToken::new("nbsr_not_a_real_token"), later)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_peer_is_dead_after_the_heartbeat_limit() {
    use tokio_tungstenite_for_tests::*;
    let (addr, accepted) = silent_peer().await;
    let client = Client::new(&format!("http://{addr}")).expect("client");
    client.resume(fake_pair());
    let settings = WsSettings::default().with_heartbeat(Duration::from_millis(50), Duration::from_millis(300)).without_reconnect();
    let ws = client.connect_ws(settings).await.expect("connect");
    let mut events = ws.events();
    let error = closed_with(&mut events).await.expect("dead");
    assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
    assert!(accepted.load(std::sync::atomic::Ordering::SeqCst) >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_handshake_carries_retry_after() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let body = r#"{"error":{"code":"unavailable","message":"full"}}"#;
            let answer =
                format!("HTTP/1.1 503 Service Unavailable\r\nRetry-After: 7\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    let client = Client::new(&format!("http://{addr}")).expect("client");
    client.resume(fake_pair());
    let error = client.connect_ws(WsSettings::default()).await.expect_err("busy");
    assert_eq!((error.status(), error.retry_after()), (Some(503), Some(Duration::from_secs(7))), "{error:?}");
}

/// A WebSocket peer that accepts the handshake and then never sends or reads anything.
mod tokio_tungstenite_for_tests {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    pub async fn silent_peer() -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accepted);
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                if let Ok(ws) = tokio_tungstenite::accept_async(socket).await {
                    count.fetch_add(1, Ordering::SeqCst);
                    held.push(ws);
                }
            }
        });
        (addr, accepted)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_request_is_answered_cancelled_with_an_honest_sent() {
    let server = Server::start();
    let (client, user) = registered(&server, "cal").await;
    // A 2 s pause before the reconnect: the request below is made and cancelled long before.
    let settings =
        WsSettings::default().with_reconnect(Reconnect::default().with_base(Duration::from_secs(2)).with_cap(Duration::from_secs(2)).with_jitter(false));
    let ws = client.connect_ws(settings).await.expect("connect");
    let mut events = ws.events();
    // Written to the connection (the server runs it): Cancelled, sent; the late answer is dropped.
    let started = SLOW_STARTED.load(Ordering::SeqCst);
    let running = ws.request(&Slow { millis: 1_000 });
    until("the slow request to reach the server", || SLOW_STARTED.load(Ordering::SeqCst) > started).await;
    running.cancel();
    let error = running.await.expect_err("cancelled");
    assert!(matches!(error, Error::Cancelled { sent: Some(true), .. }), "{error:?}");
    assert_eq!(echo(&ws, "still fine").await.expect("echo"), user.get());
    // Waiting for the connection (it reconnects): Cancelled, never sent.
    server.close_user(user, CloseCode::INTERNAL_ERROR);
    event(&mut events, "reconnecting", |e| matches!(e, WsEvent::Reconnecting { .. })).await;
    let waiting = ws.request(&Echo { text: "never".into() });
    let handle = waiting.cancel_handle();
    handle.cancel();
    let error = waiting.await.expect_err("cancelled");
    assert!(matches!(error, Error::Cancelled { sent: Some(false), .. }), "{error:?}");
    assert_eq!(error.was_sent(), Some(false));
    // A reply keeps no connection open: dropping the connection closes it.
    assert_eq!(echo(&ws, "after").await.expect("echo after the reconnect"), user.get());
    let reply = ws.request(&Echo { text: "kept".into() });
    drop(ws);
    let _ = reply.await;
    reply_less_close(&server, user).await;
}

/// Every connection of `user` is gone once the app dropped its handles (replies do not keep one).
async fn reply_less_close(server: &Server, user: UserId) {
    until("the connection to close", || server.connections_of(user) == 0).await;
}
