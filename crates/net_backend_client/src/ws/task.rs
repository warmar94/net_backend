//! The connection task: owns the socket, the request queue and the pending answers; serves one link
//! after another (reconnects) until the connection is closed for good. It is the only thing that
//! answers requests, so every request gets exactly one answer, also when the link dies.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use net_backend_protocol::{CloseCode, WsServerFrame};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode as WireCloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use super::link::{self, Link};
use super::{Shared, WsConnection, WsEvent, WsPush, WsSettings, WsState};
use crate::runtime::RuntimeThread;
use crate::{Client, Error};

/// How long a closing link may take to say goodbye.
const GOODBYE: Duration = Duration::from_secs(1);

/// A request waiting for its answer.
pub(crate) struct Pending {
    pub(crate) id: u64,
    pub(crate) text: String,
    pub(crate) deadline: Instant,
    pub(crate) answer: Box<dyn FnOnce(Result<Value, Error>) + Send>,
}

/// What the handles tell the task.
pub(crate) enum Command {
    Request(Pending),
    /// The app cancelled this request ([`crate::Reply::cancel`]).
    Cancel(u64),
    Close,
}

/// How a link ended.
enum End {
    /// The app closed the connection (or dropped every handle).
    App,
    /// The server sent a close frame.
    Closed(CloseCode, String),
    /// The link broke (network, heartbeat, protocol).
    Lost(Error),
}

struct Task {
    client: Client,
    settings: WsSettings,
    commands: mpsc::UnboundedReceiver<Command>,
    state: watch::Sender<WsState>,
    events: broadcast::Sender<WsEvent>,
    pushes: broadcast::Sender<Arc<WsPush>>,
    /// Not sent yet (made while reconnecting), oldest first.
    waiting: VecDeque<Pending>,
    /// Sent on the current link, by id.
    in_flight: HashMap<u64, Pending>,
}

/// Open the first link (one refresh + one more try on a refused token), then start the task.
pub(crate) async fn connect(client: Client, settings: WsSettings, runtime: Option<Arc<RuntimeThread>>) -> Result<WsConnection, Error> {
    let link = open_with_refresh(&client, &settings).await?;
    let (commands, receiver) = mpsc::unbounded_channel();
    let (state_sender, state) = watch::channel(WsState::Connected);
    // Only the task holds the senders: every stream ends (after its buffer) when the task does.
    let (events, events_template) = broadcast::channel(settings.event_buffer);
    let (pushes, pushes_template) = broadcast::channel(settings.push_buffer);
    let task = Task {
        client,
        settings: settings.clone(),
        commands: receiver,
        state: state_sender,
        events,
        pushes,
        waiting: VecDeque::new(),
        in_flight: HashMap::new(),
    };
    let handle = crate::runtime::current()?;
    handle.spawn(task.run(link));
    let (events, pushes) = (std::sync::Mutex::new(events_template), std::sync::Mutex::new(pushes_template));
    Ok(WsConnection::new(Shared { commands, next_id: AtomicU64::new(1), state, events, pushes, settings, _runtime: runtime }))
}

/// One connection attempt with a fresh token; a refused token gets ONE refresh and one more try.
async fn open_with_refresh(client: &Client, settings: &WsSettings) -> Result<Link, Error> {
    let deadline = deadline_after(settings.connect_timeout);
    let (token, _) = client.access_token(deadline).await?;
    match link::open(client, settings, token.expose(), deadline).await {
        Err(error) if link::wants_refresh(&error) => {
            tracing::debug!("net_backend_client: the WebSocket handshake refused the token ({error}); one refresh");
            let deadline = deadline_after(settings.connect_timeout);
            client.refresh_shared(deadline).await?;
            let (token, _) = client.access_token(deadline).await?;
            link::open(client, settings, token.expose(), deadline).await
        }
        other => other,
    }
}

fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout).unwrap_or(now)
}

impl Task {
    async fn run(mut self, first: Link) {
        let mut link = Some(first);
        let mut reconnected = false;
        let mut attempt: u32 = 0;
        let mut refreshed_after_4001 = false;
        loop {
            // A link is up: serve it.
            if let Some(current) = link.take() {
                self.set_state(WsState::Connected);
                self.event(WsEvent::Connected { reconnected });
                let up_since = Instant::now();
                let end = self.serve(current).await;
                self.fail_in_flight();
                if up_since.elapsed() >= self.settings.reconnect.as_ref().map_or(Duration::MAX, |r| r.stable_after) {
                    attempt = 0;
                    refreshed_after_4001 = false;
                }
                let error = match end {
                    End::App => return self.finish(None),
                    End::Closed(code, reason) if code == CloseCode::UNAUTHORIZED && !refreshed_after_4001 => {
                        // 4001: ONE refresh, then one new connection at once.
                        refreshed_after_4001 = true;
                        let closed = Error::Closed { code, reason };
                        match self.client.refresh_shared(deadline_after(self.settings.connect_timeout)).await {
                            Ok(_) => {}
                            Err(error @ Error::SessionEnded { .. }) => return self.finish(Some(error)),
                            Err(_) => return self.finish(Some(closed)),
                        }
                        match self.attempt_now().await {
                            Some(Ok(new)) => {
                                link = Some(new);
                                reconnected = true;
                                continue;
                            }
                            Some(Err(error)) if self.is_final(&error) => return self.finish(Some(error)),
                            Some(Err(error)) => error,
                            None => return self.finish(None),
                        }
                    }
                    End::Closed(code, reason) if code.is_permanent() => return self.finish(Some(Error::Closed { code, reason })),
                    End::Closed(code, reason) => Error::Closed { code, reason },
                    End::Lost(error) => error,
                };
                tracing::debug!("net_backend_client: the WebSocket link ended ({error})");
                let Some(reconnect) = self.settings.reconnect.clone() else { return self.finish(Some(error)) };
                // Reconnect with backoff until a link is up or a permanent answer comes.
                let mut last = error;
                loop {
                    attempt = attempt.saturating_add(1);
                    if !reconnect.may_retry(attempt) {
                        return self.finish(Some(last));
                    }
                    let mut delay = reconnect.delay(attempt, crate::tls::random_u64());
                    if let Some(wait) = last.retry_after() {
                        delay = delay.max(wait.min(crate::MAX_TIMEOUT));
                    }
                    self.set_state(WsState::Reconnecting { attempt, retry_in: delay });
                    self.event(WsEvent::Reconnecting { attempt, retry_in: delay, error: last.clone() });
                    if self.wait_queueing(tokio::time::sleep(delay)).await.is_none() {
                        return self.finish(None);
                    }
                    match self.attempt_now().await {
                        Some(Ok(new)) => {
                            link = Some(new);
                            reconnected = true;
                            break;
                        }
                        Some(Err(error)) if reconnect.is_final(&error) => return self.finish(Some(error)),
                        Some(Err(error)) => last = error,
                        None => return self.finish(None),
                    }
                }
            }
        }
    }

    /// Whether a failed attempt after a lost link ends the connection (the reconnect policy's rule;
    /// without a policy, only a permanent answer, and a TLS error, as the default policy).
    fn is_final(&self, error: &Error) -> bool {
        self.settings.reconnect.as_ref().map_or_else(|| super::Reconnect::default().is_final(error), |r| r.is_final(error))
    }

    /// One connection attempt while still queueing requests and honouring their deadlines. `None`
    /// when the app closed the connection meanwhile.
    async fn attempt_now(&mut self) -> Option<Result<Link, Error>> {
        self.set_state(WsState::Connecting);
        let client = self.client.clone();
        let settings = self.settings.clone();
        self.wait_queueing(async move { open_with_refresh(&client, &settings).await }).await
    }

    /// Run `future` while queueing new requests and timing out waiting ones. `None` if the app
    /// closed the connection first.
    async fn wait_queueing<T>(&mut self, future: impl Future<Output = T>) -> Option<T> {
        tokio::pin!(future);
        loop {
            let next = self.next_deadline();
            tokio::select! {
                biased;
                command = self.commands.recv() => match command {
                    None | Some(Command::Close) => return None,
                    Some(Command::Request(pending)) => self.queue(pending),
                    Some(Command::Cancel(id)) => self.cancel(id),
                },
                output = &mut future => return Some(output),
                () = sleep_until(next) => self.expire(),
            }
        }
    }

    fn queue(&mut self, pending: Pending) {
        if self.waiting.len().saturating_add(self.in_flight.len()) >= self.settings.max_pending {
            let limit = self.settings.max_pending;
            (pending.answer)(Err(Error::invalid(format!("more than {limit} WebSocket requests are waiting or running"))));
            return;
        }
        if Instant::now() >= pending.deadline {
            (pending.answer)(Err(Error::timeout("not sent: the request timed out before the connection was up", Some(false))));
            return;
        }
        self.waiting.push_back(pending);
    }

    /// The app cancelled request `id`: a waiting one is answered and never sent, a running one is
    /// answered and its late answer dropped (as an unknown id). An answered one: nothing to do.
    fn cancel(&mut self, id: u64) {
        if let Some(at) = self.waiting.iter().position(|p| p.id == id) {
            if let Some(pending) = self.waiting.remove(at) {
                (pending.answer)(Err(Error::Cancelled { sent: Some(false) }));
            }
        } else if let Some(pending) = self.in_flight.remove(&id) {
            (pending.answer)(Err(Error::Cancelled { sent: Some(true) }));
        }
    }

    /// The earliest deadline of a waiting or running request.
    fn next_deadline(&self) -> Option<Instant> {
        self.waiting.iter().chain(self.in_flight.values()).map(|p| p.deadline).min()
    }

    /// Answer every request whose deadline passed.
    fn expire(&mut self) {
        let now = Instant::now();
        let mut kept = VecDeque::with_capacity(self.waiting.len());
        for pending in self.waiting.drain(..) {
            if now >= pending.deadline {
                (pending.answer)(Err(Error::timeout("not sent: the WebSocket was not connected before the request timed out", Some(false))));
            } else {
                kept.push_back(pending);
            }
        }
        self.waiting = kept;
        let overdue: Vec<u64> = self.in_flight.iter().filter(|(_, p)| now >= p.deadline).map(|(id, _)| *id).collect();
        for id in overdue {
            if let Some(pending) = self.in_flight.remove(&id) {
                (pending.answer)(Err(Error::timeout("the server did not answer in time", Some(true))));
            }
        }
    }

    /// Serve one link until it ends.
    async fn serve(&mut self, mut link: Link) -> End {
        // Requests made while reconnecting go out first, in order.
        while let Some(pending) = self.waiting.pop_front() {
            if let Err(end) = self.write(&mut link, pending).await {
                return end;
            }
        }
        let mut ping = tokio::time::interval_at(Instant::now() + self.settings.ping_interval, self.settings.ping_interval);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_seen = Instant::now();
        loop {
            let next = self.next_deadline();
            tokio::select! {
                biased;
                command = self.commands.recv() => match command {
                    None | Some(Command::Close) => {
                        let frame = CloseFrame { code: WireCloseCode::Normal, reason: "".into() };
                        let _ = tokio::time::timeout(GOODBYE, link.close(Some(frame))).await;
                        return End::App;
                    }
                    Some(Command::Request(pending)) => {
                        if self.waiting.len().saturating_add(self.in_flight.len()) >= self.settings.max_pending || Instant::now() >= pending.deadline {
                            self.queue(pending);
                        } else if let Err(end) = self.write(&mut link, pending).await {
                            return end;
                        }
                    }
                    Some(Command::Cancel(id)) => self.cancel(id),
                },
                message = link.next() => {
                    last_seen = Instant::now();
                    match message {
                        Some(Ok(Message::Text(text))) => self.frame(text.as_str()),
                        Some(Ok(Message::Close(frame))) => {
                            let (code, reason) = frame.map_or((CloseCode(1005), String::new()), |f| (CloseCode(u16::from(f.code)), f.reason.as_str().to_string()));
                            // Let tungstenite finish the closing handshake (bounded).
                            let _ = tokio::time::timeout(GOODBYE, async { while link.next().await.is_some() {} }).await;
                            return End::Closed(code, reason);
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => return End::Lost(link::map_ws_error(error)),
                        None => return End::Lost(Error::network("the connection closed without a close frame", None)),
                    }
                }
                _ = ping.tick() => {
                    if last_seen.elapsed() >= self.settings.dead_after {
                        return End::Lost(Error::timeout(format!("no frame from the server for {:?} (heartbeat)", self.settings.dead_after), None));
                    }
                    match tokio::time::timeout(self.settings.dead_after, link.send(Message::Ping(Default::default()))).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => return End::Lost(link::map_ws_error(error)),
                        Err(_) => return End::Lost(Error::timeout("the heartbeat could not be written", None)),
                    }
                }
                () = sleep_until(next) => self.expire(),
            }
        }
    }

    /// Write one request (bounded by the heartbeat limit: a peer that stopped reading counts as dead).
    async fn write(&mut self, link: &mut Link, pending: Pending) -> Result<(), End> {
        let message = Message::text(pending.text.clone());
        match tokio::time::timeout(self.settings.dead_after, link.send(message)).await {
            Ok(Ok(())) => {
                self.in_flight.insert(pending.id, pending);
                Ok(())
            }
            Ok(Err(error)) => {
                let error = link::map_ws_error(error);
                (pending.answer)(Err(Error::disconnected(format!("the request could not be written: {error}"), None)));
                Err(End::Lost(error))
            }
            Err(_) => {
                (pending.answer)(Err(Error::disconnected("the request could not be written in time", None)));
                Err(End::Lost(Error::timeout("a request could not be written (the server stopped reading)", None)))
            }
        }
    }

    /// One text frame from the server.
    fn frame(&mut self, text: &str) {
        match WsServerFrame::parse(text) {
            Ok(WsServerFrame::Response(response)) => match self.in_flight.remove(&response.id) {
                Some(pending) => (pending.answer)(response.result.map_err(|error| Error::api(None, error, None))),
                None => tracing::debug!("net_backend_client: an answer to an unknown request id {} (timed out already?)", response.id),
            },
            Ok(WsServerFrame::Push(push)) => {
                let _ = self.pushes.send(Arc::new(WsPush { kind: push.kind, data: push.data }));
            }
            Ok(WsServerFrame::AuthOk(_)) => {}
            Ok(WsServerFrame::AuthFailed(error)) => tracing::debug!("net_backend_client: auth.failed on an open socket ({error})"),
            Ok(_) | Err(_) => tracing::debug!("net_backend_client: a frame that is not part of the protocol was ignored"),
        }
    }

    /// The link ended: what went out on it gets `Disconnected` (sent), never resent.
    fn fail_in_flight(&mut self) {
        for (_, pending) in self.in_flight.drain() {
            (pending.answer)(Err(Error::disconnected("the connection was lost after the request was sent", Some(true))));
        }
    }

    fn set_state(&self, state: WsState) {
        self.state.send_replace(state);
    }

    fn event(&self, event: WsEvent) {
        let _ = self.events.send(event);
    }

    /// Closed for good: answer everything still waiting, report, stop.
    fn finish(mut self, error: Option<Error>) {
        self.fail_in_flight();
        let reason = match &error {
            None => "the connection was closed by the app".to_string(),
            Some(error) => format!("the connection closed: {error}"),
        };
        for pending in self.waiting.drain(..) {
            (pending.answer)(Err(Error::disconnected(reason.clone(), Some(false))));
        }
        self.commands.close();
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Request(pending) => (pending.answer)(Err(Error::disconnected(reason.clone(), Some(false)))),
                Command::Cancel(_) | Command::Close => {}
            }
        }
        if let Some(error) = &error {
            tracing::info!("net_backend_client: the WebSocket closed for good ({error})");
        }
        self.set_state(WsState::Closed);
        self.event(WsEvent::Closed { error });
    }
}

/// Sleep until `deadline`, or forever without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
