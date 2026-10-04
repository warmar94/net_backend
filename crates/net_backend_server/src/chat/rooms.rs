//! Rooms created by players: create, rename / change the visibility, delete, join (public, or an
//! invitation), leave, invite, kick (a ban until invited again), roles (owner, moderator, member),
//! hand on, list.
//!
//! **Storage:** a `chat_rooms` row of kind `player` (`is_public`), one `chat_members` row per
//! player with a role: `owner`, `moderator`, `member`, `invited` or `banned`. Members and open
//! invitations take a place (`max_player_room_members`); the owner row counts towards the owner's
//! `max_rooms_per_player`, which binds creating a room and handing one on (a player who becomes
//! the owner because the owner left is not counted).
//!
//! **Writes** are one transaction each (run again on a reported deadlock): the player's account
//! lock first when the player's own count matters (create, hand on), then the room row's lock,
//! then plain reads and the change by primary key. So a full room or a player owning too many
//! rooms can never be passed by racing requests (the groups pattern).
//!
//! **After a change** (the commit): the sockets of a player who left or was kicked leave the hub
//! room on every instance ([`Hub::remove_from_room`](crate::ws::Hub::remove_from_room)), every
//! member and invited player gets a `chat.room` push, the [`AfterRoomChange`] hooks run.
//!
//! **Upkeep** (the module's purge task): a player room whose owner's account was deleted gets its
//! oldest moderator (else member) as owner; a room with no member left is deleted.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use net_backend_protocol::chat::{CreateRoom, RoomChange, RoomInfo, RoomMembership, RoomRole, RoomUpdate, RoomVisibility, UpdateRoom};
use net_backend_protocol::{codes, Cursor, Page, PageRequest, RoomId, UnixMillis, UserId, ValidationDetails};
use serde_json::json;

use super::events::{AfterRoomChange, BeforeChatJoin, BeforeRoomCreate, BeforeRoomInvite, BeforeRoomUpdate};
use super::service::{hub_room, kind_of, not_a_member, page_limit, ChatService};
use super::store::{
    self, CountRow, IdRow, MemberRow, NameRow, RoomRow, ACTIVE, BANNED, INVITED, KIND_DM, KIND_GROUP, KIND_PLAYER, MEMBER, MODERATOR, OWNER, PLACES,
};
use crate::auth::audit::{self, AuditRecord};
use crate::auth::AuthContext;
use crate::db::{DbTx, Retry};
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::rate_limit::RateDecision;
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

/// Every row of a room, at most (a room never has this many).
const ALL_ROWS: u64 = 1_000_000;

pub(super) fn role_of(role: &str) -> RoomRole {
    match role {
        OWNER => RoomRole::Owner,
        MODERATOR => RoomRole::Moderator,
        MEMBER => RoomRole::Member,
        INVITED => RoomRole::Invited,
        BANNED => RoomRole::Banned,
        _ => RoomRole::Unknown,
    }
}

fn quota(message: &str) -> AppError {
    AppError::new(codes::QUOTA_EXCEEDED, message)
}

fn invalid(field: &str, problem: &str) -> AppError {
    let mut details = ValidationDetails::new();
    details.add(field, problem);
    AppError::validation(details)
}

fn count(row: CountRow) -> u64 {
    u64::try_from(row.n).unwrap_or(0)
}

fn only_player_rooms() -> AppError {
    AppError::bad_request("only rooms created by players have this (a public room: chat.join; a group's room: the groups module)")
}

fn is_staff_role(role: &str) -> bool {
    role == OWNER || role == MODERATOR
}

/// Who left a room (or was kicked), and by whom.
struct Departure {
    room: RoomId,
    user: UserId,
    actor: Option<UserId>,
    change: RoomChange,
}

/// What a leave or a kick did (pushed after the commit).
enum Outcome {
    /// Nothing changed.
    Nothing,
    /// The player left (or was kicked); `successor` became the owner.
    Done { successor: Option<UserId> },
    /// The room was deleted (no member left); these players were in it (invitations).
    Deleted { others: Vec<UserId> },
}

impl ChatService {
    // ---- helpers ----------------------------------------------------------------------------------

    async fn lock_user(tx: &mut DbTx, user: i64) -> Result<(), AppError> {
        let dialect = tx.dialect();
        match tx.fetch_optional::<IdRow, _>(&store::lock_user(user, dialect)).await? {
            Some(_) => Ok(()),
            None => Err(AppError::not_found("no such account")),
        }
    }

    /// The player room's row, locked for the transaction (404: gone or not a player room).
    async fn locked_room(tx: &mut DbTx, room: i64) -> Result<RoomRow, AppError> {
        let dialect = tx.dialect();
        let row = tx.fetch_optional::<RoomRow, _>(&store::lock_room(room, dialect)).await?.ok_or_else(|| AppError::not_found("no such room"))?;
        if row.kind == KIND_PLAYER {
            Ok(row)
        } else {
            Err(only_player_rooms())
        }
    }

    async fn row_in(tx: &mut DbTx, room: i64, user: UserId) -> Result<Option<MemberRow>, AppError> {
        Ok(tx.fetch_optional::<MemberRow, _>(&store::member(room, user.get())).await?)
    }

    /// The next owner after `leaving`: the oldest moderator, else the oldest member.
    async fn successor(tx: &mut DbTx, room: i64) -> Result<Option<MemberRow>, AppError> {
        for role in [MODERATOR, MEMBER] {
            if let Some(row) = tx.fetch_optional::<MemberRow, _>(&store::oldest_with(room, role)).await? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    /// A player room (404 for an unknown room, 400 for another kind).
    async fn player_row(&self, state: &AppState, room: RoomId) -> Result<RoomRow, AppError> {
        let row = self.room_row(state, room).await?;
        if row.kind == KIND_PLAYER {
            Ok(row)
        } else {
            Err(only_player_rooms())
        }
    }

    /// Whether `actor` holds `chat.moderate` (acts as the owner of every player room).
    pub(crate) fn may_moderate(state: &AppState, actor: &AuthContext) -> bool {
        actor.has_permission(state, super::MODERATE.name())
    }

    /// A player room as `viewer` sees it: the owner and the viewer's role.
    pub(super) async fn player_info(&self, state: &AppState, row: &RoomRow, viewer: Option<UserId>) -> Result<RoomInfo, AppError> {
        Ok(self.player_infos(state, std::slice::from_ref(row), viewer, None).await?.pop().unwrap_or_else(|| self.info(row, viewer)))
    }

    /// Player rooms as `viewer` sees them (`roles`: the viewer's rows when known).
    async fn player_infos(
        &self,
        state: &AppState,
        rows: &[RoomRow],
        viewer: Option<UserId>,
        roles: Option<&HashMap<i64, String>>,
    ) -> Result<Vec<RoomInfo>, AppError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let db = state.db();
        let owners: HashMap<i64, i64> =
            db.fetch_all::<MemberRow, _>(&store::with_role_in(&ids, OWNER)).await?.into_iter().map(|m| (m.room_id, m.user_id)).collect();
        let fetched: HashMap<i64, String>;
        let roles = match (roles, viewer) {
            (Some(roles), _) => Some(roles),
            (None, Some(viewer)) => {
                fetched = db.fetch_all::<MemberRow, _>(&store::rows_of_user_in(viewer.get(), &ids)).await?.into_iter().map(|m| (m.room_id, m.role)).collect();
                Some(&fetched)
            }
            (None, None) => None,
        };
        Ok(rows
            .iter()
            .map(|row| {
                let mut info = self.info(row, viewer);
                if let Some(owner) = owners.get(&row.id) {
                    info = info.with_owner(UserId(*owner));
                }
                if let Some(role) = roles.and_then(|r| r.get(&row.id)) {
                    info = info.with_role(role_of(role));
                }
                info
            })
            .collect())
    }

    /// The players a room's pushes go to: members and invited players.
    async fn audience(state: &AppState, room: i64) -> Result<Vec<UserId>, AppError> {
        Ok(state.db().fetch_all::<MemberRow, _>(&store::room_rows(room, &PLACES, None, ALL_ROWS)).await?.into_iter().map(|m| UserId(m.user_id)).collect())
    }

    /// Push `update` to `users` (each once).
    fn push_to(state: &AppState, users: &[UserId], update: &RoomUpdate) {
        let mut seen = HashSet::new();
        for user in users.iter().filter(|u| seen.insert(**u)) {
            if let Err(error) = state.ws().push_user(*user, update) {
                tracing::warn!(%error, "chat: a room update push failed");
            }
        }
    }

    /// Push `update` to the room's members and invited players, plus `extra`.
    async fn announce(&self, state: &AppState, room: i64, update: RoomUpdate, extra: &[UserId]) {
        match Self::audience(state, room).await {
            Ok(mut users) => {
                users.extend_from_slice(extra);
                Self::push_to(state, &users, &update);
            }
            Err(error) => tracing::warn!(%error, room, "chat: reading a room's members for a push failed"),
        }
    }

    async fn after_change(state: &AppState, ctx: &HookCtx, event: AfterRoomChange) {
        state.hooks().run_after(ctx, Arc::new(event)).await;
    }

    fn change(room: RoomId, change: RoomChange, user: Option<UserId>, actor: Option<UserId>, role: Option<RoomRole>) -> AfterRoomChange {
        AfterRoomChange { room, change, created: false, user, actor, role }
    }

    /// Cut a player's sockets out of the room (every instance).
    fn cut(state: &AppState, room: RoomId, user: UserId) {
        if let Err(error) = state.ws().remove_from_room(user, &hub_room(room)) {
            tracing::warn!(%error, "chat: removing a player's sockets from a room failed");
        }
    }

    // ---- create, read, change, delete -------------------------------------------------------------

    /// `user` creates a player room and owns it: `player_rooms` on, the create rate, the
    /// [`BeforeRoomCreate`] hooks, then one transaction (the owner's room count).
    pub async fn create_player_room(&self, state: &AppState, ctx: &HookCtx, user: UserId, request: CreateRoom) -> Result<RoomInfo, AppError> {
        if !self.0.config.player_rooms {
            return Err(AppError::forbidden("players cannot create rooms on this server"));
        }
        request.validate()?;
        if let RateDecision::Deny { retry_after_ms } = self.0.create_rate.check(user) {
            return Err(AppError::rate_limited(retry_after_ms));
        }
        let event = state.hooks().run_before(ctx, BeforeRoomCreate { user_id: user, name: request.name, visibility: request.visibility }).await?;
        let mut checked = CreateRoom::new(event.name.trim());
        checked.visibility = event.visibility;
        checked.validate()?;
        let public = checked.visibility == RoomVisibility::Public;
        let max = u64::from(self.0.config.max_rooms_per_player);
        let now = state.now().get();
        let id = in_tx!(state, |tx| async {
            Self::lock_user(&mut tx, user.get()).await?;
            if count(tx.fetch_one::<CountRow, _>(&store::count_owned(user.get())).await?) >= max {
                return Err(quota("you own too many rooms"));
            }
            let id = tx.insert_id(&store::insert_player_room(&checked.name, public, now)?, "id").await?;
            tx.execute(&store::insert_member_role(id, user.get(), OWNER, now)?).await?;
            Ok(id)
        })?;
        let mut created = Self::change(RoomId(id), RoomChange::Joined, Some(user), Some(user), Some(RoomRole::Owner));
        created.created = true;
        Self::after_change(state, ctx, created).await;
        let row = self.room_row(state, RoomId(id)).await?;
        self.player_info(state, &row, Some(user)).await
    }

    /// One room as `user` sees it: public rooms for anyone, DM and group rooms for their members,
    /// player rooms for their members and invited players (public ones for anyone).
    pub async fn get_room(&self, state: &AppState, user: UserId, room: RoomId) -> Result<RoomInfo, AppError> {
        self.get_room_as(state, user, false, room).await
    }

    /// [`get_room`](Self::get_room) for a player: `staff` (a holder of `chat.moderate`) sees every
    /// player room.
    pub(crate) async fn get_room_as(&self, state: &AppState, user: UserId, staff: bool, room: RoomId) -> Result<RoomInfo, AppError> {
        let row = self.room_row(state, room).await?;
        match row.kind.as_str() {
            KIND_DM if !Self::is_participant(&row, user) => Err(not_a_member()),
            KIND_GROUP if !Self::is_member(state, &row, user).await? => Err(not_a_member()),
            KIND_PLAYER => {
                let mine = Self::member_row(state, &row, user).await?;
                let visible = staff || row.is_public == 1 || mine.as_ref().is_some_and(|m| PLACES.contains(&m.role.as_str()));
                if !visible {
                    return Err(not_a_member());
                }
                self.player_info(state, &row, Some(user)).await
            }
            _ => Ok(self.info(&row, Some(user))),
        }
    }

    /// Rename a player room (owner, moderators) or change its visibility (owner); staff with
    /// `chat.moderate` may do both. The [`BeforeRoomUpdate`] hooks may change or refuse.
    pub(crate) async fn update_player_room(
        &self,
        state: &AppState,
        ctx: &HookCtx,
        actor: &AuthContext,
        room: RoomId,
        update: UpdateRoom,
    ) -> Result<RoomInfo, AppError> {
        update.validate()?;
        let row = self.player_row(state, room).await?;
        let user = actor.user_id;
        if !Self::may_moderate(state, actor) {
            let role = Self::member_row(state, &row, user).await?.map(|m| m.role);
            match role.as_deref() {
                Some(OWNER) => {}
                Some(MODERATOR) if update.visibility.is_none() => {}
                Some(MODERATOR) => return Err(AppError::forbidden("only the owner changes the visibility")),
                Some(MEMBER) => return Err(AppError::forbidden("only the owner and moderators change the room")),
                _ => return Err(not_a_member()),
            }
        }
        let event = BeforeRoomUpdate { room, user_id: user, name: update.name, visibility: update.visibility };
        let event = state.hooks().run_before(ctx, event).await?;
        let mut checked = UpdateRoom::new();
        checked.name = event.name.map(|n| n.trim().to_string());
        checked.visibility = event.visibility;
        checked.validate()?;
        let public = checked.visibility.map(|v| v == RoomVisibility::Public);
        in_tx!(state, |tx| async {
            Self::locked_room(&mut tx, row.id).await?;
            tx.execute(&store::update_player_room(row.id, checked.name.as_deref(), public)).await?;
            Ok(())
        })?;
        self.forget(row.id);
        let row = self.room_row(state, room).await?;
        let shared = self.player_info(state, &row, None).await?;
        self.announce(state, row.id, RoomUpdate::new(room, RoomChange::Updated).by(user).with_info(shared), &[]).await;
        Self::after_change(state, ctx, Self::change(room, RoomChange::Updated, None, Some(user), None)).await;
        self.player_info(state, &row, Some(user)).await
    }

    /// Delete a player room (its owner, or staff with `chat.moderate`: audited as
    /// `chat.room_deleted`; `None`: server code): its messages, members and markers go with it,
    /// every socket leaves it, members and invited players get `chat.room` `deleted`.
    pub(crate) async fn delete_player_room(&self, state: &AppState, ctx: &HookCtx, actor: Option<&AuthContext>, room: RoomId) -> Result<(), AppError> {
        let row = self.player_row(state, room).await?;
        let staff = actor.is_some_and(|a| Self::may_moderate(state, a));
        let owner = match actor {
            Some(a) if !staff => {
                let mine = Self::member_row(state, &row, a.user_id).await?;
                match mine.map(|m| m.role) {
                    Some(role) if role == OWNER => true,
                    Some(role) if ACTIVE.contains(&role.as_str()) => return Err(AppError::forbidden("only the owner deletes the room")),
                    _ => return Err(not_a_member()),
                }
            }
            _ => false,
        };
        let users = Self::audience(state, row.id).await?;
        let by = actor.map(|a| a.user_id);
        let now = state.now().get();
        in_tx!(state, |tx| async {
            Self::locked_room(&mut tx, row.id).await?;
            tx.execute(&store::delete_room(row.id)).await?;
            if !owner {
                let mut record = AuditRecord::new("chat.room_deleted").actor(by);
                record = record.request_id(ctx.request_id()).data(json!({ "room": row.id, "name": row.name }));
                audit::record_tx(&mut tx, UnixMillis(now), &record).await?;
            }
            Ok(())
        })?;
        self.forget(row.id);
        for user in &users {
            Self::cut(state, room, *user);
        }
        let mut update = RoomUpdate::new(room, RoomChange::Deleted);
        if let Some(by) = by {
            update = update.by(by);
        }
        Self::push_to(state, &users, &update);
        Self::after_change(state, ctx, Self::change(room, RoomChange::Deleted, None, by, None)).await;
        Ok(())
    }

    /// Delete a player room from server code (audited without an actor).
    pub async fn delete_room(&self, state: &AppState, room: RoomId) -> Result<(), AppError> {
        self.delete_player_room(state, &Self::ctx(state), None, room).await
    }

    // ---- membership -------------------------------------------------------------------------------

    /// Make `user` a member of a player room (or nothing if it is one): a public room or an
    /// invitation; a ban or a private room without an invitation is refused, except for `staff`
    /// (a holder of `chat.moderate`: it joins every player room). Runs [`BeforeChatJoin`] (also
    /// for a member: a connection joins after this).
    pub(super) async fn become_member(&self, state: &AppState, ctx: &HookCtx, row: &RoomRow, user: UserId, staff: bool) -> Result<(), AppError> {
        let room = RoomId(row.id);
        let mine = Self::member_row(state, row, user).await?;
        match mine.as_ref().map(|m| m.role.as_str()) {
            _ if staff => {}
            Some(BANNED) => return Err(AppError::forbidden("you were removed from this room (an invitation lets you back in)")),
            None if row.is_public != 1 => return Err(not_a_member()),
            _ => {}
        }
        let event = BeforeChatJoin { room, kind: kind_of(row), key: None, user_id: user };
        state.hooks().run_before(ctx, event).await?;
        if mine.as_ref().is_some_and(|m| ACTIVE.contains(&m.role.as_str())) {
            return Ok(());
        }
        let max = u64::from(self.0.config.max_player_room_members);
        let now = state.now().get();
        let joined = in_tx!(state, |tx| async {
            let locked = Self::locked_room(&mut tx, row.id).await?;
            match Self::row_in(&mut tx, row.id, user).await? {
                Some(m) if ACTIVE.contains(&m.role.as_str()) => Ok(false),
                Some(m) if m.role == BANNED && !staff => Err(AppError::forbidden("you were removed from this room (an invitation lets you back in)")),
                Some(m) if m.role == BANNED => {
                    if count(tx.fetch_one::<CountRow, _>(&store::count_rows(row.id, &PLACES)).await?) >= max {
                        return Err(quota("this room is full"));
                    }
                    tx.execute(&store::set_role(m.id, MEMBER, now)).await?;
                    Ok(true)
                }
                Some(m) => {
                    // An invitation: its place is taken already.
                    tx.execute(&store::set_role(m.id, MEMBER, now)).await?;
                    Ok(true)
                }
                None if locked.is_public != 1 && !staff => Err(not_a_member()),
                None => {
                    if count(tx.fetch_one::<CountRow, _>(&store::count_rows(row.id, &PLACES)).await?) >= max {
                        return Err(quota("this room is full"));
                    }
                    tx.execute(&store::insert_member_role(row.id, user.get(), MEMBER, now)?).await?;
                    Ok(true)
                }
            }
        })?;
        if joined {
            self.announce(state, row.id, RoomUpdate::new(room, RoomChange::Joined).about(user).by(user), &[]).await;
            Self::after_change(state, ctx, Self::change(room, RoomChange::Joined, Some(user), Some(user), Some(RoomRole::Member))).await;
        }
        Ok(())
    }

    /// `POST …/join`: become a member (public room, or accept an invitation; `staff`: any room).
    pub(crate) async fn join_player_room(&self, state: &AppState, ctx: &HookCtx, user: UserId, staff: bool, room: RoomId) -> Result<RoomInfo, AppError> {
        let row = self.player_row(state, room).await?;
        self.become_member(state, ctx, &row, user, staff).await?;
        self.player_info(state, &row, Some(user)).await
    }

    /// `user` stops being a member (or declines an invitation; a ban stays). The owner's room goes
    /// to the oldest moderator, else the oldest member; without one it is deleted.
    pub async fn leave_player_room(&self, state: &AppState, ctx: &HookCtx, user: UserId, room: RoomId) -> Result<(), AppError> {
        let row = self.player_row(state, room).await?;
        let outcome = in_tx!(state, |tx| async {
            Self::locked_room(&mut tx, row.id).await?;
            let Some(mine) = Self::row_in(&mut tx, row.id, user).await? else { return Ok(Outcome::Nothing) };
            if mine.role == BANNED {
                return Ok(Outcome::Nothing);
            }
            tx.execute(&store::delete_member_row(mine.id)).await?;
            Self::after_leaving(&mut tx, row.id, &mine).await
        })?;
        self.finish_departure(state, ctx, Departure { room, user, actor: Some(user), change: RoomChange::Left }, outcome).await;
        Ok(())
    }

    /// After `gone` left (or was banned) inside a transaction: a new owner if it was the owner;
    /// the room deleted when no member is left.
    async fn after_leaving(tx: &mut DbTx, room: i64, gone: &MemberRow) -> Result<Outcome, AppError> {
        if gone.role != OWNER {
            return Ok(Outcome::Done { successor: None });
        }
        match Self::successor(tx, room).await? {
            Some(next) => {
                tx.execute(&store::change_role(next.id, OWNER)).await?;
                Ok(Outcome::Done { successor: Some(UserId(next.user_id)) })
            }
            None => {
                let others =
                    tx.fetch_all::<MemberRow, _>(&store::room_rows(room, &PLACES, None, ALL_ROWS)).await?.into_iter().map(|m| UserId(m.user_id)).collect();
                tx.execute(&store::delete_room(room)).await?;
                Ok(Outcome::Deleted { others })
            }
        }
    }

    /// The pushes, cuts and hooks of a leave or a kick.
    async fn finish_departure(&self, state: &AppState, ctx: &HookCtx, departure: Departure, outcome: Outcome) {
        let Departure { room, user, actor, change } = departure;
        let mut update = RoomUpdate::new(room, change).about(user);
        if let Some(actor) = actor {
            update = update.by(actor);
        }
        match outcome {
            Outcome::Nothing => {}
            Outcome::Done { successor } => {
                Self::cut(state, room, user);
                self.announce(state, room.get(), update, &[user]).await;
                Self::after_change(state, ctx, Self::change(room, change, Some(user), actor, None)).await;
                if let Some(next) = successor {
                    self.announce(state, room.get(), RoomUpdate::new(room, RoomChange::Owner).about(next).with_role(RoomRole::Owner), &[]).await;
                    Self::after_change(state, ctx, Self::change(room, RoomChange::Owner, Some(next), None, Some(RoomRole::Owner))).await;
                }
            }
            Outcome::Deleted { others } => {
                self.forget(room.get());
                Self::cut(state, room, user);
                Self::push_to(state, &[user], &update);
                let mut all = others;
                all.push(user);
                Self::push_to(state, &all, &RoomUpdate::new(room, RoomChange::Deleted));
                Self::after_change(state, ctx, Self::change(room, change, Some(user), actor, None)).await;
                Self::after_change(state, ctx, Self::change(room, RoomChange::Deleted, None, None, None)).await;
            }
        }
    }

    /// Whether `role` (the actor's row) may invite: the owner and moderators.
    fn may_invite(role: Option<&str>) -> Result<(), AppError> {
        match role {
            Some(role) if is_staff_role(role) => Ok(()),
            Some(MEMBER) => Err(AppError::forbidden("only the owner and moderators invite")),
            _ => Err(not_a_member()),
        }
    }

    /// Invite `invitee` (owner, moderators, staff with `chat.moderate`): a place is taken; a ban is
    /// lifted; a member or an open invitation is no change. Refused (403 `forbidden`) when the
    /// invitee blocked the inviter (with the friends module); the [`BeforeRoomInvite`] hooks run
    /// after the rights check and may refuse; the invited player gets a `chat.invite`
    /// notification (with the notifications module). The rate is the route's.
    pub(crate) async fn invite_to_room(&self, state: &AppState, ctx: &HookCtx, actor: &AuthContext, room: RoomId, invitee: UserId) -> Result<(), AppError> {
        let row = self.player_row(state, room).await?;
        let user = actor.user_id;
        if invitee == user {
            return Err(AppError::bad_request("you cannot invite yourself"));
        }
        let staff = Self::may_moderate(state, actor);
        if !staff {
            Self::may_invite(Self::member_row(state, &row, user).await?.map(|m| m.role).as_deref())?;
        }
        if blocks::blocked(state, invitee, user).await? {
            return Err(AppError::forbidden("this player does not take invitations from you"));
        }
        state.hooks().run_before(ctx, BeforeRoomInvite { room, user_id: user, invitee }).await?;
        let max = u64::from(self.0.config.max_player_room_members);
        let now = state.now().get();
        let changed = in_tx!(state, |tx| async {
            // The invitee's account first, then the room (the order of `transfer_room`).
            Self::lock_user(&mut tx, invitee.get()).await?;
            Self::locked_room(&mut tx, row.id).await?;
            if !staff {
                Self::may_invite(Self::row_in(&mut tx, row.id, user).await?.map(|m| m.role).as_deref())?;
            }
            match Self::row_in(&mut tx, row.id, invitee).await? {
                Some(m) if m.role == BANNED => {
                    if count(tx.fetch_one::<CountRow, _>(&store::count_rows(row.id, &PLACES)).await?) >= max {
                        return Err(quota("this room is full"));
                    }
                    tx.execute(&store::set_role(m.id, INVITED, now)).await?;
                    Ok(true)
                }
                Some(_) => Ok(false),
                None => {
                    if count(tx.fetch_one::<CountRow, _>(&store::count_rows(row.id, &PLACES)).await?) >= max {
                        return Err(quota("this room is full"));
                    }
                    tx.execute(&store::insert_member_role(row.id, invitee.get(), INVITED, now)?).await?;
                    Ok(true)
                }
            }
        })?;
        if changed {
            self.announce(state, row.id, RoomUpdate::new(room, RoomChange::Invited).about(invitee).by(user), &[]).await;
            self.notify_invite(state, ctx, invitee, user, &row).await;
            Self::after_change(state, ctx, Self::change(room, RoomChange::Invited, Some(invitee), Some(user), Some(RoomRole::Invited))).await;
        }
        Ok(())
    }

    /// Kick `target` (owner: anyone; moderators: members and invited players; staff with
    /// `chat.moderate`: anyone): banned until invited again; its sockets leave the room. An
    /// invitation is withdrawn the same way. Kicking the owner (staff) hands the room on.
    pub(crate) async fn kick_from_room(&self, state: &AppState, ctx: &HookCtx, actor: &AuthContext, room: RoomId, target: UserId) -> Result<(), AppError> {
        let row = self.player_row(state, room).await?;
        let user = actor.user_id;
        if target == user {
            return Err(AppError::bad_request("leave the room instead"));
        }
        let staff = Self::may_moderate(state, actor);
        let now = state.now().get();
        let outcome = in_tx!(state, |tx| async {
            Self::locked_room(&mut tx, row.id).await?;
            // The actor's rights first: nobody else learns who is banned (or in the room).
            let mine = if staff { None } else { Some(Self::row_in(&mut tx, row.id, user).await?.ok_or_else(not_a_member)?) };
            match mine.as_ref().map(|m| m.role.as_str()) {
                None | Some(OWNER | MODERATOR) => {}
                Some(MEMBER) => return Err(AppError::forbidden("only the owner and moderators kick")),
                Some(_) => return Err(not_a_member()),
            }
            let theirs = Self::row_in(&mut tx, row.id, target).await?.ok_or_else(|| AppError::not_found("the player is not in this room"))?;
            if theirs.role == BANNED {
                return Ok(Outcome::Nothing);
            }
            if mine.as_ref().is_some_and(|m| m.role == MODERATOR) && theirs.role != MEMBER && theirs.role != INVITED {
                return Err(AppError::forbidden("moderators kick members and invited players only"));
            }
            tx.execute(&store::set_role(theirs.id, BANNED, now)).await?;
            Self::after_leaving(&mut tx, row.id, &theirs).await
        })?;
        self.finish_departure(state, ctx, Departure { room, user: target, actor: Some(user), change: RoomChange::Kicked }, outcome).await;
        Ok(())
    }

    /// Make a member a moderator or a member again (the owner, or staff with `chat.moderate`).
    pub(crate) async fn set_room_role(
        &self,
        state: &AppState,
        ctx: &HookCtx,
        actor: &AuthContext,
        room: RoomId,
        target: UserId,
        role: RoomRole,
    ) -> Result<(), AppError> {
        let new = match role {
            RoomRole::Moderator => MODERATOR,
            RoomRole::Member => MEMBER,
            _ => return Err(invalid("role", "must be moderator or member (hand the room on with the owner route)")),
        };
        let row = self.player_row(state, room).await?;
        let user = actor.user_id;
        let staff = Self::may_moderate(state, actor);
        let changed = in_tx!(state, |tx| async {
            Self::locked_room(&mut tx, row.id).await?;
            if !staff {
                match Self::row_in(&mut tx, row.id, user).await? {
                    Some(m) if m.role == OWNER => {}
                    Some(m) if ACTIVE.contains(&m.role.as_str()) => return Err(AppError::forbidden("only the owner sets roles")),
                    _ => return Err(not_a_member()),
                }
            }
            let theirs = Self::row_in(&mut tx, row.id, target).await?;
            match theirs {
                Some(m) if m.role == OWNER => Err(AppError::bad_request("the owner's role changes by handing the room on")),
                Some(m) if m.role == new => Ok(false),
                Some(m) if m.role == MODERATOR || m.role == MEMBER => {
                    tx.execute(&store::change_role(m.id, new)).await?;
                    Ok(true)
                }
                _ => Err(AppError::not_found("the player is not a member of this room")),
            }
        })?;
        if changed {
            self.announce(state, row.id, RoomUpdate::new(room, RoomChange::Role).about(target).by(user).with_role(role), &[]).await;
            Self::after_change(state, ctx, Self::change(room, RoomChange::Role, Some(target), Some(user), Some(role))).await;
        }
        Ok(())
    }

    /// Hand a player room to a member (the owner, or staff with `chat.moderate`); the old owner
    /// becomes a moderator. The new owner's room count is checked.
    pub(crate) async fn transfer_room(&self, state: &AppState, ctx: &HookCtx, actor: &AuthContext, room: RoomId, to: UserId) -> Result<(), AppError> {
        let row = self.player_row(state, room).await?;
        let user = actor.user_id;
        if to == user {
            return Err(AppError::bad_request("you own the room already"));
        }
        let staff = Self::may_moderate(state, actor);
        let max = u64::from(self.0.config.max_rooms_per_player);
        let old = in_tx!(state, |tx| async {
            Self::lock_user(&mut tx, to.get()).await?;
            Self::locked_room(&mut tx, row.id).await?;
            let mine = Self::row_in(&mut tx, row.id, user).await?;
            if !staff && mine.as_ref().is_none_or(|m| m.role != OWNER) {
                return Err(if mine.as_ref().is_some_and(|m| ACTIVE.contains(&m.role.as_str())) {
                    AppError::forbidden("only the owner hands the room on")
                } else {
                    not_a_member()
                });
            }
            let theirs = Self::row_in(&mut tx, row.id, to).await?;
            let theirs = match theirs {
                Some(m) if m.role == MODERATOR || m.role == MEMBER => m,
                Some(m) if m.role == OWNER => return Ok(None),
                _ => return Err(AppError::not_found("the player is not a member of this room")),
            };
            if count(tx.fetch_one::<CountRow, _>(&store::count_owned(to.get())).await?) >= max {
                return Err(quota("the player owns too many rooms"));
            }
            let current = tx.fetch_optional::<MemberRow, _>(&store::oldest_with(row.id, OWNER)).await?;
            if let Some(current) = &current {
                tx.execute(&store::change_role(current.id, MODERATOR)).await?;
            }
            tx.execute(&store::change_role(theirs.id, OWNER)).await?;
            Ok(Some(current.map(|c| UserId(c.user_id))))
        })?;
        let Some(old) = old else { return Ok(()) };
        self.announce(state, row.id, RoomUpdate::new(room, RoomChange::Owner).about(to).by(user).with_role(RoomRole::Owner), &[]).await;
        Self::after_change(state, ctx, Self::change(room, RoomChange::Owner, Some(to), Some(user), Some(RoomRole::Owner))).await;
        if let Some(old) = old {
            self.announce(state, row.id, RoomUpdate::new(room, RoomChange::Role).about(old).by(user).with_role(RoomRole::Moderator), &[]).await;
            Self::after_change(state, ctx, Self::change(room, RoomChange::Role, Some(old), Some(user), Some(RoomRole::Moderator))).await;
        }
        Ok(())
    }

    // ---- lists ------------------------------------------------------------------------------------

    /// A page of a player room's rows, oldest first: members and invited players for its members
    /// and invited players; bans too for the owner, moderators and staff with `chat.moderate`.
    pub(crate) async fn room_members(&self, state: &AppState, actor: &AuthContext, room: RoomId, page: &PageRequest) -> Result<Page<RoomMembership>, AppError> {
        let limit = page_limit(page)?;
        let row = self.player_row(state, room).await?;
        let mine = Self::member_row(state, &row, actor.user_id).await?.map(|m| m.role);
        let staff = Self::may_moderate(state, actor) || mine.as_deref().is_some_and(is_staff_role);
        if !staff && !mine.as_deref().is_some_and(|r| PLACES.contains(&r)) {
            return Err(not_a_member());
        }
        let after = match &page.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let all = [OWNER, MODERATOR, MEMBER, INVITED, BANNED];
        let roles: &[&str] = if staff { &all } else { &PLACES };
        let db = state.db();
        let mut rows = db.fetch_all::<MemberRow, _>(&store::room_rows(row.id, roles, after, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        let users: Vec<i64> = rows.iter().map(|r| r.user_id).collect();
        let names: HashMap<i64, String> = if users.is_empty() {
            HashMap::new()
        } else {
            db.fetch_all::<NameRow, _>(&store::names(&users)).await?.into_iter().filter_map(|n| n.display_name.map(|d| (n.id, d))).collect()
        };
        let items = rows
            .iter()
            .map(|r| {
                let entry = RoomMembership::new(UserId(r.user_id), role_of(&r.role), UnixMillis(r.created_at));
                match names.get(&r.user_id) {
                    Some(name) => entry.with_name(name.clone()),
                    None => entry,
                }
            })
            .collect();
        Ok(Page::new(items, next))
    }

    /// `user`'s player rooms and invitations (with its role), oldest membership first.
    pub async fn my_rooms(&self, state: &AppState, user: UserId, page: &PageRequest) -> Result<Page<RoomInfo>, AppError> {
        let limit = page_limit(page)?;
        let after = match &page.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let db = state.db();
        let mut memberships = db.fetch_all::<MemberRow, _>(&store::memberships(user.get(), &PLACES, after, limit + 1)).await?;
        let more = memberships.len() as u64 > limit;
        memberships.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { memberships.last().map(|m| Cursor::new(m.id.to_string())) } else { None };
        let ids: Vec<i64> = memberships.iter().map(|m| m.room_id).collect();
        if ids.is_empty() {
            return Ok(Page::new(Vec::new(), next));
        }
        let rows: HashMap<i64, RoomRow> = db.fetch_all::<RoomRow, _>(&store::rooms_by_id(&ids)).await?.into_iter().map(|r| (r.id, r)).collect();
        let ordered: Vec<RoomRow> = ids.iter().filter_map(|id| rows.get(id).cloned()).collect();
        let roles: HashMap<i64, String> = memberships.into_iter().map(|m| (m.room_id, m.role)).collect();
        Ok(Page::new(self.player_infos(state, &ordered, Some(user), Some(&roles)).await?, next))
    }

    /// The public player rooms, oldest first (with `user`'s role where it has one).
    pub async fn public_player_rooms(&self, state: &AppState, user: UserId, page: &PageRequest) -> Result<Page<RoomInfo>, AppError> {
        let limit = page_limit(page)?;
        let after = match &page.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let mut rows = state.db().fetch_all::<RoomRow, _>(&store::public_player_rooms(after, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        Ok(Page::new(self.player_infos(state, &rows, Some(user), None).await?, next))
    }

    // ---- upkeep -----------------------------------------------------------------------------------

    /// Player rooms without an owner (its account was deleted) get the oldest moderator, else the
    /// oldest member, as owner; rooms with no member left are deleted. The number of rooms fixed.
    pub async fn room_upkeep(&self, state: &AppState) -> Result<u64, AppError> {
        let ctx = Self::ctx(state);
        let ids: Vec<i64> = state.db().fetch_all::<IdRow, _>(&store::ownerless_player_rooms(100)).await?.into_iter().map(|r| r.id).collect();
        let mut fixed = 0u64;
        for id in ids {
            let outcome = in_tx!(state, |tx| async {
                let dialect = tx.dialect();
                let Some(_) = tx.fetch_optional::<RoomRow, _>(&store::lock_room(id, dialect)).await? else { return Ok(Outcome::Nothing) };
                if tx.fetch_optional::<MemberRow, _>(&store::oldest_with(id, OWNER)).await?.is_some() {
                    return Ok(Outcome::Nothing);
                }
                let gone = MemberRow { id: 0, room_id: id, user_id: 0, role: OWNER.to_string(), created_at: 0 };
                Self::after_leaving(&mut tx, id, &gone).await
            })?;
            let room = RoomId(id);
            match outcome {
                Outcome::Nothing => continue,
                Outcome::Done { successor } => {
                    if let Some(next) = successor {
                        self.announce(state, id, RoomUpdate::new(room, RoomChange::Owner).about(next).with_role(RoomRole::Owner), &[]).await;
                        Self::after_change(state, &ctx, Self::change(room, RoomChange::Owner, Some(next), None, Some(RoomRole::Owner))).await;
                    }
                }
                Outcome::Deleted { others } => {
                    self.forget(id);
                    Self::push_to(state, &others, &RoomUpdate::new(room, RoomChange::Deleted));
                    Self::after_change(state, &ctx, Self::change(room, RoomChange::Deleted, None, None, None)).await;
                }
            }
            fixed += 1;
        }
        Ok(fixed)
    }

    // ---- other modules ----------------------------------------------------------------------------

    #[cfg(feature = "notifications")]
    async fn notify_invite(&self, state: &AppState, ctx: &HookCtx, to: UserId, from: UserId, row: &RoomRow) {
        use crate::notifications::{NewNotification, NotificationService};
        if !self.0.config.notify {
            return;
        }
        let Some(notifications) = state.get::<NotificationService>() else { return };
        let note = NewNotification::new("chat.invite").with_sender(from).with_data(json!({ "room": row.id, "name": row.name }));
        if let Err(error) = notifications.send_with(state, ctx, to, note).await {
            if error.status().is_server_error() {
                tracing::warn!(%error, "chat: the invitation notification could not be sent (the invitation is stored)");
            } else {
                tracing::debug!(%error, "chat: the invitation notification was refused (the invitation is stored)");
            }
        }
    }

    #[cfg(not(feature = "notifications"))]
    async fn notify_invite(&self, _state: &AppState, _ctx: &HookCtx, _to: UserId, _from: UserId, _row: &RoomRow) {}
}

/// The friends module's blocks (when it is compiled in and registered).
pub(crate) mod blocks {
    use net_backend_protocol::UserId;

    use crate::error::AppError;
    use crate::state::AppState;

    /// Whether `by` blocked `user` (`false` without the friends module).
    pub(crate) async fn blocked(state: &AppState, by: UserId, user: UserId) -> Result<bool, AppError> {
        #[cfg(feature = "friends")]
        if let Some(friends) = state.get::<crate::friends::FriendService>() {
            return friends.is_blocked(state, by, user).await;
        }
        let _ = (state, by, user);
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip() {
        for (name, role) in
            [(OWNER, RoomRole::Owner), (MODERATOR, RoomRole::Moderator), (MEMBER, RoomRole::Member), (INVITED, RoomRole::Invited), (BANNED, RoomRole::Banned)]
        {
            assert_eq!(role_of(name), role);
        }
        assert_eq!(role_of("chief"), RoomRole::Unknown);
        assert!(is_staff_role(OWNER) && is_staff_role(MODERATOR) && !is_staff_role(MEMBER));
        assert!(!PLACES.contains(&BANNED) && PLACES.contains(&INVITED) && !ACTIVE.contains(&INVITED));
    }
}
