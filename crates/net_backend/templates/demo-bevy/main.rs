//! The {{name}} demo: a small Bevy app on bevy_net_backend. A row of buttons per module of the
//! server (only the modules it reports in `/v1/info`); each button makes one call, and the log
//! shows what the client sent, what came back and what the server pushed. The server's own window
//! shows the other side.
//!
//! ```text
//! {{demo_run}}
//! {{demo_run}} -- player2@example.com
//! ```
//!
//! The server is `http://127.0.0.1:8080` (the one `cargo run` starts in the server folder); the
//! variable `NET_BACKEND_URL` names another. The account is a development account
//! (`player@example.com`, or the one the argument names: a second player in a second window):
//! Register once, then Login. The development password is used on this machine only; another
//! server needs the variable `NET_BACKEND_PASSWORD`. Friend codes, join codes and player ids are
//! typed on the keyboard into the input line. Guides: <https://docs.rs/bevy_net_backend>,
//! <https://docs.rs/net_backend_protocol>.

mod invite;
#[cfg(feature = "steam")]
mod steam;

use std::collections::HashMap;
use std::time::Duration;

use bevy::input::ButtonState;
use bevy::input::keyboard::{KeyCode, KeyboardInput};
use bevy::prelude::*;
use bevy::text::FontSize;
use bevy_net_backend::http::Method;
use bevy_net_backend::prelude::*;
use net_backend_protocol::auth::{
    AuthSession, LoginRequest, LogoutRequest, RefreshRequest, RegisterRequest, TokenPair,
};
use net_backend_protocol::chat::{
    ChatHistory, ChatMessage, EditMessage, JoinRoom, ListMembers, MarkRead, MessageEdited,
    Presence, ReadReceipt, RoomInfo, RoomMembers, SendAck, SendMessage, SetTyping, TypingUpdate,
};
use net_backend_protocol::files::{FileInfo, ListFiles};
use net_backend_protocol::friends::{
    AcceptFriend, AddFriend, FriendCode, FriendEntry, FriendPresence, GetFriendCode,
    ListFriendRequests, ListFriends,
};
use net_backend_protocol::groups::{
    CreateGroup, GroupInfo, GroupList, InviteToGroup, Invitee, MyGroups,
};
use net_backend_protocol::leaderboards::{
    GetAroundMe, GetLeaderboard, LeaderboardPage, PostScore, ScoreAck, SubmitScore,
};
use net_backend_protocol::lobbies::{
    CreateLobby, JoinLobbyByCode, LeaveLobby, LobbyCode, LobbyInfo, LobbyMemberUpdate, LobbyUpdate,
    SetLobbyReady, SetReady,
};
use net_backend_protocol::matchmaking::{
    CancelTicket, CreateTicket, ListQueues, MatchFound, MatchTicket, Queues, TicketExpired,
};
use net_backend_protocol::notifications::{
    MarkAck, MarkNotifications, Notification, NotificationQuery,
};
use net_backend_protocol::routes::{self, HttpMethod};
use net_backend_protocol::storage::{
    GetObject, ListObjects, ObjectAck, PutObject, StorageObject, StorageObjectInfo, WriteObject,
};
use net_backend_protocol::version::GetServerInfo;
use net_backend_protocol::{
    Ack, ErrorBody, FileId, GroupId, HttpCall, LobbyId, MessageId, PROTOCOL_HEADER,
    PROTOCOL_VERSION, Page, PayloadKind, RoomId, ServerInfo, UserId,
};

/// The project's name.
const NAME: &str = "{{name}}";
/// The development account (another one: its email as the argument; this machine only).
const DEV_EMAIL: &str = "player@example.com";
const DEV_PASSWORD: &str = "dev password 1234";
/// Register / Login on another server without `NET_BACKEND_PASSWORD`.
const NO_PASSWORD: &str = "xx set NET_BACKEND_PASSWORD for a server on another machine";
/// The WebSocket connection's name.
const MAIN: &str = "main";
/// The save the storage buttons use.
const COLLECTION: &str = "saves";
const KEY: &str = "slot1";
/// The leaderboard the buttons use (config.toml: `[[modules.leaderboards.boards]]`).
const BOARD: &str = "highscore";

/// The buttons.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum Action {
    Info,
    Clear,
    Register,
    Login,
    Refresh,
    Logout,
    Save,
    Load,
    List,
    Join,
    Send,
    History,
    Presence,
    Edit,
    Typing,
    ReadMarker,
    Submit,
    Top,
    Around,
    Notifications,
    MarkAllRead,
    MyCode,
    AddFriend,
    Friends,
    Accept,
    CreateGroup,
    Invite,
    Groups,
    CreateLobby,
    JoinLobby,
    Ready,
    LeaveLobby,
    Queues,
    QueueDuel,
    QueueSolo,
    Cancel,
    Upload,
    Files,
    Download,
}

/// What an HTTP request in flight was.
#[derive(Clone, Copy)]
enum Http {
    Info,
    Register,
    Login,
    Refresh,
    Logout,
    Save,
    Load,
    List,
    Submit,
    Board,
    Notifications,
    MarkAllRead,
    MyCode,
    Friend,
    Friends,
    Requests,
    Ack,
    Group,
    Groups,
    Lobby,
    Ready(bool),
    Queues,
    Ticket,
    Upload,
    Files,
    Download,
}

#[derive(Resource, Default)]
struct Demo {
    url: String,
    email: String,
    /// `NET_BACKEND_PASSWORD`, else the development password on this machine (`None`: another
    /// server without the variable).
    password: Option<String>,
    modules: Vec<String>,
    tokens: Option<TokenPair>,
    room: Option<RoomId>,
    my_last: Option<MessageId>,
    last_seen: Option<MessageId>,
    requests: Vec<UserId>,
    group: Option<GroupId>,
    lobby: Option<LobbyId>,
    /// The current lobby's join code (Steam's "Join Game" carries it).
    lobby_code: Option<LobbyCode>,
    ready: bool,
    file: Option<FileId>,
    sent: u32,
    /// The input line (typed on the keyboard): friend codes, join codes, player ids.
    input: String,
    pending: HashMap<RequestId, Http>,
    /// WebSocket requests answered by a plain `Ack`, by what they were.
    acks: HashMap<RequestId, &'static str>,
    /// Newest first.
    log: Vec<String>,
}

impl Demo {
    /// A line in the window's log (newest first), also printed to the terminal.
    fn log(&mut self, line: impl Into<String>) {
        let line = line.into();
        info!("{line}");
        self.log.insert(0, line);
        self.log.truncate(200);
    }

    fn has(&self, module: &str) -> bool {
        self.modules.iter().any(|m| m == module)
    }
}

/// Where the Steam row goes (feature `steam`; hidden without it).
#[derive(Component)]
struct SteamSlot;

/// A row of buttons and the module it belongs to (`None`: always shown).
#[derive(Component)]
struct Row(Option<&'static str>);

/// The texts `draw` keeps current.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum Live {
    Status,
    Input,
    Log,
}

fn main() -> AppExit {
    let url = std::env::var("NET_BACKEND_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    // The first argument with an `@` (a start by Steam adds `+nb_lobby <number>`); arguments
    // that are not Unicode are skipped.
    let email = std::env::args_os()
        .skip(1)
        .filter_map(|arg| arg.into_string().ok())
        .find(|arg| arg.contains('@'))
        .unwrap_or_else(|| DEV_EMAIL.into());
    // The development password only for a server on this machine.
    let password = std::env::var("NET_BACKEND_PASSWORD")
        .ok()
        .or_else(|| is_loopback(&url).then(|| DEV_PASSWORD.to_string()));
    let http = HttpConfig::new(url.clone())
        .with_timeout(Duration::from_secs(15))
        .with_header(PROTOCOL_HEADER, &PROTOCOL_VERSION.to_string());
    let window = Window {
        title: format!("{NAME} demo ({email})"),
        resolution: (1280, 860).into(),
        ..default()
    };
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(window),
        ..default()
    }))
    .add_plugins(BackendPlugin::new(http))
    // The protocol's WebSocket types are the client's typed requests and pushes.
    .add_ws_request::<JoinRoom>()
    .add_ws_request::<SendMessage>()
    .add_ws_request::<ChatHistory>()
    .add_ws_request::<ListMembers>()
    .add_ws_request::<EditMessage>()
    .add_ws_request::<SetTyping>()
    .add_ws_request::<MarkRead>()
    .add_ws_push::<ChatMessage>()
    .add_ws_push::<Presence>()
    .add_ws_push::<MessageEdited>()
    .add_ws_push::<TypingUpdate>()
    .add_ws_push::<ReadReceipt>()
    .add_ws_push::<Notification>()
    .add_ws_push::<FriendPresence>()
    .add_ws_push::<LobbyMemberUpdate>()
    .add_ws_push::<LobbyUpdate>()
    .add_ws_push::<MatchFound>()
    .add_ws_push::<TicketExpired>()
    .insert_resource(Demo {
        url,
        email,
        password,
        ..default()
    })
    .insert_resource(ClearColor(Color::srgb_u8(14, 16, 22)))
    .add_systems(Startup, (spawn_ui, start))
    .add_systems(
        Update,
        (
            type_input,
            click,
            on_http,
            on_ws_state,
            on_join,
            on_send,
            on_history,
            on_members,
            on_edit,
            on_ack,
            on_chat_pushes,
            on_pushes,
            show_rows,
            paint_buttons,
            draw,
        )
            .chain(),
    );
    #[cfg(feature = "steam")]
    steam::add(&mut app);
    app.run()
}

fn start(http: Res<HttpClient>, mut demo: ResMut<Demo>) {
    let line = format!(
        "server {}: Info, then Register (first time) or Login as {}",
        demo.url, demo.email
    );
    demo.log(line);
    send(
        &http,
        &mut demo,
        &GetServerInfo::new(),
        Http::Info,
        "GET /v1/info",
    );
}

// ---- the window ----------------------------------------------------------------------------

fn text(value: impl Into<String>, size: f32) -> impl Bundle {
    (
        Text::new(value),
        TextFont {
            font_size: FontSize::Px(size),
            ..default()
        },
        TextColor(Color::srgb_u8(232, 234, 240)),
    )
}

/// A button with its action and label.
fn button(action: impl Component, label: &str) -> impl Bundle {
    (
        Button,
        action,
        Node {
            padding: UiRect::axes(Val::Px(10.0), Val::Px(5.0)),
            ..default()
        },
        BackgroundColor(Color::srgb_u8(48, 56, 80)),
        children![text(label, 14.0)],
    )
}

/// The rows: a title, the module (`None`: always shown) and the buttons.
type RowSpec = (
    &'static str,
    Option<&'static str>,
    &'static [(Action, &'static str)],
);

const ROWS: &[RowSpec] = &[
    (
        "Server",
        None,
        &[(Action::Info, "Info"), (Action::Clear, "Clear log")],
    ),
    (
        "Account",
        Some("auth"),
        &[
            (Action::Register, "Register"),
            (Action::Login, "Login"),
            (Action::Refresh, "Refresh"),
            (Action::Logout, "Logout"),
        ],
    ),
    (
        "Saves",
        Some("storage"),
        &[
            (Action::Save, "Save"),
            (Action::Load, "Load"),
            (Action::List, "List"),
        ],
    ),
    (
        "Chat",
        Some("chat"),
        &[
            (Action::Join, "Join world"),
            (Action::Send, "Send"),
            (Action::History, "History"),
            (Action::Presence, "Presence"),
            (Action::Edit, "Edit my last"),
            (Action::Typing, "Typing"),
            (Action::ReadMarker, "Read marker"),
        ],
    ),
    (
        "Scores",
        Some("leaderboards"),
        &[
            (Action::Submit, "Submit"),
            (Action::Top, "Top"),
            (Action::Around, "Around me"),
        ],
    ),
    (
        "Notices",
        Some("notifications"),
        &[
            (Action::Notifications, "List"),
            (Action::MarkAllRead, "Mark all read"),
        ],
    ),
    (
        "Friends",
        Some("friends"),
        &[
            (Action::MyCode, "My code"),
            (Action::AddFriend, "Add by code (input)"),
            (Action::Friends, "List"),
            (Action::Accept, "Accept"),
        ],
    ),
    (
        "Groups",
        Some("groups"),
        &[
            (Action::CreateGroup, "Create"),
            (Action::Invite, "Invite player id (input)"),
            (Action::Groups, "List"),
        ],
    ),
    (
        "Lobbies",
        Some("lobbies"),
        &[
            (Action::CreateLobby, "Create"),
            (Action::JoinLobby, "Join by code (input)"),
            (Action::Ready, "Ready"),
            (Action::LeaveLobby, "Leave"),
        ],
    ),
    (
        "Matching",
        Some("matchmaking"),
        &[
            (Action::Queues, "Queues"),
            (Action::QueueDuel, "Queue duel"),
            (Action::QueueSolo, "Queue solo"),
            (Action::Cancel, "Cancel"),
        ],
    ),
    (
        "Files",
        Some("files"),
        &[
            (Action::Upload, "Upload"),
            (Action::Files, "List"),
            (Action::Download, "Download"),
        ],
    ),
];

fn spawn_ui(mut commands: Commands) {
    commands.spawn(Camera2d);
    commands
        .spawn(Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(Val::Px(12.0)),
            row_gap: Val::Px(6.0),
            ..default()
        })
        .with_children(|root| {
            root.spawn((text(format!("{NAME} demo"), 20.0), Live::Status));
            for (title, module, buttons) in ROWS {
                let display = if module.is_none() {
                    Display::Flex
                } else {
                    Display::None
                };
                root.spawn((row_node(display), Row(*module)))
                    .with_children(|row| {
                        row.spawn((
                            text(*title, 14.0),
                            Node {
                                width: Val::Px(76.0),
                                ..default()
                            },
                        ));
                        for (action, label) in buttons.iter() {
                            row.spawn(button(*action, label));
                        }
                    });
            }
            // The Steam row (feature `steam`) goes here.
            root.spawn((row_node(Display::None), SteamSlot));
            root.spawn((text("", 14.0), Live::Input));
            root.spawn(text(
                "Log (client side: -> sent, <- answer, << push, xx error; newest first; also in the terminal)",
                13.0,
            ));
            // The log fills the rest of the window; older lines are cut off at its bottom.
            root.spawn((
                Node {
                    flex_grow: 1.0,
                    padding: UiRect::all(Val::Px(8.0)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(Color::srgb_u8(22, 25, 34)),
            ))
            .with_children(|log| {
                log.spawn((text("", 13.0), Live::Log));
            });
        });
}

/// A row of buttons (it wraps when the window is narrow).
fn row_node(display: Display) -> Node {
    Node {
        column_gap: Val::Px(6.0),
        row_gap: Val::Px(4.0),
        align_items: AlignItems::Center,
        flex_wrap: FlexWrap::Wrap,
        display,
        ..default()
    }
}

/// Shows the rows of the modules the server has.
fn show_rows(demo: Res<Demo>, mut rows: Query<(&Row, &mut Node)>) {
    if !demo.is_changed() {
        return;
    }
    for (row, mut node) in &mut rows {
        let display = match row.0 {
            Some(module) if !demo.has(module) => Display::None,
            _ => Display::Flex,
        };
        if node.display != display {
            node.display = display;
        }
    }
}

fn paint_buttons(mut buttons: Query<(&Interaction, &mut BackgroundColor), With<Button>>) {
    for (interaction, mut background) in &mut buttons {
        let color = match interaction {
            Interaction::Pressed => Color::srgb_u8(90, 110, 170),
            Interaction::Hovered => Color::srgb_u8(64, 76, 110),
            Interaction::None => Color::srgb_u8(48, 56, 80),
        };
        background.set_if_neq(BackgroundColor(color));
    }
}

/// The input line: letters, digits and `-` typed on the keyboard; Backspace deletes.
fn type_input(mut keys: MessageReader<KeyboardInput>, mut demo: ResMut<Demo>) {
    for key in keys.read() {
        if key.state != ButtonState::Pressed {
            continue;
        }
        if key.key_code == KeyCode::Backspace {
            demo.input.pop();
        } else if let Some(typed) = &key.text {
            for c in typed.chars() {
                if (c.is_ascii_alphanumeric() || c == '-') && demo.input.len() < 32 {
                    demo.input.push(c);
                }
            }
        }
    }
}

fn draw(demo: Res<Demo>, connections: Res<WsConnections>, mut texts: Query<(&mut Text, &Live)>) {
    let account = if demo.tokens.is_some() {
        "logged in"
    } else {
        "not logged in"
    };
    let ws = match connections.state(MAIN) {
        Some(WsState::Connected) => "connected",
        Some(_) => "connecting",
        None => "closed",
    };
    for (mut text, live) in &mut texts {
        let line = if *live == Live::Status {
            format!(
                "{NAME} demo  |  {}  |  {}: {account}  |  WebSocket {ws}{}",
                demo.url,
                demo.email,
                demo.lobby_code
                    .as_ref()
                    .map(|c| format!("  |  lobby {}", c.grouped()))
                    .unwrap_or_default()
            )
        } else if *live == Live::Input {
            format!(
                "Input: {}_   (type a code or a player id; Backspace deletes)",
                demo.input
            )
        } else if demo.is_changed() {
            demo.log
                .iter()
                .take(60)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            continue;
        };
        if text.0 != line {
            text.0 = line;
        }
    }
}

// ---- requests ------------------------------------------------------------------------------

/// Sends a protocol call over HTTP: method, path and payload come from its `HttpCall` impl.
fn send<C: HttpCall>(http: &HttpClient, demo: &mut Demo, call: &C, what: Http, label: &str) {
    send_raw(http, demo, request(call), what, label);
}

/// The HTTP request of a protocol call: method, path and payload from its `HttpCall` impl.
fn request<C: HttpCall>(call: &C) -> OutgoingRequest {
    let method = match C::ROUTE.method {
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
        _ => Method::GET,
    };
    let mut request = OutgoingRequest::new(method, call.path().unwrap_or_default());
    match C::PAYLOAD {
        PayloadKind::Json => request = request.with_json(call.payload()),
        PayloadKind::Query => {
            let pairs = net_backend_protocol::http_call::query_pairs(call.payload());
            for (name, value) in pairs.unwrap_or_default() {
                request = request.with_query(name, value);
            }
        }
        _ => {}
    }
    request
}

fn send_raw(
    http: &HttpClient,
    demo: &mut Demo,
    request: OutgoingRequest,
    what: Http,
    label: &str,
) -> RequestId {
    let id = http.send(request);
    demo.pending.insert(id, what);
    demo.log(format!("-> {label}"));
    id
}

/// A WebSocket request answered by `Ack`.
fn ws_ack<R: WsRequest>(ws: &WsClient, demo: &mut Demo, request: &R, label: &'static str) {
    let id = ws.request(MAIN, request);
    demo.acks.insert(id, label);
    demo.log(format!("-> {label}"));
}

fn click(
    buttons: Query<(&Interaction, &Action), Changed<Interaction>>,
    http: Res<HttpClient>,
    ws: Res<WsClient>,
    mut demo: ResMut<Demo>,
) {
    for (interaction, action) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let demo = &mut *demo;
        let email = demo.email.clone();
        let input = demo.input.trim().to_string();
        match action {
            Action::Info => send(
                &http,
                demo,
                &GetServerInfo::new(),
                Http::Info,
                "GET /v1/info",
            ),
            Action::Clear => demo.log.clear(),
            Action::Register => {
                let Some(password) = demo.password.clone() else {
                    demo.log(NO_PASSWORD);
                    continue;
                };
                let call = RegisterRequest::new(email.as_str(), password.as_str());
                send(&http, demo, &call, Http::Register, "register");
            }
            Action::Login => {
                let Some(password) = demo.password.clone() else {
                    demo.log(NO_PASSWORD);
                    continue;
                };
                let call = LoginRequest::new(email.as_str(), password.as_str());
                send(&http, demo, &call, Http::Login, "login");
            }
            Action::Refresh => match demo.tokens.clone() {
                Some(tokens) => {
                    let call = RefreshRequest::new(tokens.refresh_token);
                    send(&http, demo, &call, Http::Refresh, "refresh the tokens");
                }
                None => demo.log("xx refresh: log in first"),
            },
            Action::Logout => {
                let mut call = LogoutRequest::this_session();
                if let Some(tokens) = &demo.tokens {
                    call = call.with_refresh_token(tokens.refresh_token.clone());
                }
                send(&http, demo, &call, Http::Logout, "logout");
                ws.disconnect(MAIN);
            }
            Action::Save => {
                let value = serde_json::json!({ "level": 3, "gold": 120 + demo.sent });
                let call = WriteObject::new(COLLECTION, KEY, PutObject::new(value));
                send(&http, demo, &call, Http::Save, "save saves/slot1");
            }
            Action::Load => {
                let call = GetObject::new(COLLECTION, KEY);
                send(&http, demo, &call, Http::Load, "load saves/slot1");
            }
            Action::List => {
                let call = ListObjects::new(COLLECTION);
                send(&http, demo, &call, Http::List, "list saves");
            }
            Action::Join => {
                if demo.tokens.is_some() {
                    ws.request(MAIN, &JoinRoom::new("world"));
                    demo.log("-> chat.join world");
                } else {
                    demo.log("xx join: log in first");
                }
            }
            Action::Send => match demo.room {
                Some(room) => {
                    demo.sent += 1;
                    let text = format!("hello #{} from the {NAME} demo", demo.sent);
                    ws.request(MAIN, &SendMessage::new(room, text.clone()));
                    demo.log(format!("-> chat.send \"{text}\""));
                }
                None => demo.log("xx send: join world first"),
            },
            Action::History => match demo.room {
                Some(room) => {
                    ws.request(MAIN, &ChatHistory::new(room));
                    demo.log("-> chat.history world");
                }
                None => demo.log("xx history: join world first"),
            },
            Action::Presence => match demo.room {
                Some(room) => {
                    ws.request(MAIN, &ListMembers::new(room));
                    demo.log("-> chat.members world");
                }
                None => demo.log("xx presence: join world first"),
            },
            Action::Edit => match (demo.room, demo.my_last) {
                (Some(room), Some(message)) => {
                    let text = format!("hello #{} (edited)", demo.sent);
                    ws.request(MAIN, &EditMessage::new(room, message, text));
                    demo.log(format!("-> chat.edit message {message}"));
                }
                _ => demo.log("xx edit: send a message first"),
            },
            Action::Typing => match demo.room {
                Some(room) => ws_ack(&ws, demo, &SetTyping::started(room), "chat.set_typing"),
                None => demo.log("xx typing: join world first"),
            },
            Action::ReadMarker => match (demo.room, demo.last_seen) {
                (Some(room), Some(message)) => {
                    ws_ack(&ws, demo, &MarkRead::new(room, message), "chat.mark_read")
                }
                _ => demo.log("xx read marker: no message seen yet"),
            },
            Action::Submit => {
                let score = 100 + i64::from(demo.sent) * 7;
                demo.sent += 1;
                let call = PostScore::new(BOARD, SubmitScore::new(score));
                send(&http, demo, &call, Http::Submit, &format!("submit {score}"));
            }
            Action::Top => {
                let call = GetLeaderboard::new(BOARD);
                send(&http, demo, &call, Http::Board, "top of highscore");
            }
            Action::Around => {
                let call = GetAroundMe::new(BOARD);
                send(&http, demo, &call, Http::Board, "around me on highscore");
            }
            Action::Notifications => {
                let call = NotificationQuery::new();
                send(
                    &http,
                    demo,
                    &call,
                    Http::Notifications,
                    "list notifications",
                );
            }
            Action::MarkAllRead => {
                let call = MarkNotifications::all_read();
                send(
                    &http,
                    demo,
                    &call,
                    Http::MarkAllRead,
                    "mark every notification read",
                );
            }
            Action::MyCode => {
                send(
                    &http,
                    demo,
                    &GetFriendCode::new(),
                    Http::MyCode,
                    "my friend code",
                );
            }
            Action::AddFriend => {
                let call = AddFriend::by_code(input.clone());
                send(
                    &http,
                    demo,
                    &call,
                    Http::Friend,
                    &format!("friend request to code {input}"),
                );
            }
            Action::Friends => {
                send(
                    &http,
                    demo,
                    &ListFriends::new(),
                    Http::Friends,
                    "list friends",
                );
                let call = ListFriendRequests::received();
                send(&http, demo, &call, Http::Requests, "list received requests");
            }
            Action::Accept => {
                let users = std::mem::take(&mut demo.requests);
                if users.is_empty() {
                    demo.log("xx accept: no request listed (List first)");
                }
                for user in users {
                    let call = AcceptFriend::new(user);
                    send(
                        &http,
                        demo,
                        &call,
                        Http::Friend,
                        &format!("accept player {user}"),
                    );
                }
            }
            Action::CreateGroup => {
                let name = format!("Testers {}", demo.sent + 1);
                demo.sent += 1;
                let label = format!("create the group `{name}`");
                send(&http, demo, &CreateGroup::new(name), Http::Group, &label);
            }
            Action::Invite => match (demo.group, input.parse::<i64>()) {
                (Some(group), Ok(user)) => {
                    let call = InviteToGroup::new(group, Invitee::new(UserId::new(user)));
                    let label = format!("invite player {user} to group {group}");
                    send(&http, demo, &call, Http::Ack, &label);
                }
                (None, _) => demo.log("xx invite: create or list a group first"),
                (_, Err(_)) => demo.log("xx invite: type the player's id (a number) first"),
            },
            Action::Groups => send(&http, demo, &MyGroups::new(), Http::Groups, "my groups"),
            Action::CreateLobby => {
                let call = CreateLobby::new(4);
                send(&http, demo, &call, Http::Lobby, "create a lobby of 4");
            }
            Action::JoinLobby => {
                let call = JoinLobbyByCode::new(input.clone());
                send(
                    &http,
                    demo,
                    &call,
                    Http::Lobby,
                    &format!("join the lobby with code {input}"),
                );
            }
            Action::Ready => match demo.lobby {
                Some(lobby) => {
                    let ready = !demo.ready;
                    let call = SetLobbyReady::new(lobby, SetReady::new(ready));
                    send(
                        &http,
                        demo,
                        &call,
                        Http::Ready(ready),
                        &format!("ready = {ready}"),
                    );
                }
                None => demo.log("xx ready: create or join a lobby first"),
            },
            Action::LeaveLobby => match demo.lobby.take() {
                Some(lobby) => {
                    demo.lobby_code = None;
                    let call = LeaveLobby::new(lobby);
                    send(
                        &http,
                        demo,
                        &call,
                        Http::Ack,
                        &format!("leave lobby {lobby}"),
                    );
                }
                None => demo.log("xx leave: no lobby"),
            },
            Action::Queues => send(&http, demo, &ListQueues::new(), Http::Queues, "queues"),
            Action::QueueDuel | Action::QueueSolo => {
                let queue = if *action == Action::QueueDuel {
                    "duel"
                } else {
                    "solo"
                };
                let call = CreateTicket::new(queue);
                send(
                    &http,
                    demo,
                    &call,
                    Http::Ticket,
                    &format!("queue in `{queue}`"),
                );
            }
            Action::Cancel => {
                send(
                    &http,
                    demo,
                    &CancelTicket::new(),
                    Http::Ack,
                    "cancel my ticket",
                );
            }
            Action::Upload => {
                let body = format!("hello #{} from the {NAME} demo", demo.sent);
                demo.sent += 1;
                // The file part only (the `meta` part is optional); the protocol names the parts.
                let form = Multipart::new().file(
                    net_backend_protocol::files::UPLOAD_FILE_PART,
                    "hello.txt",
                    "text/plain",
                    body.into_bytes(),
                );
                let request =
                    OutgoingRequest::new(Method::POST, routes::files::LIST).with_multipart(&form);
                send_raw(&http, demo, request, Http::Upload, "upload hello.txt");
            }
            Action::Files => send(&http, demo, &ListFiles::new(), Http::Files, "list my files"),
            Action::Download => match demo.file {
                Some(file) => {
                    let request =
                        OutgoingRequest::new(Method::GET, routes::file_content_path(file));
                    send_raw(
                        &http,
                        demo,
                        request,
                        Http::Download,
                        &format!("download file {file}"),
                    );
                }
                None => demo.log("xx download: upload or list first"),
            },
        }
    }
}

// ---- answers -------------------------------------------------------------------------------

/// The protocol's error (code and message) of a refused request, or the transport error.
fn failure(error: &BackendError) -> String {
    if let BackendError::Status(raw) = error
        && let Ok(body) = raw.json::<ErrorBody>()
    {
        return format!("{} ({})", body.error.code, body.error.message);
    }
    error.to_string()
}

fn on_http(
    mut answers: MessageReader<HttpResponse>,
    mut demo: ResMut<Demo>,
    mut credentials: ResMut<BackendCredentials>,
    ws: Res<WsClient>,
) {
    for answer in answers.read() {
        let Some(what) = demo.pending.remove(&answer.id) else {
            continue;
        };
        let raw = match &answer.result {
            Ok(raw) => raw,
            Err(error) => {
                demo.log(format!("xx {}", failure(error)));
                continue;
            }
        };
        let demo = &mut *demo;
        let line = match what {
            Http::Info => raw.json::<ServerInfo>().map(|info| {
                demo.modules = info.modules.clone();
                format!("<- protocol {}, modules {:?}", info.protocol, info.modules)
            }),
            Http::Register | Http::Login => raw.json::<AuthSession>().map(|session| {
                credentials.set(session.tokens.access_token.clone());
                demo.tokens = Some(session.tokens);
                // The WebSocket: every module's pushes arrive on it (the room is joined on connect).
                let ws_url = format!("{}{}", demo.url.replacen("http", "ws", 1), routes::WS);
                let header = PROTOCOL_VERSION.to_string();
                ws.connect(
                    MAIN,
                    WsSettings::new(ws_url).with_header(PROTOCOL_HEADER, &header),
                );
                format!(
                    "<- logged in: account {}; connecting the WebSocket",
                    session.account.id
                )
            }),
            Http::Refresh => raw.json::<TokenPair>().map(|tokens| {
                credentials.set(tokens.access_token.clone());
                demo.tokens = Some(tokens);
                "<- new token pair".to_string()
            }),
            Http::Logout => {
                credentials.clear();
                // What belonged to the session (the server drops the lobby place when the
                // WebSocket stays closed).
                demo.tokens = None;
                demo.room = None;
                demo.my_last = None;
                demo.requests.clear();
                demo.group = None;
                demo.lobby = None;
                demo.lobby_code = None;
                demo.ready = false;
                demo.file = None;
                Ok("<- logged out".to_string())
            }
            Http::Save => raw
                .json::<ObjectAck>()
                .map(|ack| format!("<- saved, version {}", ack.version.get())),
            Http::Load => raw
                .json::<StorageObject>()
                .map(|object| format!("<- version {}: {}", object.version.get(), object.value)),
            Http::List => raw.json::<Page<StorageObjectInfo>>().map(|page| {
                let keys: Vec<&str> = page.items.iter().map(|o| o.key.as_str()).collect();
                format!("<- {} saves: {}", keys.len(), keys.join(", "))
            }),
            Http::Submit => raw.json::<ScoreAck>().map(|ack| {
                format!(
                    "<- score {} (changed: {}), rank {}",
                    ack.score, ack.changed, ack.rank
                )
            }),
            Http::Board => raw.json::<LeaderboardPage>().map(|page| {
                let rows: Vec<String> = page
                    .items
                    .iter()
                    .take(8)
                    .map(|e| format!("#{} {}: {}", e.rank, who(e.user, &e.name), e.score))
                    .collect();
                format!("<- {} entries: {}", page.items.len(), rows.join(", "))
            }),
            Http::Notifications => raw.json::<Page<Notification>>().map(|page| {
                let rows: Vec<String> = page
                    .items
                    .iter()
                    .take(6)
                    .map(|n| {
                        let read = if n.read { "read" } else { "new" };
                        format!("{} {} [{read}]", n.id, n.kind)
                    })
                    .collect();
                format!("<- {} notifications: {}", page.items.len(), rows.join(", "))
            }),
            Http::MarkAllRead => raw
                .json::<MarkAck>()
                .map(|ack| format!("<- {} marked, {} unread", ack.changed, ack.unread)),
            Http::MyCode => raw
                .json::<FriendCode>()
                .map(|code| format!("<- my friend code: {}", code.code)),
            Http::Friend => raw
                .json::<FriendEntry>()
                .map(|f| format!("<- player {}: {:?}", f.user, f.state)),
            Http::Friends => raw.json::<Page<FriendEntry>>().map(|page| {
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
                format!("<- {} friends: {}", names.len(), names.join(", "))
            }),
            Http::Requests => raw.json::<Page<FriendEntry>>().map(|page| {
                demo.requests = page.items.iter().map(|f| f.user).collect();
                format!(
                    "<- {} requests from {:?}",
                    demo.requests.len(),
                    demo.requests
                )
            }),
            Http::Ack => raw.json::<Ack>().map(|_| "<- done".to_string()),
            Http::Group => raw.json::<GroupInfo>().map(|group| {
                demo.group = Some(group.id);
                format!("<- group {} `{}`", group.id, group.name)
            }),
            Http::Groups => raw.json::<GroupList>().map(|list| {
                if demo.group.is_none() {
                    demo.group = list.groups.first().map(|g| g.id);
                }
                let names: Vec<String> = list
                    .groups
                    .iter()
                    .map(|g| format!("{} `{}` ({} members)", g.id, g.name, g.members))
                    .collect();
                format!("<- {} groups: {}", names.len(), names.join(", "))
            }),
            Http::Lobby => raw.json::<LobbyInfo>().map(|lobby| {
                demo.lobby = Some(lobby.id);
                demo.lobby_code = lobby.code.clone();
                demo.ready = false;
                let code = lobby.code.as_ref().map(|c| c.grouped()).unwrap_or_default();
                format!(
                    "<- lobby {} ({}/{} players), join code {code}",
                    lobby.id, lobby.members, lobby.max_players
                )
            }),
            Http::Ready(ready) => raw.json::<Ack>().map(|_| {
                demo.ready = ready;
                format!("<- ready: {ready}")
            }),
            Http::Queues => raw.json::<Queues>().map(|list| {
                let queues: Vec<String> = list
                    .queues
                    .iter()
                    .map(|q| format!("`{}` ({} players, {} waiting)", q.key, q.players, q.waiting))
                    .collect();
                format!("<- queues: {}", queues.join(", "))
            }),
            Http::Ticket => raw
                .json::<MatchTicket>()
                .map(|ticket| format!("<- ticket {} {:?}", ticket.id, ticket.status)),
            Http::Upload => raw.json::<FileInfo>().map(|file| {
                demo.file = Some(file.id);
                format!("<- uploaded {}", file_line(&file))
            }),
            Http::Files => raw.json::<Page<FileInfo>>().map(|page| {
                if demo.file.is_none() {
                    demo.file = page.items.first().map(|f| f.id);
                }
                let files: Vec<String> = page.items.iter().take(6).map(file_line).collect();
                format!("<- {} files: {}", page.items.len(), files.join(", "))
            }),
            Http::Download => Ok(format!(
                "<- {} bytes: {}",
                raw.body().len(),
                String::from_utf8_lossy(raw.body())
            )),
        };
        match line {
            Ok(line) => demo.log(line),
            Err(error) => demo.log(format!("xx the answer did not decode: {error}")),
        }
    }
}

fn on_ws_state(
    mut changes: MessageReader<WsStateChanged>,
    ws: Res<WsClient>,
    mut demo: ResMut<Demo>,
) {
    for change in changes.read() {
        match &change.state {
            // Room membership ends with each connection: join (again) on every connect.
            WsState::Connected => {
                demo.log("<- WebSocket connected (pushes arrive here)");
                if demo.has("chat") {
                    ws.request(MAIN, &JoinRoom::new("world"));
                    demo.log("-> chat.join world");
                }
            }
            WsState::Disconnected => {
                demo.room = None;
                match &change.error {
                    Some(error) => demo.log(format!("xx WebSocket closed: {error}")),
                    None => demo.log("<- WebSocket closed"),
                }
            }
            _ => {}
        }
    }
}

fn on_join(mut answers: MessageReader<WsResponse<RoomInfo>>, mut demo: ResMut<Demo>) {
    for answer in answers.read() {
        match &answer.result {
            Ok(room) => {
                demo.room = Some(room.id);
                demo.log(format!("<- joined world (room {})", room.id));
            }
            Err(error) => demo.log(format!("xx join: {error}")),
        }
    }
}

fn on_send(mut answers: MessageReader<WsResponse<SendAck>>, mut demo: ResMut<Demo>) {
    for answer in answers.read() {
        match &answer.result {
            Ok(ack) => {
                demo.my_last = Some(ack.message_id);
                demo.log(format!("<- sent, message {}", ack.message_id));
            }
            Err(error) => demo.log(format!("xx send: {error}")),
        }
    }
}

fn on_history(mut answers: MessageReader<WsResponse<Page<ChatMessage>>>, mut demo: ResMut<Demo>) {
    for answer in answers.read() {
        match &answer.result {
            Ok(page) => {
                if let Some(newest) = page.items.first() {
                    demo.last_seen = demo.last_seen.max(Some(newest.id));
                }
                let mut line = format!("<- {} messages (newest first)", page.items.len());
                for message in page.items.iter().take(6) {
                    let edited = if message.edited_at.is_some() {
                        " (edited)"
                    } else {
                        ""
                    };
                    line.push_str(&format!(
                        "\n     {}: {}{edited}",
                        sender(message),
                        message.text
                    ));
                }
                demo.log(line);
            }
            Err(error) => demo.log(format!("xx history: {error}")),
        }
    }
}

fn on_members(mut answers: MessageReader<WsResponse<RoomMembers>>, mut demo: ResMut<Demo>) {
    for answer in answers.read() {
        match &answer.result {
            Ok(members) => {
                let names: Vec<String> = members
                    .members
                    .iter()
                    .map(|m| who(m.user, &m.name))
                    .collect();
                demo.log(format!("<- {} online: {}", members.count, names.join(", ")));
            }
            Err(error) => demo.log(format!("xx presence: {error}")),
        }
    }
}

/// The answer to `chat.edit`: the message as it is now.
fn on_edit(mut answers: MessageReader<WsResponse<ChatMessage>>, mut demo: ResMut<Demo>) {
    for answer in answers.read() {
        match &answer.result {
            Ok(message) => demo.log(format!("<- edited: {}", message.text)),
            Err(error) => demo.log(format!("xx edit: {error}")),
        }
    }
}

fn on_ack(mut answers: MessageReader<WsResponse<Ack>>, mut demo: ResMut<Demo>) {
    for answer in answers.read() {
        let what = demo.acks.remove(&answer.id).unwrap_or("request");
        match &answer.result {
            Ok(_) => demo.log(format!("<- {what}: done")),
            Err(error) => demo.log(format!("xx {what}: {error}")),
        }
    }
}

fn on_chat_pushes(
    mut messages: MessageReader<WsPush<ChatMessage>>,
    mut presence: MessageReader<WsPush<Presence>>,
    mut edited: MessageReader<WsPush<MessageEdited>>,
    mut typing: MessageReader<WsPush<TypingUpdate>>,
    mut read: MessageReader<WsPush<ReadReceipt>>,
    mut demo: ResMut<Demo>,
) {
    for push in messages.read() {
        let message = &push.data;
        if demo.room == Some(message.room) {
            demo.last_seen = demo.last_seen.max(Some(message.id));
        }
        demo.log(format!(
            "<< chat.message {}: {}",
            sender(message),
            message.text
        ));
    }
    for push in presence.read() {
        let p = &push.data;
        demo.log(format!(
            "<< chat.presence {}: {:?}",
            who(p.user, &p.name),
            p.event
        ));
    }
    for push in edited.read() {
        let e = &push.data;
        demo.log(format!("<< chat.edited message {}: {}", e.id, e.text));
    }
    for push in typing.read() {
        let t = &push.data;
        let what = if t.typing { "types" } else { "stopped typing" };
        demo.log(format!("<< chat.typing player {} {what}", t.user));
    }
    for push in read.read() {
        let r = &push.data;
        demo.log(format!(
            "<< chat.read player {} read up to {}",
            r.user, r.message
        ));
    }
}

fn on_pushes(
    mut notifications: MessageReader<WsPush<Notification>>,
    mut friends: MessageReader<WsPush<FriendPresence>>,
    mut members: MessageReader<WsPush<LobbyMemberUpdate>>,
    mut lobbies: MessageReader<WsPush<LobbyUpdate>>,
    mut matches: MessageReader<WsPush<MatchFound>>,
    mut expired: MessageReader<WsPush<TicketExpired>>,
    mut demo: ResMut<Demo>,
) {
    for push in notifications.read() {
        let n = &push.data;
        let text = n.text.clone().unwrap_or_default();
        demo.log(format!("<< notify.new {} ({}): {text}", n.id, n.kind));
    }
    for push in friends.read() {
        let f = &push.data;
        let state = if f.online { "online" } else { "offline" };
        demo.log(format!("<< friends.presence player {} is {state}", f.user));
    }
    for push in members.read() {
        let u = &push.data;
        demo.log(format!(
            "<< lobby.member lobby {}: {} {:?} (ready {})",
            u.lobby,
            who(u.member.user, &u.member.name),
            u.change,
            u.member.ready
        ));
    }
    for push in lobbies.read() {
        let u = &push.data;
        demo.log(format!(
            "<< lobby.changed lobby {}: {:?}, state {:?}",
            u.lobby.id, u.changes, u.lobby.state
        ));
    }
    for push in matches.read() {
        let m = &push.data;
        demo.log(format!(
            "<< match.found in `{}`: players {:?}",
            m.queue, m.players
        ));
    }
    for push in expired.read() {
        demo.log(format!(
            "<< match.expired: ticket in `{}` ran out",
            push.data.queue
        ));
    }
}

fn file_line(file: &FileInfo) -> String {
    format!(
        "file {} `{}` ({} bytes, {:?})",
        file.id, file.name, file.size, file.visibility
    )
}

fn who(user: UserId, name: &Option<String>) -> String {
    name.clone().unwrap_or_else(|| format!("player {user}"))
}

fn sender(message: &ChatMessage) -> String {
    who(message.sender, &message.sender_name)
}

/// Whether the URL's host is this machine (127.0.0.1, ::1, localhost): the development password
/// is used there only.
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
