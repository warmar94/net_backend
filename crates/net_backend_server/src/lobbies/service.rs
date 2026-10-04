//! [`LobbyService`]: lobbies, members, ready flags, metadata, join codes and the pushes.
//!
//! **Writes** are one transaction each (run again on a reported deadlock, [`Retry`]): the
//! player's account lock first when the player's lobby count matters (create, join), then the
//! lobby's row lock, then plain reads, the limits counted, the rows changed by primary key. Members
//! are counted under the lobby's lock, so a full lobby or a player in too many lobbies can never be
//! passed by racing joins.
//!
//! **After the commit:** the chat room (created with the lobby, a joining player added, a leaving
//! or kicked one removed, the room deleted when the lobby closes or is removed), the
//! `lobby.member` / `lobby.changed` pushes to the members, and the [`AfterLobbyChange`] hooks. A
//! failed chat change or push is logged; the lobby change stays. The purge deletes the chat rooms
//! no lobby names any more (a failed deletion, a crash between the commit and the deletion).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use net_backend_protocol::lobbies::{
    CreateLobby, LobbyChange, LobbyCode, LobbyInfo, LobbyList, LobbyMember, LobbyMemberUpdate, LobbySearch, LobbyState, LobbyUpdate, LobbyVisibility,
    MemberChange, UpdateLobby, LOBBY_CODE_LEN,
};
use net_backend_protocol::{codes, Cursor, LobbyId, Page, RoomId, UnixMillis, UserId, ValidationDetails};

use super::config::LobbiesConfig;
use super::events::{AfterLobbyChange, BeforeLobbyCreate, BeforeLobbyJoin, BeforeLobbyUpdate, LobbyEvent};
use super::store::{
    self, CountRow, IdRow, LobbyCountRow, LobbyRow, MemberRow, MetaIdRow, MetaRow, NameRow, RoomRefRow, FRIENDS, IN_GAME, OPEN, PRIVATE, PUBLIC,
};
use crate::db::{DbTx, Retry};
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::rate_limit::{KeyedBuckets, RateDecision};
use crate::state::AppState;

/// One write transaction, run again on a reported deadlock.
macro_rules! in_tx {
    ($state:expr, |$tx:ident| $body:expr) => {{
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        loop {
            let mut $tx = $state.db().begin_write().await?;
            let result: Result<_, AppError> = $body.await;
            match $tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => break other,
            }
        }
    }};
}

/// Attempts at a join code no other lobby has.
const CODE_ATTEMPTS: usize = 8;
/// The marker of a join code another lobby has (a new code is tried).
const CODE_TAKEN: &str = "the join code is taken";
/// Lobbies one purge pass looks at.
const PURGE_BATCH: u64 = 500;
/// The chat rooms of this module carry this origin.
#[cfg(feature = "chat")]
const ROOM_ORIGIN: &str = "lobbies";
/// A chat room of this module younger than this is never taken for an orphan (its lobby may still
/// be storing it), ms.
#[cfg(feature = "chat")]
const ORPHAN_AGE_MS: i64 = 3_600_000;

/// Who acts on a lobby. Host-only actions (change, kick, a new code, a new host, close) need the
/// lobby's host for a [`Player`](Self::Player); a [`Manager`](Self::Manager) (a player with the
/// `lobbies.manage` permission) and [`Server`](Self::Server) code act on any lobby.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LobbyActor {
    /// A player: host-only actions on the lobby it hosts.
    Player(UserId),
    /// A player allowed to manage every lobby (staff).
    Manager(UserId),
    /// Server code.
    Server,
}

impl LobbyActor {
    /// The acting player, if any.
    pub fn user(self) -> Option<UserId> {
        match self {
            Self::Player(user) | Self::Manager(user) => Some(user),
            Self::Server => None,
        }
    }

    fn may_manage(self, host: Option<i64>) -> bool {
        match self {
            Self::Player(user) => host == Some(user.get()),
            Self::Manager(_) | Self::Server => true,
        }
    }
}

fn visibility_name(visibility: LobbyVisibility) -> &'static str {
    match visibility {
        LobbyVisibility::Private => PRIVATE,
        LobbyVisibility::Friends => FRIENDS,
        _ => PUBLIC,
    }
}

fn visibility_of(name: &str) -> LobbyVisibility {
    match name {
        PUBLIC => LobbyVisibility::Public,
        PRIVATE => LobbyVisibility::Private,
        FRIENDS => LobbyVisibility::Friends,
        _ => LobbyVisibility::Unknown,
    }
}

fn state_name(state: LobbyState) -> &'static str {
    match state {
        LobbyState::InGame => IN_GAME,
        _ => OPEN,
    }
}

fn state_of(name: &str) -> LobbyState {
    match name {
        OPEN => LobbyState::Open,
        IN_GAME => LobbyState::InGame,
        _ => LobbyState::Unknown,
    }
}

fn no_lobby() -> AppError {
    AppError::not_found("no such lobby")
}

fn not_a_member() -> AppError {
    AppError::new(codes::NOT_A_MEMBER, "you are not a member of this lobby")
}

fn not_host() -> AppError {
    AppError::forbidden("only the lobby's host may do this")
}

fn quota(message: &str) -> AppError {
    AppError::new(codes::QUOTA_EXCEEDED, message)
}

fn invalid(field: &str, problem: &str) -> AppError {
    let mut details = ValidationDetails::new();
    details.add(field, problem);
    AppError::validation(details)
}

fn code_taken() -> AppError {
    AppError::conflict(CODE_TAKEN)
}

fn is_code_taken(error: &AppError) -> bool {
    error.code() == codes::CONFLICT && error.api_error().message == CODE_TAKEN
}

fn count(row: CountRow) -> u64 {
    u64::try_from(row.n).unwrap_or(0)
}

/// A new random join code: 40 random bits (never 0, `22222222`).
fn new_code() -> Result<String, AppError> {
    loop {
        let mut bytes = [0u8; 5];
        getrandom::fill(&mut bytes).map_err(|e| AppError::internal(std::io::Error::other(format!("the system random generator failed: {e}"))))?;
        let number = bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        if let Some(code) = LobbyCode::from_u64(number).filter(|_| number != 0) {
            debug_assert_eq!(code.as_str().len(), LOBBY_CODE_LEN);
            return Ok(code.as_str().to_string());
        }
    }
}

fn buckets(rate: u32, window_secs: u32) -> Option<KeyedBuckets<UserId>> {
    (rate > 0).then(|| KeyedBuckets::new(rate, Duration::from_secs(u64::from(window_secs)), 100_000))
}

fn check(buckets: Option<&KeyedBuckets<UserId>>, user: UserId) -> Result<(), AppError> {
    match buckets.map(|b| b.check(user)) {
        Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
        _ => Ok(()),
    }
}

/// Why a member goes.
#[derive(Clone, Copy)]
enum Removal {
    /// It leaves (the acting player: itself, or `None` for the server's own: a disconnect).
    Leave(Option<UserId>),
    /// It is kicked.
    Kick(LobbyActor),
}

/// A member's removal, for the work after the commit.
struct Removed {
    row: LobbyRow,
    member: MemberRow,
    /// The members left, in the order they joined.
    remaining: Vec<i64>,
    new_host: Option<i64>,
}

/// An update's result, for the work after the commit.
enum Updated {
    Changed(Vec<LobbyChange>),
    Closed { row: LobbyRow, members: Vec<i64>, metadata: BTreeMap<String, String> },
}

struct Inner {
    config: LobbiesConfig,
    create_rate: Option<KeyedBuckets<UserId>>,
    join_rate: Option<KeyedBuckets<UserId>>,
    bad_codes: Option<KeyedBuckets<UserId>>,
    update_rate: Option<KeyedBuckets<UserId>>,
}

/// Lobbies, members, ready flags, metadata, join codes and the pushes. A state value
/// (`Ext<LobbyService>` in handlers, `state.get::<LobbyService>()` elsewhere) once the
/// [`Lobbies`](super::Lobbies) module is registered. The players' actions come through the routes;
/// server code acts with the same methods ([`LobbyActor::Server`] for host-only actions), adds a
/// player with [`add_player`](Self::add_player) and reads with [`lobby`](Self::lobby) and
/// [`members_of`](Self::members_of).
///
/// ```no_run
/// use net_backend_server::lobbies::LobbyService;
/// use net_backend_server::protocol::LobbyId;
/// use net_backend_server::{AppError, AppState};
///
/// // A game rule: the match starts when every member is ready.
/// async fn all_ready(state: &AppState, lobby: LobbyId) -> Result<bool, AppError> {
///     let lobbies = state.get::<LobbyService>().ok_or_else(|| AppError::unavailable("no lobbies module"))?;
///     let info = lobbies.lobby(state, lobby).await?.ok_or_else(|| AppError::not_found("no such lobby"))?;
///     Ok(info.players.iter().all(|member| member.ready))
/// }
/// ```
#[derive(Clone)]
pub struct LobbyService(Arc<Inner>);

impl std::fmt::Debug for LobbyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LobbyService").field(&self.0.config).finish()
    }
}

impl LobbyService {
    pub(crate) fn new(config: LobbiesConfig) -> Self {
        let create_rate = buckets(config.create_rate, config.create_rate_window_secs);
        let join_rate = buckets(config.join_rate, config.join_rate_window_secs);
        let bad_codes = buckets(config.bad_code_rate, config.bad_code_rate_window_secs);
        let update_rate = buckets(config.update_rate, config.update_rate_window_secs);
        Self(Arc::new(Inner { config, create_rate, join_rate, bad_codes, update_rate }))
    }

    /// The settings.
    pub fn config(&self) -> &LobbiesConfig {
        &self.0.config
    }

    /// Count one lobby creation of `user` against `create_rate` (429 `rate_limited` over it).
    pub(crate) fn check_create_rate(&self, user: UserId) -> Result<(), AppError> {
        check(self.0.create_rate.as_ref(), user)
    }

    /// Count one join attempt of `user` against `join_rate` (429 `rate_limited` over it).
    pub(crate) fn check_join_rate(&self, user: UserId) -> Result<(), AppError> {
        check(self.0.join_rate.as_ref(), user)
    }

    /// Count one lobby change of `user` (a PATCH, a new code) against `update_rate` (429
    /// `rate_limited` over it).
    pub(crate) fn check_update_rate(&self, user: UserId) -> Result<(), AppError> {
        check(self.0.update_rate.as_ref(), user)
    }

    // ---- reading ---------------------------------------------------------------------------------

    fn build(&self, row: &LobbyRow, members: u64, metadata: BTreeMap<String, String>, member_view: bool, players: Vec<LobbyMember>) -> LobbyInfo {
        let mut info = LobbyInfo::new(
            LobbyId(row.id),
            visibility_of(&row.visibility),
            state_of(&row.state),
            u32::try_from(row.max_players).unwrap_or(0),
            u32::try_from(members).unwrap_or(u32::MAX),
            UnixMillis(row.created_at),
        )
        .with_metadata(metadata)
        .with_players(players);
        if let Some(host) = row.host_id {
            info = info.with_host(UserId(host));
        }
        if member_view {
            if let Some(code) = LobbyCode::parse(&row.code) {
                info = info.with_code(code);
            }
            if let Some(room) = row.chat_room {
                info = info.with_chat_room(RoomId(room));
            }
        }
        info
    }

    async fn lobby_row(state: &AppState, lobby: i64) -> Result<LobbyRow, AppError> {
        state.db().fetch_optional::<LobbyRow, _>(&store::lobby_by_id(lobby)).await?.ok_or_else(no_lobby)
    }

    async fn member_row(state: &AppState, lobby: i64, user: UserId) -> Result<Option<MemberRow>, AppError> {
        Ok(state.db().fetch_optional::<MemberRow, _>(&store::member(lobby, user.get())).await?)
    }

    async fn metadata(state: &AppState, lobbies: &[i64]) -> Result<HashMap<i64, BTreeMap<String, String>>, AppError> {
        let mut out: HashMap<i64, BTreeMap<String, String>> = HashMap::new();
        if lobbies.is_empty() {
            return Ok(out);
        }
        for row in state.db().fetch_all::<MetaRow, _>(&store::metadata_of(lobbies)).await? {
            out.entry(row.lobby_id).or_default().insert(row.meta_key, row.meta_value);
        }
        Ok(out)
    }

    async fn names(state: &AppState, users: &[i64]) -> Result<HashMap<i64, String>, AppError> {
        if users.is_empty() {
            return Ok(HashMap::new());
        }
        Ok(state.db().fetch_all::<NameRow, _>(&store::names(users)).await?.into_iter().filter_map(|r| r.display_name.map(|n| (r.id, n))).collect())
    }

    fn member_of(row: &MemberRow, names: &HashMap<i64, String>) -> LobbyMember {
        let member = LobbyMember::new(UserId(row.user_id), UnixMillis(row.joined_at)).with_ready(row.ready != 0);
        match names.get(&row.user_id) {
            Some(name) => member.with_name(name.clone()),
            None => member,
        }
    }

    async fn players(state: &AppState, lobby: i64) -> Result<Vec<LobbyMember>, AppError> {
        let rows = state.db().fetch_all::<MemberRow, _>(&store::members(lobby)).await?;
        let names = Self::names(state, &rows.iter().map(|r| r.user_id).collect::<Vec<_>>()).await?;
        Ok(rows.iter().map(|r| Self::member_of(r, &names)).collect())
    }

    /// One lobby: its metadata, and its members (or only their count).
    async fn view(&self, state: &AppState, row: &LobbyRow, member_view: bool, with_players: bool) -> Result<LobbyInfo, AppError> {
        let metadata = Self::metadata(state, &[row.id]).await?.remove(&row.id).unwrap_or_default();
        if with_players {
            let players = Self::players(state, row.id).await?;
            Ok(self.build(row, players.len() as u64, metadata, member_view, players))
        } else {
            let members = count(state.db().fetch_one::<CountRow, _>(&store::count_members(row.id)).await?);
            Ok(self.build(row, members, metadata, member_view, Vec::new()))
        }
    }

    /// Whether `user`, not a member, may see the lobby: a public one, or a friends-only one of a
    /// friend.
    async fn may_see(&self, state: &AppState, row: &LobbyRow, user: UserId) -> Result<bool, AppError> {
        match row.visibility.as_str() {
            PUBLIC => Ok(true),
            FRIENDS => friends::is_friend(state, row.host_id, user).await,
            _ => Ok(false),
        }
    }

    /// One lobby with its members, as `user` sees it (404 for a lobby `user` may not see).
    pub async fn get(&self, state: &AppState, user: UserId, lobby: LobbyId) -> Result<LobbyInfo, AppError> {
        let row = Self::lobby_row(state, lobby.get()).await?;
        let member = Self::member_row(state, lobby.get(), user).await?.is_some();
        if !member && !self.may_see(state, &row, user).await? {
            return Err(no_lobby());
        }
        self.view(state, &row, member, true).await
    }

    /// One lobby with its members and its join code (server code: every lobby; `None` if there is
    /// no such lobby).
    pub async fn lobby(&self, state: &AppState, lobby: LobbyId) -> Result<Option<LobbyInfo>, AppError> {
        match state.db().fetch_optional::<LobbyRow, _>(&store::lobby_by_id(lobby.get())).await? {
            Some(row) => Ok(Some(self.view(state, &row, true, true).await?)),
            None => Ok(None),
        }
    }

    /// The members of `lobby`, in the order they joined.
    pub async fn members_of(&self, state: &AppState, lobby: LobbyId) -> Result<Vec<UserId>, AppError> {
        Ok(state.db().fetch_all::<MemberRow, _>(&store::members(lobby.get())).await?.into_iter().map(|r| UserId(r.user_id)).collect())
    }

    /// `user`'s lobbies with their members, oldest membership first.
    pub async fn mine(&self, state: &AppState, user: UserId) -> Result<LobbyList, AppError> {
        let memberships = state.db().fetch_all::<MemberRow, _>(&store::memberships(user.get())).await?;
        if memberships.is_empty() {
            return Ok(LobbyList::new(Vec::new()));
        }
        let ids: Vec<i64> = memberships.iter().map(|m| m.lobby_id).collect();
        let rows: HashMap<i64, LobbyRow> = state.db().fetch_all::<LobbyRow, _>(&store::lobbies_by_id(&ids)).await?.into_iter().map(|r| (r.id, r)).collect();
        let mut lobbies = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = rows.get(&id) {
                lobbies.push(self.view(state, row, true, true).await?);
            }
        }
        Ok(LobbyList::new(lobbies))
    }

    /// Open lobbies by metadata filters, newest first: every public one, or (with `friends`) the
    /// public and friends-only ones `user`'s friends host.
    pub async fn search(&self, state: &AppState, user: UserId, search: &LobbySearch) -> Result<Page<LobbyInfo>, AppError> {
        search.validate()?;
        let page = search.page();
        let limit = u64::from(page.limit_or_default());
        let before = match &page.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let hosts = if search.friends {
            let Some(friends) = friends::friend_ids(state, user).await? else {
                return Err(invalid("friends", "needs the friends module on the server"));
            };
            if friends.is_empty() {
                return Ok(Page::new(Vec::new(), None));
            }
            Some(friends)
        } else {
            None
        };
        let filters: Vec<(String, String)> = search.filters.iter().map(|f| (f.key.clone(), f.value.clone())).collect();
        let query = store::Search { filters: &filters, hosts: hosts.as_deref(), include_full: search.include_full, before, limit: limit + 1 };
        let mut rows = state.db().fetch_all::<LobbyRow, _>(&store::search(query)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let counts: HashMap<i64, u64> = if ids.is_empty() {
            HashMap::new()
        } else {
            state
                .db()
                .fetch_all::<LobbyCountRow, _>(&store::member_counts(&ids))
                .await?
                .into_iter()
                .map(|r| (r.lobby_id, u64::try_from(r.n).unwrap_or(0)))
                .collect()
        };
        let mut metadata = Self::metadata(state, &ids).await?;
        let items = rows
            .iter()
            .map(|r| self.build(r, counts.get(&r.id).copied().unwrap_or(0), metadata.remove(&r.id).unwrap_or_default(), false, Vec::new()))
            .collect();
        Ok(Page::new(items, next))
    }

    // ---- create -----------------------------------------------------------------------------------

    fn check_metadata(&self, metadata: &BTreeMap<String, String>) -> Result<(), AppError> {
        if metadata.len() > self.0.config.max_metadata_keys as usize {
            return Err(invalid("metadata", &format!("holds more than {} keys", self.0.config.max_metadata_keys)));
        }
        let bytes: usize = metadata.iter().map(|(k, v)| k.len() + v.len()).sum();
        if bytes > self.0.config.max_metadata_bytes {
            return Err(invalid("metadata", &format!("is larger than {} bytes", self.0.config.max_metadata_bytes)));
        }
        Ok(())
    }

    fn check_settings(&self, state: &AppState, visibility: Option<LobbyVisibility>, max_players: Option<u32>) -> Result<(), AppError> {
        if visibility == Some(LobbyVisibility::Friends) && !friends::available(state) {
            return Err(invalid("visibility", "friends needs the friends module on the server"));
        }
        if max_players.is_some_and(|m| m > self.0.config.max_players) {
            return Err(invalid("max_players", &format!("must be at most {}", self.0.config.max_players)));
        }
        Ok(())
    }

    fn check_create(&self, state: &AppState, request: &CreateLobby) -> Result<(), AppError> {
        request.validate()?;
        self.check_settings(state, Some(request.visibility), Some(request.max_players))?;
        self.check_metadata(&request.metadata)
    }

    /// `user` creates a lobby and hosts it: the [`BeforeLobbyCreate`] hooks, then one transaction
    /// (the player's lobby count, a unique join code), then its chat room. The rate is the route's.
    pub async fn create(&self, state: &AppState, ctx: &HookCtx, user: UserId, request: CreateLobby) -> Result<LobbyInfo, AppError> {
        self.check_create(state, &request)?;
        let event = state.hooks().run_before(ctx, BeforeLobbyCreate { user, request }).await?;
        let request = event.request;
        self.check_create(state, &request)?;
        let max_lobbies = u64::from(self.0.config.max_lobbies_per_user);
        let visibility = visibility_name(request.visibility);
        let mut attempts = 0;
        let id = loop {
            let code = new_code()?;
            let result = in_tx!(state, |tx| async {
                Self::lock_user(&mut tx, user.get()).await?;
                if count(tx.fetch_one::<CountRow, _>(&store::count_memberships(user.get())).await?) >= max_lobbies {
                    return Err(quota("you are in too many lobbies: leave one first"));
                }
                let now = state.now().get();
                let new = store::NewLobby { code: &code, visibility, host: user.get(), max_players: i64::from(request.max_players), now };
                let id = match tx.insert_id(&store::insert_lobby(new)?, "id").await {
                    Ok(id) => id,
                    Err(error) if error.is_unique_violation() => return Err(code_taken()),
                    Err(error) => return Err(error.into()),
                };
                tx.execute(&store::insert_member(id, user.get(), now)?).await?;
                for (key, value) in &request.metadata {
                    tx.execute(&store::insert_meta(id, key, value)?).await?;
                }
                Ok(id)
            });
            match result {
                Err(error) if is_code_taken(&error) => {
                    attempts += 1;
                    if attempts >= CODE_ATTEMPTS {
                        return Err(AppError::internal(std::io::Error::other("no free lobby code was found")));
                    }
                }
                other => break other?,
            }
        };
        if let Some(room) = self.chat_create(state, user).await {
            if let Err(error) = state.db().execute(&store::set_chat_room(id, room)).await {
                tracing::warn!(%error, lobby = id, "lobbies: storing the chat room failed");
            }
        }
        self.after(state, ctx, LobbyId(id), Some(user), Some(user), LobbyEvent::Created).await;
        let row = Self::lobby_row(state, id).await?;
        self.view(state, &row, true, true).await
    }

    // ---- join -------------------------------------------------------------------------------------

    async fn lock_user(tx: &mut DbTx, user: i64) -> Result<(), AppError> {
        let dialect = tx.dialect();
        match tx.fetch_optional::<IdRow, _>(&store::lock_user(user, dialect)).await? {
            Some(_) => Ok(()),
            None => Err(AppError::not_found("no such account")),
        }
    }

    async fn locked_lobby(tx: &mut DbTx, lobby: i64) -> Result<LobbyRow, AppError> {
        let dialect = tx.dialect();
        tx.fetch_optional::<LobbyRow, _>(&store::lock_lobby(lobby, dialect)).await?.ok_or_else(no_lobby)
    }

    /// `user` joins a lobby by its id: a public one, or a friends-only one of a friend (404 for
    /// the others: they are joined with the code). A member already: the lobby again. The rate is
    /// the route's.
    pub async fn join(&self, state: &AppState, ctx: &HookCtx, user: UserId, lobby: LobbyId) -> Result<LobbyInfo, AppError> {
        let row = Self::lobby_row(state, lobby.get()).await?;
        if Self::member_row(state, lobby.get(), user).await?.is_some() {
            return self.view(state, &row, true, true).await;
        }
        if !self.may_see(state, &row, user).await? {
            return Err(no_lobby());
        }
        self.join_row(state, ctx, user, row, false).await
    }

    /// `user` joins the lobby with this join code (any visibility). A code that matches no lobby
    /// counts against `bad_code_rate`; past it, every code answers 429 until the window frees one.
    /// The join rate is the route's.
    pub async fn join_by_code(&self, state: &AppState, ctx: &HookCtx, user: UserId, code: &str) -> Result<LobbyInfo, AppError> {
        let Some(code) = LobbyCode::parse(code) else {
            return Err(invalid("code", &format!("must be {LOBBY_CODE_LEN} characters of the join-code alphabet")));
        };
        if let Some(RateDecision::Deny { retry_after_ms }) = self.0.bad_codes.as_ref().map(|b| b.peek(&user)) {
            return Err(AppError::rate_limited(retry_after_ms));
        }
        let Some(row) = state.db().fetch_optional::<LobbyRow, _>(&store::lobby_by_code(code.as_str())).await? else {
            if let Some(bad) = &self.0.bad_codes {
                let _ = bad.check(user);
            }
            return Err(AppError::not_found("no lobby has this code"));
        };
        if Self::member_row(state, row.id, user).await?.is_some() {
            return self.view(state, &row, true, true).await;
        }
        self.join_row(state, ctx, user, row, true).await
    }

    /// Server code adds `user` to a lobby (any visibility; the lobby must be open and have room;
    /// the [`BeforeLobbyJoin`] hooks run). A member already: the lobby again.
    pub async fn add_player(&self, state: &AppState, ctx: &HookCtx, lobby: LobbyId, user: UserId) -> Result<LobbyInfo, AppError> {
        let row = Self::lobby_row(state, lobby.get()).await?;
        if Self::member_row(state, lobby.get(), user).await?.is_some() {
            return self.view(state, &row, true, true).await;
        }
        self.join_row(state, ctx, user, row, false).await
    }

    async fn join_row(&self, state: &AppState, ctx: &HookCtx, user: UserId, row: LobbyRow, by_code: bool) -> Result<LobbyInfo, AppError> {
        if row.state != OPEN {
            return Err(AppError::conflict("the lobby is not open"));
        }
        if friends::is_blocked(state, row.host_id, user).await? {
            return Err(AppError::forbidden("you may not join this lobby"));
        }
        let lobby = LobbyId(row.id);
        state.hooks().run_before(ctx, BeforeLobbyJoin { lobby, user, by_code }).await?;
        let max_lobbies = u64::from(self.0.config.max_lobbies_per_user);
        let joined = in_tx!(state, |tx| async {
            Self::lock_user(&mut tx, user.get()).await?;
            let row = Self::locked_lobby(&mut tx, lobby.get()).await?;
            if tx.fetch_optional::<MemberRow, _>(&store::member(lobby.get(), user.get())).await?.is_some() {
                return Ok(None);
            }
            if row.state != OPEN {
                return Err(AppError::conflict("the lobby is not open"));
            }
            if count(tx.fetch_one::<CountRow, _>(&store::count_memberships(user.get())).await?) >= max_lobbies {
                return Err(quota("you are in too many lobbies: leave one first"));
            }
            let members = count(tx.fetch_one::<CountRow, _>(&store::count_members(lobby.get())).await?);
            if members >= u64::try_from(row.max_players).unwrap_or(0) {
                return Err(quota("the lobby is full"));
            }
            tx.execute(&store::insert_member(lobby.get(), user.get(), state.now().get())?).await?;
            Ok(Some(row))
        })?;
        let row = match joined {
            Some(row) => row,
            None => return self.view(state, &Self::lobby_row(state, lobby.get()).await?, true, true).await,
        };
        if let Some(room) = row.chat_room {
            self.chat_add(state, room, user).await;
        }
        let info = self.view(state, &row, true, true).await?;
        if let Some(member) = info.players.iter().find(|m| m.user == user) {
            let push = LobbyMemberUpdate::new(lobby, MemberChange::Joined, member.clone());
            self.push(state, info.players.iter().map(|m| m.user.get()), &push);
        }
        self.after(state, ctx, lobby, Some(user), Some(user), LobbyEvent::Joined).await;
        Ok(info)
    }

    // ---- leave, kick ------------------------------------------------------------------------------

    /// Remove `user` from a lobby (a kick has its rights checked). The lobby's host passes to the
    /// member who joined first; the last member's leaving removes the lobby.
    async fn remove(&self, state: &AppState, ctx: &HookCtx, lobby: LobbyId, user: UserId, how: Removal) -> Result<bool, AppError> {
        let removed = in_tx!(state, |tx| async {
            let row = Self::locked_lobby(&mut tx, lobby.get()).await?;
            if let Removal::Kick(actor) = how {
                if !actor.may_manage(row.host_id) {
                    return Err(Self::refused(&mut tx, lobby.get(), actor).await);
                }
            }
            let Some(member) = tx.fetch_optional::<MemberRow, _>(&store::member(lobby.get(), user.get())).await? else { return Ok(None) };
            tx.execute(&store::delete_member(member.id)).await?;
            let remaining: Vec<i64> = tx.fetch_all::<MemberRow, _>(&store::members(lobby.get())).await?.into_iter().map(|m| m.user_id).collect();
            let mut new_host = None;
            if remaining.is_empty() {
                tx.execute(&store::delete_lobby(lobby.get())).await?;
            } else if row.host_id == Some(user.get()) || row.host_id.is_none() {
                new_host = remaining.first().copied();
                let change = store::LobbyUpdate { host: new_host, ..store::LobbyUpdate::default() };
                tx.execute(&store::update_lobby(lobby.get(), change, state.now().get())).await?;
            }
            Ok(Some(Removed { row, member, remaining, new_host }))
        })?;
        let Some(removed) = removed else { return Ok(false) };
        if let Some(room) = removed.row.chat_room {
            if removed.remaining.is_empty() {
                self.chat_delete(state, room).await;
            } else {
                self.chat_remove(state, room, user).await;
            }
        }
        let names = Self::names(state, &[user.get()]).await.unwrap_or_default();
        let (change, event, actor) = match how {
            Removal::Kick(actor) => (MemberChange::Kicked, LobbyEvent::Kicked, actor.user()),
            Removal::Leave(actor) => (MemberChange::Left, LobbyEvent::Left, actor),
        };
        let push = LobbyMemberUpdate::new(lobby, change, Self::member_of(&removed.member, &names));
        self.push(state, removed.remaining.iter().copied().chain([user.get()]), &push);
        self.after(state, ctx, lobby, actor, Some(user), event).await;
        if removed.remaining.is_empty() {
            self.after(state, ctx, lobby, actor, None, LobbyEvent::Closed).await;
        } else if let Some(host) = removed.new_host {
            self.changed(state, lobby, vec![LobbyChange::Host]).await;
            self.after(state, ctx, lobby, actor, Some(UserId(host)), LobbyEvent::HostChanged).await;
        }
        Ok(true)
    }

    /// `user` leaves a lobby; `true` if it was a member (404 for an unknown lobby).
    pub async fn leave(&self, state: &AppState, ctx: &HookCtx, user: UserId, lobby: LobbyId) -> Result<bool, AppError> {
        self.remove(state, ctx, lobby, user, Removal::Leave(Some(user))).await
    }

    /// `actor` (the host, a manager or the server) removes `user` from a lobby; `true` if it was a
    /// member.
    pub async fn kick(&self, state: &AppState, ctx: &HookCtx, actor: LobbyActor, lobby: LobbyId, user: UserId) -> Result<bool, AppError> {
        if actor.user() == Some(user) {
            return Err(invalid("user", "is the caller: leave the lobby instead"));
        }
        self.remove(state, ctx, lobby, user, Removal::Kick(actor)).await
    }

    /// `user` leaves every lobby it is in (server code; the module's disconnect rule uses it; the
    /// hooks see no acting player); how many it left.
    pub async fn leave_all(&self, state: &AppState, ctx: &HookCtx, user: UserId) -> Result<usize, AppError> {
        let memberships = state.db().fetch_all::<MemberRow, _>(&store::memberships(user.get())).await?;
        let mut left = 0;
        for membership in memberships {
            match self.remove(state, ctx, LobbyId(membership.lobby_id), user, Removal::Leave(None)).await {
                Ok(true) => left += 1,
                Ok(false) => {}
                // The lobby closed meanwhile.
                Err(error) if error.code() == codes::NOT_FOUND => {}
                Err(error) => return Err(error),
            }
        }
        Ok(left)
    }

    // ---- ready ------------------------------------------------------------------------------------

    /// `user` sets its ready flag (403 `not_a_member` when not in the lobby).
    pub async fn set_ready(&self, state: &AppState, ctx: &HookCtx, user: UserId, lobby: LobbyId, ready: bool) -> Result<(), AppError> {
        let changed = in_tx!(state, |tx| async {
            Self::locked_lobby(&mut tx, lobby.get()).await?;
            let member = tx.fetch_optional::<MemberRow, _>(&store::member(lobby.get(), user.get())).await?.ok_or_else(not_a_member)?;
            if (member.ready != 0) == ready {
                return Ok(false);
            }
            tx.execute(&store::set_ready(member.id, ready)).await?;
            Ok(true)
        })?;
        if changed {
            let players = Self::players(state, lobby.get()).await?;
            if let Some(member) = players.iter().find(|m| m.user == user) {
                let push = LobbyMemberUpdate::new(lobby, MemberChange::Ready, member.clone());
                self.push(state, players.iter().map(|m| m.user.get()), &push);
            }
            self.after(state, ctx, lobby, Some(user), Some(user), LobbyEvent::Ready(ready)).await;
        }
        Ok(())
    }

    // ---- host actions -----------------------------------------------------------------------------

    fn check_update(&self, state: &AppState, update: &UpdateLobby) -> Result<(), AppError> {
        // One removal and one new key per allowed key is the most a valid change needs: more is
        // refused before any work (the hooks, the database).
        let max_entries = 2 * self.0.config.max_metadata_keys as usize;
        if update.metadata.len() > max_entries {
            return Err(invalid("metadata", &format!("holds more than {max_entries} changes")));
        }
        update.validate()?;
        self.check_settings(state, update.visibility, update.max_players)
    }

    /// Why `actor` may not manage the lobby: 404 `not_found` for a player who is not a member (a
    /// stranger never learns that a lobby id exists), else 403 `forbidden`.
    async fn refused(tx: &mut DbTx, lobby: i64, actor: LobbyActor) -> AppError {
        if let LobbyActor::Player(user) = actor {
            match tx.fetch_optional::<MemberRow, _>(&store::member(lobby, user.get())).await {
                Ok(None) => return no_lobby(),
                Ok(Some(_)) => {}
                Err(error) => return error.into(),
            }
        }
        not_host()
    }

    /// The rights check before the hooks run (a hook never sees a refused change); the
    /// transaction checks again under the lobby's lock.
    async fn check_manage(state: &AppState, lobby: LobbyId, actor: LobbyActor) -> Result<(), AppError> {
        let row = Self::lobby_row(state, lobby.get()).await?;
        if actor.may_manage(row.host_id) {
            return Ok(());
        }
        match actor {
            LobbyActor::Player(user) if Self::member_row(state, lobby.get(), user).await?.is_none() => Err(no_lobby()),
            _ => Err(not_host()),
        }
    }

    /// `actor` (the host, a manager or the server) changes a lobby: the rights, the
    /// [`BeforeLobbyUpdate`] hooks, then the change (a size below the member count, or metadata
    /// past the limits: 422; more than two metadata changes per allowed key: 422 before anything
    /// else). The metadata is merged in memory and only the changed keys are written. State
    /// `closed` removes the lobby and its chat room; `in_game` back to `open` resets every ready
    /// flag. Answers the lobby as it is now (after `closed`: as it was, with state `closed`). A
    /// player who is not a member gets 404 `not_found`.
    pub async fn update(&self, state: &AppState, ctx: &HookCtx, actor: LobbyActor, lobby: LobbyId, update: UpdateLobby) -> Result<LobbyInfo, AppError> {
        self.check_update(state, &update)?;
        Self::check_manage(state, lobby, actor).await?;
        let event = state.hooks().run_before(ctx, BeforeLobbyUpdate { lobby, user: actor.user(), update }).await?;
        let update = event.update;
        self.check_update(state, &update)?;
        let (max_keys, max_bytes) = (self.0.config.max_metadata_keys as usize, self.0.config.max_metadata_bytes);
        let outcome = in_tx!(state, |tx| async {
            let row = Self::locked_lobby(&mut tx, lobby.get()).await?;
            if !actor.may_manage(row.host_id) {
                return Err(Self::refused(&mut tx, lobby.get(), actor).await);
            }
            if update.state == Some(LobbyState::Closed) {
                let members: Vec<i64> = tx.fetch_all::<MemberRow, _>(&store::members(lobby.get())).await?.into_iter().map(|m| m.user_id).collect();
                let metadata = tx.fetch_all::<MetaRow, _>(&store::metadata_of(&[lobby.get()])).await?.into_iter().map(|m| (m.meta_key, m.meta_value)).collect();
                tx.execute(&store::delete_lobby(lobby.get())).await?;
                return Ok(Updated::Closed { row, members, metadata });
            }
            let mut changes = Vec::new();
            let mut change = store::LobbyUpdate::default();
            if let Some(visibility) = update.visibility.map(visibility_name).filter(|v| *v != row.visibility) {
                change.visibility = Some(visibility);
                changes.push(LobbyChange::Settings);
            }
            if let Some(max) = update.max_players.map(i64::from).filter(|m| *m != row.max_players) {
                let members = count(tx.fetch_one::<CountRow, _>(&store::count_members(lobby.get())).await?);
                if u64::try_from(max).unwrap_or(0) < members {
                    return Err(invalid("max_players", "is below the number of members"));
                }
                change.max_players = Some(max);
                if !changes.contains(&LobbyChange::Settings) {
                    changes.push(LobbyChange::Settings);
                }
            }
            if let Some(new_state) = update.state.map(state_name).filter(|s| *s != row.state) {
                change.state = Some(new_state);
                changes.push(LobbyChange::State);
                if new_state == OPEN {
                    tx.execute(&store::reset_ready(lobby.get())).await?;
                }
            }
            if !update.metadata.is_empty() {
                // One read, the merge and the limits in memory, then only the changed keys.
                let rows = tx.fetch_all::<MetaIdRow, _>(&store::meta_rows(lobby.get())).await?;
                let current: HashMap<&str, &MetaIdRow> = rows.iter().map(|r| (r.meta_key.as_str(), r)).collect();
                let mut merged: BTreeMap<&str, &str> = rows.iter().map(|r| (r.meta_key.as_str(), r.meta_value.as_str())).collect();
                for (key, value) in &update.metadata {
                    match value {
                        Some(value) => merged.insert(key.as_str(), value.as_str()),
                        None => merged.remove(key.as_str()),
                    };
                }
                if merged.len() > max_keys {
                    return Err(invalid("metadata", &format!("would hold more than {max_keys} keys")));
                }
                if merged.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > max_bytes {
                    return Err(invalid("metadata", &format!("would be larger than {max_bytes} bytes")));
                }
                let mut metadata_changed = false;
                for (key, value) in &update.metadata {
                    match (current.get(key.as_str()), value) {
                        (Some(row), Some(value)) if row.meta_value == *value => continue,
                        (Some(row), Some(value)) => tx.execute(&store::set_meta(row.id, value)).await?,
                        (None, Some(value)) => tx.execute(&store::insert_meta(lobby.get(), key, value)?).await?,
                        (Some(row), None) => tx.execute(&store::delete_meta(row.id)).await?,
                        (None, None) => continue,
                    };
                    metadata_changed = true;
                }
                if metadata_changed {
                    changes.push(LobbyChange::Metadata);
                }
            }
            if !changes.is_empty() {
                tx.execute(&store::update_lobby(lobby.get(), change, state.now().get())).await?;
            }
            Ok(Updated::Changed(changes))
        })?;
        match outcome {
            Updated::Closed { mut row, members, metadata } => {
                if let Some(room) = row.chat_room {
                    self.chat_delete(state, room).await;
                }
                row.state = "closed".into();
                let mut info = self.build(&row, members.len() as u64, metadata, true, Vec::new());
                info.state = LobbyState::Closed;
                self.push(state, members.iter().copied(), &LobbyUpdate::new(vec![LobbyChange::State], info.clone()));
                self.after(state, ctx, lobby, actor.user(), None, LobbyEvent::Closed).await;
                Ok(info)
            }
            Updated::Changed(changes) => {
                if !changes.is_empty() {
                    self.changed(state, lobby, changes.clone()).await;
                    self.after(state, ctx, lobby, actor.user(), None, LobbyEvent::Updated(changes)).await;
                }
                let row = Self::lobby_row(state, lobby.get()).await?;
                self.view(state, &row, true, true).await
            }
        }
    }

    /// Close a lobby (`actor`: the host, a manager or the server): it is removed, its members get a
    /// last `lobby.changed` with state `closed`.
    pub async fn close(&self, state: &AppState, ctx: &HookCtx, actor: LobbyActor, lobby: LobbyId) -> Result<LobbyInfo, AppError> {
        self.update(state, ctx, actor, lobby, UpdateLobby::new().with_state(LobbyState::Closed)).await
    }

    /// A new join code (`actor`: the host, a manager or the server); the old one stops working. A
    /// player who is not a member gets 404 `not_found`.
    pub async fn new_code(&self, state: &AppState, ctx: &HookCtx, actor: LobbyActor, lobby: LobbyId) -> Result<LobbyInfo, AppError> {
        let mut attempts = 0;
        loop {
            let code = new_code()?;
            let result = in_tx!(state, |tx| async {
                let row = Self::locked_lobby(&mut tx, lobby.get()).await?;
                if !actor.may_manage(row.host_id) {
                    return Err(Self::refused(&mut tx, lobby.get(), actor).await);
                }
                let change = store::LobbyUpdate { code: Some(&code), ..store::LobbyUpdate::default() };
                match tx.execute(&store::update_lobby(lobby.get(), change, state.now().get())).await {
                    Ok(_) => Ok(()),
                    Err(error) if error.is_unique_violation() => Err(code_taken()),
                    Err(error) => Err(error.into()),
                }
            });
            match result {
                Err(error) if is_code_taken(&error) => {
                    attempts += 1;
                    if attempts >= CODE_ATTEMPTS {
                        return Err(AppError::internal(std::io::Error::other("no free lobby code was found")));
                    }
                }
                other => {
                    other?;
                    break;
                }
            }
        }
        self.changed(state, lobby, vec![LobbyChange::Code]).await;
        self.after(state, ctx, lobby, actor.user(), None, LobbyEvent::CodeChanged).await;
        let row = Self::lobby_row(state, lobby.get()).await?;
        self.view(state, &row, true, true).await
    }

    /// `actor` (the host, a manager or the server) hands the lobby to the member `to`. A player
    /// who is not a member gets 404 `not_found`.
    pub async fn transfer(&self, state: &AppState, ctx: &HookCtx, actor: LobbyActor, lobby: LobbyId, to: UserId) -> Result<(), AppError> {
        let changed = in_tx!(state, |tx| async {
            let row = Self::locked_lobby(&mut tx, lobby.get()).await?;
            if !actor.may_manage(row.host_id) {
                return Err(Self::refused(&mut tx, lobby.get(), actor).await);
            }
            if tx.fetch_optional::<MemberRow, _>(&store::member(lobby.get(), to.get())).await?.is_none() {
                return Err(AppError::not_found("no such member"));
            }
            if row.host_id == Some(to.get()) {
                return Ok(false);
            }
            let change = store::LobbyUpdate { host: Some(to.get()), ..store::LobbyUpdate::default() };
            tx.execute(&store::update_lobby(lobby.get(), change, state.now().get())).await?;
            Ok(true)
        })?;
        if changed {
            self.changed(state, lobby, vec![LobbyChange::Host]).await;
            self.after(state, ctx, lobby, actor.user(), Some(to), LobbyEvent::HostChanged).await;
        }
        Ok(())
    }

    // ---- upkeep -----------------------------------------------------------------------------------

    /// Remove lobbies left without members (with their chat rooms; a lobby a player joined
    /// meanwhile stays), give host-less lobbies (their host's account was deleted) the member who
    /// joined first, and delete the chat rooms of this module no lobby names (older than an hour);
    /// the number of lobbies removed. The module's background task runs it every
    /// `purge_interval_secs`.
    pub async fn purge(&self, state: &AppState) -> Result<u64, AppError> {
        let ctx = HookCtx::new(state.clone(), None);
        let mut removed = 0;
        for row in state.db().fetch_all::<RoomRefRow, _>(&store::empty_lobbies(PURGE_BATCH)).await? {
            if state.db().execute(&store::delete_empty_lobby(row.id)).await? > 0 {
                removed += 1;
                if let Some(room) = row.chat_room {
                    self.chat_delete(state, room).await;
                }
                self.after(state, &ctx, LobbyId(row.id), None, None, LobbyEvent::Closed).await;
            }
        }
        for row in state.db().fetch_all::<IdRow, _>(&store::hostless_lobbies(PURGE_BATCH)).await? {
            let lobby = LobbyId(row.id);
            let host = in_tx!(state, |tx| async {
                let row = Self::locked_lobby(&mut tx, lobby.get()).await?;
                if row.host_id.is_some() {
                    return Ok(None);
                }
                let Some(first) = tx.fetch_all::<MemberRow, _>(&store::members(lobby.get())).await?.first().map(|m| m.user_id) else { return Ok(None) };
                let change = store::LobbyUpdate { host: Some(first), ..store::LobbyUpdate::default() };
                tx.execute(&store::update_lobby(lobby.get(), change, state.now().get())).await?;
                Ok(Some(first))
            });
            match host {
                Ok(Some(host)) => {
                    self.changed(state, lobby, vec![LobbyChange::Host]).await;
                    self.after(state, &ctx, lobby, None, Some(UserId(host)), LobbyEvent::HostChanged).await;
                }
                Ok(None) => {}
                Err(error) if error.code() == codes::NOT_FOUND => {}
                Err(error) => return Err(error),
            }
        }
        match self.purge_orphan_rooms(state).await {
            Ok(0) => {}
            Ok(rooms) => tracing::info!(rooms, "lobbies: deleted chat rooms no lobby names"),
            Err(error) => tracing::warn!(%error, "lobbies: deleting chat rooms no lobby names failed"),
        }
        Ok(removed)
    }

    /// A WebSocket connection of `user` closed on this instance (the module's hook): without a
    /// connection left here after `disconnect_grace_secs`, it leaves its lobbies.
    pub(crate) fn disconnected(&self, state: &AppState, user: UserId) {
        if !self.0.config.leave_on_disconnect || state.ws().is_online(user) {
            return;
        }
        let (service, state) = (self.clone(), state.clone());
        let grace = Duration::from_secs(u64::from(self.0.config.disconnect_grace_secs));
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(grace) => {}
                _ = state.shutdown().wait() => return,
            }
            if state.ws().is_online(user) {
                return;
            }
            let ctx = HookCtx::new(state.clone(), None);
            if let Err(error) = service.leave_all(&state, &ctx, user).await {
                tracing::warn!(%error, "lobbies: leaving the lobbies of a disconnected player failed");
            }
        });
    }

    // ---- pushes and hooks -------------------------------------------------------------------------

    fn push<P: net_backend_protocol::ServerPush>(&self, state: &AppState, users: impl Iterator<Item = i64>, push: &P) {
        if !state.config().ws.enabled {
            return;
        }
        for user in users {
            if let Err(error) = state.ws().push_user(UserId(user), push) {
                tracing::warn!(%error, kind = P::KIND, "lobbies: a push failed");
            }
        }
    }

    /// `lobby.changed` to every member, with the lobby as it is now.
    async fn changed(&self, state: &AppState, lobby: LobbyId, changes: Vec<LobbyChange>) {
        let row = match Self::lobby_row(state, lobby.get()).await {
            Ok(row) => row,
            Err(error) => {
                tracing::debug!(%error, "lobbies: the lobby is gone before its lobby.changed push");
                return;
            }
        };
        match self.view(state, &row, true, true).await {
            Ok(mut info) => {
                let members: Vec<i64> = info.players.iter().map(|m| m.user.get()).collect();
                info.players = Vec::new();
                self.push(state, members.into_iter(), &LobbyUpdate::new(changes, info));
            }
            Err(error) => tracing::warn!(%error, "lobbies: reading the lobby for a lobby.changed push failed"),
        }
    }

    async fn after(&self, state: &AppState, ctx: &HookCtx, lobby: LobbyId, actor: Option<UserId>, user: Option<UserId>, event: LobbyEvent) {
        state.hooks().run_after(ctx, Arc::new(AfterLobbyChange { lobby, actor, user, event })).await;
    }

    // ---- the chat module --------------------------------------------------------------------------

    /// A chat group room with the host, when the chat module is registered and `chat_room` is on.
    #[cfg(feature = "chat")]
    async fn chat_create(&self, state: &AppState, host: UserId) -> Option<i64> {
        let chat = state.get::<crate::chat::ChatService>().filter(|_| self.0.config.chat_room)?;
        match chat.create_module_room(state, ROOM_ORIGIN, &[host]).await {
            Ok(room) => Some(room.get()),
            Err(error) => {
                tracing::warn!(%error, "lobbies: creating the chat room failed (the lobby has none)");
                None
            }
        }
    }

    #[cfg(feature = "chat")]
    async fn chat_add(&self, state: &AppState, room: i64, user: UserId) {
        if let Some(chat) = state.get::<crate::chat::ChatService>() {
            if let Err(error) = chat.add_member(state, RoomId(room), user).await {
                tracing::warn!(%error, room, "lobbies: adding a member to the chat room failed");
            }
        }
    }

    #[cfg(feature = "chat")]
    async fn chat_remove(&self, state: &AppState, room: i64, user: UserId) {
        if let Some(chat) = state.get::<crate::chat::ChatService>() {
            if let Err(error) = chat.remove_member(state, RoomId(room), user).await {
                tracing::warn!(%error, room, "lobbies: removing a member from the chat room failed");
            }
        }
    }

    /// Delete a closed lobby's chat room.
    #[cfg(feature = "chat")]
    async fn chat_delete(&self, state: &AppState, room: i64) {
        if let Some(chat) = state.get::<crate::chat::ChatService>() {
            if let Err(error) = chat.delete_group_room(state, RoomId(room)).await {
                tracing::warn!(%error, room, "lobbies: deleting the chat room failed (the purge tries again)");
            }
        }
    }

    /// Delete the chat rooms of this module that no lobby names (and that are older than
    /// [`ORPHAN_AGE_MS`]); how many.
    #[cfg(feature = "chat")]
    async fn purge_orphan_rooms(&self, state: &AppState) -> Result<u64, AppError> {
        let Some(chat) = state.get::<crate::chat::ChatService>() else { return Ok(0) };
        let before = state.now().get().saturating_sub(ORPHAN_AGE_MS);
        let (mut after, mut deleted) = (0, 0);
        loop {
            let rooms = chat.module_rooms(state, ROOM_ORIGIN, before, after, PURGE_BATCH).await?;
            let Some(last) = rooms.last().copied() else { break };
            let named: std::collections::HashSet<i64> =
                state.db().fetch_all::<RoomRefRow, _>(&store::lobbies_with_rooms(&rooms)).await?.into_iter().filter_map(|r| r.chat_room).collect();
            for room in rooms.iter().copied().filter(|r| !named.contains(r)) {
                if chat.delete_group_room(state, RoomId(room)).await? {
                    deleted += 1;
                }
            }
            if (rooms.len() as u64) < PURGE_BATCH {
                break;
            }
            after = last;
        }
        Ok(deleted)
    }

    #[cfg(not(feature = "chat"))]
    async fn chat_create(&self, _state: &AppState, _host: UserId) -> Option<i64> {
        None
    }

    #[cfg(not(feature = "chat"))]
    async fn chat_delete(&self, _state: &AppState, _room: i64) {}

    #[cfg(not(feature = "chat"))]
    async fn purge_orphan_rooms(&self, _state: &AppState) -> Result<u64, AppError> {
        Ok(0)
    }

    #[cfg(not(feature = "chat"))]
    async fn chat_add(&self, _state: &AppState, _room: i64, _user: UserId) {}

    #[cfg(not(feature = "chat"))]
    async fn chat_remove(&self, _state: &AppState, _room: i64, _user: UserId) {}
}

/// The friends module, when it is compiled in and registered.
#[cfg(feature = "friends")]
mod friends {
    use net_backend_protocol::UserId;

    use crate::error::AppError;
    use crate::friends::FriendService;
    use crate::state::AppState;

    pub(super) fn available(state: &AppState) -> bool {
        state.get::<FriendService>().is_some()
    }

    /// Whether `user` is a friend of the host.
    pub(super) async fn is_friend(state: &AppState, host: Option<i64>, user: UserId) -> Result<bool, AppError> {
        match (state.get::<FriendService>(), host) {
            (Some(friends), Some(host)) => friends.are_friends(state, UserId(host), user).await,
            _ => Ok(false),
        }
    }

    /// Whether the host blocked `user`.
    pub(super) async fn is_blocked(state: &AppState, host: Option<i64>, user: UserId) -> Result<bool, AppError> {
        match (state.get::<FriendService>(), host) {
            (Some(friends), Some(host)) => friends.is_blocked(state, UserId(host), user).await,
            _ => Ok(false),
        }
    }

    /// `user`'s friends (`None`: no friends module).
    pub(super) async fn friend_ids(state: &AppState, user: UserId) -> Result<Option<Vec<i64>>, AppError> {
        match state.get::<FriendService>() {
            Some(friends) => Ok(Some(friends.friends_of(state, user).await?.into_iter().map(UserId::get).collect())),
            None => Ok(None),
        }
    }
}

#[cfg(not(feature = "friends"))]
mod friends {
    use net_backend_protocol::UserId;

    use crate::error::AppError;
    use crate::state::AppState;

    pub(super) fn available(_state: &AppState) -> bool {
        false
    }

    pub(super) async fn is_friend(_state: &AppState, _host: Option<i64>, _user: UserId) -> Result<bool, AppError> {
        Ok(false)
    }

    pub(super) async fn is_blocked(_state: &AppState, _host: Option<i64>, _user: UserId) -> Result<bool, AppError> {
        Ok(false)
    }

    pub(super) async fn friend_ids(_state: &AppState, _user: UserId) -> Result<Option<Vec<i64>>, AppError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_codes_and_actors() {
        for visibility in [LobbyVisibility::Public, LobbyVisibility::Private, LobbyVisibility::Friends] {
            assert_eq!(visibility_of(visibility_name(visibility)), visibility);
        }
        assert_eq!(visibility_of("x"), LobbyVisibility::Unknown);
        for lobby_state in [LobbyState::Open, LobbyState::InGame] {
            assert_eq!(state_of(state_name(lobby_state)), lobby_state);
        }
        assert_eq!(state_of("x"), LobbyState::Unknown);
        for _ in 0..50 {
            let code = new_code().unwrap_or_default();
            assert!(LobbyCode::parse(&code).is_some_and(|c| c.to_u64() != 0), "{code}");
        }
        assert!(is_code_taken(&code_taken()) && !is_code_taken(&AppError::conflict("x")));
        assert!(LobbyActor::Player(UserId(1)).may_manage(Some(1)) && !LobbyActor::Player(UserId(1)).may_manage(Some(2)));
        assert!(LobbyActor::Manager(UserId(1)).may_manage(None) && LobbyActor::Server.may_manage(Some(2)));
        assert_eq!(LobbyActor::Server.user(), None);
        let service = LobbyService::new(LobbiesConfig::default());
        let mut metadata = BTreeMap::new();
        for n in 0..33 {
            metadata.insert(format!("k{n}"), "v".to_string());
        }
        assert!(service.check_metadata(&metadata).is_err());
        metadata.clear();
        metadata.insert("k".into(), "v".repeat(5000));
        assert!(service.check_metadata(&metadata).is_err());
        let row = LobbyRow {
            id: 5,
            code: "K7M2Q9XD".into(),
            visibility: PRIVATE.into(),
            state: OPEN.into(),
            host_id: Some(1),
            max_players: 4,
            chat_room: Some(9),
            created_at: 1,
        };
        let public = service.build(&row, 2, BTreeMap::new(), false, Vec::new());
        assert_eq!((public.code, public.chat_room, public.members, public.host), (None, None, 2, Some(UserId(1))));
        let member = service.build(&row, 2, BTreeMap::new(), true, Vec::new());
        assert_eq!((member.code_number, member.chat_room), (Some(590_122_524_587), Some(RoomId(9))));
    }
}
