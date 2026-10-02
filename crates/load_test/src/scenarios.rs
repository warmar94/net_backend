//! The load scenarios. Each prints one result (see [`crate::stats::report`]).

use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use http::Method;
use net_backend_protocol::auth::{AuthSession, LoginRequest, RegisterRequest};
use net_backend_protocol::chat::{ChatMessage, JoinRoom, OpenDirect, RoomInfo, SendMessage};
use net_backend_protocol::storage::{BatchPut, BatchPutItem, GetObject, PutObject, WriteObject};
use net_backend_protocol::{auth::GetAccount, RoomId, UserId, WsRequestFrame, WsServerFrame};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{watch, Barrier, Semaphore};
use tokio_tungstenite::tungstenite::Message;

use crate::net::{Http, Target, Ws};
use crate::stats::{now_ms, report, Samples, Tally};

/// One prepared account (a line of the users file).
#[derive(Clone, Serialize, Deserialize)]
pub struct User {
    pub email: String,
    pub user_id: i64,
    pub access_token: String,
}

/// Read the users file written by `users`.
pub fn load_users(path: &Path, needed: usize) -> Result<Vec<User>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e} (run `users` first)", path.display()))?;
    let users: Vec<User> =
        text.lines().filter(|l| !l.trim().is_empty()).map(serde_json::from_str).collect::<Result<_, _>>().map_err(|e| format!("{}: {e}", path.display()))?;
    if users.len() < needed {
        return Err(format!("{} holds {} users; this run needs {needed} (run `users --count {needed}`)", path.display(), users.len()));
    }
    Ok(users)
}

fn seconds(d: Duration) -> f64 {
    (d.as_secs_f64() * 100.0).round() / 100.0
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

// ---- users --------------------------------------------------------------------------------------

/// Register `count` accounts (or log in where the address exists) and write their tokens.
pub async fn users(target: &Target, out: &Path, count: usize, prefix: &str, password: &str, concurrency: usize) -> Result<(), String> {
    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let errors = Tally::default();
    let (registered, logged_in) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let latency = Samples::default();
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(count);
    for i in 0..count {
        let (target, sem, errors, registered, logged_in, latency) =
            (target.clone(), sem.clone(), errors.clone(), registered.clone(), logged_in.clone(), latency.clone());
        let (email, password, name) = (format!("{prefix}-{i}@example.com"), password.to_string(), format!("{prefix} {i}"));
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.ok()?;
            let mut http = Http::new(&target);
            let t0 = Instant::now();
            let register = RegisterRequest::new(email.clone(), password.clone()).with_display_name(name);
            let answer = match http.call(&register, None).await {
                Ok(answer) => answer,
                Err(e) => {
                    errors.add(e);
                    return None;
                }
            };
            let answer = if answer.status.is_success() {
                registered.fetch_add(1, Ordering::Relaxed);
                answer
            } else if answer.error_code() == "email_taken" {
                match http.call(&LoginRequest::new(email.clone(), password), None).await {
                    Ok(a) if a.status.is_success() => {
                        logged_in.fetch_add(1, Ordering::Relaxed);
                        a
                    }
                    Ok(a) => {
                        errors.add(format!("login {}", a.error_code()));
                        return None;
                    }
                    Err(e) => {
                        errors.add(e);
                        return None;
                    }
                }
            } else {
                errors.add(format!("register {}", answer.error_code()));
                return None;
            };
            latency.add(micros(t0.elapsed()));
            match answer.json::<AuthSession>() {
                Ok(session) => Some(User { email, user_id: session.account.id.get(), access_token: session.tokens.access_token.expose().to_string() }),
                Err(e) => {
                    errors.add(e);
                    None
                }
            }
        }));
    }
    let mut lines = String::new();
    let mut written = 0usize;
    for task in tasks {
        if let Ok(Some(user)) = task.await {
            lines.push_str(&serde_json::to_string(&user).map_err(|e| e.to_string())?);
            lines.push('\n');
            written += 1;
        }
    }
    std::fs::write(out, lines).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    report(
        "users",
        json!({
            "requested": count, "written": written, "file": out.display().to_string(),
            "registered": registered.load(Ordering::Relaxed), "logged_in": logged_in.load(Ordering::Relaxed),
            "errors": errors.snapshot(), "seconds": seconds(started.elapsed()), "latency": latency.summary(),
        }),
    );
    Ok(())
}

// ---- sockets ------------------------------------------------------------------------------------

/// Open `count` authenticated WebSockets (users round-robin), hold them, count what closes them.
/// With `reconnect`, a socket the server closes during the hold (a restart: 1001) connects again at
/// once, like a client would: a reconnect storm, measured separately.
pub async fn sockets(target: &Target, users: &[User], count: usize, hold: Duration, concurrency: usize, reconnect: bool) -> Result<(), String> {
    // The server's default cap is 5 sockets per account (`ws.max_connections_per_user`).
    if count > users.len() * 5 {
        eprintln!(
            "sockets: {count} sockets over {} accounts is more than 5 per account; the server closes the oldest (4009) unless ws.max_connections_per_user is raised",
            users.len()
        );
    }
    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let (errors, closes, reconnect_errors) = (Tally::default(), Tally::default(), Tally::default());
    let (connected, attempted, reconnected) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (latency, reconnect_latency) = (Samples::default(), Samples::default());
    let started = Instant::now();
    let end = tokio::time::Instant::now() + hold;
    let mut tasks = Vec::with_capacity(count);
    for i in 0..count {
        let token = users[i % users.len()].access_token.clone();
        let (target, sem, errors, closes, reconnect_errors) = (target.clone(), sem.clone(), errors.clone(), closes.clone(), reconnect_errors.clone());
        let (connected, attempted, reconnected, latency, reconnect_latency) =
            (connected.clone(), attempted.clone(), reconnected.clone(), latency.clone(), reconnect_latency.clone());
        tasks.push(tokio::spawn(async move {
            let permit = sem.acquire_owned().await;
            let t0 = Instant::now();
            let ws = target.ws(&token).await;
            drop(permit);
            attempted.fetch_add(1, Ordering::Relaxed);
            let mut ws = match ws {
                Ok(ws) => ws,
                Err(e) => {
                    errors.add(e);
                    return;
                }
            };
            latency.add(micros(t0.elapsed()));
            connected.fetch_add(1, Ordering::Relaxed);
            loop {
                let closed = match tokio::time::timeout_at(end, ws.next()).await {
                    Err(_) => break,
                    Ok(None) => "eof".to_string(),
                    Ok(Some(Err(e))) => format!("error {}", short(&e.to_string())),
                    Ok(Some(Ok(Message::Close(frame)))) => format!("close {}", frame.map_or(1005, |f| u16::from(f.code))),
                    Ok(Some(Ok(_))) => continue,
                };
                closes.add(closed);
                if !reconnect {
                    return;
                }
                // Reconnect at once (then every second while refused) until the hold ends.
                let t0 = Instant::now();
                loop {
                    if tokio::time::Instant::now() >= end {
                        return;
                    }
                    match target.ws(&token).await {
                        Ok(new) => {
                            reconnect_latency.add(micros(t0.elapsed()));
                            reconnected.fetch_add(1, Ordering::Relaxed);
                            ws = new;
                            break;
                        }
                        Err(e) => {
                            reconnect_errors.add(e);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }
            let _ = ws.close(None).await;
        }));
    }
    // Report the connect phase once every attempt finished (bounded: a task that died counts as done).
    let waves = u32::try_from(count.div_ceil(concurrency.max(1))).unwrap_or(u32::MAX);
    let deadline = Instant::now() + Duration::from_secs(20).saturating_mul(waves) + Duration::from_secs(30);
    while attempted.load(Ordering::Relaxed) < count && Instant::now() < deadline && !tasks.iter().all(|t| t.is_finished()) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let connect_seconds = seconds(started.elapsed());
    eprintln!("sockets: {} connected, {} failed in {connect_seconds} s; holding for {} s", connected.load(Ordering::Relaxed), errors.total(), hold.as_secs());
    let mut panicked = 0u64;
    for task in tasks {
        if task.await.is_err() {
            panicked += 1;
        }
    }
    report(
        "sockets",
        json!({
            "requested": count, "connected": connected.load(Ordering::Relaxed), "connect_errors": errors.snapshot(),
            "connect_seconds": connect_seconds, "connect_latency": latency.summary(),
            "hold_seconds": hold.as_secs(), "closed_during_hold": closes.snapshot(),
            "reconnect": reconnect, "reconnected": reconnected.load(Ordering::Relaxed),
            "reconnect_errors": reconnect_errors.snapshot(), "reconnect_latency": reconnect_latency.summary(),
            "failed_tasks": panicked,
        }),
    );
    Ok(())
}

fn short(text: &str) -> String {
    text.chars().take(60).collect()
}

// ---- shared WebSocket helpers -------------------------------------------------------------------

type Sink = SplitSink<Ws, Message>;
type Source = SplitStream<Ws>;

/// Send one typed request and wait for its answer (pushes before it are skipped).
async fn ws_call<C: net_backend_protocol::WsCall>(ws: &mut Ws, id: u64, call: C) -> Result<C::Response, String> {
    let text = serde_json::to_string(&WsRequestFrame::call(id, call)).map_err(|e| e.to_string())?;
    ws.send(Message::text(text)).await.map_err(|e| format!("send: {e}"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let message = tokio::time::timeout_at(deadline, ws.next()).await.map_err(|_| "no answer within 30 s".to_string())?;
        match message {
            Some(Ok(Message::Text(text))) => {
                if let Ok(WsServerFrame::Response(frame)) = WsServerFrame::parse(text.as_str()) {
                    if frame.id == id {
                        return match frame.result {
                            Ok(data) => serde_json::from_value(data).map_err(|e| format!("answer: {e}")),
                            Err(error) => Err(error.code),
                        };
                    }
                }
            }
            Some(Ok(Message::Close(frame))) => return Err(format!("closed {}", frame.map_or(1005, |f| u16::from(f.code)))),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(format!("read: {e}")),
            None => return Err("closed".into()),
        }
    }
}

/// What a reader counts: test messages delivered (with their latency) and answers to sends.
#[derive(Clone, Default)]
struct Counters {
    delivered: Arc<AtomicU64>,
    latency: Samples,
    acks: Tally,
    closes: Tally,
}

/// Read pushes and answers until `stop`.
fn spawn_reader(mut source: Source, counters: Counters, mut stop: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut local = Vec::new();
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                message = source.next() => match message {
                    Some(Ok(Message::Text(text))) => match WsServerFrame::parse(text.as_str()) {
                        Ok(WsServerFrame::Push(push)) if push.kind == net_backend_protocol::kinds::CHAT_MESSAGE => {
                            if let Ok(message) = serde_json::from_value::<ChatMessage>(push.data) {
                                if let Some(sent) = message.text.strip_prefix("lt ").and_then(|rest| rest.split(' ').next()).and_then(|ms| ms.parse::<u64>().ok()) {
                                    counters.delivered.fetch_add(1, Ordering::Relaxed);
                                    local.push(now_ms().saturating_sub(sent).saturating_mul(1000));
                                }
                            }
                        }
                        Ok(WsServerFrame::Response(frame)) => match frame.result {
                            Ok(_) => counters.acks.add("ok"),
                            Err(error) => counters.acks.add(error.code),
                        },
                        _ => {}
                    },
                    Some(Ok(Message::Close(frame))) => {
                        counters.closes.add(format!("close {}", frame.map_or(1005, |f| u16::from(f.code))));
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        counters.closes.add(format!("error {}", short(&e.to_string())));
                        break;
                    }
                    None => {
                        counters.closes.add("eof");
                        break;
                    }
                },
            }
        }
        counters.latency.extend(&local);
    })
}

/// Send `chat.send` frames without waiting for the answers (the reader counts them).
async fn send_burst(sink: &mut Sink, room: RoomId, first_id: u64, count: u64, every: Option<Duration>, sent: &AtomicU64, errors: &Tally) {
    let mut ticker = every.map(|d| {
        let mut t = tokio::time::interval(d);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        t
    });
    for n in 0..count {
        if let Some(t) = ticker.as_mut() {
            t.tick().await;
        }
        let text = format!("lt {} {n}", now_ms());
        let frame = WsRequestFrame::call(first_id + n, SendMessage::new(room, text));
        let Ok(json) = serde_json::to_string(&frame) else { continue };
        match sink.send(Message::text(json)).await {
            Ok(()) => {
                sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                errors.add(format!("send {}", short(&e.to_string())));
                break;
            }
        }
    }
}

/// Wait (one deadline: `drain` from now) until every sent request is answered (accepted or refused)
/// AND every accepted message reached `per_message` sockets. The answers of one socket come one after
/// another, so "deliveries == accepted x N" alone would be true long before the burst is answered.
async fn drain(counters: &Counters, sent: u64, per_message: u64, drain: Duration) {
    let end = Instant::now() + drain;
    while Instant::now() < end {
        let answers = counters.acks.snapshot();
        let answered: u64 = answers.values().sum();
        let accepted = answers.get("ok").copied().unwrap_or(0);
        if answered >= sent && counters.delivered.load(Ordering::Relaxed) >= accepted * per_message {
            // A short grace for late duplicates.
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---- chat ---------------------------------------------------------------------------------------

/// `members` users join the public room `room`; the first `senders` of them send `rate` messages per
/// second each for `duration`. Every member (the sender too) should get every accepted message.
#[allow(clippy::too_many_arguments)] // one flat call from the command line
pub async fn chat(
    target: &Target,
    users: &[User],
    room: &str,
    members: usize,
    senders: usize,
    rate: f64,
    duration: Duration,
    drain_for: Duration,
) -> Result<(), String> {
    let senders = senders.min(members);
    let errors = Tally::default();
    let counters = Counters::default();
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut joined: Vec<(Sink, RoomId)> = Vec::with_capacity(members);
    let mut readers = Vec::with_capacity(members);
    let started = Instant::now();
    // Join in parallel batches (each join is a WebSocket handshake + one request).
    let sem = Arc::new(Semaphore::new(50));
    let mut joins = Vec::with_capacity(members);
    for user in users.iter().take(members) {
        let (target, token, room, sem) = (target.clone(), user.access_token.clone(), room.to_string(), sem.clone());
        joins.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.map_err(|e| e.to_string())?;
            let mut ws = target.ws(&token).await?;
            let info: RoomInfo = ws_call(&mut ws, 1, JoinRoom::new(room.as_str())).await.map_err(|e| format!("join: {e}"))?;
            Ok::<(Ws, RoomId), String>((ws, info.id))
        }));
    }
    for join in joins {
        match join.await {
            Ok(Ok((ws, room_id))) => {
                let (sink, source) = ws.split();
                readers.push(spawn_reader(source, counters.clone(), stop_rx.clone()));
                joined.push((sink, room_id));
            }
            Ok(Err(e)) => errors.add(e),
            Err(e) => errors.add(format!("task: {e}")),
        }
    }
    let join_seconds = seconds(started.elapsed());
    let member_count = joined.len() as u64;
    eprintln!("chat: {member_count} of {members} members joined `{room}` in {join_seconds} s");
    if joined.is_empty() {
        return Err(format!("nobody joined: {:?}", errors.snapshot()));
    }
    let per_sender = (rate * duration.as_secs_f64()).floor().max(1.0) as u64;
    let every = (rate > 0.0).then(|| Duration::from_secs_f64(1.0 / rate));
    let sent = Arc::new(AtomicU64::new(0));
    let send_started = Instant::now();
    let mut sending = Vec::new();
    let mut idle_sinks = Vec::new();
    for (index, (mut sink, room_id)) in joined.into_iter().enumerate() {
        if index < senders {
            let (sent, errors) = (sent.clone(), errors.clone());
            sending.push(tokio::spawn(async move {
                send_burst(&mut sink, room_id, 2, per_sender, every, &sent, &errors).await;
                sink
            }));
        } else {
            idle_sinks.push(sink);
        }
    }
    for task in sending {
        if let Ok(sink) = task.await {
            idle_sinks.push(sink);
        }
    }
    let send_seconds = seconds(send_started.elapsed());
    drain(&counters, sent.load(Ordering::Relaxed), member_count, drain_for).await;
    let _ = stop_tx.send(true);
    for reader in readers {
        let _ = reader.await;
    }
    drop(idle_sinks);
    let accepted = counters.acks.snapshot().get("ok").copied().unwrap_or(0);
    let expected = accepted * member_count;
    let delivered = counters.delivered.load(Ordering::Relaxed);
    report(
        "chat",
        json!({
            "room": room, "members": member_count, "join_seconds": join_seconds, "join_errors": errors.snapshot(),
            "senders": senders, "rate_per_sender": rate, "sent": sent.load(Ordering::Relaxed), "send_seconds": send_seconds,
            "answers": counters.acks.snapshot(), "unanswered": sent.load(Ordering::Relaxed).saturating_sub(counters.acks.total()), "accepted": accepted,
            "deliveries_expected": expected, "delivered": delivered, "lost": expected.saturating_sub(delivered),
            "deliveries_per_second": if send_seconds > 0.0 { (delivered as f64 / send_seconds).round() } else { 0.0 },
            "latency": counters.latency.summary(), "closes": counters.closes.snapshot(),
        }),
    );
    Ok(())
}

// ---- dm -----------------------------------------------------------------------------------------

/// `pairs` pairs of users open their DM room; both members of each pair send `messages` messages
/// at once into it (no waiting for answers). Both should get every accepted message.
pub async fn dm(target: &Target, users: &[User], pairs: usize, messages: u64, drain_for: Duration) -> Result<(), String> {
    let errors = Tally::default();
    let counters = Counters::default();
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut sides: Vec<(Sink, RoomId)> = Vec::new();
    let mut readers = Vec::new();
    for pair in 0..pairs {
        let (a, b) = (&users[2 * pair], &users[2 * pair + 1]);
        let mut http = Http::new(target);
        let room = match http.call(&OpenDirect::new(UserId::new(b.user_id)), Some(&a.access_token)).await {
            Ok(answer) if answer.status.is_success() => answer.json::<RoomInfo>()?.id,
            Ok(answer) => {
                errors.add(format!("open dm {}", answer.error_code()));
                continue;
            }
            Err(e) => {
                errors.add(e);
                continue;
            }
        };
        for user in [a, b] {
            match target.ws(&user.access_token).await {
                Ok(ws) => {
                    let (sink, source) = ws.split();
                    readers.push(spawn_reader(source, counters.clone(), stop_rx.clone()));
                    sides.push((sink, room));
                }
                Err(e) => errors.add(e),
            }
        }
    }
    if sides.is_empty() {
        return Err(format!("no DM room opened: {:?}", errors.snapshot()));
    }
    let barrier = Arc::new(Barrier::new(sides.len()));
    let sent = Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    let mut tasks = Vec::new();
    for (mut sink, room) in sides {
        let (barrier, sent, errors) = (barrier.clone(), sent.clone(), errors.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            send_burst(&mut sink, room, 1, messages, None, &sent, &errors).await;
            sink
        }));
    }
    let mut sinks = Vec::new();
    for task in tasks {
        if let Ok(sink) = task.await {
            sinks.push(sink);
        }
    }
    let send_seconds = seconds(started.elapsed());
    // Each accepted DM goes to both members (one socket each).
    drain(&counters, sent.load(Ordering::Relaxed), 2, drain_for).await;
    let _ = stop_tx.send(true);
    for reader in readers {
        let _ = reader.await;
    }
    drop(sinks);
    let accepted = counters.acks.snapshot().get("ok").copied().unwrap_or(0);
    let delivered = counters.delivered.load(Ordering::Relaxed);
    report(
        "dm",
        json!({
            "pairs": pairs, "messages_per_side": messages, "sent": sent.load(Ordering::Relaxed), "send_seconds": send_seconds,
            "answers": counters.acks.snapshot(), "unanswered": sent.load(Ordering::Relaxed).saturating_sub(counters.acks.total()), "accepted": accepted, "deliveries_expected": accepted * 2, "delivered": delivered,
            "lost": (accepted * 2).saturating_sub(delivered), "latency": counters.latency.summary(),
            "errors": errors.snapshot(), "closes": counters.closes.snapshot(),
        }),
    );
    Ok(())
}

// ---- storage ------------------------------------------------------------------------------------

/// `count` players make their first save at the same moment (`if_absent` writes of `bytes`).
pub async fn saves(target: &Target, users: &[User], count: usize, bytes: usize, collection: &str, key: &str) -> Result<(), String> {
    let barrier = Arc::new(Barrier::new(count));
    let statuses = Tally::default();
    let latency = Samples::default();
    let value = json!({ "blob": "x".repeat(bytes.saturating_sub(12)) });
    let mut tasks = Vec::with_capacity(count);
    for user in users.iter().take(count) {
        let (target, barrier, statuses, latency, value) = (target.clone(), barrier.clone(), statuses.clone(), latency.clone(), value.clone());
        let (token, call) = (user.access_token.clone(), WriteObject::new(collection, key, PutObject::new(value).if_absent()));
        tasks.push(tokio::spawn(async move {
            let mut http = Http::new(&target);
            // Open the connection first, so the barrier releases requests, not handshakes.
            let warm = http.request(Method::GET, net_backend_protocol::routes::INFO, None, None).await;
            barrier.wait().await;
            if let Err(e) = warm {
                statuses.add(e);
                return;
            }
            let t0 = Instant::now();
            match http.call(&call, Some(&token)).await {
                Ok(answer) if answer.status.is_success() => {
                    latency.add(micros(t0.elapsed()));
                    statuses.add("ok");
                }
                Ok(answer) => statuses.add(answer.error_code()),
                Err(e) => statuses.add(e),
            }
        }));
    }
    let started = Instant::now();
    for task in tasks {
        let _ = task.await;
    }
    report(
        "saves",
        json!({ "players": count, "value_bytes": bytes, "answers": statuses.snapshot(), "seconds": seconds(started.elapsed()), "latency": latency.summary() }),
    );
    Ok(())
}

/// One batch put per user of `objects` objects of `object_bytes` each (16 x 256 KiB = the 4 MiB maximum).
pub async fn batch(target: &Target, users: &[User], count: usize, offset: usize, objects: usize, object_bytes: usize) -> Result<(), String> {
    let statuses = Tally::default();
    let latency = Samples::default();
    // A JSON string's size is its length plus the two quotes.
    let item_value = Value::String("x".repeat(object_bytes.saturating_sub(2)));
    let items: Vec<BatchPutItem> = (0..objects).map(|i| BatchPutItem::new("batch", format!("obj-{i}"), PutObject::new(item_value.clone()))).collect();
    let call = BatchPut::new(items);
    let body_bytes = serde_json::to_vec(&call).map_err(|e| e.to_string())?.len();
    let mut tasks = Vec::new();
    for user in users.iter().skip(offset).take(count) {
        let (target, statuses, latency, call, token) = (target.clone(), statuses.clone(), latency.clone(), call.clone(), user.access_token.clone());
        tasks.push(tokio::spawn(async move {
            let mut http = Http::new(&target);
            let t0 = Instant::now();
            match http.call(&call, Some(&token)).await {
                Ok(answer) if answer.status.is_success() => {
                    latency.add(micros(t0.elapsed()));
                    statuses.add("ok");
                }
                Ok(answer) => statuses.add(format!("{} {}", answer.status.as_u16(), answer.error_code())),
                Err(e) => statuses.add(e),
            }
        }));
    }
    let started = Instant::now();
    for task in tasks {
        let _ = task.await;
    }
    report(
        "batch",
        json!({ "requests": count, "objects": objects, "object_bytes": object_bytes, "body_bytes": body_bytes, "answers": statuses.snapshot(), "seconds": seconds(started.elapsed()), "latency": latency.summary() }),
    );
    Ok(())
}

// ---- http ---------------------------------------------------------------------------------------

/// Which route the HTTP scenario hammers.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Route {
    /// `GET /v1/info` (no token, no database).
    Info,
    /// `GET /v1/account` (token check + one account read).
    Account,
    /// `GET /v1/storage/save/slot-1` (token check + one object read; 404 until `saves` ran).
    StorageGet,
}

/// `concurrency` keep-alive connections send requests back to back for `duration`.
pub async fn http_rate(target: &Target, users: &[User], route: Route, concurrency: usize, duration: Duration) -> Result<(), String> {
    let statuses = Tally::default();
    let latency = Samples::default();
    let end = Instant::now() + duration;
    let mut tasks = Vec::with_capacity(concurrency);
    for worker in 0..concurrency {
        let token = users.get(worker % users.len().max(1)).map(|u| u.access_token.clone());
        let (target, statuses, latency) = (target.clone(), statuses.clone(), latency.clone());
        tasks.push(tokio::spawn(async move {
            let mut http = Http::new(&target);
            let mut local = Vec::new();
            while Instant::now() < end {
                let t0 = Instant::now();
                let answer = match route {
                    Route::Info => http.request(Method::GET, net_backend_protocol::routes::INFO, None, None).await,
                    Route::Account => http.call(&GetAccount::new(), token.as_deref()).await,
                    Route::StorageGet => http.call(&GetObject::new("save", "slot-1"), token.as_deref()).await,
                };
                match answer {
                    Ok(answer) => {
                        local.push(micros(t0.elapsed()));
                        statuses.add(answer.status.as_u16().to_string());
                    }
                    Err(e) => {
                        statuses.add(e);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            latency.extend(&local);
        }));
    }
    let started = Instant::now();
    for task in tasks {
        let _ = task.await;
    }
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let answered = latency.len();
    report(
        "http",
        json!({
            "route": format!("{route:?}"), "connections": concurrency, "seconds": seconds(started.elapsed()),
            "answers": statuses.snapshot(), "requests_per_second": (answered as f64 / elapsed).round(), "latency": latency.summary(),
        }),
    );
    Ok(())
}
