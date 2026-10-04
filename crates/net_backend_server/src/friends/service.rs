//! [`FriendService`]: requests, friendships, blocks, friend codes and the online state.
//!
//! **Writes between two players** (a request, an acceptance, a decline, a cancel, a removal, a
//! block, an unblock) are one transaction: both account locks (the lower id first), the two rows
//! read, the limits counted, the rows changed by primary key. Two players acting on each other at
//! the same moment therefore run one after the other, and a limit can never be passed by racing
//! requests. A transaction the database still aborts as a deadlock is run again ([`Retry`]).
//!
//! **Online state:** this instance keeps the players it has a WebSocket for; a background task
//! moves their stored `online_until` forward every third of the online window, so other instances
//! see them online too. A heartbeat moves it forward once. A player is online while it has a
//! connection on this instance or while its `online_until` lies ahead. Each instance also keeps a
//! row per connected player in `friend_presence` (its random instance id): when a player's last
//! connection on one instance closes while another instance still holds one, the player stays
//! online and no `friends.presence` push goes out; the same holds for a connection opening while
//! another instance already holds one.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::StatusCode;
use net_backend_protocol::auth::provider;
use net_backend_protocol::friends::{
    normalize_friend_code, AddFriend, FriendCode, FriendEntry, FriendPresence, FriendSettings, FriendState, SteamPlayer, UpdateFriendSettings,
    FRIEND_CODE_ALPHABET, FRIEND_CODE_LEN,
};
use net_backend_protocol::{codes, Cursor, Page, PageRequest, UnixMillis, UserId, ValidationDetails};

use super::config::FriendsConfig;
use super::events::{AfterFriendChange, BeforeFriendRequest, BeforeSteamMatch, FriendChange};
use super::store::{
    self, CountRow, FriendOfRow, IdRow, LinkRow, NameRow, OtherRow, PairRow, ProfileRow, SettingsRow, SteamRow, SubjectRow, UserRow, BLOCKED, FRIEND, RECEIVED,
    SENT,
};
use crate::auth::audit::{self, AuditRecord};
use crate::db::{DbTx, Retry};
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::rate_limit::{KeyedBuckets, RateDecision};
use crate::state::AppState;

/// Players whose stored online time one statement moves forward.
const SEEN_CHUNK: usize = 500;

/// Steam IDs (and found accounts) one lookup statement carries.
const STEAM_CHUNK: usize = 500;

/// Attempts at a friend code no other player has.
const CODE_ATTEMPTS: usize = 8;

/// A write between two players.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Add,
    Accept,
    Decline,
    Cancel,
    Remove,
    Block,
    Unblock,
}

/// What a write did: the caller's state afterwards (with since when), and the change if any.
struct Outcome {
    state: Option<(FriendState, i64)>,
    change: Option<FriendChange>,
}

impl Outcome {
    fn unchanged(state: Option<(FriendState, i64)>) -> Self {
        Self { state, change: None }
    }
}

struct Inner {
    config: FriendsConfig,
    rate: Option<KeyedBuckets<UserId>>,
    steam_rate: Option<KeyedBuckets<UserId>>,
    /// Heartbeats, code resets and settings changes per player (`update_rate`).
    update_rate: Option<KeyedBuckets<UserId>>,
    /// The players with a WebSocket connection on this instance.
    local: Mutex<HashSet<UserId>>,
    /// This instance's id in `friend_presence` (random per process).
    instance: i64,
    /// One lock per player whose presence is being decided on this instance: the connect,
    /// disconnect and heartbeat paths of one player run one at a time, so its friends get the
    /// `friends.presence` pushes in the order of the decisions ("online" never after "offline").
    presence_locks: Mutex<HashMap<UserId, Arc<tokio::sync::Mutex<()>>>>,
    /// The players whose connections on this instance changed since the presence task last
    /// looked (see `FriendService::changed`).
    pending: Mutex<HashSet<UserId>>,
    /// Wakes the presence task.
    wake: tokio::sync::Notify,
}

/// A player's turn at its presence (see `Inner::presence_locks`); the map entry goes with the last
/// turn.
struct PresenceTurn<'a> {
    locks: &'a Mutex<HashMap<UserId, Arc<tokio::sync::Mutex<()>>>>,
    user: UserId,
    lock: Arc<tokio::sync::Mutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for PresenceTurn<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut locks = self.locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Only the map and this turn hold it: nobody waits.
        if Arc::strong_count(&self.lock) == 2 {
            locks.remove(&self.user);
        }
    }
}

/// A random instance id (positive); the clock when the system random generator fails.
fn instance_id() -> i64 {
    let mut bytes = [0u8; 8];
    let id = match getrandom::fill(&mut bytes) {
        Ok(()) => i64::from_le_bytes(bytes),
        Err(_) => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as i64),
    };
    (id & i64::MAX).max(1)
}

/// Friend requests, friendships, blocks, friend codes and the online state. A state value
/// (`Ext<FriendService>` in handlers, `state.get::<FriendService>()` elsewhere) once the
/// [`Friends`](super::Friends) module is registered. The players' own actions come through the
/// routes; server code reads relations with [`state_of`](Self::state_of),
/// [`are_friends`](Self::are_friends), [`is_blocked`](Self::is_blocked),
/// [`friends_of`](Self::friends_of) and [`is_online`](Self::is_online), and may act for a player
/// with the same methods the routes use.
///
/// ```no_run
/// use net_backend_server::friends::FriendService;
/// use net_backend_server::protocol::UserId;
/// use net_backend_server::{AppError, AppState};
///
/// // A game rule in a chat hook: no direct messages from a player the peer blocked.
/// async fn may_message(state: &AppState, from: UserId, to: UserId) -> Result<bool, AppError> {
///     match state.get::<FriendService>() {
///         Some(friends) => Ok(!friends.is_blocked(state, to, from).await?),
///         None => Ok(true),
///     }
/// }
/// ```
#[derive(Clone)]
pub struct FriendService(Arc<Inner>);

impl std::fmt::Debug for FriendService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FriendService").field(&self.0.config).finish()
    }
}

fn state_name(state: FriendState) -> &'static str {
    match state {
        FriendState::Friend => FRIEND,
        FriendState::Sent => SENT,
        FriendState::Received => RECEIVED,
        _ => BLOCKED,
    }
}

fn state_of_row(state: &str) -> FriendState {
    match state {
        FRIEND => FriendState::Friend,
        SENT => FriendState::Sent,
        RECEIVED => FriendState::Received,
        BLOCKED => FriendState::Blocked,
        _ => FriendState::Unknown,
    }
}

fn quota(message: &str) -> AppError {
    AppError::new(codes::QUOTA_EXCEEDED, message)
}

fn not_yourself() -> AppError {
    let mut details = ValidationDetails::new();
    details.add("user", "cannot be the caller");
    AppError::validation(details)
}

/// A new random friend code (each byte modulo 32: no bias, 256 is a multiple of 32).
fn new_code() -> Result<String, AppError> {
    let mut bytes = [0u8; FRIEND_CODE_LEN];
    getrandom::fill(&mut bytes).map_err(|e| AppError::internal(std::io::Error::other(format!("the system random generator failed: {e}"))))?;
    Ok(bytes.iter().map(|b| char::from(FRIEND_CODE_ALPHABET[usize::from(*b) % FRIEND_CODE_ALPHABET.len()])).collect())
}

async fn count(tx: &mut DbTx, user: i64, state: &str) -> Result<u64, AppError> {
    Ok(u64::try_from(tx.fetch_one::<CountRow, _>(&store::count(user, state)).await?.n).unwrap_or(0))
}

impl FriendService {
    pub(crate) fn new(config: FriendsConfig) -> Self {
        let rate =
            (config.request_rate > 0).then(|| KeyedBuckets::new(config.request_rate, Duration::from_secs(u64::from(config.request_rate_window_secs)), 100_000));
        let steam_rate =
            (config.steam_rate > 0).then(|| KeyedBuckets::new(config.steam_rate, Duration::from_secs(u64::from(config.steam_rate_window_secs)), 100_000));
        let update_rate =
            (config.update_rate > 0).then(|| KeyedBuckets::new(config.update_rate, Duration::from_secs(u64::from(config.update_rate_window_secs)), 100_000));
        Self(Arc::new(Inner {
            config,
            rate,
            steam_rate,
            update_rate,
            local: Mutex::new(HashSet::new()),
            instance: instance_id(),
            presence_locks: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
            wake: tokio::sync::Notify::new(),
        }))
    }

    /// The settings.
    pub fn config(&self) -> &FriendsConfig {
        &self.0.config
    }

    /// Wait for `user`'s turn at its presence on this instance.
    async fn presence_turn(&self, user: UserId) -> PresenceTurn<'_> {
        let locks = &self.0.presence_locks;
        let lock = Arc::clone(locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).entry(user).or_default());
        let guard = Arc::clone(&lock).lock_owned().await;
        PresenceTurn { locks, user, lock, guard: Some(guard) }
    }

    fn local(&self) -> std::sync::MutexGuard<'_, HashSet<UserId>> {
        self.0.local.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Count one friend request of `user` against `request_rate` (429 `rate_limited` over it).
    pub(crate) fn check_rate(&self, user: UserId) -> Result<(), AppError> {
        match self.0.rate.as_ref().map(|rate| rate.check(user)) {
            Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
            _ => Ok(()),
        }
    }

    // ---- requests, friendships, blocks ----------------------------------------------------------

    /// The account an [`AddFriend`] names: 404 when none, 409 when several accounts have that
    /// display name.
    async fn resolve(&self, state: &AppState, add: &AddFriend) -> Result<UserId, AppError> {
        if let Some(user) = add.user {
            return Ok(user);
        }
        if let Some(name) = &add.name {
            let rows = state.db().fetch_all::<IdRow, _>(&store::by_name(name.trim())).await?;
            return match rows.as_slice() {
                [] => Err(AppError::not_found("no player has this name")),
                [one] => Ok(UserId(one.id)),
                _ => Err(AppError::conflict("several players have this name: use the friend code")),
            };
        }
        let code = add.code.as_deref().and_then(normalize_friend_code).ok_or_else(|| AppError::bad_request("not a friend code"))?;
        match state.db().fetch_optional::<UserRow, _>(&store::by_code(&code)).await? {
            Some(row) => Ok(UserId(row.user_id)),
            None => Err(AppError::not_found("no player has this friend code")),
        }
    }

    /// Send `user`'s friend request (by id, name or code; the rate is the route's): the
    /// [`BeforeFriendRequest`] hooks, then the request is stored (or, when the other player had
    /// asked first, the two become friends). Answers the caller's entry.
    pub async fn add(&self, state: &AppState, ctx: &HookCtx, user: UserId, add: &AddFriend) -> Result<FriendEntry, AppError> {
        add.validate()?;
        let other = self.resolve(state, add).await?;
        if other == user {
            return Err(not_yourself());
        }
        state.hooks().run_before(ctx, BeforeFriendRequest { from: user, to: other }).await?;
        let outcome = self.write(state, Op::Add, user, other).await?;
        self.after(state, ctx, user, other, outcome.change).await;
        self.entry(state, other, outcome.state).await
    }

    /// Accept the request `user` received from `other` (404 when there is none; already friends:
    /// the entry again).
    pub async fn accept(&self, state: &AppState, ctx: &HookCtx, user: UserId, other: UserId) -> Result<FriendEntry, AppError> {
        let outcome = self.write(state, Op::Accept, user, other).await?;
        self.after(state, ctx, user, other, outcome.change).await;
        self.entry(state, other, outcome.state).await
    }

    /// Decline the request `user` received from `other`; `true` if there was one.
    pub async fn decline(&self, state: &AppState, ctx: &HookCtx, user: UserId, other: UserId) -> Result<bool, AppError> {
        self.simple(state, ctx, Op::Decline, user, other).await
    }

    /// Withdraw the request `user` sent to `other`; `true` if there was one.
    pub async fn cancel(&self, state: &AppState, ctx: &HookCtx, user: UserId, other: UserId) -> Result<bool, AppError> {
        self.simple(state, ctx, Op::Cancel, user, other).await
    }

    /// End the friendship of `user` and `other`; `true` if there was one.
    pub async fn remove(&self, state: &AppState, ctx: &HookCtx, user: UserId, other: UserId) -> Result<bool, AppError> {
        self.simple(state, ctx, Op::Remove, user, other).await
    }

    /// `user` blocks `other` (ends a friendship and the open requests between them); `true` if it
    /// was not blocked before.
    pub async fn block(&self, state: &AppState, ctx: &HookCtx, user: UserId, other: UserId) -> Result<bool, AppError> {
        self.simple(state, ctx, Op::Block, user, other).await
    }

    /// `user` lifts its block of `other`; `true` if there was one.
    pub async fn unblock(&self, state: &AppState, ctx: &HookCtx, user: UserId, other: UserId) -> Result<bool, AppError> {
        self.simple(state, ctx, Op::Unblock, user, other).await
    }

    async fn simple(&self, state: &AppState, ctx: &HookCtx, op: Op, user: UserId, other: UserId) -> Result<bool, AppError> {
        let outcome = self.write(state, op, user, other).await?;
        self.after(state, ctx, user, other, outcome.change).await;
        Ok(outcome.change.is_some())
    }

    /// One write between two players, in one transaction (run again on a reported deadlock).
    async fn write(&self, state: &AppState, op: Op, user: UserId, other: UserId) -> Result<Outcome, AppError> {
        if user == other {
            return Err(not_yourself());
        }
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        loop {
            let mut tx = state.db().begin_write().await?;
            let now = state.now().get();
            let result = self.write_in(&mut tx, op, user.get(), other.get(), now).await;
            match tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => return other,
            }
        }
    }

    async fn write_in(&self, tx: &mut DbTx, op: Op, user: i64, other: i64, now: i64) -> Result<Outcome, AppError> {
        let config = &self.0.config;
        let dialect = tx.dialect();
        for id in [user.min(other), user.max(other)] {
            if tx.fetch_optional::<IdRow, _>(&store::lock_user(id, dialect)).await?.is_none() {
                return Err(AppError::not_found("no such account"));
            }
        }
        let mine = tx.fetch_optional::<LinkRow, _>(&store::link(user, other)).await?;
        let theirs = tx.fetch_optional::<LinkRow, _>(&store::link(other, user)).await?;
        let mine_state = mine.as_ref().map(|r| r.state.as_str());
        let theirs_state = theirs.as_ref().map(|r| r.state.as_str());
        let kept = |row: &Option<LinkRow>| row.as_ref().map(|r| (state_of_row(&r.state), r.updated_at));
        match op {
            Op::Add => {
                if theirs_state == Some(BLOCKED) {
                    return Err(AppError::forbidden("this player does not take friend requests from you"));
                }
                match mine_state {
                    Some(BLOCKED) => Err(AppError::conflict("you blocked this player: unblock them first")),
                    Some(FRIEND) | Some(SENT) => Ok(Outcome::unchanged(kept(&mine))),
                    Some(RECEIVED) => self.befriend(tx, user, other, mine, theirs, now).await,
                    _ => {
                        if count(tx, user, SENT).await? >= u64::from(config.max_pending) {
                            return Err(quota("you have too many open friend requests"));
                        }
                        if count(tx, other, RECEIVED).await? >= u64::from(config.max_pending) {
                            return Err(quota("this player has too many open friend requests"));
                        }
                        tx.execute(&store::insert_link(user, other, SENT, now)?).await?;
                        tx.execute(&store::insert_link(other, user, RECEIVED, now)?).await?;
                        Ok(Outcome { state: Some((FriendState::Sent, now)), change: Some(FriendChange::Requested) })
                    }
                }
            }
            Op::Accept => match mine_state {
                Some(RECEIVED) => self.befriend(tx, user, other, mine, theirs, now).await,
                Some(FRIEND) => Ok(Outcome::unchanged(kept(&mine))),
                _ => Err(AppError::not_found("no friend request from this player")),
            },
            Op::Decline | Op::Cancel | Op::Remove => {
                let (own, their, change) = match op {
                    Op::Decline => (RECEIVED, SENT, FriendChange::Declined),
                    Op::Cancel => (SENT, RECEIVED, FriendChange::Cancelled),
                    _ => (FRIEND, FRIEND, FriendChange::Removed),
                };
                let Some(row) = mine.filter(|r| r.state == own) else { return Ok(Outcome::unchanged(None)) };
                tx.execute(&store::delete_link(row.id)).await?;
                if let Some(row) = theirs.filter(|r| r.state == their) {
                    tx.execute(&store::delete_link(row.id)).await?;
                }
                Ok(Outcome { state: None, change: Some(change) })
            }
            Op::Block => {
                if mine_state == Some(BLOCKED) {
                    return Ok(Outcome::unchanged(kept(&mine)));
                }
                if count(tx, user, BLOCKED).await? >= u64::from(config.max_blocks) {
                    return Err(quota("you have blocked too many players"));
                }
                match &mine {
                    Some(row) => tx.execute(&store::set_state(row.id, BLOCKED, now)).await?,
                    None => tx.execute(&store::insert_link(user, other, BLOCKED, now)?).await?,
                };
                if let Some(row) = theirs.filter(|r| r.state != BLOCKED) {
                    tx.execute(&store::delete_link(row.id)).await?;
                }
                Ok(Outcome { state: Some((FriendState::Blocked, now)), change: Some(FriendChange::Blocked) })
            }
            Op::Unblock => {
                let Some(row) = mine.filter(|r| r.state == BLOCKED) else { return Ok(Outcome::unchanged(None)) };
                tx.execute(&store::delete_link(row.id)).await?;
                Ok(Outcome { state: None, change: Some(FriendChange::Unblocked) })
            }
        }
    }

    /// `user` accepts `other`'s request: both rows become `friend` (within both players' limits).
    async fn befriend(&self, tx: &mut DbTx, user: i64, other: i64, mine: Option<LinkRow>, theirs: Option<LinkRow>, now: i64) -> Result<Outcome, AppError> {
        let max = u64::from(self.0.config.max_friends);
        if count(tx, user, FRIEND).await? >= max {
            return Err(quota("you have reached the friend limit"));
        }
        if count(tx, other, FRIEND).await? >= max {
            return Err(quota("this player has reached the friend limit"));
        }
        match mine {
            Some(row) => tx.execute(&store::set_state(row.id, FRIEND, now)).await?,
            None => tx.execute(&store::insert_link(user, other, FRIEND, now)?).await?,
        };
        match theirs {
            Some(row) => tx.execute(&store::set_state(row.id, FRIEND, now)).await?,
            None => tx.execute(&store::insert_link(other, user, FRIEND, now)?).await?,
        };
        Ok(Outcome { state: Some((FriendState::Friend, now)), change: Some(FriendChange::Accepted) })
    }

    /// After a stored change: the notification (if any) and the [`AfterFriendChange`] hooks.
    async fn after(&self, state: &AppState, ctx: &HookCtx, user: UserId, other: UserId, change: Option<FriendChange>) {
        let Some(change) = change else { return };
        match change {
            FriendChange::Requested => self.notify(state, ctx, other, "friends.request", user).await,
            FriendChange::Accepted => self.notify(state, ctx, other, "friends.accepted", user).await,
            _ => {}
        }
        state.hooks().run_after(ctx, Arc::new(AfterFriendChange { user, other, change })).await;
    }

    #[cfg(feature = "notifications")]
    async fn notify(&self, state: &AppState, ctx: &HookCtx, to: UserId, kind: &str, from: UserId) {
        use crate::notifications::{NewNotification, NotificationService};
        if !self.0.config.notify {
            return;
        }
        let Some(notifications) = state.get::<NotificationService>() else { return };
        if let Err(error) = notifications.send_with(state, ctx, to, NewNotification::new(kind).with_sender(from)).await {
            if error.status().is_server_error() {
                tracing::warn!(%error, kind, "friends: the notification could not be sent (the change is stored)");
            } else {
                tracing::debug!(%error, kind, "friends: the notification was refused (the change is stored)");
            }
        }
    }

    #[cfg(not(feature = "notifications"))]
    async fn notify(&self, _state: &AppState, _ctx: &HookCtx, _to: UserId, _kind: &str, _from: UserId) {}

    /// The caller's entry about `other` after a write.
    async fn entry(&self, state: &AppState, other: UserId, after: Option<(FriendState, i64)>) -> Result<FriendEntry, AppError> {
        let Some((friend_state, since)) = after else { return Err(AppError::not_found("no such relation")) };
        let mut entries = self.entries(state, vec![(other.get(), friend_state, since)]).await?;
        entries.pop().ok_or_else(|| AppError::internal(std::io::Error::other("an entry went missing")))
    }

    /// Entries with names (and, for friends, the online state).
    async fn entries(&self, state: &AppState, rows: Vec<(i64, FriendState, i64)>) -> Result<Vec<FriendEntry>, AppError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let names: HashMap<i64, String> =
            state.db().fetch_all::<NameRow, _>(&store::names(&ids)).await?.into_iter().filter_map(|r| r.display_name.map(|n| (r.id, n))).collect();
        let friends: Vec<i64> = rows.iter().filter(|r| r.1 == FriendState::Friend).map(|r| r.0).collect();
        let profiles: HashMap<i64, ProfileRow> = if friends.is_empty() {
            HashMap::new()
        } else {
            state.db().fetch_all::<ProfileRow, _>(&store::profiles(&friends)).await?.into_iter().map(|p| (p.user_id, p)).collect()
        };
        let now = state.now().get();
        Ok(rows
            .into_iter()
            .map(|(id, friend_state, since)| {
                let mut entry = FriendEntry::new(UserId(id), friend_state, UnixMillis(since));
                if let Some(name) = names.get(&id) {
                    entry = entry.with_name(name.clone());
                }
                if friend_state == FriendState::Friend {
                    let profile = profiles.get(&id);
                    let online = state.ws().is_online(UserId(id)) || profile.and_then(|p| p.online_until).is_some_and(|until| until > now);
                    entry = entry.with_online(online, profile.and_then(|p| p.last_seen_at).map(UnixMillis));
                }
                entry
            })
            .collect())
    }

    /// A page of `user`'s friends, requests (`Sent` / `Received`) or blocks, newest first.
    pub async fn list(&self, state: &AppState, user: UserId, which: FriendState, page: &PageRequest) -> Result<Page<FriendEntry>, AppError> {
        if which == FriendState::Unknown {
            return Err(AppError::bad_request("unknown list"));
        }
        page.validate()?;
        let limit = u64::from(page.limit_or_default());
        let before = match &page.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let mut rows = state.db().fetch_all::<LinkRow, _>(&store::page(user.get(), state_name(which), before, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        let entries = self.entries(state, rows.into_iter().map(|r| (r.other_id, state_of_row(&r.state), r.updated_at)).collect()).await?;
        Ok(Page::new(entries, next))
    }

    // ---- reading relations (server code) --------------------------------------------------------

    /// How `user` relates to `other` (`None`: no relation).
    pub async fn state_of(&self, state: &AppState, user: UserId, other: UserId) -> Result<Option<FriendState>, AppError> {
        Ok(state.db().fetch_optional::<LinkRow, _>(&store::link(user.get(), other.get())).await?.map(|r| state_of_row(&r.state)))
    }

    /// Whether `a` and `b` are friends.
    pub async fn are_friends(&self, state: &AppState, a: UserId, b: UserId) -> Result<bool, AppError> {
        Ok(self.state_of(state, a, b).await? == Some(FriendState::Friend))
    }

    /// Whether `by` blocked `user`.
    pub async fn is_blocked(&self, state: &AppState, by: UserId, user: UserId) -> Result<bool, AppError> {
        Ok(self.state_of(state, by, user).await? == Some(FriendState::Blocked))
    }

    /// `user`'s friends (at most `max_friends`).
    pub async fn friends_of(&self, state: &AppState, user: UserId) -> Result<Vec<UserId>, AppError> {
        let limit = u64::from(self.0.config.max_friends);
        Ok(state.db().fetch_all::<OtherRow, _>(&store::friend_ids(user.get(), limit)).await?.into_iter().map(|r| UserId(r.other_id)).collect())
    }

    /// Whether `user` is online: a WebSocket connection on this instance, or a stored online time
    /// ahead (another instance's connection, or a heartbeat).
    pub async fn is_online(&self, state: &AppState, user: UserId) -> Result<bool, AppError> {
        if state.ws().is_online(user) {
            return Ok(true);
        }
        let now = state.now().get();
        Ok(state.db().fetch_optional::<ProfileRow, _>(&store::profile(user.get())).await?.and_then(|p| p.online_until).is_some_and(|until| until > now))
    }

    // ---- friend codes ---------------------------------------------------------------------------

    /// `user`'s profile row, made (with a new friend code) on first use.
    async fn profile(&self, state: &AppState, user: UserId) -> Result<ProfileRow, AppError> {
        let db = state.db();
        if let Some(profile) = db.fetch_optional::<ProfileRow, _>(&store::profile(user.get())).await? {
            return Ok(profile);
        }
        for _ in 0..CODE_ATTEMPTS {
            match db.execute(&store::insert_profile(user.get(), &new_code()?, state.now().get())?).await {
                Ok(_) => {}
                Err(error) if error.is_foreign_key_violation() => return Err(AppError::not_found("no such account")),
                // The code is taken, or another request made the profile at the same moment.
                Err(error) if error.is_unique_violation() => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(profile) = db.fetch_optional::<ProfileRow, _>(&store::profile(user.get())).await? {
                return Ok(profile);
            }
        }
        Err(AppError::internal(std::io::Error::other("no free friend code was found")))
    }

    /// `user`'s friend code (made on first use).
    pub async fn code(&self, state: &AppState, user: UserId) -> Result<FriendCode, AppError> {
        Ok(FriendCode::new(self.profile(state, user).await?.code))
    }

    /// A new friend code for `user`; the old one stops working.
    pub async fn reset_code(&self, state: &AppState, user: UserId) -> Result<FriendCode, AppError> {
        self.profile(state, user).await?;
        for _ in 0..CODE_ATTEMPTS {
            let code = new_code()?;
            match state.db().execute(&store::set_code(user.get(), &code)).await {
                Ok(_) => return Ok(FriendCode::new(code)),
                Err(error) if error.is_unique_violation() => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(AppError::internal(std::io::Error::other("no free friend code was found")))
    }

    // ---- Steam IDs and settings -----------------------------------------------------------------

    /// Count one heartbeat, code reset or settings change of `user` against `update_rate` (429
    /// `rate_limited` over it).
    pub(crate) fn check_update_rate(&self, user: UserId) -> Result<(), AppError> {
        match self.0.update_rate.as_ref().map(|rate| rate.check(user)) {
            Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
            _ => Ok(()),
        }
    }

    /// Count one Steam ID lookup of `user` against `steam_rate` (429 `rate_limited` over it).
    pub(crate) fn check_steam_rate(&self, user: UserId) -> Result<(), AppError> {
        match self.0.steam_rate.as_ref().map(|rate| rate.check(user)) {
            Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
            _ => Ok(()),
        }
    }

    /// The SteamID64 of `user`'s linked Steam account, if any.
    pub async fn steam_id_of(&self, state: &AppState, user: UserId) -> Result<Option<u64>, AppError> {
        let row = state.db().fetch_optional::<SubjectRow, _>(&store::own_steam(user.get(), provider::STEAM)).await?;
        Ok(row.and_then(|r| r.subject.parse().ok()))
    }

    /// Which of `steam_ids` belong to accounts here, for `user` (the route's checks: Steam login
    /// on, the shape and the cap, the rate): `user` must have a Steam account linked (403
    /// `forbidden` otherwise); the [`BeforeSteamMatch`] hooks may refuse or remove Steam IDs. Found
    /// are accounts with that Steam account linked, other than `user`, not banned, not hidden
    /// ([`set_steam_findable`](Self::set_steam_findable)), and without a block between them and
    /// `user` in either direction. Each with its display name and `user`'s relation to it
    /// (`friend`, `sent`, `received`), in the order of `steam_ids`.
    pub async fn steam_match(&self, state: &AppState, ctx: &HookCtx, user: UserId, steam_ids: Vec<u64>) -> Result<Vec<SteamPlayer>, AppError> {
        let Some(own) = self.steam_id_of(state, user).await? else {
            return Err(AppError::forbidden("link a Steam account to this account first"));
        };
        let asked = steam_ids.len();
        let wanted: HashSet<u64> = steam_ids.iter().copied().collect();
        let event = state.hooks().run_before(ctx, BeforeSteamMatch { user, own_steam_id: own, steam_ids }).await?;
        // Only removals count: a hook cannot widen the lookup.
        let mut seen = HashSet::new();
        let ids: Vec<u64> = event.steam_ids.into_iter().filter(|id| wanted.contains(id) && seen.insert(*id)).collect();
        let players = self.find_steam(state, user, &ids).await?;
        let record = AuditRecord::new("friends.steam_match")
            .actor(Some(user))
            .request_id(ctx.request_id())
            .data(serde_json::json!({ "asked": asked, "found": players.len() }));
        audit::record_logged(state.db(), UnixMillis(state.now().get()), &record).await;
        Ok(players)
    }

    async fn find_steam(&self, state: &AppState, user: UserId, ids: &[u64]) -> Result<Vec<SteamPlayer>, AppError> {
        let now = state.now().get();
        let mut found: HashMap<String, (i64, Option<String>)> = HashMap::new();
        for chunk in ids.chunks(STEAM_CHUNK) {
            let subjects: Vec<String> = chunk.iter().map(u64::to_string).collect();
            for row in state.db().fetch_all::<SteamRow, _>(&store::steam_accounts(provider::STEAM, &subjects, user.get(), now)).await? {
                found.insert(row.subject, (row.user_id, row.display_name));
            }
        }
        // The caller's relations to them, and their blocks of the caller.
        let accounts: Vec<i64> = found.values().map(|(id, _)| *id).collect();
        let mut relation: HashMap<i64, FriendState> = HashMap::new();
        let mut blocked: HashSet<i64> = HashSet::new();
        for chunk in accounts.chunks(STEAM_CHUNK) {
            for row in state.db().fetch_all::<PairRow, _>(&store::links_between(user.get(), chunk)).await? {
                let (other, mine) = if row.user_id == user.get() { (row.other_id, true) } else { (row.user_id, false) };
                if row.state == BLOCKED {
                    blocked.insert(other);
                } else if mine {
                    relation.insert(other, state_of_row(&row.state));
                }
            }
        }
        Ok(ids
            .iter()
            .filter_map(|id| {
                let steam_id = id.to_string();
                let (account, name) = found.get(&steam_id)?;
                if blocked.contains(account) {
                    return None;
                }
                let mut player = SteamPlayer::new(steam_id.clone(), UserId(*account));
                if let Some(name) = name {
                    player = player.with_name(name.clone());
                }
                if let Some(state) = relation.get(account).copied().filter(|s| matches!(s, FriendState::Friend | FriendState::Sent | FriendState::Received)) {
                    player = player.with_state(state);
                }
                Some(player)
            })
            .collect())
    }

    /// `user`'s friends settings (the defaults when it never changed one).
    pub async fn settings(&self, state: &AppState, user: UserId) -> Result<FriendSettings, AppError> {
        let row = state.db().fetch_optional::<SettingsRow, _>(&store::settings(user.get())).await?;
        Ok(FriendSettings::new(row.is_none_or(|r| r.steam_hidden_at.is_none())))
    }

    /// Change `user`'s friends settings (fields left out keep their value); answers all of them.
    pub async fn update_settings(&self, state: &AppState, user: UserId, update: &UpdateFriendSettings) -> Result<FriendSettings, AppError> {
        if let Some(findable) = update.steam_findable {
            self.set_steam_findable(state, user, findable).await?;
        }
        self.settings(state, user).await
    }

    /// Whether other players find `user` by its linked Steam account (on by default). Off: it is
    /// left out of every Steam ID lookup, exactly as if it had no account here.
    pub async fn set_steam_findable(&self, state: &AppState, user: UserId, findable: bool) -> Result<(), AppError> {
        let db = state.db();
        let now = state.now().get();
        let hidden_at = (!findable).then_some(now);
        if db.execute(&store::update_settings(user.get(), hidden_at, now)).await? > 0 {
            return Ok(());
        }
        match db.execute(&store::insert_settings(user.get(), hidden_at, now)?).await {
            Ok(_) => Ok(()),
            Err(error) if error.is_foreign_key_violation() => Err(AppError::not_found("no such account")),
            // The row exists (MySQL counts an update without a change as 0 rows, or another request
            // made it at the same moment): set it.
            Err(error) if error.is_unique_violation() => {
                db.execute(&store::update_settings(user.get(), hidden_at, now)).await?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    // ---- online state ---------------------------------------------------------------------------

    /// `user` is online for the online window (a heartbeat); its friends get `friends.presence`
    /// when it was offline.
    pub async fn heartbeat(&self, state: &AppState, user: UserId) -> Result<(), AppError> {
        let _turn = self.presence_turn(user).await;
        let profile = self.profile(state, user).await?;
        let now = state.now().get();
        let was_online = state.ws().is_online(user) || profile.online_until.is_some_and(|until| until > now);
        state.db().execute(&store::seen(&[user.get()], now + self.0.config.window_ms(), now)).await?;
        if !was_online {
            self.push_presence(state, user, FriendPresence::new(user, true)).await;
        }
        Ok(())
    }

    async fn push_presence(&self, state: &AppState, user: UserId, push: FriendPresence) {
        if !self.0.config.presence || !state.config().ws.enabled {
            return;
        }
        let friends = match self.friends_of(state, user).await {
            Ok(friends) => friends,
            Err(error) => {
                tracing::warn!(%error, "friends: reading the friends for a presence push failed");
                return;
            }
        };
        for friend in friends {
            if let Err(error) = state.ws().push_user(friend, &push) {
                tracing::warn!(%error, "friends: a friends.presence push failed");
            }
        }
    }

    /// Store that this instance holds a connection of `user` until `until`.
    async fn hold(&self, state: &AppState, user: UserId, until: i64) -> Result<(), AppError> {
        let (db, me) = (state.db(), self.0.instance);
        // MySQL counts an update without a change as 0 rows: then the insert's unique violation
        // says the row exists.
        if db.execute(&store::touch_presence(user.get(), me, until)).await? > 0 {
            return Ok(());
        }
        match db.execute(&store::insert_presence(user.get(), me, until)?).await {
            Ok(_) => Ok(()),
            Err(error) if error.is_unique_violation() => Ok(()),
            Err(error) if error.is_foreign_key_violation() => Err(AppError::not_found("no such account")),
            Err(error) => Err(error.into()),
        }
    }

    /// A WebSocket connection of `user` opened or closed on this instance (the module's hooks).
    /// No database work here: the module's presence task settles the player's online state right
    /// after ([`settle`](Self::settle)), together with every other player whose connections
    /// changed meanwhile.
    pub(crate) fn changed(&self, user: UserId) {
        self.pending().insert(user);
        self.0.wake.notify_one();
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashSet<UserId>> {
        self.0.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The module's presence task: settles the players whose connections changed (in batches),
    /// and moves the stored online time of the connected players forward every `every`
    /// ([`refresh`](Self::refresh)); ends when `stop` is notified or the server shuts down.
    pub(crate) async fn run_presence(&self, state: AppState, every: Duration, stop: Arc<tokio::sync::Notify>) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // The work runs after the `select!`, so a stop never cuts it off half-way.
            let refresh = tokio::select! {
                _ = state.shutdown().wait() => break,
                _ = stop.notified() => break,
                _ = self.0.wake.notified() => false,
                _ = tick.tick() => true,
            };
            self.settle_pending(&state).await;
            if refresh {
                if let Err(error) = self.refresh(&state).await {
                    tracing::warn!(%error, "friends: refreshing the online state failed");
                }
            }
        }
    }

    /// Settle every player whose connections changed since the last look.
    async fn settle_pending(&self, state: &AppState) {
        loop {
            let batch: Vec<UserId> = {
                let mut pending = self.pending();
                let batch: Vec<UserId> = pending.iter().take(SEEN_CHUNK).copied().collect();
                for user in &batch {
                    pending.remove(user);
                }
                batch
            };
            if batch.is_empty() {
                return;
            }
            if let Err(error) = self.settle(state, &batch, false).await {
                tracing::warn!(%error, "friends: storing the online state failed");
            }
        }
    }

    /// Bring the stored online state of `users` in line with their WebSocket connections on this
    /// instance (`stopping`: as if they had none). A player with a connection that this instance
    /// did not hold yet comes online: online time and presence row stored, its friends told when
    /// no other instance holds it. A player held here without a connection left goes offline: its
    /// row removed and, when no other instance holds it, the offline state stored and its friends
    /// told. Every other player stays as it is (a second connection, or one that opened and
    /// closed before it was looked at, costs nothing). Each statement carries the whole batch, so
    /// their number grows neither with the players nor with their friends. Each player's turn is
    /// held throughout, so its pushes go out in the order of the decisions.
    async fn settle(&self, state: &AppState, users: &[UserId], stopping: bool) -> Result<(), AppError> {
        let mut users = users.to_vec();
        users.sort_unstable();
        users.dedup();
        let mut result = Ok(());
        for chunk in users.chunks(SEEN_CHUNK) {
            // In id order: two batches never wait for each other's turns crosswise.
            let mut turns = Vec::with_capacity(chunk.len());
            for user in chunk {
                turns.push(self.presence_turn(*user).await);
            }
            let (mut up, mut down) = (Vec::new(), Vec::new());
            {
                let local = self.local();
                for user in chunk {
                    match (local.contains(user), !stopping && state.ws().is_online(*user)) {
                        (false, true) => up.push(*user),
                        (true, false) => down.push(*user),
                        _ => {}
                    }
                }
            }
            if !up.is_empty() {
                if let Err(error) = self.came_online(state, &up).await {
                    result = result.and(Err(error));
                }
            }
            if !down.is_empty() {
                if let Err(error) = self.went_offline(state, &down).await {
                    result = result.and(Err(error));
                }
            }
            drop(turns);
        }
        result
    }

    /// Whether `friends.presence` pushes go out at all.
    fn pushes(&self, state: &AppState) -> bool {
        self.0.config.presence && state.config().ws.enabled
    }

    /// `users` got their first connection on this instance (see [`settle`](Self::settle)).
    async fn came_online(&self, state: &AppState, users: &[UserId]) -> Result<(), AppError> {
        // Held here from now on: the refresh repairs a write that fails below, and a stop in
        // between still stores them offline.
        self.local().extend(users.iter().copied());
        let db = state.db();
        let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
        let now = state.now().get();
        let until = now + self.0.config.window_ms();
        let stored = db.execute(&store::seen(&ids, until, now)).await?;
        // A player's first connection ever (no profile yet), or MySQL's unchanged rows.
        if usize::try_from(stored).unwrap_or(0) < ids.len() && self.make_profiles(state, &ids).await? {
            db.execute(&store::seen(&ids, until, now)).await?;
        }
        match db.execute(&store::insert_presences(&ids, self.0.instance, until)?).await {
            Ok(_) => {}
            // A row left by a removal that failed, or an account deleted meanwhile: one by one.
            Err(error) if error.is_unique_violation() || error.is_foreign_key_violation() => {
                for user in users {
                    match self.hold(state, *user, until).await {
                        Err(error) if error.status() != StatusCode::NOT_FOUND => return Err(error),
                        _ => {}
                    }
                }
            }
            Err(error) => return Err(error.into()),
        }
        if !self.pushes(state) {
            return Ok(());
        }
        let friends = self.friends_of_all(state, &ids).await?;
        if friends.is_empty() {
            return Ok(());
        }
        let with: Vec<i64> = friends.keys().copied().collect();
        let elsewhere = self.held_elsewhere(state, &with, now).await?;
        for (user, friends) in friends {
            if !elsewhere.contains(&user) {
                self.push_to(state, &friends, &FriendPresence::new(UserId(user), true));
            }
        }
        Ok(())
    }

    /// `users` have no connection left on this instance (see [`settle`](Self::settle)).
    async fn went_offline(&self, state: &AppState, users: &[UserId]) -> Result<(), AppError> {
        let db = state.db();
        let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
        let now = state.now().get();
        db.execute(&store::delete_presences(&ids, self.0.instance)).await?;
        let elsewhere = self.held_elsewhere(state, &ids, now).await?;
        let off: Vec<i64> = ids.iter().copied().filter(|id| !elsewhere.contains(id)).collect();
        if !off.is_empty() {
            db.execute(&store::seen(&off, now, now)).await?;
        }
        // Only now: when a write above fails, the players stay held here, and the next refresh
        // (or the stop) tries again.
        {
            let mut local = self.local();
            for user in users {
                local.remove(user);
            }
        }
        if off.is_empty() || !self.pushes(state) {
            return Ok(());
        }
        for (user, friends) in self.friends_of_all(state, &off).await? {
            self.push_to(state, &friends, &FriendPresence::new(UserId(user), false).with_last_seen(UnixMillis(now)));
        }
        Ok(())
    }

    /// Make the missing profiles of `users` (one statement; one by one when it fails: a code
    /// taken, a profile made at the same moment, an account deleted). Whether one was missing.
    async fn make_profiles(&self, state: &AppState, users: &[i64]) -> Result<bool, AppError> {
        let db = state.db();
        let have: HashSet<i64> = db.fetch_all::<ProfileRow, _>(&store::profiles(users)).await?.into_iter().map(|p| p.user_id).collect();
        let missing = users.iter().filter(|u| !have.contains(u)).map(|u| Ok((*u, new_code()?))).collect::<Result<Vec<(i64, String)>, AppError>>()?;
        if missing.is_empty() {
            return Ok(false);
        }
        if db.execute(&store::insert_profiles(&missing, state.now().get())?).await.is_err() {
            for (user, _) in &missing {
                match self.profile(state, UserId(*user)).await {
                    Err(error) if error.status() != StatusCode::NOT_FOUND => return Err(error),
                    _ => {}
                }
            }
        }
        Ok(true)
    }

    /// The friends of each of `users` that has any (one statement for up to `SEEN_CHUNK` players).
    async fn friends_of_all(&self, state: &AppState, users: &[i64]) -> Result<HashMap<i64, Vec<UserId>>, AppError> {
        // At most about 50 000 rows per statement.
        let per = (50_000 / self.0.config.max_friends.max(1) as usize).clamp(1, SEEN_CHUNK);
        let mut friends: HashMap<i64, Vec<UserId>> = HashMap::new();
        for chunk in users.chunks(per) {
            for row in state.db().fetch_all::<FriendOfRow, _>(&store::friends_of_many(chunk)).await? {
                friends.entry(row.user_id).or_default().push(UserId(row.other_id));
            }
        }
        Ok(friends)
    }

    /// Which of `users` another instance holds a connection of right now.
    async fn held_elsewhere(&self, state: &AppState, users: &[i64], now: i64) -> Result<HashSet<i64>, AppError> {
        let rows = state.db().fetch_all::<UserRow, _>(&store::held_elsewhere(users, self.0.instance, now)).await?;
        Ok(rows.into_iter().map(|r| r.user_id).collect())
    }

    fn push_to(&self, state: &AppState, friends: &[UserId], push: &FriendPresence) {
        for friend in friends {
            if let Err(error) = state.ws().push_user(*friend, push) {
                tracing::warn!(%error, "friends: a friends.presence push failed");
            }
        }
    }

    /// The server stops (its WebSockets are closed): every player this instance held goes
    /// offline (stored and pushed), as each socket's disconnect would have done.
    pub(crate) async fn stopping(&self, state: &AppState) {
        let users: Vec<UserId> = self.local().iter().copied().collect();
        if let Err(error) = self.settle(state, &users, true).await {
            tracing::warn!(%error, "friends: storing the offline state at shutdown failed");
        }
    }

    /// Move the stored online time of this instance's connected players forward; a player that is
    /// no longer connected goes offline. The module's background task runs it every third of the
    /// online window.
    pub async fn refresh(&self, state: &AppState) -> Result<(), AppError> {
        let users: Vec<UserId> = self.local().iter().copied().collect();
        let (live, gone): (Vec<UserId>, Vec<UserId>) = users.into_iter().partition(|u| state.ws().is_online(*u));
        if !gone.is_empty() {
            self.settle(state, &gone, false).await?;
        }
        let now = state.now().get();
        let until = now + self.0.config.window_ms();
        let ids: Vec<i64> = live.iter().map(|u| u.get()).collect();
        for chunk in ids.chunks(SEEN_CHUNK) {
            state.db().execute(&store::seen(chunk, until, now)).await?;
            let held = state.db().execute(&store::refresh_presence(chunk, self.0.instance, until)).await?;
            // A row went missing (purged while this instance stalled, or MySQL's unchanged rows):
            // store each again.
            if usize::try_from(held).unwrap_or(0) < chunk.len() {
                for user in chunk {
                    self.hold(state, UserId(*user), until).await?;
                }
            }
        }
        // Rows of instances that stopped without removing them.
        state.db().execute(&store::purge_presence(now - self.0.config.window_ms())).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_states() {
        for _ in 0..50 {
            let code = new_code().unwrap_or_default();
            assert_eq!(normalize_friend_code(&code).as_deref(), Some(code.as_str()));
        }
        for state in [FriendState::Friend, FriendState::Sent, FriendState::Received, FriendState::Blocked] {
            assert_eq!(state_of_row(state_name(state)), state);
        }
        assert_eq!(state_of_row("other"), FriendState::Unknown);
        assert_eq!(not_yourself().code(), codes::VALIDATION_FAILED);
        assert_eq!(quota("x").code(), codes::QUOTA_EXCEEDED);
    }
}
