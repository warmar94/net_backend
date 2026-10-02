//! [`ChatService`]: rooms, membership, sending, history, presence, direct messages, deletion.
//!
//! **Locking:** a public or group send is one INSERT (no shared row every sender would lock). A
//! DM send updates the room's `last_activity_at` FIRST and then inserts the message, in one
//! transaction: on MySQL the message's foreign-key check takes a shared lock on the room row, and
//! two senders that each held it and then asked for the exclusive lock of the UPDATE deadlocked;
//! with the UPDATE first, concurrent DM sends of one room simply queue. Writes the database still
//! aborts as a deadlock are run again ([`Retry`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use net_backend_protocol::chat::{
    ChatMessage, MessageDeleted, Presence as PresencePush, PresenceEvent, RoomInfo, RoomKind, RoomMember, RoomMembers, RoomRef, SendAck, SendMessage,
};
use net_backend_protocol::{codes, Cursor, MessageId, Page, PageRequest, RoomId, UnixMillis, UserId};
use serde_json::json;

use super::config::{is_valid_room_name, ChatConfig, RoomSpec};
use super::events::{AfterChatDelete, AfterChatSend, BeforeChatJoin, BeforeChatSend, BeforeDirectOpen};
use super::presence::Presence;
use super::store::{self, IdRow, MessageRow, NameRow, RoomRow, KIND_DM, KIND_GROUP, KIND_ROOM};
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

fn kind_of(row: &RoomRow) -> RoomKind {
    match row.kind.as_str() {
        KIND_DM => RoomKind::Dm,
        KIND_GROUP => RoomKind::Group,
        _ => RoomKind::Room,
    }
}

fn not_a_member() -> AppError {
    AppError::new(codes::NOT_A_MEMBER, "join the room first (or: not a member of this room)")
}

fn page_limit(page: &PageRequest) -> Result<u64, AppError> {
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
pub struct ChatService(Arc<Inner>);

struct Inner {
    config: ChatConfig,
    rate: KeyedBuckets<UserId>,
    /// DM opens per user (`dm_open_rate`; `None` = no limit).
    dm_rate: Option<KeyedBuckets<UserId>>,
    presence: Presence,
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
        let presence = Presence::new(config.presence, config.presence_max_members, config.presence_per_second);
        Self(Arc::new(Inner { config, rate, dm_rate, presence, rooms: RwLock::new(HashMap::new()) }))
    }

    /// The settings.
    pub fn config(&self) -> &ChatConfig {
        &self.0.config
    }

    fn ctx(state: &AppState) -> HookCtx {
        HookCtx::new(state.clone(), None)
    }

    // ---- rooms ----------------------------------------------------------------------------------

    fn cached(&self, id: i64) -> Option<RoomRow> {
        let rooms = self.0.rooms.read().unwrap_or_else(|p| p.into_inner());
        rooms.get(&id).filter(|(at, _)| at.elapsed() < ROOM_CACHE_TTL).map(|(_, row)| row.clone())
    }

    fn remember(&self, row: &RoomRow) {
        let mut rooms = self.0.rooms.write().unwrap_or_else(|p| p.into_inner());
        if rooms.len() >= ROOM_CACHE {
            rooms.clear();
        }
        rooms.insert(row.id, (Instant::now(), row.clone()));
    }

    fn forget(&self, id: i64) {
        self.0.rooms.write().unwrap_or_else(|p| p.into_inner()).remove(&id);
    }

    async fn room_row(&self, state: &AppState, room: RoomId) -> Result<RoomRow, AppError> {
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
            None if row.kind == KIND_GROUP => self.0.config.max_group_members,
            None => self.0.config.max_room_members,
        }
    }

    fn info(&self, row: &RoomRow, viewer: Option<UserId>) -> RoomInfo {
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
        info
    }

    fn is_participant(row: &RoomRow, user: UserId) -> bool {
        row.dm_a == Some(user.get()) || row.dm_b == Some(user.get())
    }

    async fn is_group_member(state: &AppState, row: &RoomRow, user: UserId) -> Result<bool, AppError> {
        Ok(state.db().fetch_optional::<IdRow, _>(&store::member(row.id, user.get())).await?.is_some())
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
        let mut seen = HashSet::new();
        let members: Vec<UserId> = members.iter().copied().filter(|m| seen.insert(*m)).collect();
        let now = state.now().get();
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        let id = loop {
            let mut tx = state.db().begin_write().await?;
            let result = async {
                let id = tx.insert_id(&store::insert_room(KIND_GROUP, None, name, None, None, now)?, "id").await?;
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
                other => break other?,
            }
        };
        let row = self.room_row(state, RoomId(id)).await?;
        Ok(self.info(&row, None))
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

    /// Join `room` on `connection` (public rooms; group rooms for their members; a DM room needs
    /// no join and answers its info).
    pub(crate) async fn join(&self, state: &AppState, ctx: &HookCtx, connection: ConnectionId, user: UserId, room: &RoomRef) -> Result<RoomInfo, AppError> {
        let row = self.resolve(state, room).await?;
        let group = row.kind == KIND_GROUP;
        match row.kind.as_str() {
            KIND_DM => {
                return if Self::is_participant(&row, user) { Ok(self.info(&row, Some(user))) } else { Err(not_a_member()) };
            }
            KIND_GROUP if !Self::is_group_member(state, &row, user).await? => return Err(not_a_member()),
            _ => {}
        }
        let event = BeforeChatJoin { room: RoomId(row.id), kind: kind_of(&row), key: row.room_key.clone(), user_id: user };
        state.hooks().run_before(ctx, event).await?;
        // Everything that can fail runs before the hub join: an error never leaves the socket joined.
        let name = state.db().fetch_all::<NameRow, _>(&store::names(&[user.get()])).await?.into_iter().next().and_then(|r| r.display_name);
        let hub = state.ws();
        let room_id = RoomId(row.id);
        let cap = usize::try_from(self.cap_of(&row)).unwrap_or(usize::MAX);
        if hub.join_with_cap(connection, hub_room(room_id), cap)? {
            // A removal that ran between the membership check and the hub join saw no socket to
            // take out: check again now that the socket is in (the removal deletes the row first).
            if group {
                let still = Self::is_group_member(state, &row, user).await;
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
                joined && (kind != KIND_GROUP || Self::is_group_member(state, &row, user).await?)
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
        let event = state.hooks().run_before(ctx, BeforeChatSend { room, kind, sender: user, text, nonce: nonce.clone() }).await?;
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

    fn push_message<P: net_backend_protocol::ServerPush>(&self, hub: &Hub, row: &RoomRow, push: &P) -> Result<(), AppError> {
        if row.kind == KIND_DM {
            for user in [row.dm_a, row.dm_b].into_iter().flatten() {
                hub.push_user(UserId(user), push)?;
            }
        } else {
            hub.push_room(&hub_room(RoomId(row.id)), push)?;
        }
        Ok(())
    }

    /// A page of a room's history, newest first (public rooms: anyone; group and DM rooms: their
    /// members). A message's `nonce` is shown to its sender only.
    pub async fn history(&self, state: &AppState, user: UserId, room: RoomId, page: &PageRequest) -> Result<Page<ChatMessage>, AppError> {
        let limit = page_limit(page)?;
        let row = self.room_row(state, room).await?;
        let allowed = match row.kind.as_str() {
            KIND_DM => Self::is_participant(&row, user),
            KIND_GROUP => Self::is_group_member(state, &row, user).await?,
            _ => true,
        };
        if !allowed {
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
        let moderator = actor.is_some_and(|a| self.0.config.moderator_roles.iter().any(|role| a.has_role(role)));
        let own = actor.is_some_and(|a| a.user_id == sender);
        let allowed = actor.is_none() || moderator || (own && self.0.config.allow_self_delete);
        if !allowed {
            return Err(AppError::forbidden("only its sender or a moderator may delete a message"));
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
