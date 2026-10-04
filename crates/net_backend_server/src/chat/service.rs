//! [`ChatService`]: rooms, membership, sending, history, presence, direct messages, deletion.
//!
//! **Locking:** a public or group send is one INSERT (no shared row every sender would lock). A
//! DM send updates the room's `last_activity_at` FIRST and then inserts the message, in one
//! transaction: on MySQL the message's foreign-key check takes a shared lock on the room row, and
//! two senders that each held it and then asked for the exclusive lock of the UPDATE deadlocked;
//! with the UPDATE first, concurrent DM sends of one room simply queue. Writes the database still
//! aborts as a deadlock are run again ([`Retry`]).
//!
//! Player rooms live in `rooms.rs`, read markers and typing in `extras.rs` (more `impl
//! ChatService` blocks).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use net_backend_protocol::chat::{
    ChatMessage, EditMessage, MessageDeleted, MessageEdited, Presence as PresencePush, PresenceEvent, RoomInfo, RoomKind, RoomMember, RoomMembers, RoomRef,
    RoomVisibility, SendAck, SendMessage,
};
use net_backend_protocol::{codes, Cursor, MessageId, Page, PageRequest, RoomId, UnixMillis, UserId};
use serde_json::json;

use super::config::{is_valid_room_name, ChatConfig, RoomSpec};
use super::events::{AfterChatDelete, AfterChatEdit, AfterChatSend, BeforeChatJoin, BeforeChatSend, BeforeDirectOpen};
use super::extras::{ReadPushes, Typing};
use super::presence::Presence;
use super::store::{self, IdRow, MemberRow, MessageRow, NameRow, RoomRow, ACTIVE, KIND_DM, KIND_GROUP, KIND_PLAYER, KIND_ROOM, MODERATOR, OWNER};
use crate::auth::audit::{self, AuditRecord};
use crate::auth::AuthContext;
use crate::db::Retry;
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::rate_limit::{KeyedBuckets, RateDecision};
use crate::state::AppState;
use crate::ws::{ConnectionId, Hub};

/// Rooms kept in memory (the cache is cleared when full).
const ROOM_CACHE: usize = 10_000;

/// How long a cached room is trusted: a name or cap changed by another instance shows after this.
const ROOM_CACHE_TTL: Duration = Duration::from_secs(60);

/// Messages the retention purge deletes per statement.
const PURGE_BATCH: u64 = 1000;

/// The hub room of a chat room.
pub(crate) fn hub_room(room: RoomId) -> String {
    format!("chat:{room}")
}

/// The chat room of a hub room, if it is one.
pub(crate) fn chat_room(hub_room: &str) -> Option<RoomId> {
    hub_room.strip_prefix("chat:").and_then(|id| id.parse::<i64>().ok()).map(RoomId)
}

pub(super) fn kind_of(row: &RoomRow) -> RoomKind {
    match row.kind.as_str() {
        KIND_DM => RoomKind::Dm,
        KIND_GROUP => RoomKind::Group,
        KIND_PLAYER => RoomKind::Player,
        _ => RoomKind::Room,
    }
}

/// Rooms with stored members (group and player rooms).
pub(super) fn has_members(row: &RoomRow) -> bool {
    row.kind == KIND_GROUP || row.kind == KIND_PLAYER
}

pub(super) fn not_a_member() -> AppError {
    AppError::new(codes::NOT_A_MEMBER, "join the room first (or: not a member of this room)")
}

pub(super) fn page_limit(page: &PageRequest) -> Result<u64, AppError> {
    page.validate()?;
    Ok(u64::from(page.limit_or_default()))
}

fn check_room_name(name: Option<&str>) -> Result<(), AppError> {
    match name {
        Some(name) if !is_valid_room_name(name) => Err(AppError::bad_request("a room name must be 1-64 characters without control or invisible characters")),
        _ => Ok(()),
    }
}

/// Rooms, membership, messages and presence. A state value (`Ext<ChatService>` in handlers,
/// `state.get::<ChatService>()` elsewhere) once the [`Chat`](super::Chat) module is registered.
/// Server code creates public and group rooms, manages group members, sends (system) messages and
/// deletes messages through it.
#[derive(Clone)]
pub struct ChatService(pub(super) Arc<Inner>);

pub(super) struct Inner {
    pub(super) config: ChatConfig,
    pub(super) rate: KeyedBuckets<UserId>,
    /// DM opens per user (`dm_open_rate`; `None` = no limit).
    dm_rate: Option<KeyedBuckets<UserId>>,
    /// Player room invitations per user (`invite_rate`; `None` = no limit).
    invite_rate: Option<KeyedBuckets<UserId>>,
    /// Read markers per user (`read_rate`).
    pub(super) read_rate: KeyedBuckets<UserId>,
    /// Player rooms created per user (`room_create_rate`).
    pub(super) create_rate: KeyedBuckets<UserId>,
    pub(super) presence: Presence,
    /// The coalesced `chat.read` pushes.
    pub(super) reads: ReadPushes,
    /// The typing throttle.
    pub(super) typing: Typing,
    rooms: RwLock<HashMap<i64, (Instant, RoomRow)>>,
}

impl std::fmt::Debug for ChatService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatService").field("config", &self.0.config).finish_non_exhaustive()
    }
}

impl ChatService {
    pub(crate) fn new(config: ChatConfig) -> Self {
        let rate = KeyedBuckets::new(config.rate_messages, Duration::from_secs(u64::from(config.rate_window_secs)), 100_000);
        let dm_rate =
            (config.dm_open_rate > 0).then(|| KeyedBuckets::new(config.dm_open_rate, Duration::from_secs(u64::from(config.dm_open_window_secs)), 100_000));
        let invite_rate =
            (config.invite_rate > 0).then(|| KeyedBuckets::new(config.invite_rate, Duration::from_secs(u64::from(config.invite_rate_window_secs)), 100_000));
        let presence = Presence::new(config.presence, config.presence_max_members, config.presence_per_second);
        let read_rate = KeyedBuckets::new(config.read_rate, Duration::from_secs(u64::from(config.read_window_secs)), 100_000);
        let create_rate = KeyedBuckets::new(config.room_create_rate, Duration::from_secs(u64::from(config.room_create_window_secs)), 100_000);
        let reads = ReadPushes::new(Duration::from_millis(u64::from(config.read_push_interval_ms)));
        let typing = Typing::new(Duration::from_millis(u64::from(config.typing_interval_ms)), Duration::from_millis(u64::from(config.typing_ttl_ms)));
        Self(Arc::new(Inner { config, rate, dm_rate, invite_rate, read_rate, create_rate, presence, reads, typing, rooms: RwLock::new(HashMap::new()) }))
    }

    /// The settings.
    pub fn config(&self) -> &ChatConfig {
        &self.0.config
    }

    pub(super) fn ctx(state: &AppState) -> HookCtx {
        HookCtx::new(state.clone(), None)
    }

    // ---- rooms ----------------------------------------------------------------------------------

    fn cached(&self, id: i64) -> Option<RoomRow> {
        let rooms = self.0.rooms.read().unwrap_or_else(|p| p.into_inner());
        rooms.get(&id).filter(|(at, _)| at.elapsed() < ROOM_CACHE_TTL).map(|(_, row)| row.clone())
    }

    pub(super) fn remember(&self, row: &RoomRow) {
        let mut rooms = self.0.rooms.write().unwrap_or_else(|p| p.into_inner());
        if rooms.len() >= ROOM_CACHE {
            rooms.clear();
        }
        rooms.insert(row.id, (Instant::now(), row.clone()));
    }

    pub(super) fn forget(&self, id: i64) {
        self.0.rooms.write().unwrap_or_else(|p| p.into_inner()).remove(&id);
    }

    pub(super) async fn room_row(&self, state: &AppState, room: RoomId) -> Result<RoomRow, AppError> {
        if let Some(row) = self.cached(room.get()) {
            return Ok(row);
        }
        let row = state.db().fetch_optional::<RoomRow, _>(&store::room_by_id(room.get())).await?.ok_or_else(|| AppError::not_found("no such room"))?;
        self.remember(&row);
        Ok(row)
    }

    async fn resolve(&self, state: &AppState, room: &RoomRef) -> Result<RoomRow, AppError> {
        match room {
            RoomRef::Id(id) => self.room_row(state, *id).await,
            RoomRef::Key(key) => {
                let row = state.db().fetch_optional::<RoomRow, _>(&store::room_by_key(key)).await?.ok_or_else(|| AppError::not_found("no such room"))?;
                self.remember(&row);
                Ok(row)
            }
            _ => Err(AppError::bad_request("unknown room reference")),
        }
    }

    fn cap_of(&self, row: &RoomRow) -> u32 {
        match row.max_members.and_then(|m| u32::try_from(m).ok()) {
            Some(max) => max,
            None if has_members(row) => self.0.config.max_group_members,
            None => self.0.config.max_room_members,
        }
    }

    pub(super) fn info(&self, row: &RoomRow, viewer: Option<UserId>) -> RoomInfo {
        let mut info = RoomInfo::new(RoomId(row.id), kind_of(row));
        if let Some(key) = &row.room_key {
            info = info.with_key(key.clone());
        }
        if let Some(name) = &row.name {
            info = info.with_name(name.clone());
        }
        if row.kind == KIND_DM {
            if let (Some(viewer), Some(a), Some(b)) = (viewer, row.dm_a, row.dm_b) {
                info = info.with_peer(UserId(if a == viewer.get() { b } else { a }));
            }
        } else {
            info = info.with_members(Some(self.0.presence.count(RoomId(row.id))), Some(self.cap_of(row)));
        }
        if row.kind == KIND_PLAYER {
            info = info.with_visibility(if row.is_public == 1 { RoomVisibility::Public } else { RoomVisibility::Private });
        }
        info
    }

    pub(super) fn is_participant(row: &RoomRow, user: UserId) -> bool {
        row.dm_a == Some(user.get()) || row.dm_b == Some(user.get())
    }

    /// `user`'s row in a group or player room (any role).
    pub(super) async fn member_row(state: &AppState, row: &RoomRow, user: UserId) -> Result<Option<MemberRow>, AppError> {
        Ok(state.db().fetch_optional::<MemberRow, _>(&store::member(row.id, user.get())).await?)
    }

    /// Whether `user` is a member (owner, moderator or member) of a group or player room.
    pub(super) async fn is_member(state: &AppState, row: &RoomRow, user: UserId) -> Result<bool, AppError> {
        Ok(Self::member_row(state, row, user).await?.is_some_and(|m| ACTIVE.contains(&m.role.as_str())))
    }

    /// Whether `user` may read `row` (history, markers): DM participants, group and player room
    /// members, anyone in a public room.
    pub(super) async fn may_read(state: &AppState, row: &RoomRow, user: UserId) -> Result<bool, AppError> {
        Ok(match row.kind.as_str() {
            KIND_DM => Self::is_participant(row, user),
            KIND_GROUP | KIND_PLAYER => Self::is_member(state, row, user).await?,
            _ => true,
        })
    }

    /// [`may_read`](Self::may_read), where `staff` (a holder of `chat.moderate`) reads every
    /// player room as its owner does.
    pub(super) async fn may_read_as(state: &AppState, row: &RoomRow, user: UserId, staff: bool) -> Result<bool, AppError> {
        if staff && row.kind == KIND_PLAYER {
            return Ok(true);
        }
        Self::may_read(state, row, user).await
    }

    /// Whether `user` holds the chat moderation permission or one of `moderator_roles`.
    fn is_moderator(&self, state: &AppState, actor: &AuthContext) -> bool {
        self.0.config.moderator_roles.iter().any(|role| actor.has_role(role)) || actor.has_permission(state, super::MODERATE.name())
    }

    /// Create a public room, or update its name and cap (by key). Rooms in the configuration are
    /// created at start.
    pub async fn create_room(&self, state: &AppState, spec: &RoomSpec) -> Result<RoomInfo, AppError> {
        if !net_backend_protocol::chat::is_valid_room_key(&spec.key) {
            return Err(AppError::bad_request("not a room key (1-64 bytes of a-z, 0-9, _ - .)"));
        }
        check_room_name(spec.name.as_deref())?;
        if spec.max_members.is_some_and(|m| !(1..=10_000_000).contains(&m)) {
            return Err(AppError::bad_request("max_members must be between 1 and 10000000"));
        }
        let max = spec.max_members.map(i64::from);
        let db = state.db();
        let row = match db.fetch_optional::<RoomRow, _>(&store::room_by_key(&spec.key)).await? {
            Some(row) => {
                db.execute(&store::update_room(row.id, spec.name.as_deref(), max)).await?;
                row.id
            }
            None => match db.insert_id(&store::insert_room(KIND_ROOM, Some(&spec.key), spec.name.as_deref(), max, None, state.now().get())?, "id").await {
                Ok(id) => id,
                // Another instance created it at the same moment.
                Err(error) if error.is_unique_violation() => {
                    db.fetch_optional::<RoomRow, _>(&store::room_by_key(&spec.key)).await?.ok_or_else(|| AppError::internal(error))?.id
                }
                Err(error) => return Err(error.into()),
            },
        };
        self.forget(row);
        let row = self.room_row(state, RoomId(row)).await?;
        Ok(self.info(&row, None))
    }

    /// Create a group room with these members (server code: a guild, a party). A member listed
    /// twice counts once; an unknown account: 404 `not_found` (nothing is created).
    pub async fn create_group(&self, state: &AppState, name: Option<&str>, members: &[UserId]) -> Result<RoomInfo, AppError> {
        check_room_name(name)?;
        let id = self.insert_group(state, name, members, None).await?;
        let row = self.room_row(state, RoomId(id)).await?;
        Ok(self.info(&row, None))
    }

    /// A group room for another module (a lobby's, a group's), tagged with `origin` so that the
    /// module's upkeep finds it again ([`module_rooms`](Self::module_rooms)) when no lobby or
    /// group names it any more.
    #[cfg(any(feature = "groups", feature = "lobbies"))]
    pub(crate) async fn create_module_room(&self, state: &AppState, origin: &'static str, members: &[UserId]) -> Result<RoomId, AppError> {
        Ok(RoomId(self.insert_group(state, None, members, Some(origin)).await?))
    }

    async fn insert_group(&self, state: &AppState, name: Option<&str>, members: &[UserId], origin: Option<&str>) -> Result<i64, AppError> {
        let mut seen = HashSet::new();
        let members: Vec<UserId> = members.iter().copied().filter(|m| seen.insert(*m)).collect();
        let now = state.now().get();
        let insert = match origin {
            Some(origin) => store::insert_module_room(origin, now)?,
            None => store::insert_room(KIND_GROUP, None, name, None, None, now)?,
        };
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        loop {
            let mut tx = state.db().begin_write().await?;
            let result = async {
                let id = tx.insert_id(&insert, "id").await?;
                for member in &members {
                    match tx.execute(&store::insert_member(id, member.get(), now)?).await {
                        Ok(_) => {}
                        Err(error) if error.is_foreign_key_violation() => return Err(AppError::not_found(format!("no such account: {member}"))),
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok::<i64, AppError>(id)
            }
            .await;
            match tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => break other,
            }
        }
    }

    /// Delete a group room (server code; the lobbies and groups modules call it when a lobby
    /// closes or a group is deleted): its member rows, messages and read markers go with it, and
    /// every member's sockets leave it on every instance. `false`: no such room (deleted
    /// already); 400 `bad_request` for a room of another kind.
    pub async fn delete_group_room(&self, state: &AppState, room: RoomId) -> Result<bool, AppError> {
        let db = state.db();
        let Some(row) = db.fetch_optional::<RoomRow, _>(&store::room_by_id(room.get())).await? else {
            self.forget(room.get());
            return Ok(false);
        };
        if row.kind != KIND_GROUP {
            return Err(AppError::bad_request("only group rooms are deleted this way (a player room: the room routes)"));
        }
        let members: Vec<UserId> = db.fetch_all::<MemberRow, _>(&store::all_rows(row.id)).await?.into_iter().map(|m| UserId(m.user_id)).collect();
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        let deleted = loop {
            match db.execute(&store::delete_room(row.id)).await.map_err(AppError::from) {
                Err(error) if retry.again(&error).await => continue,
                other => break other?,
            }
        };
        self.forget(row.id);
        for user in members {
            if let Err(error) = state.ws().remove_from_room(user, &hub_room(room)) {
                tracing::warn!(%error, "chat: removing a player's sockets from a deleted room failed");
            }
        }
        Ok(deleted > 0)
    }

    /// The ids of the group rooms `origin` created before `before` (Unix ms) with an id above
    /// `after`, oldest first, at most `limit` (a module's upkeep pages through them).
    #[cfg(any(feature = "groups", feature = "lobbies"))]
    pub(crate) async fn module_rooms(&self, state: &AppState, origin: &str, before: i64, after: i64, limit: u64) -> Result<Vec<i64>, AppError> {
        Ok(state.db().fetch_all::<IdRow, _>(&store::module_rooms(origin, before, after, limit)).await?.into_iter().map(|r| r.id).collect())
    }

    /// Add a member to a group room (already one: fine).
    pub async fn add_member(&self, state: &AppState, room: RoomId, user: UserId) -> Result<(), AppError> {
        let row = self.room_row(state, room).await?;
        if row.kind != KIND_GROUP {
            return Err(AppError::bad_request("only group rooms have members"));
        }
        match state.db().execute(&store::insert_member(row.id, user.get(), state.now().get())?).await {
            Ok(_) => Ok(()),
            Err(error) if error.is_unique_violation() => Ok(()),
            Err(error) if error.is_foreign_key_violation() => Err(AppError::not_found("no such account")),
            Err(error) => Err(error.into()),
        }
    }

    /// Remove a member from a group room: it can no longer join, send or read the history at
    /// once (every instance checks the membership), and its sockets leave the room on EVERY
    /// instance ([`Hub::remove_from_room`] through the `Broadcaster`).
    pub async fn remove_member(&self, state: &AppState, room: RoomId, user: UserId) -> Result<(), AppError> {
        let row = self.room_row(state, room).await?;
        if row.kind != KIND_GROUP {
            return Err(AppError::bad_request("only group rooms have members"));
        }
        // The row first: a join racing with this re-checks it after its hub join (see `join`).
        state.db().execute(&store::delete_member(row.id, user.get())).await?;
        state.ws().remove_from_room(user, &hub_room(room))?;
        Ok(())
    }

    /// One room, as a client would see it (`None`: no such room).
    pub async fn room(&self, state: &AppState, room: RoomId) -> Result<Option<RoomInfo>, AppError> {
        match self.room_row(state, room).await {
            Ok(row) => Ok(Some(self.info(&row, None))),
            Err(error) if error.status().as_u16() == 404 => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The public rooms, by id.
    pub async fn list_rooms(&self, state: &AppState, page: &PageRequest) -> Result<Page<RoomInfo>, AppError> {
        let limit = page_limit(page)?;
        let after = match &page.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let mut rows = state.db().fetch_all::<RoomRow, _>(&store::public_rooms(after, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        Ok(Page::new(rows.iter().map(|r| self.info(r, None)).collect(), next))
    }

    // ---- direct messages ------------------------------------------------------------------------

    /// Count one DM open of `user` against `dm_open_rate` (429 `rate_limited` over it).
    pub(crate) fn check_dm_rate(&self, user: UserId) -> Result<(), AppError> {
        match self.0.dm_rate.as_ref().map(|rate| rate.check(user)) {
            Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
            _ => Ok(()),
        }
    }

    /// Count one player room invitation of `user` against `invite_rate` (429 `rate_limited` over
    /// it).
    pub(crate) fn check_invite_rate(&self, user: UserId) -> Result<(), AppError> {
        match self.0.invite_rate.as_ref().map(|rate| rate.check(user)) {
            Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
            _ => Ok(()),
        }
    }

    /// Open (or find) the direct-message room of `user` and `peer`.
    pub async fn open_direct(&self, state: &AppState, ctx: &HookCtx, user: UserId, peer: UserId) -> Result<RoomInfo, AppError> {
        if user == peer {
            return Err(AppError::bad_request("a direct-message room needs another user"));
        }
        let db = state.db();
        if db.fetch_all::<NameRow, _>(&store::names(&[peer.get()])).await?.is_empty() {
            return Err(AppError::not_found("no such account"));
        }
        state.hooks().run_before(ctx, BeforeDirectOpen { user_id: user, peer }).await?;
        let (a, b) = if user.get() < peer.get() { (user.get(), peer.get()) } else { (peer.get(), user.get()) };
        let row = match db.fetch_optional::<RoomRow, _>(&store::dm_room(a, b)).await? {
            Some(row) => row,
            None => {
                let created = db.insert_id(&store::insert_room(KIND_DM, None, None, None, Some((a, b)), state.now().get())?, "id").await;
                match created {
                    Ok(id) => self.room_row(state, RoomId(id)).await?,
                    // The peer opened it at the same moment.
                    Err(error) if error.is_unique_violation() => {
                        db.fetch_optional::<RoomRow, _>(&store::dm_room(a, b)).await?.ok_or_else(|| AppError::internal(error))?
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        };
        self.remember(&row);
        Ok(self.info(&row, Some(user)))
    }

    /// The direct-message rooms of `user`, newest activity first.
    pub async fn list_directs(&self, state: &AppState, user: UserId, page: &PageRequest) -> Result<Page<RoomInfo>, AppError> {
        let limit = page_limit(page)?;
        let before = match &page.cursor {
            Some(cursor) => {
                let parsed = cursor.as_str().split_once('.').and_then(|(a, i)| Some((a.parse::<i64>().ok()?, i.parse::<i64>().ok()?)));
                Some(parsed.ok_or_else(|| AppError::bad_request("the cursor is not valid"))?)
            }
            None => None,
        };
        let mut rows = state.db().fetch_all::<RoomRow, _>(&store::dms_of(user.get(), before, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(format!("{}.{}", r.last_activity_at, r.id))) } else { None };
        Ok(Page::new(rows.iter().map(|r| self.info(r, Some(user))).collect(), next))
    }

    // ---- joining and presence -------------------------------------------------------------------

    /// Join `room` on `connection` (public rooms; group rooms for their members; player rooms for
    /// their members, invited players and `staff` (a holder of `chat.moderate`), public ones for
    /// anyone; a DM room needs no join and answers its info).
    pub(crate) async fn join(
        &self,
        state: &AppState,
        ctx: &HookCtx,
        connection: ConnectionId,
        user: UserId,
        staff: bool,
        room: &RoomRef,
    ) -> Result<RoomInfo, AppError> {
        let row = self.resolve(state, room).await?;
        let group = has_members(&row);
        match row.kind.as_str() {
            KIND_DM => {
                return if Self::is_participant(&row, user) { Ok(self.info(&row, Some(user))) } else { Err(not_a_member()) };
            }
            KIND_GROUP if !Self::is_member(state, &row, user).await? => return Err(not_a_member()),
            KIND_PLAYER => {
                // The membership first (a public room or an invitation; runs `BeforeChatJoin`).
                self.become_member(state, ctx, &row, user, staff).await?;
            }
            _ => {}
        }
        if row.kind != KIND_PLAYER {
            let event = BeforeChatJoin { room: RoomId(row.id), kind: kind_of(&row), key: row.room_key.clone(), user_id: user };
            state.hooks().run_before(ctx, event).await?;
        }
        // Everything that can fail runs before the hub join: an error never leaves the socket joined.
        let name = state.db().fetch_all::<NameRow, _>(&store::names(&[user.get()])).await?.into_iter().next().and_then(|r| r.display_name);
        let hub = state.ws();
        let room_id = RoomId(row.id);
        let cap = usize::try_from(self.cap_of(&row)).unwrap_or(usize::MAX);
        if hub.join_with_cap(connection, hub_room(room_id), cap)? {
            // A removal that ran between the membership check and the hub join saw no socket to
            // take out: check again now that the socket is in (the removal deletes the row first).
            if group {
                let still = Self::is_member(state, &row, user).await;
                if !matches!(still, Ok(true)) {
                    hub.leave(connection, &hub_room(room_id));
                    return Err(still.err().unwrap_or_else(not_a_member));
                }
            }
            let change = self.0.presence.add(room_id, user, connection, name);
            // The socket may have closed or been removed meanwhile (its leave ran before this add): undo.
            if !hub.rooms_of(connection).iter().any(|r| chat_room(r) == Some(room_id)) {
                self.0.presence.remove(room_id, user, connection);
            } else if change.transition {
                self.push_presence(hub, room_id, user, PresenceEvent::Joined, change.name, change.count);
            }
        }
        if row.kind == KIND_PLAYER {
            return self.player_info(state, &row, Some(user)).await;
        }
        Ok(self.info(&row, Some(user)))
    }

    /// Leave `room` on `connection` (not joined: fine).
    pub(crate) fn leave(&self, hub: &Hub, connection: ConnectionId, user: UserId, room: RoomId) {
        if hub.leave(connection, &hub_room(room)) {
            self.left(hub, room, user, connection);
        }
    }

    /// The hub took a socket out of a hub room (a removal through the `Broadcaster`).
    pub(crate) fn hub_room_left(&self, hub: &Hub, connection: ConnectionId, user: UserId, room: &str) {
        if let Some(room) = chat_room(room) {
            self.left(hub, room, user, connection);
        }
    }

    fn left(&self, hub: &Hub, room: RoomId, user: UserId, connection: ConnectionId) {
        let change = self.0.presence.remove(room, user, connection);
        if change.transition {
            self.push_presence(hub, room, user, PresenceEvent::Left, change.name, change.count.saturating_add(1));
        }
    }

    /// A socket closed: its chat rooms lose it.
    pub(crate) fn disconnected(&self, hub: &Hub, connection: ConnectionId, user: UserId, rooms: &[Arc<str>]) {
        for room in rooms.iter().filter_map(|r| chat_room(r)) {
            self.left(hub, room, user, connection);
        }
    }

    fn push_presence(&self, hub: &Hub, room: RoomId, user: UserId, event: PresenceEvent, name: Option<String>, size: u32) {
        if !self.0.presence.should_push(room, size) {
            return;
        }
        let count = self.0.presence.count(room);
        let mut push = PresencePush::new(room, user, event).with_count(count);
        if let Some(name) = name {
            push = push.with_name(name);
        }
        if let Err(error) = hub.push_room(&hub_room(room), &push) {
            tracing::warn!(%error, "chat: a presence push failed");
        }
    }

    /// Who is online in `room` (the caller must have joined it on this connection). A DM room
    /// lists only the caller: the peer's online state is never revealed through a DM (anyone may
    /// open a DM with anyone).
    pub(crate) async fn members(&self, state: &AppState, connection: Option<ConnectionId>, user: UserId, room: RoomId) -> Result<RoomMembers, AppError> {
        let row = self.room_row(state, room).await?;
        if row.kind == KIND_DM {
            if !Self::is_participant(&row, user) {
                return Err(not_a_member());
            }
            let name = state.db().fetch_all::<NameRow, _>(&store::names(&[user.get()])).await?.into_iter().next().and_then(|n| n.display_name);
            let me = match name {
                Some(name) => RoomMember::new(user).with_name(name),
                None => RoomMember::new(user),
            };
            return Ok(RoomMembers::new(room, vec![me], 1));
        }
        let joined = connection.is_none_or(|c| state.ws().rooms_of(c).iter().any(|r| chat_room(r) == Some(room)));
        if !joined {
            return Err(not_a_member());
        }
        let (members, count, truncated) = self.0.presence.members(room);
        let answer = RoomMembers::new(room, members, count);
        Ok(if truncated { answer.truncated() } else { answer })
    }

    /// The online members of a room (server code; this instance).
    pub fn online(&self, room: RoomId) -> RoomMembers {
        let (members, count, truncated) = self.0.presence.members(room);
        let answer = RoomMembers::new(room, members, count);
        if truncated {
            answer.truncated()
        } else {
            answer
        }
    }

    // ---- messages -------------------------------------------------------------------------------

    /// Send from a connection: access (joined on this connection; group rooms: still a member;
    /// DMs: a member), the rate limit, the hooks, store, push (the answer goes out before the
    /// sender's own echo).
    pub(crate) async fn send(
        &self,
        state: &AppState,
        ctx: &HookCtx,
        connection: ConnectionId,
        user: UserId,
        request: SendMessage,
    ) -> Result<SendAck, AppError> {
        request.validate(self.0.config.max_text_chars as usize)?;
        let row = self.room_row(state, request.room).await?;
        let allowed = match row.kind.as_str() {
            KIND_DM => Self::is_participant(&row, user),
            kind => {
                let joined = state.ws().rooms_of(connection).iter().any(|r| chat_room(r) == Some(request.room));
                // A member removed on another instance may still be joined here until the
                // removal's control delivery arrives: the table is the truth.
                joined && (kind != KIND_GROUP && kind != KIND_PLAYER || Self::is_member(state, &row, user).await?)
            }
        };
        if !allowed {
            return Err(not_a_member());
        }
        // Counted once the message could be sent (refused requests are the hub's frame limit's).
        if let RateDecision::Deny { retry_after_ms } = self.0.rate.check(user) {
            return Err(AppError::rate_limited(retry_after_ms));
        }
        let message = self.store_and_push(state, ctx, &row, user, request.text, request.nonce).await?;
        Ok(SendAck::new(message.id, message.sent_at))
    }

    /// Send as `sender` from server code (a system message, a bot): no join or rate limit, the
    /// hooks and the text rules apply; DM rooms only for their members.
    pub async fn send_as(&self, state: &AppState, sender: UserId, room: RoomId, text: &str) -> Result<ChatMessage, AppError> {
        SendMessage::new(room, text).validate(self.0.config.max_text_chars as usize)?;
        let row = self.room_row(state, room).await?;
        if row.kind == KIND_DM && !Self::is_participant(&row, sender) {
            return Err(not_a_member());
        }
        self.store_and_push(state, &Self::ctx(state), &row, sender, text.to_string(), None).await
    }

    async fn store_and_push(
        &self,
        state: &AppState,
        ctx: &HookCtx,
        row: &RoomRow,
        user: UserId,
        text: String,
        nonce: Option<String>,
    ) -> Result<ChatMessage, AppError> {
        let room = RoomId(row.id);
        let kind = kind_of(row);
        let event = state.hooks().run_before(ctx, BeforeChatSend { room, kind, sender: user, text, nonce: nonce.clone(), edit: None }).await?;
        // A hook's rewrite follows the same rules.
        SendMessage::new(room, event.text.clone()).validate(self.0.config.max_text_chars as usize)?;
        let db = state.db();
        let name = db.fetch_all::<NameRow, _>(&store::names(&[user.get()])).await?.into_iter().next().and_then(|r| r.display_name);
        let now = state.now().get();
        let insert = store::insert_message(row.id, user.get(), name.as_deref(), &event.text, nonce.as_deref(), now)?;
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        let id = loop {
            let result = if row.kind == KIND_DM {
                // A DM's activity orders its members' DM lists: the room and the message move
                // together. The room row FIRST (module docs: the MySQL deadlock).
                let mut tx = db.begin_write().await?;
                let stored = async {
                    tx.execute(&store::touch_room(row.id, now)).await?;
                    Ok::<i64, AppError>(tx.insert_id(&insert, "id").await?)
                }
                .await;
                tx.finish(stored).await
            } else {
                // Public and group rooms: one insert (no shared row every sender would lock).
                db.insert_id(&insert, "id").await.map_err(AppError::from)
            };
            match result {
                Err(error) if retry.again(&error).await => continue,
                other => break other?,
            }
        };
        let mut message = ChatMessage::new(MessageId(id), room, user, event.text, UnixMillis(now));
        if let Some(name) = name {
            message = message.with_sender_name(name);
        }
        // The message ends the sender's typing indicator on the clients: the next one pushes at once.
        self.0.typing.clear(room, user);
        self.push_sent(state.ws(), row, &message, nonce.as_deref())?;
        if let Some(nonce) = nonce {
            message = message.with_nonce(nonce);
        }
        let after = AfterChatSend { message: message.clone(), kind };
        // After hooks run in their own task: they never delay the sender's answer.
        let (hooks, ctx) = (state.hooks().clone(), ctx.clone());
        tokio::spawn(async move { hooks.run_after(&ctx, Arc::new(after)).await });
        Ok(message)
    }

    /// Push a new message: a DM to both members (the sender's `nonce` only to the sender), a room
    /// message to the room (every member sees the nonce: one frame for all).
    fn push_sent(&self, hub: &Hub, row: &RoomRow, message: &ChatMessage, nonce: Option<&str>) -> Result<(), AppError> {
        let with_nonce = match nonce {
            Some(nonce) => message.clone().with_nonce(nonce.to_string()),
            None => message.clone(),
        };
        if row.kind == KIND_DM {
            for member in [row.dm_a, row.dm_b].into_iter().flatten().map(UserId) {
                hub.push_user(member, if member == message.sender { &with_nonce } else { message })?;
            }
        } else {
            hub.push_room(&hub_room(RoomId(row.id)), &with_nonce)?;
        }
        Ok(())
    }

    pub(super) fn push_message<P: net_backend_protocol::ServerPush>(&self, hub: &Hub, row: &RoomRow, push: &P) -> Result<(), AppError> {
        if row.kind == KIND_DM {
            for user in [row.dm_a, row.dm_b].into_iter().flatten() {
                hub.push_user(UserId(user), push)?;
            }
        } else {
            hub.push_room(&hub_room(RoomId(row.id)), push)?;
        }
        Ok(())
    }

    /// A page of a room's history, newest first (public rooms: anyone; group, player and DM rooms:
    /// their members). A message's `nonce` is shown to its sender only.
    pub async fn history(&self, state: &AppState, user: UserId, room: RoomId, page: &PageRequest) -> Result<Page<ChatMessage>, AppError> {
        self.history_as(state, user, false, room, page).await
    }

    /// [`history`](Self::history) for a player: `staff` (a holder of `chat.moderate`) reads every
    /// player room.
    pub(crate) async fn history_as(
        &self,
        state: &AppState,
        user: UserId,
        staff: bool,
        room: RoomId,
        page: &PageRequest,
    ) -> Result<Page<ChatMessage>, AppError> {
        let limit = page_limit(page)?;
        let row = self.room_row(state, room).await?;
        if !Self::may_read_as(state, &row, user, staff).await? {
            return Err(not_a_member());
        }
        let before = match &page.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let since = self.0.config.cutoff(state.now().get());
        let mut rows = state.db().fetch_all::<MessageRow, _>(&store::history(row.id, before, since, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        let items = rows
            .into_iter()
            .map(|r| {
                let mut message = ChatMessage::new(MessageId(r.id), RoomId(r.room_id), UserId(r.sender_id), r.body, UnixMillis(r.created_at));
                if let Some(name) = r.sender_name {
                    message = message.with_sender_name(name);
                }
                if let Some(nonce) = r.nonce.filter(|_| r.sender_id == user.get()) {
                    message = message.with_nonce(nonce);
                }
                if let Some(at) = r.edited_at {
                    message = message.with_edited_at(UnixMillis(at));
                }
                message
            })
            .collect();
        Ok(Page::new(items, next))
    }

    /// Delete a message as `actor`: its sender (if allowed) or a moderator (audited); `None`:
    /// server code. Pushes `chat.deleted` to the room.
    pub(crate) async fn delete(&self, state: &AppState, ctx: &HookCtx, actor: Option<&AuthContext>, room: RoomId, message: MessageId) -> Result<(), AppError> {
        let row = self.room_row(state, room).await?;
        let db = state.db();
        let stored = db.fetch_optional::<MessageRow, _>(&store::message(row.id, message.get())).await?;
        let stored = stored.filter(|m| m.deleted_at.is_none()).ok_or_else(|| AppError::not_found("no such message"))?;
        let sender = UserId(stored.sender_id);
        let moderator = actor.is_some_and(|a| self.is_moderator(state, a));
        let own = actor.is_some_and(|a| a.user_id == sender);
        // A player room's owner and moderators moderate their room.
        let room_staff = match actor {
            Some(a) if row.kind == KIND_PLAYER && !own && !moderator => {
                Self::member_row(state, &row, a.user_id).await?.is_some_and(|m| m.role == OWNER || m.role == MODERATOR)
            }
            _ => false,
        };
        let allowed = actor.is_none() || moderator || room_staff || (own && self.0.config.allow_self_delete);
        if !allowed {
            return Err(AppError::forbidden("only its sender or a moderator may delete a message"));
        }
        // Its sender deletes only while it can still read the room (not after leaving a group or
        // player room, or being removed from it).
        if let Some(actor) = actor.filter(|_| own && !moderator) {
            if !Self::may_read(state, &row, actor.user_id).await? {
                return Err(not_a_member());
            }
        }
        let now = state.now().get();
        let by = actor.map_or(0, |a| a.user_id.get());
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        loop {
            let mut tx = db.begin_write().await?;
            let result = async {
                let changed = tx.execute(&store::delete_message(stored.id, by, now)).await?;
                if changed == 0 {
                    return Err(AppError::not_found("no such message"));
                }
                if !own {
                    let mut record = AuditRecord::new("chat.message_deleted").actor(actor.map(|a| a.user_id)).target_user(sender);
                    record = record.request_id(ctx.request_id()).data(json!({ "room": row.id, "message": stored.id }));
                    audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
                }
                Ok(())
            }
            .await;
            match tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => break other?,
            }
        }
        self.push_message(state.ws(), &row, &MessageDeleted::new(message, room))?;
        let after = AfterChatDelete { room, message, sender, deleted_by: actor.map(|a| a.user_id) };
        state.hooks().run_after(ctx, Arc::new(after)).await;
        Ok(())
    }

    /// Delete a message from server code (audited as `chat.message_deleted` without an actor).
    pub async fn delete_message(&self, state: &AppState, room: RoomId, message: MessageId) -> Result<(), AppError> {
        self.delete(state, &Self::ctx(state), None, room, message).await
    }

    /// Change a message's text as `actor`: its sender (with `allow_edit`, within
    /// `edit_window_secs`, still able to read the room; counted on the send rate) or a holder of
    /// `chat.moderate` (any message, audited as `chat.message_edited`); `None`: server code. The
    /// text rules and the [`BeforeChatSend`] hooks (with `edit`) apply; the room gets `chat.edited`.
    pub(crate) async fn edit(&self, state: &AppState, ctx: &HookCtx, actor: Option<&AuthContext>, request: EditMessage) -> Result<ChatMessage, AppError> {
        let max = self.0.config.max_text_chars as usize;
        request.validate(max)?;
        let (room, message) = (request.room, request.message);
        let row = self.room_row(state, room).await?;
        let db = state.db();
        let stored = db.fetch_optional::<MessageRow, _>(&store::message(row.id, message.get())).await?;
        let stored = stored.filter(|m| m.deleted_at.is_none()).ok_or_else(|| AppError::not_found("no such message"))?;
        let sender = UserId(stored.sender_id);
        let now = state.now().get();
        let moderator = actor.is_some_and(|a| a.has_permission(state, super::MODERATE.name()));
        let own = actor.is_some_and(|a| a.user_id == sender);
        if let Some(actor) = actor.filter(|_| !moderator) {
            if !own {
                return Err(AppError::forbidden("only its sender or a moderator may edit a message"));
            }
            if !self.0.config.allow_edit {
                return Err(AppError::forbidden("editing messages is turned off"));
            }
            let window = i64::from(self.0.config.edit_window_secs) * 1000;
            if window > 0 && now.saturating_sub(stored.created_at) > window {
                return Err(AppError::forbidden("the time to edit this message is over"));
            }
            if !Self::may_read(state, &row, actor.user_id).await? {
                return Err(not_a_member());
            }
            if let RateDecision::Deny { retry_after_ms } = self.0.rate.check(actor.user_id) {
                return Err(AppError::rate_limited(retry_after_ms));
            }
        }
        let kind = kind_of(&row);
        let event = BeforeChatSend { room, kind, sender, text: request.edit.text, nonce: None, edit: Some(message) };
        let event = state.hooks().run_before(ctx, event).await?;
        SendMessage::new(room, event.text.clone()).validate(max)?;
        let by = actor.map(|a| a.user_id);
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        loop {
            let mut tx = db.begin_write().await?;
            let result = async {
                if tx.execute(&store::edit_message(stored.id, &event.text, by.map(|u| u.get()), now)).await? == 0 {
                    return Err(AppError::not_found("no such message"));
                }
                if !own {
                    let mut record = AuditRecord::new("chat.message_edited").actor(by).target_user(sender);
                    record = record.request_id(ctx.request_id()).data(json!({ "room": row.id, "message": stored.id }));
                    audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
                }
                Ok(())
            }
            .await;
            match tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => break other?,
            }
        }
        let mut push = MessageEdited::new(message, room, event.text.clone(), UnixMillis(now));
        if let Some(by) = by {
            push = push.by(by);
        }
        self.push_message(state.ws(), &row, &push)?;
        let after = AfterChatEdit { room, message, sender, edited_by: by, text: event.text.clone() };
        let (hooks, hook_ctx) = (state.hooks().clone(), ctx.clone());
        tokio::spawn(async move { hooks.run_after(&hook_ctx, Arc::new(after)).await });
        let mut edited = ChatMessage::new(message, room, sender, event.text, UnixMillis(stored.created_at)).with_edited_at(UnixMillis(now));
        if let Some(name) = stored.sender_name {
            edited = edited.with_sender_name(name);
        }
        if let Some(nonce) = stored.nonce.filter(|_| own) {
            edited = edited.with_nonce(nonce);
        }
        Ok(edited)
    }

    /// Change a message's text from server code (audited as `chat.message_edited` without an
    /// actor; the hooks and the text rules apply).
    pub async fn edit_message(&self, state: &AppState, room: RoomId, message: MessageId, text: &str) -> Result<ChatMessage, AppError> {
        self.edit(state, &Self::ctx(state), None, EditMessage::new(room, message, text)).await
    }

    /// Delete the messages older than the retention, in batches of 1000 (short statements, short
    /// locks); the number deleted.
    pub async fn purge(&self, state: &AppState) -> Result<u64, AppError> {
        let Some(cutoff) = self.0.config.cutoff(state.now().get()) else { return Ok(0) };
        let mut deleted = 0u64;
        loop {
            let ids: Vec<i64> = state.db().fetch_all::<IdRow, _>(&store::expired(cutoff, PURGE_BATCH)).await?.into_iter().map(|r| r.id).collect();
            if ids.is_empty() {
                return Ok(deleted);
            }
            deleted = deleted.saturating_add(state.db().execute(&store::delete_messages(&ids)).await?);
            if (ids.len() as u64) < PURGE_BATCH {
                return Ok(deleted);
            }
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_room_names() {
        assert_eq!(hub_room(RoomId(12)), "chat:12");
        assert_eq!(chat_room("chat:12"), Some(RoomId(12)));
        assert_eq!(chat_room("lobby"), None);
        assert_eq!(chat_room("chat:x"), None);
        assert!(check_room_name(None).is_ok() && check_room_name(Some("Guild")).is_ok() && check_room_name(Some("")).is_err());
    }
}
