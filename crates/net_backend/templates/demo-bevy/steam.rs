//! Feature `steam` (off by default): a Steam row through
//! [bevy_steam_kit](https://docs.rs/bevy_steam_kit). The kit owns Steam: its one callback pump,
//! the friends list (`SteamFriends`), the login ticket (`AuthRequest`), game invites
//! (`InviteToGame`), the overlay (`OpenOverlay`), rich presence (`SetRichPresence`) and the joins
//! that arrive from Steam (`ConnectRequested`, `JoinRequested`). The server calls go through
//! bevy_net_backend like every other button.
//!
//! - Steam login / link: a Web API ticket for the server (`POST /v1/auth/steam`; logged in, it
//!   links Steam to the account).
//! - Steam friends here: which Steam friends have an account on the server
//!   (`POST /v1/friends/steam`); "Add found" sends them friend requests.
//! - "Join Game" for the current lobby (rich presence `connect = +nb_lobby <code number>`),
//!   "Invite (overlay)" (Steam's invite dialog) and "Invite friends" (a Steam game invite to each
//!   Steam friend in this game now and each one found here).
//! - Joins from Steam (an accepted invite, "Join Game", a start with `+nb_lobby <number>`) join the
//!   lobby by its code (after the login when needed; a demo in another lobby leaves it first).
//! - Findable: whether others find this account by its Steam account (`PUT /v1/friends/settings`).
//!
//! ```text
//! {{demo_run}} --features steam
//! ```
//!
//! The app id is `STEAM_APP_ID` (default 480, Valve's test app "Spacewar"); the identity of the
//! login ticket is `NET_BACKEND_STEAM_IDENTITY` (default the project's name; the server's
//! `modules.auth.steam_identity`). Without a running Steam the row says so and the demo works as
//! without the feature; when Steam quits while the demo runs, the log says so and the demo goes on
//! without Steam.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use bevy_steam_kit::{
    AuthError, AuthRequest, AuthRequestId, ConnectRequested, FriendsChanged, FriendsError,
    FriendsSettings, GameInviteSent, InviteToGame, JoinRequested, LobbyError, LobbySettings,
    OpenOverlay, OverlayError, OverlayToggled, RealSteamBackend, SetRichPresence, SteamAuth,
    SteamBackendRes, SteamFriends, SteamKitPlugin, SteamKitSystems, SteamLost, SteamOverlay,
    WebApiTicketReady,
};
use net_backend_protocol::auth::SteamLoginRequest;
use net_backend_protocol::friends::{
    AddFriend, FriendSettings, SteamMatch, SteamMatchResult, SteamPlayer, UpdateFriendSettings,
};
use net_backend_protocol::lobbies::{JoinLobbyByCode, LeaveLobby, LobbyCode};

use crate::invite::{CONNECT_PREFIX, code_in, code_of, connect_string};
use crate::{Demo, Http, NAME, SteamSlot, button, failure, request, send, send_raw, text};

/// The Steam row's buttons.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum SteamAction {
    Login,
    Friends,
    AddFound,
    Invite,
    InviteFound,
    Findable,
}

/// The Steam row's status text.
#[derive(Component)]
struct SteamStatus;

/// Where Steam is.
#[derive(Clone, PartialEq, Eq)]
enum SteamState {
    /// Started with this app id.
    Running(u32),
    /// Not started (Steam not running, not logged in, ...).
    NotRunning(String),
    /// It quit while the demo ran.
    Lost(String),
}

/// What the Steam row knows.
#[derive(Resource)]
struct SteamRow {
    state: SteamState,
    /// The friends found on the server.
    found: Vec<SteamPlayer>,
    findable: Option<bool>,
    /// A join from Steam that waits for a login.
    pending: Option<LobbyCode>,
    /// The leave request sent before that join (the join waits for its answer).
    leaving: Option<RequestId>,
    /// The code in rich presence.
    shown: Option<LobbyCode>,
    /// The ticket asked for, then the login request that carries it (cancelled once answered).
    ticket: Option<AuthRequestId>,
    login: Option<(RequestId, AuthRequestId)>,
    /// What the friends line said last: (friends, in this game).
    counted: Option<(usize, usize)>,
    requests: HashMap<RequestId, SteamHttp>,
}

impl SteamRow {
    fn running(&self) -> bool {
        matches!(self.state, SteamState::Running(_))
    }
}

enum SteamHttp {
    Friends,
    Settings,
}

/// Adds the kit and starts Steam (before the app runs).
pub fn add(app: &mut App) {
    // Our own prefix and a join code number; no Steam lobbies are created, so the kit sets no
    // presence of its own. Starts with `+nb_lobby <number>` arrive as `ConnectRequested`.
    let lobby = LobbySettings {
        connect_prefix: CONNECT_PREFIX.into(),
        set_connect_presence: false,
        check_launch_args: false,
    };
    let friends = FriendsSettings {
        launch_connect_prefix: Some(CONNECT_PREFIX.into()),
        ..Default::default()
    };
    app.add_plugins(
        SteamKitPlugin::default()
            .with_lobby(lobby)
            .with_friends(friends),
    );
    let app_id = std::env::var("STEAM_APP_ID")
        .ok()
        .and_then(|id| id.trim().parse().ok())
        .unwrap_or(480);
    // The kit never starts Steam: the game does, and hands the client to the kit (which keeps it
    // alive and pumps it). The demo keeps no client of its own.
    let state = match steamworks::Client::init_app(app_id) {
        Ok(client) => {
            app.insert_resource(SteamBackendRes(Box::new(RealSteamBackend::new(client))));
            SteamState::Running(app_id)
        }
        Err(error) => SteamState::NotRunning(error.to_string()),
    };
    app.insert_resource(SteamRow {
        state,
        found: Vec::new(),
        findable: None,
        pending: None,
        leaving: None,
        shown: None,
        ticket: None,
        login: None,
        counted: None,
        requests: HashMap::new(),
    })
    .add_systems(Startup, spawn_row.after(crate::spawn_ui))
    .add_systems(
        Update,
        (
            steam_lost,
            click,
            tickets,
            joins,
            presence,
            pending_join,
            on_http,
            answers,
            status,
        )
            .chain()
            .before(SteamKitSystems::Requests),
    );
}

fn spawn_row(
    mut commands: Commands,
    mut slot: Query<(Entity, &mut Node), With<SteamSlot>>,
    row: Res<SteamRow>,
    mut demo: ResMut<Demo>,
) {
    match &row.state {
        SteamState::Running(app_id) => demo.log(format!("<- Steam started (app {app_id})")),
        SteamState::NotRunning(error) | SteamState::Lost(error) => demo.log(format!(
            "xx Steam is not available ({error}): the demo works without it"
        )),
    }
    let Ok((slot, mut node)) = slot.single_mut() else {
        return;
    };
    node.display = Display::Flex;
    let buttons = [
        (SteamAction::Login, "Steam login / link"),
        (SteamAction::Friends, "Steam friends here"),
        (SteamAction::AddFound, "Add found"),
        (SteamAction::Invite, "Invite (overlay)"),
        (SteamAction::InviteFound, "Invite friends"),
        (SteamAction::Findable, "Findable on / off"),
    ];
    commands.entity(slot).with_children(|parent| {
        parent.spawn((
            text("Steam", 14.0),
            Node {
                width: Val::Px(76.0),
                ..default()
            },
        ));
        for (action, label) in buttons {
            parent.spawn(button(action, label));
        }
        parent.spawn((text("", 13.0), SteamStatus));
    });
}

/// Steam quit (or its process ended) while the demo runs: the kit makes no Steam call any more,
/// and neither does the demo.
fn steam_lost(
    mut lost: MessageReader<SteamLost>,
    mut row: ResMut<SteamRow>,
    mut demo: ResMut<Demo>,
) {
    for event in lost.read() {
        let reason = format!("{:?}", event.reason);
        demo.log(format!(
            "xx Steam is gone ({reason}): the demo goes on without Steam"
        ));
        row.state = SteamState::Lost(reason);
        row.ticket = None;
        row.shown = None;
    }
}

#[allow(clippy::too_many_arguments)]
fn click(
    buttons: Query<(&Interaction, &SteamAction), Changed<Interaction>>,
    friends: Res<SteamFriends>,
    overlay: Res<SteamOverlay>,
    mut auth: ResMut<SteamAuth>,
    mut tickets: MessageWriter<AuthRequest>,
    mut invites: MessageWriter<InviteToGame>,
    mut open: MessageWriter<OpenOverlay>,
    http: Res<HttpClient>,
    mut row: ResMut<SteamRow>,
    mut demo: ResMut<Demo>,
) {
    for (interaction, action) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if !row.running() {
            demo.log("xx Steam is not running (start Steam, then the demo)");
            continue;
        }
        match action {
            SteamAction::Login => {
                if row.ticket.is_some() || row.login.is_some() {
                    demo.log("xx Steam login: one is on its way");
                    continue;
                }
                let id = auth.next_id();
                tickets.write(AuthRequest::web_api_ticket(id, identity()));
                row.ticket = Some(id);
                demo.log("-> Steam: a login ticket for the server");
            }
            SteamAction::Friends => {
                if !friends.is_loaded() {
                    demo.log("xx Steam friends: the list is not read yet");
                    continue;
                }
                let ids: Vec<u64> = friends.list().iter().map(|f| f.steam_id).collect();
                let label = format!("which of my {} Steam friends play here", ids.len());
                let id = http.send(request(&SteamMatch::new(ids)));
                row.requests.insert(id, SteamHttp::Friends);
                demo.log(format!("-> {label}"));
            }
            SteamAction::AddFound => {
                let new: Vec<SteamPlayer> = row
                    .found
                    .iter()
                    .filter(|p| p.state.is_none())
                    .cloned()
                    .collect();
                if new.is_empty() {
                    demo.log("xx add: no Steam friend without a relation found (Steam friends here first)");
                }
                for player in new {
                    let call = AddFriend::by_id(player.user);
                    let label = format!("friend request to player {}", player.user);
                    send(&http, &mut demo, &call, Http::Friend, &label);
                }
            }
            SteamAction::Invite => match &demo.lobby_code {
                Some(code) => {
                    let connect = connect_string(code);
                    open.write(OpenOverlay::invite_dialog_connect(connect.clone()));
                    demo.log(format!("-> Steam invite dialog: {connect}"));
                    if !overlay.is_enabled() {
                        demo.log("xx the Steam overlay is not available to this window (Invite friends works without it)");
                    }
                }
                None => demo.log("xx invite: create or join a lobby first"),
            },
            SteamAction::InviteFound => match &demo.lobby_code {
                Some(code) => {
                    // The Steam friends in this game now, and the ones found on the server.
                    let mut targets: Vec<u64> =
                        friends.playing_this_game().map(|f| f.steam_id).collect();
                    for player in &row.found {
                        if let Ok(steam_id) = player.steam_id.parse::<u64>()
                            && !targets.contains(&steam_id)
                        {
                            targets.push(steam_id);
                        }
                    }
                    if targets.is_empty() {
                        demo.log("xx invite: no Steam friend in this game or found here (Steam friends here)");
                        continue;
                    }
                    let connect = connect_string(code);
                    demo.log(format!(
                        "-> Steam invites ({connect}) to {} friends",
                        targets.len()
                    ));
                    for steam_id in targets {
                        invites.write(InviteToGame {
                            steam_id,
                            connect: connect.clone(),
                        });
                    }
                }
                None => demo.log("xx invite: create or join a lobby first"),
            },
            SteamAction::Findable => {
                let wanted = !row.findable.unwrap_or(true);
                let call = UpdateFriendSettings::new().steam_findable(wanted);
                let id = http.send(request(&call));
                row.requests.insert(id, SteamHttp::Settings);
                demo.log(format!("-> findable through Steam = {wanted}"));
            }
        }
    }
}

/// The login ticket: log in with Steam, or (logged in) link Steam to the account. The ticket is
/// cancelled at Steam once the server answered (`on_http`).
fn tickets(
    mut ready: MessageReader<WebApiTicketReady>,
    mut errors: MessageReader<AuthError>,
    http: Res<HttpClient>,
    mut row: ResMut<SteamRow>,
    mut demo: ResMut<Demo>,
) {
    for answer in ready.read() {
        if row.ticket != Some(answer.id) {
            continue;
        }
        row.ticket = None;
        let label = if demo.tokens.is_some() {
            "link Steam to this account"
        } else {
            "Steam login"
        };
        let call = SteamLoginRequest::new(answer.ticket.to_hex(), identity());
        let id = send_raw(&http, &mut demo, request(&call), Http::Login, label);
        row.login = Some((id, answer.id));
    }
    for error in errors.read() {
        if row.ticket == Some(error.id) {
            row.ticket = None;
            demo.log(format!(
                "xx Steam ticket: {:?} ({})",
                error.kind, error.message
            ));
        }
    }
}

/// An accepted invite, "Join Game" or a start with `+nb_lobby <number>`. The kit's friends part
/// reports the raw connect string, its lobby part the number (both for one join): one join each.
fn joins(
    mut connects: MessageReader<ConnectRequested>,
    mut lobby_joins: MessageReader<JoinRequested>,
    mut row: ResMut<SteamRow>,
    mut demo: ResMut<Demo>,
) {
    let mut codes: Vec<(LobbyCode, String)> = Vec::new();
    for join in connects.read() {
        match code_in(&join.connect) {
            Some(code) => codes.push((code, format!("{:?}", join.source))),
            None => demo.log(format!("xx Steam: {:?} has no join code", join.connect)),
        }
    }
    for join in lobby_joins.read() {
        match code_of(join.lobby) {
            Some(code) => codes.push((code, format!("{:?}", join.source))),
            None => demo.log(format!("xx Steam: {} is not a join code", join.lobby)),
        }
    }
    let mut seen: Vec<LobbyCode> = Vec::new();
    codes.retain(|(code, _)| {
        let new = !seen.contains(code);
        seen.push(code.clone());
        new
    });
    for (code, source) in codes {
        demo.log(format!(
            "<< Steam: join lobby {} ({source})",
            code.grouped()
        ));
        row.pending = Some(code);
    }
}

/// "Join Game" shows the current lobby's code (and goes away without a lobby).
fn presence(
    demo: Res<Demo>,
    mut row: ResMut<SteamRow>,
    mut writer: MessageWriter<SetRichPresence>,
) {
    if !row.running() || demo.lobby_code == row.shown {
        return;
    }
    writer.write(SetRichPresence {
        key: "connect".into(),
        value: demo.lobby_code.as_ref().map(connect_string),
    });
    row.shown = demo.lobby_code.clone();
}

fn pending_join(http: Res<HttpClient>, mut row: ResMut<SteamRow>, mut demo: ResMut<Demo>) {
    if demo.tokens.is_none() || row.leaving.is_some() {
        return;
    }
    let Some(code) = row.pending.take() else {
        return;
    };
    if demo.lobby_code.as_ref() == Some(&code) {
        demo.log(format!("<< Steam: already in the lobby {}", code.grouped()));
        return;
    }
    // The server allows one lobby at a time: leave the current one first, then join.
    if let Some(lobby) = demo.lobby.take() {
        demo.lobby_code = None;
        let label = format!("leave lobby {lobby} for the Steam join");
        let call = request(&LeaveLobby::new(lobby));
        row.leaving = Some(send_raw(&http, &mut demo, call, Http::Ack, &label));
        row.pending = Some(code);
        return;
    }
    let call = JoinLobbyByCode::new(code.as_str());
    let label = format!("join the lobby {}", code.grouped());
    send(&http, &mut demo, &call, Http::Lobby, &label);
}

fn on_http(
    mut answers: MessageReader<HttpResponse>,
    friends: Res<SteamFriends>,
    mut tickets: MessageWriter<AuthRequest>,
    mut row: ResMut<SteamRow>,
    mut demo: ResMut<Demo>,
) {
    for answer in answers.read() {
        // The leave before a Steam join was answered (the main log shows how): the join goes next.
        if row.leaving == Some(answer.id) {
            row.leaving = None;
            continue;
        }
        // The Steam login was answered (the main log shows how): the ticket is no longer needed.
        if let Some((request, ticket)) = row.login
            && request == answer.id
        {
            row.login = None;
            if row.running() {
                tickets.write(AuthRequest::cancel(ticket));
            }
            continue;
        }
        let Some(what) = row.requests.remove(&answer.id) else {
            continue;
        };
        let raw = match &answer.result {
            Ok(raw) => raw,
            Err(error) => {
                demo.log(format!("xx {}", failure(error)));
                continue;
            }
        };
        match what {
            SteamHttp::Friends => match raw.json::<SteamMatchResult>() {
                Ok(result) => {
                    let rows: Vec<String> = result
                        .players
                        .iter()
                        .map(|p| {
                            let steam = p
                                .steam_id
                                .parse::<u64>()
                                .ok()
                                .and_then(|id| friends.get(id))
                                .map(|f| f.display_name().to_string())
                                .unwrap_or_else(|| p.steam_id.clone());
                            let here = p
                                .name
                                .clone()
                                .unwrap_or_else(|| format!("player {}", p.user));
                            let relation = p
                                .state
                                .as_ref()
                                .map(|s| format!("{s:?}"))
                                .unwrap_or_else(|| "no relation".into());
                            format!("{steam} = {here} ({relation})")
                        })
                        .collect();
                    demo.log(format!(
                        "<- {} Steam friends play here: {}",
                        rows.len(),
                        rows.join(", ")
                    ));
                    row.found = result.players;
                }
                Err(error) => demo.log(format!("xx the answer did not decode: {error}")),
            },
            SteamHttp::Settings => match raw.json::<FriendSettings>() {
                Ok(settings) => {
                    demo.log(format!(
                        "<- findable through Steam: {}",
                        settings.steam_findable
                    ));
                    row.findable = Some(settings.steam_findable);
                }
                Err(error) => demo.log(format!("xx the answer did not decode: {error}")),
            },
        }
    }
}

/// What Steam answered: invites, the overlay, refused requests, the friends list.
#[allow(clippy::too_many_arguments)]
fn answers(
    mut sent: MessageReader<GameInviteSent>,
    mut friend_errors: MessageReader<FriendsError>,
    mut overlay_errors: MessageReader<OverlayError>,
    mut toggled: MessageReader<OverlayToggled>,
    mut lobby_errors: MessageReader<LobbyError>,
    mut changed: MessageReader<FriendsChanged>,
    friends: Res<SteamFriends>,
    mut row: ResMut<SteamRow>,
    mut demo: ResMut<Demo>,
) {
    for invite in sent.read() {
        let name = friends
            .get(invite.steam_id)
            .map(|f| f.display_name().to_string())
            .unwrap_or_else(|| invite.steam_id.to_string());
        let answer = if invite.ok { "sent" } else { "refused" };
        demo.log(format!("<- Steam invite to {name}: {answer}"));
    }
    for error in friend_errors.read() {
        demo.log(format!(
            "xx Steam {:?}: {:?} ({})",
            error.request, error.kind, error.message
        ));
    }
    for error in overlay_errors.read() {
        demo.log(format!(
            "xx Steam overlay: {:?} ({})",
            error.kind, error.message
        ));
    }
    for overlay in toggled.read() {
        let state = if overlay.active { "open" } else { "closed" };
        demo.log(format!("<- Steam overlay {state}"));
    }
    for error in lobby_errors.read() {
        demo.log(format!("xx Steam: {:?} ({})", error.kind, error.message));
    }
    if changed.read().count() > 0 && row.running() {
        let count = (friends.list().len(), friends.playing_this_game().count());
        if row.counted != Some(count) {
            row.counted = Some(count);
            demo.log(format!(
                "<- Steam friends: {}, {} online, {} in this game",
                count.0,
                friends.online().count(),
                count.1
            ));
        }
    }
}

fn status(
    row: Res<SteamRow>,
    friends: Res<SteamFriends>,
    mut texts: Query<&mut Text, With<SteamStatus>>,
) {
    if !row.is_changed() && !friends.is_changed() {
        return;
    }
    let line = match &row.state {
        SteamState::Running(app_id) if friends.is_loaded() => format!(
            "app {app_id}, {} friends, {} in this game",
            friends.list().len(),
            friends.playing_this_game().count()
        ),
        SteamState::Running(app_id) => format!("app {app_id}"),
        SteamState::NotRunning(_) => "not running".to_string(),
        SteamState::Lost(_) => "gone (it quit)".to_string(),
    };
    for mut text in &mut texts {
        if text.0 != line {
            text.0 = line.clone();
        }
    }
}

/// The identity of the Steam login ticket (the server's `modules.auth.steam_identity`).
fn identity() -> String {
    std::env::var("NET_BACKEND_STEAM_IDENTITY").unwrap_or_else(|_| NAME.to_string())
}
