//! The {{name}} demo: a small window on net_backend_client. A panel per module of the server
//! (only the modules it reports in `/v1/info`); each button makes one call, and the log shows what
//! the client sent, what came back and what the server pushed. The server's own window shows the
//! other side.
//!
//! ```text
//! cargo run -p demo                                 # the server at http://127.0.0.1:8080
//! cargo run -p demo -- player2@example.com          # a second player in a second window
//! cargo run -p demo -- https://api.example.com      # another server (or NET_BACKEND_URL)
//! ```
//!
//! Two players: start the demo twice, the second with another email (or type it into the email
//! field), and Register each the first time. Plain `http://` goes to this machine only (127.0.0.1,
//! ::1, localhost). The development password is filled in for a server on this machine only.
//! Every call runs on its own thread with the client's blocking interface, so the window never
//! waits. The log is also printed in the terminal.
//! Client guide: <https://docs.rs/net_backend_client>.

mod invite;
#[cfg(feature = "steam")]
mod steam;

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui;
use net_backend_client::blocking::Client;
use net_backend_client::files::{DownloadOptions, FileUpload};
use net_backend_client::protocol::auth::{LoginRequest, RegisterRequest};
use net_backend_client::protocol::chat::{
    ChatHistory, ChatMessage, EditMessage, JoinRoom, ListMembers, MarkRead, MessageEdited,
    Presence, ReadReceipt, SendMessage, SetTyping, TypingUpdate,
};
use net_backend_client::protocol::files::{FileInfo, ListFiles};
use net_backend_client::protocol::friends::{
    AcceptFriend, AddFriend, FriendPresence, GetFriendCode, ListFriendRequests, ListFriends,
};
use net_backend_client::protocol::groups::{CreateGroup, InviteToGroup, Invitee, MyGroups};
use net_backend_client::protocol::leaderboards::{
    GetAroundMe, GetLeaderboard, LeaderboardEntry, PostScore, SubmitScore,
};
use net_backend_client::protocol::lobbies::{
    CreateLobby, JoinLobbyByCode, LeaveLobby, LobbyCode, LobbyInfo, LobbyMemberUpdate, LobbyUpdate,
    SetLobbyReady, SetReady,
};
use net_backend_client::protocol::matchmaking::{
    CancelTicket, CreateTicket, ListQueues, MatchFound, TicketExpired,
};
use net_backend_client::protocol::notifications::{
    MarkNotifications, Notification, NotificationQuery,
};
use net_backend_client::protocol::storage::{GetObject, ListObjects, PutObject, WriteObject};
use net_backend_client::protocol::{
    FileId, GroupId, LobbyId, MessageId, RoomId, ServerPush, UserId,
};
use net_backend_client::ws::{PushStream, WsConnection, WsSettings};

/// The project's name.
const NAME: &str = "{{name}}";
/// The server when neither an argument nor `NET_BACKEND_URL` names one.
const DEFAULT_URL: &str = "http://127.0.0.1:8080";
/// The development account (another one: its email as an argument).
const DEV_EMAIL: &str = "player@example.com";
/// The save collection the storage buttons use.
const COLLECTION: &str = "saves";

fn main() -> eframe::Result {
    // The arguments: a server URL and an email, in any order (a start by Steam adds
    // `+nb_lobby <number>`); arguments that are not Unicode are skipped.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .filter_map(|arg| arg.into_string().ok())
        .collect();
    let url = args
        .iter()
        .find(|arg| arg.starts_with("http://") || arg.starts_with("https://"))
        .cloned()
        .or_else(|| std::env::var("NET_BACKEND_URL").ok())
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let email = args
        .iter()
        .find(|arg| arg.contains('@') && !arg.contains("://"))
        .cloned()
        .unwrap_or_else(|| DEV_EMAIL.to_string());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{NAME} demo ({email})"))
            .with_inner_size([1180.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native(
        &format!("{NAME} demo"),
        options,
        Box::new(move |cc| Ok(Box::new(Demo::new(url, email, cc.egui_ctx.clone())))),
    )
}

/// A change to the window's state, made on the window's thread.
type Update = Box<dyn FnOnce(&mut Demo) + Send>;

/// What a background call reports back to the window.
enum Event {
    Log(Tone, String),
    Update(Update),
}

#[derive(Clone, Copy)]
enum Tone {
    Sent,
    Ok,
    Error,
    Push,
}

impl Tone {
    fn mark(self) -> &'static str {
        match self {
            Tone::Sent => "->",
            Tone::Ok => "<-",
            Tone::Error => "xx",
            Tone::Push => "<<",
        }
    }
}

struct Line {
    time: String,
    tone: Tone,
    text: String,
}

/// A push stream of one kind and how its pushes read in the log.
struct Pushes<P> {
    stream: PushStream<P>,
    show: fn(&P) -> String,
}

/// Every push kind the panels show.
struct Streams {
    messages: Pushes<ChatMessage>,
    presence: Pushes<Presence>,
    edited: Pushes<MessageEdited>,
    typing: Pushes<TypingUpdate>,
    read: Pushes<ReadReceipt>,
    notifications: Pushes<Notification>,
    friends: Pushes<FriendPresence>,
    lobby_members: Pushes<LobbyMemberUpdate>,
    lobby_changes: Pushes<LobbyUpdate>,
    matches: Pushes<MatchFound>,
    expired: Pushes<TicketExpired>,
}

fn pushes<P: ServerPush>(ws: &WsConnection, show: fn(&P) -> String) -> Pushes<P> {
    Pushes {
        stream: ws.subscribe::<P>(),
        show,
    }
}

impl Streams {
    fn new(ws: &WsConnection) -> Self {
        Self {
            messages: pushes(ws, |m: &ChatMessage| {
                format!("chat.message {}: {}", sender(m), m.text)
            }),
            presence: pushes(ws, |p: &Presence| {
                let who = p
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("player {}", p.user));
                format!("chat.presence {who}: {:?}", p.event)
            }),
            edited: pushes(ws, |e: &MessageEdited| {
                format!("chat.edited message {}: {}", e.id, e.text)
            }),
            typing: pushes(ws, |t: &TypingUpdate| {
                let what = if t.typing { "types" } else { "stopped typing" };
                format!("chat.typing player {} {what}", t.user)
            }),
            read: pushes(ws, |r: &ReadReceipt| {
                format!("chat.read player {} read up to {}", r.user, r.message)
            }),
            notifications: pushes(ws, |n: &Notification| {
                let text = n.text.clone().unwrap_or_default();
                format!("notify.new {} ({}): {text}", n.id, n.kind)
            }),
            friends: pushes(ws, |f: &FriendPresence| {
                let state = if f.online { "online" } else { "offline" };
                format!("friends.presence player {} is {state}", f.user)
            }),
            lobby_members: pushes(ws, |u: &LobbyMemberUpdate| {
                let who = u
                    .member
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("player {}", u.member.user));
                format!(
                    "lobby.member lobby {}: {who} {:?} (ready {})",
                    u.lobby, u.change, u.member.ready
                )
            }),
            lobby_changes: pushes(ws, |u: &LobbyUpdate| {
                format!(
                    "lobby.changed lobby {}: {:?}, state {:?}",
                    u.lobby.id, u.changes, u.lobby.state
                )
            }),
            matches: pushes(ws, |m: &MatchFound| {
                format!("match.found in `{}`: players {:?}", m.queue, m.players)
            }),
            expired: pushes(ws, |e: &TicketExpired| {
                format!("match.expired: ticket in `{}` ran out", e.queue)
            }),
        }
    }

    /// The pushes that arrived since the last look, as log lines.
    fn drain(&mut self, room: Option<RoomId>, last_seen: &mut Option<MessageId>) -> Vec<String> {
        fn take<P: ServerPush>(
            pushes: &mut Pushes<P>,
            lines: &mut Vec<String>,
            mut seen: impl FnMut(&P),
        ) {
            while let Some(push) = pushes.stream.try_next() {
                match push {
                    Ok(push) => {
                        seen(&push);
                        lines.push((pushes.show)(&push));
                    }
                    Err(error) => lines.push(format!("push: {error}")),
                }
            }
        }
        let mut lines = Vec::new();
        take(&mut self.messages, &mut lines, |m| {
            if room == Some(m.room) {
                *last_seen = Some(m.id);
            }
        });
        take(&mut self.presence, &mut lines, |_| {});
        take(&mut self.edited, &mut lines, |_| {});
        take(&mut self.typing, &mut lines, |_| {});
        take(&mut self.read, &mut lines, |_| {});
        take(&mut self.notifications, &mut lines, |_| {});
        take(&mut self.friends, &mut lines, |_| {});
        take(&mut self.lobby_members, &mut lines, |_| {});
        take(&mut self.lobby_changes, &mut lines, |_| {});
        take(&mut self.matches, &mut lines, |_| {});
        take(&mut self.expired, &mut lines, |_| {});
        lines
    }
}

struct Demo {
    url: String,
    client: Option<Client>,
    ctx: egui::Context,
    events: Receiver<Event>,
    sender: Sender<Event>,
    modules: Vec<String>,
    account: Option<UserId>,
    ws: Option<WsConnection>,
    streams: Option<Streams>,
    // Fields and what the last answers named (the next buttons use them).
    email: String,
    password: String,
    save_key: String,
    save_value: String,
    message: String,
    room: Option<RoomId>,
    my_last: Option<MessageId>,
    last_seen: Option<MessageId>,
    board: String,
    score: String,
    friend_code: String,
    requests: Vec<UserId>,
    group_name: String,
    group: Option<GroupId>,
    player: String,
    lobby_code: String,
    lobby: Option<LobbyId>,
    joined_code: Option<LobbyCode>,
    ready: bool,
    queues: Vec<String>,
    file: Option<FileId>,
    #[cfg(feature = "steam")]
    steam: steam::SteamState,
    log: Vec<Line>,
}

impl Demo {
    fn new(url: String, email: String, ctx: egui::Context) -> Self {
        // The development password is filled in for a server on this machine only.
        let password = if is_loopback(&url) {
            "dev password 1234".to_string()
        } else {
            String::new()
        };
        let (sender, events) = channel();
        let mut demo = Self {
            client: None,
            ctx,
            events,
            sender,
            modules: Vec::new(),
            account: None,
            ws: None,
            streams: None,
            email,
            password,
            save_key: "slot1".into(),
            save_value: r#"{"level": 3, "gold": 120}"#.into(),
            message: format!("hello from the {NAME} demo"),
            room: None,
            my_last: None,
            last_seen: None,
            board: "highscore".into(),
            score: "100".into(),
            friend_code: String::new(),
            requests: Vec::new(),
            group_name: "The Testers".into(),
            group: None,
            player: String::new(),
            lobby_code: String::new(),
            lobby: None,
            joined_code: None,
            ready: false,
            queues: Vec::new(),
            file: None,
            #[cfg(feature = "steam")]
            steam: steam::SteamState::default(),
            log: Vec::new(),
            url,
        };
        match Client::new(&demo.url) {
            Ok(client) => {
                demo.client = Some(client);
                demo.info();
            }
            Err(error) => demo.push(Tone::Error, format!("{}: {error}", demo.url)),
        }
        #[cfg(feature = "steam")]
        demo.steam_start();
        demo
    }

    /// A line in the window's log, also printed to the terminal.
    fn push(&mut self, tone: Tone, text: String) {
        let time = clock();
        println!("{time} {} {text}", tone.mark());
        self.log.push(Line { time, tone, text });
    }

    /// Runs `call` on its own thread with a clone of the client; its events come back to the window.
    fn run(&mut self, label: String, call: impl FnOnce(&Client, &Sender<Event>) + Send + 'static) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.push(Tone::Sent, label);
        let (sender, ctx) = (self.sender.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            call(&client, &sender);
            ctx.request_repaint();
        });
    }

    /// Runs a WebSocket call on its own thread (the socket opens at login).
    fn run_ws(
        &mut self,
        label: String,
        call: impl FnOnce(&WsConnection, &Sender<Event>) + Send + 'static,
    ) {
        let Some(ws) = self.ws.clone() else {
            self.push(Tone::Error, format!("{label}: log in first"));
            return;
        };
        self.push(Tone::Sent, label);
        let (sender, ctx) = (self.sender.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            call(&ws, &sender);
            ctx.request_repaint();
        });
    }

    fn info(&mut self) {
        self.run("GET /v1/info".into(), |client, out| match client.info() {
            Ok(info) => {
                let text = format!("protocol {}, modules {:?}", info.protocol, info.modules);
                ok(out, text);
                let modules = info.modules.clone();
                update(out, move |d| d.modules = modules);
            }
            Err(error) => report(out, "info", error),
        });
    }

    fn has(&self, module: &str) -> bool {
        self.modules.iter().any(|m| m == module)
    }

    fn account_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("email");
            ui.text_edit_singleline(&mut self.email);
        });
        ui.horizontal(|ui| {
            ui.label("password");
            ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
        });
        ui.horizontal(|ui| {
            let (email, password) = (self.email.clone(), self.password.clone());
            if ui.button("Register").clicked() {
                let (e, p) = (email.clone(), password.clone());
                self.run(format!("register {email}"), move |client, out| match client
                    .register(RegisterRequest::new(e.as_str(), p.as_str()))
                {
                    Ok(session) => logged_in(client, out, "registered", session.account.id),
                    Err(error) => report(out, "register", error),
                });
            }
            if ui.button("Login").clicked() {
                let (e, p) = (email.clone(), password.clone());
                self.run(format!("login {email}"), move |client, out| {
                    match client.login(LoginRequest::new(e.as_str(), p.as_str())) {
                        Ok(session) => logged_in(client, out, "logged in", session.account.id),
                        Err(error) => report(out, "login", error),
                    }
                });
            }
            if ui.button("Refresh").clicked() {
                self.run("refresh the tokens".into(), |client, out| {
                    match client.refresh() {
                        Ok(pair) => {
                            let left = pair.access_expires_at.0 - now_ms();
                            ok(
                                out,
                                format!("new token pair, access valid for {} s", left / 1000),
                            );
                        }
                        Err(error) => report(out, "refresh", error),
                    }
                });
            }
            if ui.button("Logout").clicked() {
                self.run("logout".into(), |client, out| match client.logout() {
                    Ok(()) => {
                        ok(out, "logged out".into());
                        update(out, |d| d.account = None);
                    }
                    Err(error) => report(out, "logout", error),
                });
                if let Some(ws) = self.ws.take() {
                    ws.close();
                }
                // What belonged to the session (the server drops the lobby place when the
                // WebSocket stays closed).
                self.streams = None;
                self.room = None;
                self.my_last = None;
                self.requests.clear();
                self.group = None;
                self.lobby = None;
                self.joined_code = None;
                self.ready = false;
                self.file = None;
            }
        });
    }

    fn storage_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("key");
            ui.add(egui::TextEdit::singleline(&mut self.save_key).desired_width(70.0));
            ui.label("value");
            ui.text_edit_singleline(&mut self.save_value);
        });
        ui.horizontal(|ui| {
            let key = self.save_key.clone();
            if ui.button("Save").clicked() {
                match serde_json::from_str::<serde_json::Value>(&self.save_value) {
                    Ok(value) => {
                        let k = key.clone();
                        self.run(format!("save {COLLECTION}/{key}"), move |client, out| {
                            let call =
                                WriteObject::new(COLLECTION, k.as_str(), PutObject::new(value));
                            match client.call(&call) {
                                Ok(ack) => ok(
                                    out,
                                    format!(
                                        "saved {COLLECTION}/{k}, version {}",
                                        ack.version.get()
                                    ),
                                ),
                                Err(error) => report(out, "save", error),
                            }
                        });
                    }
                    Err(error) => self.push(Tone::Error, format!("the value is not JSON: {error}")),
                }
            }
            if ui.button("Load").clicked() {
                let k = key.clone();
                self.run(
                    format!("load {COLLECTION}/{key}"),
                    move |client, out| match client.call(&GetObject::new(COLLECTION, k.as_str())) {
                        Ok(object) => ok(
                            out,
                            format!(
                                "{COLLECTION}/{k} version {}: {}",
                                object.version.get(),
                                object.value
                            ),
                        ),
                        Err(error) => report(out, "load", error),
                    },
                );
            }
            if ui.button("List").clicked() {
                self.run(format!("list {COLLECTION}"), |client, out| {
                    match client.call(&ListObjects::new(COLLECTION)) {
                        Ok(page) => {
                            let keys: Vec<String> =
                                page.items.iter().map(|o| o.key.clone()).collect();
                            ok(out, format!("{} objects: {}", keys.len(), keys.join(", ")));
                        }
                        Err(error) => report(out, "list", error),
                    }
                });
            }
        });
    }

    fn chat_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("message");
            ui.text_edit_singleline(&mut self.message);
        });
        ui.horizontal(|ui| {
            if ui.button("Join `world`").clicked() {
                self.run_ws("chat.join world".into(), join);
            }
            let room = self.room;
            if ui.button("Send").clicked() {
                let text = self.message.clone();
                self.in_room(
                    room,
                    format!("chat.send \"{text}\""),
                    move |ws, out, room| match ws.request(&SendMessage::new(room, text)).wait() {
                        Ok(ack) => {
                            ok(out, format!("sent, message {}", ack.message_id));
                            update(out, move |d| d.my_last = Some(ack.message_id));
                        }
                        Err(error) => report(out, "send", error),
                    },
                );
            }
            if ui.button("History").clicked() {
                self.in_room(room, "chat.history world".into(), |ws, out, room| match ws
                    .request(&ChatHistory::new(room))
                    .wait()
                {
                    Ok(page) => {
                        let mut text = format!("{} messages (newest first)", page.items.len());
                        for message in page.items.iter().take(8) {
                            let edited = if message.edited_at.is_some() {
                                " (edited)"
                            } else {
                                ""
                            };
                            text.push_str(&format!(
                                "\n    {}: {}{edited}",
                                sender(message),
                                message.text
                            ));
                        }
                        ok(out, text);
                        if let Some(newest) = page.items.first().map(|m| m.id) {
                            update(out, move |d| d.last_seen = d.last_seen.max(Some(newest)));
                        }
                    }
                    Err(error) => report(out, "history", error),
                });
            }
            if ui.button("Presence").clicked() {
                self.in_room(room, "chat.members world".into(), |ws, out, room| match ws
                    .request(&ListMembers::new(room))
                    .wait()
                {
                    Ok(members) => {
                        let names: Vec<String> = members
                            .members
                            .iter()
                            .map(|m| {
                                m.name
                                    .clone()
                                    .unwrap_or_else(|| format!("player {}", m.user))
                            })
                            .collect();
                        ok(
                            out,
                            format!("{} online: {}", members.count, names.join(", ")),
                        );
                    }
                    Err(error) => report(out, "presence", error),
                });
            }
        });
        ui.horizontal(|ui| {
            let room = self.room;
            if ui.button("Edit my last").clicked() {
                match self.my_last {
                    Some(message) => {
                        let text = format!("{} (edited)", self.message);
                        self.in_room(
                            room,
                            format!("chat.edit message {message}"),
                            move |ws, out, room| match ws
                                .request(&EditMessage::new(room, message, text))
                                .wait()
                            {
                                Ok(edited) => ok(out, format!("edited: {}", edited.text)),
                                Err(error) => report(out, "edit", error),
                            },
                        );
                    }
                    None => self.push(Tone::Error, "edit: send a message first".into()),
                }
            }
            if ui.button("Typing").clicked() {
                self.in_room(
                    room,
                    "chat.set_typing world".into(),
                    |ws, out, room| match ws.request(&SetTyping::started(room)).wait() {
                        Ok(_) => ok(out, "the room sees you typing".into()),
                        Err(error) => report(out, "typing", error),
                    },
                );
            }
            if ui.button("Read marker").clicked() {
                match self.last_seen {
                    Some(message) => {
                        self.in_room(
                            room,
                            format!("chat.mark_read up to {message}"),
                            move |ws, out, room| match ws
                                .request(&MarkRead::new(room, message))
                                .wait()
                            {
                                Ok(_) => ok(out, format!("read up to message {message}")),
                                Err(error) => report(out, "read marker", error),
                            },
                        );
                    }
                    None => self.push(Tone::Error, "read marker: no message seen yet".into()),
                }
            }
        });
    }

    /// A WebSocket call in the joined room.
    fn in_room(
        &mut self,
        room: Option<RoomId>,
        label: String,
        call: impl FnOnce(&WsConnection, &Sender<Event>, RoomId) + Send + 'static,
    ) {
        match room {
            Some(room) => self.run_ws(label, move |ws, out| call(ws, out, room)),
            None => self.push(Tone::Error, format!("{label}: join `world` first")),
        }
    }

    fn leaderboards_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("board");
            ui.add(egui::TextEdit::singleline(&mut self.board).desired_width(90.0));
            ui.label("score");
            ui.add(egui::TextEdit::singleline(&mut self.score).desired_width(70.0));
        });
        ui.horizontal(|ui| {
            let board = self.board.clone();
            if ui.button("Submit").clicked() {
                match self.score.trim().parse::<i64>() {
                    Ok(score) => {
                        let b = board.clone();
                        self.run(
                            format!("submit {score} to `{board}`"),
                            move |client, out| match client
                                .call(&PostScore::new(b, SubmitScore::new(score)))
                            {
                                Ok(ack) => ok(
                                    out,
                                    format!(
                                        "score {} (changed: {}), rank {}",
                                        ack.score, ack.changed, ack.rank
                                    ),
                                ),
                                Err(error) => report(out, "submit", error),
                            },
                        );
                    }
                    Err(_) => self.push(Tone::Error, "the score is not a whole number".into()),
                }
            }
            if ui.button("Top").clicked() {
                let b = board.clone();
                self.run(format!("top of `{board}`"), move |client, out| match client
                    .call(&GetLeaderboard::new(b))
                {
                    Ok(page) => ok(out, board_lines("top", &page.items)),
                    Err(error) => report(out, "top", error),
                });
            }
            if ui.button("Around me").clicked() {
                let b = board.clone();
                self.run(
                    format!("around me on `{board}`"),
                    move |client, out| match client.call(&GetAroundMe::new(b)) {
                        Ok(page) => ok(out, board_lines("around me", &page.items)),
                        Err(error) => report(out, "around me", error),
                    },
                );
            }
        });
    }

    fn notifications_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("List").clicked() {
                self.run("list notifications".into(), |client, out| {
                    match client.call(&NotificationQuery::new()) {
                        Ok(page) => {
                            let mut text =
                                format!("{} notifications (newest first)", page.items.len());
                            for n in page.items.iter().take(8) {
                                let read = if n.read { "read" } else { "new" };
                                let body = n.text.clone().unwrap_or_default();
                                text.push_str(&format!(
                                    "\n    {} {} [{read}] {body}",
                                    n.id, n.kind
                                ));
                            }
                            ok(out, text);
                        }
                        Err(error) => report(out, "notifications", error),
                    }
                });
            }
            if ui.button("Mark all read").clicked() {
                self.run(
                    "mark every notification read".into(),
                    |client, out| match client.call(&MarkNotifications::all_read()) {
                        Ok(ack) => ok(
                            out,
                            format!("{} marked, {} unread", ack.changed, ack.unread),
                        ),
                        Err(error) => report(out, "mark read", error),
                    },
                );
            }
            ui.label("(new ones arrive as pushes)");
        });
    }

    fn friends_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("friend code");
            ui.add(egui::TextEdit::singleline(&mut self.friend_code).desired_width(110.0));
        });
        ui.horizontal(|ui| {
            if ui.button("My code").clicked() {
                self.run("my friend code".into(), |client, out| {
                    match client.call(&GetFriendCode::new()) {
                        Ok(code) => ok(out, format!("my friend code: {}", code.code)),
                        Err(error) => report(out, "friend code", error),
                    }
                });
            }
            if ui.button("Add by code").clicked() {
                let code = self.friend_code.trim().to_string();
                self.run(
                    format!("friend request to code {code}"),
                    move |client, out| match client.call(&AddFriend::by_code(code)) {
                        Ok(entry) => ok(out, format!("player {}: {:?}", entry.user, entry.state)),
                        Err(error) => report(out, "add friend", error),
                    },
                );
            }
            if ui.button("List").clicked() {
                self.run(
                    "list friends and received requests".into(),
                    |client, out| {
                        match client.call(&ListFriends::new()) {
                            Ok(page) => {
                                let names: Vec<String> = page
                                    .items
                                    .iter()
                                    .map(|f| {
                                        let online = if f.online == Some(true) {
                                            " (online)"
                                        } else {
                                            ""
                                        };
                                        format!("{}{online}", who(f.user, &f.name))
                                    })
                                    .collect();
                                ok(
                                    out,
                                    format!("{} friends: {}", names.len(), names.join(", ")),
                                );
                            }
                            Err(error) => report(out, "friends", error),
                        }
                        match client.call(&ListFriendRequests::received()) {
                            Ok(page) => {
                                let users: Vec<UserId> =
                                    page.items.iter().map(|f| f.user).collect();
                                ok(out, format!("{} requests from {users:?}", users.len()));
                                update(out, move |d| d.requests = users);
                            }
                            Err(error) => report(out, "requests", error),
                        }
                    },
                );
            }
            if ui.button("Accept").clicked() {
                let users = self.requests.clone();
                if users.is_empty() {
                    self.push(Tone::Error, "accept: no request listed (List first)".into());
                } else {
                    self.run(
                        format!("accept the requests of {users:?}"),
                        move |client, out| {
                            for user in users {
                                match client.call(&AcceptFriend::new(user)) {
                                    Ok(entry) => {
                                        ok(out, format!("player {}: {:?}", entry.user, entry.state))
                                    }
                                    Err(error) => report(out, "accept", error),
                                }
                            }
                            update(out, |d| d.requests.clear());
                        },
                    );
                }
            }
        });
    }

    fn groups_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("name");
            ui.add(egui::TextEdit::singleline(&mut self.group_name).desired_width(110.0));
            ui.label("player id");
            ui.add(egui::TextEdit::singleline(&mut self.player).desired_width(50.0));
        });
        ui.horizontal(|ui| {
            if ui.button("Create").clicked() {
                let name = self.group_name.clone();
                self.run(
                    format!("create the group `{name}`"),
                    move |client, out| match client.call(&CreateGroup::new(name)) {
                        Ok(group) => {
                            ok(
                                out,
                                format!(
                                    "group {} `{}`, {} member",
                                    group.id, group.name, group.members
                                ),
                            );
                            update(out, move |d| d.group = Some(group.id));
                        }
                        Err(error) => report(out, "create group", error),
                    },
                );
            }
            if ui.button("Invite").clicked() {
                match (self.group, self.player.trim().parse::<i64>()) {
                    (Some(group), Ok(user)) => {
                        let user = UserId::new(user);
                        self.run(
                            format!("invite player {user} to group {group}"),
                            move |client, out| match client
                                .call(&InviteToGroup::new(group, Invitee::new(user)))
                            {
                                Ok(_) => ok(
                                    out,
                                    format!("player {user} invited (a notification for them)"),
                                ),
                                Err(error) => report(out, "invite", error),
                            },
                        );
                    }
                    (None, _) => {
                        self.push(Tone::Error, "invite: create or list a group first".into())
                    }
                    (_, Err(_)) => {
                        self.push(Tone::Error, "invite: the player id is a number".into())
                    }
                }
            }
            if ui.button("List").clicked() {
                self.run("my groups".into(), |client, out| {
                    match client.call(&MyGroups::new()) {
                        Ok(list) => {
                            let names: Vec<String> = list
                                .groups
                                .iter()
                                .map(|g| format!("{} `{}` ({} members)", g.id, g.name, g.members))
                                .collect();
                            ok(out, format!("{} groups: {}", names.len(), names.join(", ")));
                            if let Some(first) = list.groups.first().map(|g| g.id) {
                                update(out, move |d| d.group = d.group.or(Some(first)));
                            }
                        }
                        Err(error) => report(out, "groups", error),
                    }
                });
            }
        });
    }

    fn lobbies_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("join code");
            ui.add(egui::TextEdit::singleline(&mut self.lobby_code).desired_width(110.0));
        });
        ui.horizontal(|ui| {
            if ui.button("Create").clicked() {
                self.run("create a lobby of 4".into(), |client, out| {
                    match client.call(&CreateLobby::new(4)) {
                        Ok(lobby) => lobby_joined(out, "created", lobby),
                        Err(error) => report(out, "create lobby", error),
                    }
                });
            }
            if ui.button("Join by code").clicked() {
                let code = self.lobby_code.trim().to_string();
                self.run(
                    format!("join the lobby with code {code}"),
                    move |client, out| match client.call(&JoinLobbyByCode::new(code)) {
                        Ok(lobby) => lobby_joined(out, "joined", lobby),
                        Err(error) => report(out, "join lobby", error),
                    },
                );
            }
            if ui.button("Ready").clicked() {
                match self.lobby {
                    Some(lobby) => {
                        let ready = !self.ready;
                        self.run(
                            format!("ready = {ready} in lobby {lobby}"),
                            move |client, out| match client
                                .call(&SetLobbyReady::new(lobby, SetReady::new(ready)))
                            {
                                Ok(_) => {
                                    ok(out, format!("ready: {ready}"));
                                    update(out, move |d| d.ready = ready);
                                }
                                Err(error) => report(out, "ready", error),
                            },
                        );
                    }
                    None => self.push(Tone::Error, "ready: create or join a lobby first".into()),
                }
            }
            if ui.button("Leave").clicked() {
                match self.lobby {
                    Some(lobby) => {
                        self.run(
                            format!("leave lobby {lobby}"),
                            move |client, out| match client.call(&LeaveLobby::new(lobby)) {
                                Ok(_) => {
                                    ok(out, "left the lobby".into());
                                    update(out, |d| {
                                        d.lobby = None;
                                        d.joined_code = None;
                                        d.ready = false;
                                    });
                                }
                                Err(error) => report(out, "leave", error),
                            },
                        )
                    }
                    None => self.push(Tone::Error, "leave: no lobby".into()),
                }
            }
        });
    }

    fn matchmaking_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Queues").clicked() {
                self.run("matchmaking queues".into(), |client, out| {
                    match client.call(&ListQueues::new()) {
                        Ok(list) => {
                            let queues: Vec<String> =
                                list.queues.iter().map(|q| q.key.clone()).collect();
                            let text: Vec<String> = list
                                .queues
                                .iter()
                                .map(|q| {
                                    format!(
                                        "`{}` ({} players, {} waiting)",
                                        q.key, q.players, q.waiting
                                    )
                                })
                                .collect();
                            ok(out, format!("queues: {}", text.join(", ")));
                            update(out, move |d| d.queues = queues);
                        }
                        Err(error) => report(out, "queues", error),
                    }
                });
            }
            for queue in self.queues.clone() {
                if ui.button(format!("Queue `{queue}`")).clicked() {
                    let q = queue.clone();
                    self.run(
                        format!("queue in `{queue}`"),
                        move |client, out| match client.call(&CreateTicket::new(q)) {
                            Ok(ticket) => {
                                ok(out, format!("ticket {} {:?}", ticket.id, ticket.status))
                            }
                            Err(error) => report(out, "queue", error),
                        },
                    );
                }
            }
            if ui.button("Cancel").clicked() {
                self.run("cancel my ticket".into(), |client, out| {
                    match client.call(&CancelTicket::new()) {
                        Ok(_) => ok(out, "out of the queue".into()),
                        Err(error) => report(out, "cancel", error),
                    }
                });
            }
        });
    }

    fn files_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Upload").clicked() {
                self.run("upload hello.txt".into(), |client, out| {
                    let text = format!("hello from the {NAME} demo at {}", clock());
                    let upload = FileUpload::bytes("hello.txt", text.into_bytes())
                        .content_type("text/plain");
                    match client.upload_file(upload) {
                        Ok(file) => {
                            ok(out, file_line("uploaded", &file));
                            update(out, move |d| d.file = Some(file.id));
                        }
                        Err(error) => report(out, "upload", error),
                    }
                });
            }
            if ui.button("List").clicked() {
                self.run("list my files".into(), |client, out| {
                    match client.call(&ListFiles::new()) {
                        Ok(page) => {
                            let mut text = format!("{} files", page.items.len());
                            for file in page.items.iter().take(8) {
                                text.push_str(&format!("\n    {}", file_line("", file)));
                            }
                            ok(out, text);
                            if let Some(first) = page.items.first().map(|f| f.id) {
                                update(out, move |d| d.file = d.file.or(Some(first)));
                            }
                        }
                        Err(error) => report(out, "files", error),
                    }
                });
            }
            if ui.button("Download").clicked() {
                match self.file {
                    Some(file) => {
                        self.run(
                            format!("download file {file}"),
                            move |client, out| match client
                                .download_file(file, DownloadOptions::default())
                            {
                                Ok(bytes) => ok(
                                    out,
                                    format!(
                                        "{} bytes: {}",
                                        bytes.len(),
                                        String::from_utf8_lossy(&bytes)
                                    ),
                                ),
                                Err(error) => report(out, "download", error),
                            },
                        );
                    }
                    None => self.push(Tone::Error, "download: upload or list first".into()),
                }
            }
        });
    }

    /// Takes the events of the background calls and the WebSocket's pushes.
    fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Log(tone, text) => self.push(tone, text),
                Event::Update(change) => change(self),
            }
        }
        if let Some(streams) = self.streams.as_mut() {
            for text in streams.drain(self.room, &mut self.last_seen) {
                self.push(Tone::Push, text);
            }
        }
    }

    /// One panel, shown when the server has the module.
    fn panel(
        &mut self,
        ui: &mut egui::Ui,
        module: &str,
        title: &str,
        show: fn(&mut Self, &mut egui::Ui),
    ) {
        if self.has(module) {
            egui::CollapsingHeader::new(title)
                .default_open(true)
                .show(ui, |ui| show(self, ui));
        }
    }
}

impl eframe::App for Demo {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        #[cfg(feature = "steam")]
        self.steam_frame(ctx);
        if self.ws.is_some() {
            // Pushes arrive without input: look for them a few times a second.
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("status").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(format!("{NAME} demo"));
                ui.label(format!("server {}", self.url));
                if ui.button("Info").clicked() {
                    self.info();
                }
                let state = match self.account {
                    Some(id) => format!("account {id}"),
                    None => "not logged in".to_string(),
                };
                let ws = if self.ws.is_some() {
                    "WebSocket open"
                } else {
                    "no WebSocket"
                };
                let lobby = match &self.joined_code {
                    Some(code) => format!(" | lobby {}", code.grouped()),
                    None => String::new(),
                };
                ui.label(format!("| {state} | {ws}{lobby}"));
            });
            ui.add_space(4.0);
        });
        egui::Panel::left("modules")
            .default_size(460.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.modules.is_empty() {
                        ui.label("No module on the server (or no answer yet): Info asks again.");
                    }
                    self.panel(ui, "auth", "Account (auth)", Self::account_panel);
                    self.panel(ui, "storage", "Saves (storage)", Self::storage_panel);
                    self.panel(ui, "chat", "Chat", Self::chat_panel);
                    self.panel(ui, "leaderboards", "Leaderboards", Self::leaderboards_panel);
                    self.panel(
                        ui,
                        "notifications",
                        "Notifications",
                        Self::notifications_panel,
                    );
                    self.panel(ui, "friends", "Friends", Self::friends_panel);
                    self.panel(ui, "groups", "Groups", Self::groups_panel);
                    self.panel(ui, "lobbies", "Lobbies", Self::lobbies_panel);
                    self.panel(ui, "matchmaking", "Matchmaking", Self::matchmaking_panel);
                    self.panel(ui, "files", "Files", Self::files_panel);
                    #[cfg(feature = "steam")]
                    if self.has("friends") || self.has("lobbies") {
                        self.panel(ui, "auth", "Steam", Self::steam_panel);
                    }
                });
            });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Log (client side: -> sent, <- answer, << push, xx error)");
                if ui.button("Clear").clicked() {
                    self.log.clear();
                }
            });
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.log {
                        let color = match line.tone {
                            Tone::Sent => egui::Color32::LIGHT_GRAY,
                            Tone::Ok => egui::Color32::from_rgb(110, 220, 160),
                            Tone::Error => egui::Color32::from_rgb(255, 110, 110),
                            Tone::Push => egui::Color32::from_rgb(110, 170, 255),
                        };
                        let text = format!("{} {} {}", line.time, line.tone.mark(), line.text);
                        ui.label(egui::RichText::new(text).monospace().color(color));
                    }
                });
        });
    }
}

/// After a login: the account, then the WebSocket (pushes of every module arrive on it).
fn logged_in(client: &Client, out: &Sender<Event>, what: &str, account: UserId) {
    ok(out, format!("{what}: account {account}"));
    update(out, move |d| d.account = Some(account));
    match client.connect_ws(WsSettings::default()) {
        Ok(ws) => {
            ok(out, "WebSocket connected (pushes arrive here)".into());
            update(out, move |d| {
                if let Some(old) = d.ws.take() {
                    old.close();
                }
                d.streams = Some(Streams::new(&ws));
                d.ws = Some(ws);
                d.room = None;
            });
        }
        Err(error) => report(out, "connect", error),
    }
}

/// Joins `world` and reports the room.
fn join(ws: &WsConnection, out: &Sender<Event>) {
    match ws.request(&JoinRoom::new("world")).wait() {
        Ok(room) => {
            let online = room
                .member_count
                .map(|n| format!(", {n} online"))
                .unwrap_or_default();
            ok(out, format!("joined `world` (room {}{online})", room.id));
            update(out, move |d| d.room = Some(room.id));
        }
        Err(error) => report(out, "join", error),
    }
}

fn lobby_joined(out: &Sender<Event>, what: &str, lobby: LobbyInfo) {
    let code = lobby.code.as_ref().map(|c| c.grouped()).unwrap_or_default();
    ok(
        out,
        format!(
            "{what} lobby {} ({}/{} players), join code {code}",
            lobby.id, lobby.members, lobby.max_players
        ),
    );
    update(out, move |d| {
        d.lobby = Some(lobby.id);
        d.joined_code = lobby.code;
        d.ready = false;
    });
}

fn board_lines(what: &str, items: &[LeaderboardEntry]) -> String {
    let mut text = format!("{what}: {} entries", items.len());
    for entry in items.iter().take(10) {
        text.push_str(&format!(
            "\n    #{} {}: {}",
            entry.rank,
            who(entry.user, &entry.name),
            entry.score
        ));
    }
    text
}

fn file_line(what: &str, file: &FileInfo) -> String {
    format!(
        "{what} file {} `{}` ({} bytes, {:?})",
        file.id, file.name, file.size, file.visibility
    )
    .trim_start()
    .to_string()
}

fn ok(out: &Sender<Event>, text: String) {
    let _ = out.send(Event::Log(Tone::Ok, text));
}

fn update(out: &Sender<Event>, change: impl FnOnce(&mut Demo) + Send + 'static) {
    let _ = out.send(Event::Update(Box::new(change)));
}

fn report(out: &Sender<Event>, what: &str, error: net_backend_client::Error) {
    let _ = out.send(Event::Log(Tone::Error, format!("{what}: {error}")));
}

fn who(user: UserId, name: &Option<String>) -> String {
    name.clone().unwrap_or_else(|| format!("player {user}"))
}

fn sender(message: &ChatMessage) -> String {
    who(message.sender, &message.sender_name)
}

fn now_ms() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
}

/// `hh:mm:ss` (UTC).
fn clock() -> String {
    let secs = now_ms() / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60
    )
}

/// Whether the URL's host is this machine (127.0.0.1, ::1, localhost): the development password
/// is filled in there only.
fn is_loopback(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
