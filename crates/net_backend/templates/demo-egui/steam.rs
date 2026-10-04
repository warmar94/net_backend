//! Feature `steam` (off by default): Steam through `steamworks` directly. The demo owns the one
//! Steam callback pump (once per frame) and adds a Steam panel: Steam login / link, the Steam
//! friends who play here (with Add), the findable setting, "Join Game" for the current lobby and
//! game invites, and joins that arrive from Steam (an accepted invite, "Join Game", or the command
//! line `+nb_lobby <number>` of a start; a demo in another lobby leaves it first).
//!
//! The app id is `STEAM_APP_ID` (default 480, Valve's test app "Spacewar"); the identity of the
//! Steam login ticket is `NET_BACKEND_STEAM_IDENTITY` (default the project's name; the server's
//! `modules.auth.steam_identity`). Steam must be running and logged in; otherwise the panel says so
//! and everything else works as without the feature.
//!
//! When Steam quits while the demo runs, the demo says so and makes no Steam call from then on.
//! `steamworks` 0.12.2 panics on Steam's own shutdown callback (`SteamServersDisconnected` with
//! the result OK) and on a join whose connect string is not UTF-8: three callbacks registered at
//! start fix those callbacks' data in place before `steamworks` reads it (the way
//! [bevy_steam_kit](https://docs.rs/bevy_steam_kit) does it).

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;
use net_backend_client::protocol::auth::SteamLoginRequest;
use net_backend_client::protocol::friends::{
    AddFriend, SteamMatch, SteamPlayer, UpdateFriendSettings,
};
use net_backend_client::protocol::lobbies::{JoinLobbyByCode, LeaveLobby, LobbyCode};
use steamworks::{AuthTicket, CallbackResult, Client, FriendFlags, SteamId, sys};

use crate::invite::{code_in, connect_string};
use crate::{Demo, NAME, Tone, lobby_joined, logged_in, ok, report, update};

/// Raised (never cleared) when Steam is gone: it said it is shutting down (the guards below), its
/// client no longer runs, or `steamworks` panicked in a callback. From then on no Steam call, not
/// even the ones a program makes when it ends.
static STEAM_GONE: AtomicBool = AtomicBool::new(false);

/// `SteamServersDisconnected` / `SteamServerConnectFailure` with the result OK (Steam quitting):
/// `steamworks` 0.12.2 panics converting OK into an error, so the result becomes "no connection".
///
/// # Safety
/// `field` points to a readable and writable 4-byte `EResult`.
unsafe fn steam_quits(field: *mut i32) {
    // SAFETY: the caller's contract; read and written as a plain `i32` (any alignment).
    unsafe {
        if field.read_unaligned() == sys::EResult::k_EResultOK as i32 {
            field.write_unaligned(sys::EResult::k_EResultNoConnection as i32);
            STEAM_GONE.store(true, Ordering::SeqCst);
        }
    }
}

struct DisconnectGuard;

// SAFETY: `ID` is the id of `SteamServersDisconnected_t`: steamworks calls `from_raw` with Steam's
// callback memory of that struct, valid and writable during the call.
unsafe impl steamworks::Callback for DisconnectGuard {
    const ID: i32 = sys::SteamServersDisconnected_t_k_iCallback as i32;

    unsafe fn from_raw(raw: *mut c_void) -> Self {
        // SAFETY: see the impl.
        unsafe {
            steam_quits(
                std::ptr::addr_of_mut!((*raw.cast::<sys::SteamServersDisconnected_t>()).m_eResult)
                    .cast::<i32>(),
            )
        };
        Self
    }
}

struct ConnectFailureGuard;

// SAFETY: as above, with `SteamServerConnectFailure_t`.
unsafe impl steamworks::Callback for ConnectFailureGuard {
    const ID: i32 = sys::SteamServerConnectFailure_t_k_iCallback as i32;

    unsafe fn from_raw(raw: *mut c_void) -> Self {
        // SAFETY: see the impl.
        unsafe {
            steam_quits(
                std::ptr::addr_of_mut!((*raw.cast::<sys::SteamServerConnectFailure_t>()).m_eResult)
                    .cast::<i32>(),
            )
        };
        Self
    }
}

/// A connect string made safe for `steamworks`' conversion, in place: a NUL in it (the last byte
/// when there is none) and `?` for every byte before it that is not valid UTF-8.
fn sanitize_connect(buf: &mut [u8]) {
    let Some(last) = buf.len().checked_sub(1) else {
        return;
    };
    let end = buf.iter().position(|&b| b == 0).unwrap_or_else(|| {
        buf[last] = 0;
        last
    });
    let mut pos = 0;
    while pos < end {
        match std::str::from_utf8(&buf[pos..end]) {
            Ok(_) => break,
            Err(e) => {
                let bad = pos + e.valid_up_to();
                let len = e.error_len().unwrap_or(end - bad);
                buf[bad..bad + len].fill(b'?');
                pos = bad + len;
            }
        }
    }
}

struct JoinGuard;

// SAFETY: `ID` is the id of `GameRichPresenceJoinRequested_t`: steamworks calls `from_raw` with
// Steam's callback memory of that struct, valid and writable during the call.
unsafe impl steamworks::Callback for JoinGuard {
    const ID: i32 = sys::GameRichPresenceJoinRequested_t_k_iCallback as i32;

    unsafe fn from_raw(raw: *mut c_void) -> Self {
        // SAFETY: see the impl; the field is a byte array (alignment 1).
        unsafe {
            let field = std::ptr::addr_of_mut!(
                (*raw.cast::<sys::GameRichPresenceJoinRequested_t>()).m_rgchConnect
            );
            let len = std::mem::size_of_val(&*field);
            sanitize_connect(std::slice::from_raw_parts_mut(field.cast::<u8>(), len));
        }
        Self
    }
}

/// Steam, while it runs.
pub struct Steam {
    /// Never dropped once Steam is gone: dropping it would call Steam (its shutdown).
    client: ManuallyDrop<Client>,
    /// The login ticket asked for (its answer comes through the pump).
    ticket: Option<AuthTicket>,
    /// The ticket the server checks; cancelled at Steam once the server answered.
    sent: Option<AuthTicket>,
    /// Steam's own "is the Steam client running" check: true at least once / off.
    seen_running: bool,
    check_off: bool,
}

/// What a pump brought.
enum SteamEvent {
    Join(LobbyCode),
    Ticket(Result<Vec<u8>, String>),
    Lost(&'static str),
}

impl Steam {
    fn init() -> Result<(Self, u32), String> {
        let app_id = std::env::var("STEAM_APP_ID")
            .ok()
            .and_then(|id| id.trim().parse().ok())
            .unwrap_or(480);
        let client = Client::init_app(app_id).map_err(|e| e.to_string())?;
        // For the whole run: dropping a handle would remove its guard.
        std::mem::forget(client.register_callback(|_: DisconnectGuard| {}));
        std::mem::forget(client.register_callback(|_: ConnectFailureGuard| {}));
        std::mem::forget(client.register_callback(|_: JoinGuard| {}));
        let steam = Self {
            client: ManuallyDrop::new(client),
            ticket: None,
            sent: None,
            seen_running: false,
            check_off: false,
        };
        Ok((steam, app_id))
    }

    /// A start by Steam (or `cargo run -p demo --features steam -- +nb_lobby <number>`).
    fn launch_code(&self) -> Option<LobbyCode> {
        let args: Vec<String> = std::env::args_os()
            .skip(1)
            .filter_map(|arg| arg.into_string().ok())
            .collect();
        let mut text = args.join(" ");
        text.push(' ');
        text.push_str(&self.client.apps().launch_command_line());
        code_in(&text)
    }

    /// Why Steam is gone, before a pump: it said it is quitting, or its client no longer runs
    /// (Steam's own process check; off when it never saw the client running).
    fn gone(&mut self) -> Option<&'static str> {
        if STEAM_GONE.load(Ordering::SeqCst) {
            return Some("Steam quit");
        }
        if self.check_off {
            return None;
        }
        // SAFETY: a plain query without arguments (a process check, no call through Steam's pipe).
        if unsafe { sys::SteamAPI_IsSteamRunning() } {
            self.seen_running = true;
            None
        } else if self.seen_running {
            STEAM_GONE.store(true, Ordering::SeqCst);
            Some("the Steam client no longer runs")
        } else {
            self.check_off = true;
            None
        }
    }

    /// THE pump: once per frame.
    fn pump(&mut self) -> Vec<SteamEvent> {
        if let Some(why) = self.gone() {
            return vec![SteamEvent::Lost(why)];
        }
        let ticket = self.ticket;
        let mut events = Vec::new();
        let pumped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.client.process_callbacks(|callback| match callback {
                CallbackResult::GameRichPresenceJoinRequested(join) => {
                    if let Some(code) = code_in(&join.connect) {
                        events.push(SteamEvent::Join(code));
                    }
                }
                CallbackResult::TicketForWebApiResponse(answer)
                    if Some(answer.ticket_handle) == ticket =>
                {
                    // Only the first `ticket_len` bytes of the buffer are the ticket.
                    let len = usize::try_from(answer.ticket_len).unwrap_or(0);
                    let bytes = answer
                        .result
                        .map(|()| answer.ticket[..len.min(answer.ticket.len())].to_vec());
                    events.push(SteamEvent::Ticket(bytes.map_err(|e| e.to_string())));
                }
                _ => {}
            })
        }));
        if pumped.is_err() {
            STEAM_GONE.store(true, Ordering::SeqCst);
            return vec![SteamEvent::Lost("steamworks panicked in a callback")];
        }
        if STEAM_GONE.load(Ordering::SeqCst) {
            // Steam said in this pump that it is quitting: what else came is dropped.
            return vec![SteamEvent::Lost("Steam quit")];
        }
        events
    }

    fn friends(&self) -> Vec<(u64, String)> {
        let friends = self.client.friends().get_friends(FriendFlags::IMMEDIATE);
        friends.iter().map(|f| (f.id().raw(), f.name())).collect()
    }
}

impl Drop for Steam {
    fn drop(&mut self) {
        if STEAM_GONE.load(Ordering::SeqCst) {
            // No Steam call: the client is left as it is.
            return;
        }
        // The demo ends with Steam running: friends no longer see "Join Game".
        self.client.friends().clear_rich_presence();
        // SAFETY: dropped once, here, and never used again.
        unsafe { ManuallyDrop::drop(&mut self.client) };
    }
}

/// The Steam panel's state.
#[derive(Default)]
pub struct SteamState {
    steam: Option<Steam>,
    /// Why there is no Steam (not running at start, or gone since).
    problem: Option<String>,
    friends: Vec<(u64, String)>,
    found: Vec<SteamPlayer>,
    findable: Option<bool>,
    /// A join from Steam that waits for a login.
    pending: Option<LobbyCode>,
    /// The code in rich presence (`connect`).
    shown: Option<LobbyCode>,
}

impl Demo {
    /// Starts Steam (once, at start-up).
    pub fn steam_start(&mut self) {
        match Steam::init() {
            Ok((steam, app_id)) => {
                self.push(Tone::Ok, format!("Steam started (app {app_id})"));
                self.steam.pending = steam.launch_code();
                if let Some(code) = &self.steam.pending {
                    self.push(
                        Tone::Ok,
                        format!("started to join lobby {}", code.grouped()),
                    );
                }
                self.steam.steam = Some(steam);
            }
            Err(error) => {
                self.push(
                    Tone::Error,
                    format!("Steam is not available ({error}): the demo works without it"),
                );
                self.steam.problem = Some(format!("not running: {error}"));
            }
        }
    }

    /// Every frame: the pump, "Join Game" for the current lobby, a waiting join.
    pub fn steam_frame(&mut self, ctx: &egui::Context) {
        let Some(steam) = self.steam.steam.as_mut() else {
            return;
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        for event in steam.pump() {
            match event {
                SteamEvent::Join(code) => {
                    self.push(Tone::Push, format!("Steam: join lobby {}", code.grouped()));
                    self.steam.pending = Some(code);
                }
                SteamEvent::Ticket(Ok(ticket)) => self.steam_login(&ticket),
                SteamEvent::Ticket(Err(error)) => {
                    if let Some(steam) = self.steam.steam.as_mut() {
                        steam.ticket = None;
                    }
                    self.push(Tone::Error, format!("Steam ticket: {error}"))
                }
                SteamEvent::Lost(why) => {
                    self.push(
                        Tone::Error,
                        format!("Steam is gone ({why}): the demo goes on without Steam"),
                    );
                    self.steam.problem = Some(format!("gone: {why}"));
                    // Dropped without a Steam call (`STEAM_GONE`).
                    self.steam.steam = None;
                    return;
                }
            }
        }
        if self.joined_code != self.steam.shown
            && let Some(steam) = &self.steam.steam
        {
            let connect = self.joined_code.as_ref().map(connect_string);
            steam
                .client
                .friends()
                .set_rich_presence("connect", connect.as_deref());
            self.steam.shown = self.joined_code.clone();
        }
        if self.account.is_some()
            && let Some(code) = self.steam.pending.take()
        {
            if self.joined_code.as_ref() == Some(&code) {
                let text = format!("Steam: already in the lobby {}", code.grouped());
                self.push(Tone::Ok, text);
                return;
            }
            // The server allows one lobby at a time: leave the current one first.
            let current = self.lobby;
            self.run(
                format!("join the lobby {}", code.grouped()),
                move |client, out| {
                    if let Some(lobby) = current {
                        match client.call(&LeaveLobby::new(lobby)) {
                            Ok(_) => {
                                ok(out, format!("left the lobby {lobby} for the Steam join"));
                                update(out, |d| {
                                    d.lobby = None;
                                    d.joined_code = None;
                                    d.ready = false;
                                });
                            }
                            Err(error) => return report(out, "leave", error),
                        }
                    }
                    match client.call(&JoinLobbyByCode::new(code.as_str())) {
                        Ok(lobby) => lobby_joined(out, "joined (from Steam)", lobby),
                        Err(error) => report(out, "join lobby", error),
                    }
                },
            );
        }
    }

    /// The ticket arrived: log in with Steam, or link Steam to the account logged in. Steam keeps
    /// the ticket valid until the server answered.
    fn steam_login(&mut self, ticket: &[u8]) {
        if let Some(steam) = self.steam.steam.as_mut() {
            steam.sent = steam.ticket.take();
        }
        let hex: String = ticket.iter().map(|b| format!("{b:02x}")).collect();
        let request = SteamLoginRequest::new(hex, identity());
        let link = self.account.is_some();
        let label = if link {
            "link Steam to this account"
        } else {
            "Steam login"
        };
        self.run(label.into(), move |client, out| {
            let answer = if link {
                client.link_steam(request)
            } else {
                client.login_steam(request)
            };
            update(out, |d| d.steam_ticket_done());
            match answer {
                Ok(session) => logged_in(client, out, "Steam", session.account.id),
                Err(error) => report(out, "Steam login", error),
            }
        });
    }

    /// The server answered the Steam login: the ticket is no longer needed.
    fn steam_ticket_done(&mut self) {
        if let Some(steam) = self.steam.steam.as_mut()
            && let Some(ticket) = steam.sent.take()
        {
            steam.client.user().cancel_authentication_ticket(ticket);
        }
    }

    pub fn steam_panel(&mut self, ui: &mut egui::Ui) {
        if let Some(problem) = &self.steam.problem {
            ui.label(format!("Steam is {problem}."));
            return;
        }
        if self.steam.steam.is_none() {
            return;
        }
        let findable = self.steam.findable.unwrap_or(true);
        let (mut login, mut lookup, mut toggle) = (false, false, false);
        ui.horizontal_wrapped(|ui| {
            login = ui.button("Steam login / link").clicked();
            lookup = ui.button("Steam friends who play here").clicked();
            toggle = ui
                .button(format!("Findable: {}", if findable { "yes" } else { "no" }))
                .clicked();
        });
        if login && let Some(steam) = self.steam.steam.as_mut() {
            if steam.ticket.is_some() || steam.sent.is_some() {
                self.push(Tone::Error, "Steam login: one is on its way".into());
            } else {
                let handle = steam
                    .client
                    .user()
                    .authentication_session_ticket_for_webapi(&identity());
                // Handle 0: Steam refused at once (not logged on); no answer follows.
                if format!("{handle:?}") == "AuthTicket(0)" {
                    self.push(Tone::Error, "Steam refused a login ticket".into());
                } else {
                    steam.ticket = Some(handle);
                    self.push(Tone::Sent, "Steam: a login ticket for the server".into());
                }
            }
        }
        if lookup && let Some(steam) = &self.steam.steam {
            let friends = steam.friends();
            let ids: Vec<u64> = friends.iter().map(|(id, _)| *id).collect();
            self.steam.friends = friends;
            self.run(
                format!("which of my {} Steam friends play here", ids.len()),
                move |client, out| match client.call(&SteamMatch::new(ids)) {
                    Ok(result) => {
                        ok(out, format!("{} of them play here", result.players.len()));
                        update(out, move |d| d.steam.found = result.players);
                    }
                    Err(error) => report(out, "Steam friends", error),
                },
            );
        }
        if toggle {
            let wanted = !findable;
            self.run(
                format!("findable through Steam = {wanted}"),
                move |client, out| match client
                    .call(&UpdateFriendSettings::new().steam_findable(wanted))
                {
                    Ok(settings) => {
                        ok(
                            out,
                            format!("findable through Steam: {}", settings.steam_findable),
                        );
                        update(out, move |d| {
                            d.steam.findable = Some(settings.steam_findable)
                        });
                    }
                    Err(error) => report(out, "settings", error),
                },
            );
        }
        let code = self.joined_code.clone();
        ui.horizontal_wrapped(|ui| match &code {
            Some(code) => {
                ui.label(format!("Join Game shows lobby {}", code.grouped()));
                if ui.button("Invite (Steam overlay)").clicked()
                    && let Some(steam) = &self.steam.steam
                {
                    let connect = connect_string(code);
                    steam
                        .client
                        .friends()
                        .activate_invite_dialog_connect_string(&connect);
                    self.push(Tone::Sent, format!("Steam invite dialog: {connect}"));
                }
            }
            None => {
                ui.label("Create or join a lobby to invite Steam friends.");
            }
        });
        for player in self.steam.found.clone() {
            let steam_name = self
                .steam
                .friends
                .iter()
                .find(|(id, _)| id.to_string() == player.steam_id);
            let name = steam_name
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| player.steam_id.clone());
            ui.horizontal_wrapped(|ui| {
                let here = player
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("player {}", player.user));
                let relation = player
                    .state
                    .as_ref()
                    .map(|s| format!("{s:?}"))
                    .unwrap_or_else(|| "no relation".into());
                ui.label(format!("{name} = {here} ({relation})"));
                if player.state.is_none() && ui.button("Add").clicked() {
                    let user = player.user;
                    self.run(
                        format!("friend request to player {user}"),
                        move |client, out| match client.call(&AddFriend::by_id(user)) {
                            Ok(entry) => {
                                ok(out, format!("player {}: {:?}", entry.user, entry.state))
                            }
                            Err(error) => report(out, "add friend", error),
                        },
                    );
                }
            });
        }
        if let Some(code) = &code
            && !self.steam.friends.is_empty()
        {
            let mut invited = None;
            ui.collapsing(
                format!("Invite a Steam friend ({})", self.steam.friends.len()),
                |ui| {
                    for (id, name) in &self.steam.friends {
                        if ui.button(format!("Invite {name}")).clicked() {
                            invited = Some((*id, name.clone()));
                        }
                    }
                },
            );
            if let Some((id, name)) = invited
                && let Some(steam) = &self.steam.steam
            {
                let connect = connect_string(code);
                let friend = steam.client.friends().get_friend(SteamId::from_raw(id));
                friend.invite_user_to_game(&connect);
                self.push(Tone::Sent, format!("Steam invite to {name}: {connect}"));
            }
        }
    }
}

/// The identity of the Steam login ticket (the server's `modules.auth.steam_identity`).
fn identity() -> String {
    std::env::var("NET_BACKEND_STEAM_IDENTITY").unwrap_or_else(|_| NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::sanitize_connect;

    #[test]
    fn connect_strings_become_safe() {
        let mut ok = *b"+nb_lobby 1\0junk";
        sanitize_connect(&mut ok);
        assert_eq!(&ok, b"+nb_lobby 1\0junk");
        let mut bad = *b"+nb\xff 1\0";
        sanitize_connect(&mut bad);
        assert_eq!(&bad, b"+nb? 1\0");
        let mut no_nul = *b"abc";
        sanitize_connect(&mut no_nul);
        assert_eq!(&no_nul, b"ab\0");
    }
}
