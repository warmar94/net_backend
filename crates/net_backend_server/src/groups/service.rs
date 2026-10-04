//! [`GroupService`]: groups, members, roles, invitations and the group chat room.
//!
//! **Writes** are one transaction each (run again on a reported deadlock, [`Retry`]): the group's
//! row lock first (after the player's account lock when the player's group count matters: create,
//! join), then plain reads, the limits counted, the rows changed by primary key. Members are counted
//! under the group's lock, so a full group or a player in too many groups can never be passed by
//! racing joins.
//!
//! **The chat room** (with the chat module): created with the group (the owner its first member),
//! a joining player added, a leaving / kicked player removed (at once on every instance), the room
//! deleted when the group is. These run after the commit; a failure is logged and the group change
//! stays (the upkeep deletes the chat rooms no group names).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use net_backend_protocol::groups::{CreateGroup, GroupInfo, GroupInvite, GroupList, GroupMember, GroupQuery, GroupRole, UpdateGroup, MAX_QUERY_CHARS};
use net_backend_protocol::{codes, Cursor, GroupId, Page, PageRequest, RoomId, UnixMillis, UserId, ValidationDetails};
use serde_json::Value;

use super::config::GroupsConfig;
use super::events::{AfterGroupChange, BeforeGroupCreate, BeforeGroupInvite, BeforeGroupJoin, BeforeGroupUpdate, GroupChange};
use super::store::{self, CountRow, GroupCountRow, GroupRow, IdRow, InviteRow, MemberRow, NameRow, UserRow, ADMIN, MEMBER, OWNER};
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

fn role_of(role: &str) -> GroupRole {
    match role {
        OWNER => GroupRole::Owner,
        ADMIN => GroupRole::Admin,
        MEMBER => GroupRole::Member,
        _ => GroupRole::Unknown,
    }
}

fn no_group() -> AppError {
    AppError::not_found("no such group")
}

fn not_a_member() -> AppError {
    AppError::new(codes::NOT_A_MEMBER, "you are not a member of this group")
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

/// The key a group name is unique and searched by: trimmed, lower case.
fn name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// What a write did, for the work after the commit.
struct Done {
    group: Option<GroupRow>,
    change: Option<GroupChange>,
    /// Chat room members to remove (a leave, a kick; a deletion deletes the room).
    remove: Vec<i64>,
}

impl Done {
    fn none() -> Self {
        Self { group: None, change: None, remove: Vec::new() }
    }
}

/// What the upkeep did to one group.
enum Upkeep {
    /// It has an owner (or is gone).
    Nothing,
    /// This member owns it now.
    Owner(i64),
    /// Nobody was left: deleted (with this chat room).
    Deleted(Option<i64>),
}

/// Groups one upkeep pass looks at.
const UPKEEP_BATCH: u64 = 500;
/// The chat rooms of this module carry this origin.
#[cfg(feature = "chat")]
const ROOM_ORIGIN: &str = "groups";
/// A chat room of this module younger than this is never taken for an orphan (its group may still
/// be storing it), ms.
#[cfg(feature = "chat")]
const ORPHAN_AGE_MS: i64 = 3_600_000;

fn buckets(rate: u32, window_secs: u32) -> Option<KeyedBuckets<UserId>> {
    (rate > 0).then(|| KeyedBuckets::new(rate, Duration::from_secs(u64::from(window_secs)), 100_000))
}

fn check(buckets: Option<&KeyedBuckets<UserId>>, user: UserId) -> Result<(), AppError> {
    match buckets.map(|b| b.check(user)) {
        Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
        _ => Ok(()),
    }
}

struct Inner {
    config: GroupsConfig,
    rate: Option<KeyedBuckets<UserId>>,
    invite_rate: Option<KeyedBuckets<UserId>>,
}

/// Groups, members, roles and invitations. A state value (`Ext<GroupService>` in handlers,
/// `state.get::<GroupService>()` elsewhere) once the [`Groups`](super::Groups) module is
/// registered. The players' actions come through the routes; server code may act for a player with
/// the same methods (the actor's rights are checked the same way), and reads roles with
/// [`role_of`](Self::role_of) and members with [`members_of`](Self::members_of).
///
/// ```no_run
/// use net_backend_server::groups::GroupService;
/// use net_backend_server::protocol::groups::GroupRole;
/// use net_backend_server::protocol::{GroupId, UserId};
/// use net_backend_server::{AppError, AppState};
///
/// // A game rule: only a group's owner and admins start a group raid.
/// async fn may_start_raid(state: &AppState, group: GroupId, player: UserId) -> Result<bool, AppError> {
///     let groups = state.get::<GroupService>().ok_or_else(|| AppError::unavailable("no groups module"))?;
///     Ok(matches!(groups.role_of(state, group, player).await?, Some(GroupRole::Owner | GroupRole::Admin)))
/// }
/// ```
#[derive(Clone)]
pub struct GroupService(Arc<Inner>);

impl std::fmt::Debug for GroupService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("GroupService").field(&self.0.config).finish()
    }
}

impl GroupService {
    pub(crate) fn new(config: GroupsConfig) -> Self {
        let rate = buckets(config.create_rate, config.create_rate_window_secs);
        let invite_rate = buckets(config.invite_rate, config.invite_rate_window_secs);
        Self(Arc::new(Inner { config, rate, invite_rate }))
    }

    /// The settings.
    pub fn config(&self) -> &GroupsConfig {
        &self.0.config
    }

    /// Count one group creation of `user` against `create_rate` (429 `rate_limited` over it).
    pub(crate) fn check_rate(&self, user: UserId) -> Result<(), AppError> {
        check(self.0.rate.as_ref(), user)
    }

    /// Count one invitation of `user` against `invite_rate` (429 `rate_limited` over it).
    pub(crate) fn check_invite_rate(&self, user: UserId) -> Result<(), AppError> {
        check(self.0.invite_rate.as_ref(), user)
    }

    fn info(&self, row: &GroupRow, members: u64, role: Option<GroupRole>) -> GroupInfo {
        let members = u32::try_from(members).unwrap_or(u32::MAX);
        let mut info =
            GroupInfo::new(GroupId(row.id), row.name.clone(), members, self.0.config.max_members, UnixMillis(row.created_at)).with_open(row.is_open != 0);
        if let Some(description) = &row.description {
            info = info.with_description(description.clone());
        }
        if let Some(metadata) = row.metadata.as_deref().and_then(|b| serde_json::from_slice::<Value>(b).ok()).filter(|v| !v.is_null()) {
            info = info.with_metadata(metadata);
        }
        if let Some(owner) = row.owner_id {
            info = info.with_owner(UserId(owner));
        }
        if let Some(room) = row.chat_room {
            info = info.with_chat_room(RoomId(room));
        }
        if let Some(role) = role {
            info = info.with_role(role);
        }
        info
    }

    fn metadata_bytes(&self, metadata: Option<&Value>) -> Result<Option<Vec<u8>>, AppError> {
        match metadata {
            Some(value) if !value.is_null() => {
                let bytes = serde_json::to_vec(value).map_err(AppError::internal)?;
                if bytes.len() > self.0.config.max_metadata_bytes {
                    return Err(invalid("metadata", &format!("is larger than {} bytes", self.0.config.max_metadata_bytes)));
                }
                Ok(Some(bytes))
            }
            _ => Ok(None),
        }
    }

    async fn group_row(state: &AppState, group: GroupId) -> Result<GroupRow, AppError> {
        state.db().fetch_optional::<GroupRow, _>(&store::group_by_id(group.get())).await?.ok_or_else(no_group)
    }

    /// The member counts of these groups.
    async fn counts(state: &AppState, groups: &[i64]) -> Result<HashMap<i64, u64>, AppError> {
        if groups.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = state.db().fetch_all::<GroupCountRow, _>(&store::member_counts(groups)).await?;
        Ok(rows.into_iter().map(|r| (r.group_id, u64::try_from(r.n).unwrap_or(0))).collect())
    }

    /// One group as `user` sees it.
    async fn info_of(&self, state: &AppState, row: &GroupRow, role: Option<GroupRole>) -> Result<GroupInfo, AppError> {
        let members = count(state.db().fetch_one::<CountRow, _>(&store::count_members(row.id)).await?);
        Ok(self.info(row, members, role))
    }

    async fn member_row(state: &AppState, group: GroupId, user: UserId) -> Result<Option<MemberRow>, AppError> {
        Ok(state.db().fetch_optional::<MemberRow, _>(&store::member(group.get(), user.get())).await?)
    }

    async fn locked_group(tx: &mut DbTx, group: i64) -> Result<GroupRow, AppError> {
        let dialect = tx.dialect();
        tx.fetch_optional::<GroupRow, _>(&store::lock_group(group, dialect)).await?.ok_or_else(no_group)
    }

    async fn lock_user(tx: &mut DbTx, user: i64) -> Result<(), AppError> {
        let dialect = tx.dialect();
        match tx.fetch_optional::<IdRow, _>(&store::lock_user(user, dialect)).await? {
            Some(_) => Ok(()),
            None => Err(AppError::not_found("no such account")),
        }
    }

    /// The actor's membership, which must be the owner's or (with `admins`) an admin's.
    async fn manager(tx: &mut DbTx, group: i64, actor: i64, admins: bool) -> Result<MemberRow, AppError> {
        let row = tx.fetch_optional::<MemberRow, _>(&store::member(group, actor)).await?;
        Self::may_manage(row, admins)
    }

    fn may_manage(row: Option<MemberRow>, admins: bool) -> Result<MemberRow, AppError> {
        let row = row.ok_or_else(not_a_member)?;
        match row.role.as_str() {
            OWNER => Ok(row),
            ADMIN if admins => Ok(row),
            _ => Err(AppError::forbidden(if admins { "only the owner and admins may do this" } else { "only the owner may do this" })),
        }
    }

    /// The rights check before the hooks run (a hook never sees a refused request); the
    /// transaction checks again under the group's lock.
    async fn check_manager(state: &AppState, group: GroupId, actor: UserId, admins: bool) -> Result<(), AppError> {
        Self::group_row(state, group).await?;
        Self::may_manage(Self::member_row(state, group, actor).await?, admins).map(|_| ())
    }

    async fn after(&self, state: &AppState, ctx: &HookCtx, group: GroupId, actor: UserId, user: UserId, change: GroupChange) {
        state.hooks().run_after(ctx, Arc::new(AfterGroupChange { group, actor: Some(actor), user: Some(user), change })).await;
    }

    // ---- create, read, change, delete -----------------------------------------------------------

    /// `user` creates a group and owns it: the [`BeforeGroupCreate`] hooks, then one transaction
    /// (the player's group count, the name unique: 409 `conflict` when taken), then its chat room.
    /// The rate is the route's.
    pub async fn create(&self, state: &AppState, ctx: &HookCtx, user: UserId, request: CreateGroup) -> Result<GroupInfo, AppError> {
        request.validate()?;
        self.metadata_bytes(request.metadata.as_ref())?;
        let event = state.hooks().run_before(ctx, BeforeGroupCreate { user, request }).await?;
        let request = event.request;
        request.validate()?;
        let metadata = self.metadata_bytes(request.metadata.as_ref())?;
        let name = request.name.trim().to_string();
        let key = name_key(&name);
        let description = request.description.as_deref().filter(|d| !d.is_empty());
        let max = u64::from(self.0.config.max_groups_per_user);
        let id = in_tx!(state, |tx| async {
            Self::lock_user(&mut tx, user.get()).await?;
            if count(tx.fetch_one::<CountRow, _>(&store::count_memberships(user.get())).await?) >= max {
                return Err(quota("you are in too many groups"));
            }
            let now = state.now().get();
            let row = store::NewGroup { name: &name, name_key: &key, description, open: request.open, metadata: metadata.clone(), owner: user.get(), now };
            let id = match tx.insert_id(&store::insert_group(row)?, "id").await {
                Ok(id) => id,
                Err(error) if error.is_unique_violation() => return Err(AppError::conflict("a group with this name exists")),
                Err(error) => return Err(error.into()),
            };
            tx.execute(&store::insert_member(id, user.get(), OWNER, now)?).await?;
            Ok(id)
        })?;
        if let Some(room) = self.chat_create(state, user).await {
            if let Err(error) = state.db().execute(&store::set_chat_room(id, room)).await {
                tracing::warn!(%error, group = id, "groups: storing the chat room failed");
            }
        }
        self.after(state, ctx, GroupId(id), user, user, GroupChange::Created).await;
        let row = Self::group_row(state, GroupId(id)).await?;
        self.info_of(state, &row, Some(GroupRole::Owner)).await
    }

    /// One group, with `user`'s role when a member (404 `not_found`).
    pub async fn get(&self, state: &AppState, user: UserId, group: GroupId) -> Result<GroupInfo, AppError> {
        let row = Self::group_row(state, group).await?;
        let role = Self::member_row(state, group, user).await?.map(|m| role_of(&m.role));
        self.info_of(state, &row, role).await
    }

    /// The groups by name (a name prefix without regard to case), with `user`'s roles.
    pub async fn search(&self, state: &AppState, user: UserId, query: &GroupQuery) -> Result<Page<GroupInfo>, AppError> {
        let page = query.page();
        page.validate()?;
        let prefix = query.query.as_deref().map(name_key);
        if prefix.as_deref().is_some_and(|p| p.chars().count() > MAX_QUERY_CHARS) {
            return Err(invalid("query", &format!("is longer than {MAX_QUERY_CHARS} characters")));
        }
        let limit = u64::from(page.limit_or_default());
        let after = page.cursor.as_ref().map(|c| c.as_str().to_string());
        let mut rows = state.db().fetch_all::<GroupRow, _>(&store::search(prefix.as_deref(), after.as_deref(), limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.name_key.clone())) } else { None };
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let roles: HashMap<i64, GroupRole> = if ids.is_empty() {
            HashMap::new()
        } else {
            state.db().fetch_all::<MemberRow, _>(&store::roles_in(user.get(), &ids)).await?.into_iter().map(|m| (m.group_id, role_of(&m.role))).collect()
        };
        let counts = Self::counts(state, &ids).await?;
        Ok(Page::new(rows.iter().map(|r| self.info(r, counts.get(&r.id).copied().unwrap_or(0), roles.get(&r.id).copied())).collect(), next))
    }

    /// `user`'s groups with its role, oldest membership first.
    pub async fn mine(&self, state: &AppState, user: UserId) -> Result<GroupList, AppError> {
        let memberships = state.db().fetch_all::<MemberRow, _>(&store::memberships(user.get())).await?;
        if memberships.is_empty() {
            return Ok(GroupList::new(Vec::new()));
        }
        let ids: Vec<i64> = memberships.iter().map(|m| m.group_id).collect();
        let groups: HashMap<i64, GroupRow> = state.db().fetch_all::<GroupRow, _>(&store::groups_by_id(&ids)).await?.into_iter().map(|g| (g.id, g)).collect();
        let counts = Self::counts(state, &ids).await?;
        Ok(GroupList::new(
            memberships
                .iter()
                .filter_map(|m| groups.get(&m.group_id).map(|g| self.info(g, counts.get(&g.id).copied().unwrap_or(0), Some(role_of(&m.role)))))
                .collect(),
        ))
    }

    /// A page of a group's members in the order they joined (404 for an unknown group).
    pub async fn members(&self, state: &AppState, group: GroupId, page: &PageRequest) -> Result<Page<GroupMember>, AppError> {
        page.validate()?;
        Self::group_row(state, group).await?;
        let limit = u64::from(page.limit_or_default());
        let after = match &page.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let mut rows = state.db().fetch_all::<MemberRow, _>(&store::members(group.get(), after, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        let names = self.names(state, rows.iter().map(|r| r.user_id).collect()).await?;
        let items = rows
            .iter()
            .map(|r| {
                let member = GroupMember::new(UserId(r.user_id), role_of(&r.role), UnixMillis(r.joined_at));
                match names.get(&r.user_id) {
                    Some(name) => member.with_name(name.clone()),
                    None => member,
                }
            })
            .collect();
        Ok(Page::new(items, next))
    }

    async fn names(&self, state: &AppState, ids: Vec<i64>) -> Result<HashMap<i64, String>, AppError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        Ok(state.db().fetch_all::<NameRow, _>(&store::names(&ids)).await?.into_iter().filter_map(|r| r.display_name.map(|n| (r.id, n))).collect())
    }

    /// `user` (the owner or an admin) changes a group: the rights, the [`BeforeGroupUpdate`]
    /// hooks, then the change (a taken name: 409 `conflict`).
    pub async fn update(&self, state: &AppState, ctx: &HookCtx, user: UserId, group: GroupId, update: UpdateGroup) -> Result<GroupInfo, AppError> {
        update.validate()?;
        self.metadata_bytes(update.metadata.as_ref())?;
        Self::check_manager(state, group, user, true).await?;
        let event = state.hooks().run_before(ctx, BeforeGroupUpdate { group, user, update }).await?;
        let update = event.update;
        update.validate()?;
        let metadata = match &update.metadata {
            Some(value) => Some(self.metadata_bytes(Some(value))?),
            None => None,
        };
        let name = update.name.as_deref().map(|n| n.trim().to_string());
        let key = name.as_deref().map(name_key);
        let done = in_tx!(state, |tx| async {
            Self::locked_group(&mut tx, group.get()).await?;
            let manager = Self::manager(&mut tx, group.get(), user.get(), true).await?;
            let change = store::GroupUpdate {
                name: name.as_deref().zip(key.as_deref()),
                description: update.description.as_deref().map(|d| (!d.is_empty()).then_some(d)),
                open: update.open,
                metadata: metadata.clone(),
                now: state.now().get(),
            };
            match tx.execute(&store::update_group(group.get(), change)).await {
                Ok(_) => {}
                Err(error) if error.is_unique_violation() => return Err(AppError::conflict("a group with this name exists")),
                Err(error) => return Err(error.into()),
            }
            Ok(role_of(&manager.role))
        })?;
        self.after(state, ctx, group, user, user, GroupChange::Updated).await;
        let row = Self::group_row(state, group).await?;
        self.info_of(state, &row, Some(done)).await
    }

    /// `user` (the owner) deletes a group: its members, invitations and chat room go.
    pub async fn delete(&self, state: &AppState, ctx: &HookCtx, user: UserId, group: GroupId) -> Result<(), AppError> {
        let done = in_tx!(state, |tx| async {
            let row = Self::locked_group(&mut tx, group.get()).await?;
            Self::manager(&mut tx, group.get(), user.get(), false).await?;
            tx.execute(&store::delete_group(group.get())).await?;
            Ok(Done { group: Some(row), change: Some(GroupChange::Deleted), remove: Vec::new() })
        })?;
        self.finish(state, ctx, group, user, user, done).await;
        Ok(())
    }

    /// Chat changes (a deleted group's room deleted, else members removed) and the after hooks of
    /// a write.
    async fn finish(&self, state: &AppState, ctx: &HookCtx, group: GroupId, actor: UserId, user: UserId, done: Done) {
        if let Some(room) = done.group.as_ref().and_then(|g| g.chat_room) {
            if done.change == Some(GroupChange::Deleted) {
                self.chat_delete(state, room).await;
            } else {
                for member in &done.remove {
                    self.chat_remove(state, room, UserId(*member)).await;
                }
            }
        }
        if let Some(change) = done.change {
            self.after(state, ctx, group, actor, user, change).await;
        }
    }

    // ---- members ---------------------------------------------------------------------------------

    /// `user` joins a group: an open one, or one that invited it (the invitation is used). With
    /// `invited_only`, only by an invitation (404 without one). A member already: the group again.
    pub async fn join(&self, state: &AppState, ctx: &HookCtx, user: UserId, group: GroupId, invited_only: bool) -> Result<GroupInfo, AppError> {
        let row = Self::group_row(state, group).await?;
        if let Some(member) = Self::member_row(state, group, user).await? {
            return self.info_of(state, &row, Some(role_of(&member.role))).await;
        }
        let invited = state.db().fetch_optional::<InviteRow, _>(&store::invite(group.get(), user.get())).await?.is_some();
        if invited_only && !invited {
            return Err(AppError::not_found("no invitation to this group"));
        }
        if !invited && row.is_open == 0 {
            return Err(AppError::forbidden("this group takes members by invitation only"));
        }
        state.hooks().run_before(ctx, BeforeGroupJoin { group, user, invited }).await?;
        let (max_groups, max_members) = (u64::from(self.0.config.max_groups_per_user), u64::from(self.0.config.max_members));
        let done = in_tx!(state, |tx| async {
            Self::lock_user(&mut tx, user.get()).await?;
            let row = Self::locked_group(&mut tx, group.get()).await?;
            if tx.fetch_optional::<MemberRow, _>(&store::member(group.get(), user.get())).await?.is_some() {
                return Ok(Done { group: Some(row), change: None, remove: Vec::new() });
            }
            let invite = tx.fetch_optional::<InviteRow, _>(&store::invite(group.get(), user.get())).await?;
            if invite.is_none() && (invited_only || row.is_open == 0) {
                return Err(AppError::not_found("no invitation to this group"));
            }
            if count(tx.fetch_one::<CountRow, _>(&store::count_memberships(user.get())).await?) >= max_groups {
                return Err(quota("you are in too many groups"));
            }
            if count(tx.fetch_one::<CountRow, _>(&store::count_members(group.get())).await?) >= max_members {
                return Err(quota("the group is full"));
            }
            tx.execute(&store::insert_member(group.get(), user.get(), MEMBER, state.now().get())?).await?;
            if let Some(invite) = invite {
                tx.execute(&store::delete_invite(invite.id)).await?;
            }
            Ok(Done { group: Some(row), change: Some(GroupChange::Joined), remove: Vec::new() })
        })?;
        if done.change.is_some() {
            if let Some(room) = done.group.as_ref().and_then(|g| g.chat_room) {
                self.chat_add(state, room, user).await;
            }
            self.after(state, ctx, group, user, user, GroupChange::Joined).await;
        }
        self.get(state, user, group).await
    }

    /// `user` leaves a group; `true` if it was a member. The owner leaves only as the last member
    /// (the group is deleted then; else 409 `conflict`: transfer first).
    pub async fn leave(&self, state: &AppState, ctx: &HookCtx, user: UserId, group: GroupId) -> Result<bool, AppError> {
        let done = in_tx!(state, |tx| async {
            let row = Self::locked_group(&mut tx, group.get()).await?;
            let Some(member) = tx.fetch_optional::<MemberRow, _>(&store::member(group.get(), user.get())).await? else { return Ok(Done::none()) };
            if member.role == OWNER {
                if count(tx.fetch_one::<CountRow, _>(&store::count_members(group.get())).await?) > 1 {
                    return Err(AppError::conflict("the owner leaves last: transfer the group to another member first"));
                }
                tx.execute(&store::delete_group(group.get())).await?;
                return Ok(Done { group: Some(row), change: Some(GroupChange::Deleted), remove: vec![user.get()] });
            }
            tx.execute(&store::delete_member(member.id)).await?;
            Ok(Done { group: Some(row), change: Some(GroupChange::Left), remove: vec![user.get()] })
        })?;
        let left = done.change.is_some();
        self.finish(state, ctx, group, user, user, done).await;
        Ok(left)
    }

    /// `actor` removes `user` from a group (the owner: anyone; admins: members); `true` if it was a
    /// member. The removed player gets a `groups.kicked` notification.
    pub async fn kick(&self, state: &AppState, ctx: &HookCtx, actor: UserId, group: GroupId, user: UserId) -> Result<bool, AppError> {
        if actor == user {
            return Err(invalid("user", "is the caller: leave the group instead"));
        }
        let done = in_tx!(state, |tx| async {
            let row = Self::locked_group(&mut tx, group.get()).await?;
            let manager = Self::manager(&mut tx, group.get(), actor.get(), true).await?;
            let Some(member) = tx.fetch_optional::<MemberRow, _>(&store::member(group.get(), user.get())).await? else { return Ok(Done::none()) };
            if manager.role != OWNER && member.role != MEMBER {
                return Err(AppError::forbidden("admins remove members only"));
            }
            tx.execute(&store::delete_member(member.id)).await?;
            Ok(Done { group: Some(row), change: Some(GroupChange::Kicked), remove: vec![user.get()] })
        })?;
        let kicked = done.change.is_some();
        if let Some(row) = &done.group {
            if kicked {
                self.notify(state, ctx, user, "groups.kicked", actor, row).await;
            }
        }
        self.finish(state, ctx, group, actor, user, done).await;
        Ok(kicked)
    }

    /// `actor` (the owner) gives a member the role `admin` or `member`.
    pub async fn set_role(&self, state: &AppState, ctx: &HookCtx, actor: UserId, group: GroupId, user: UserId, role: GroupRole) -> Result<(), AppError> {
        let name = match role {
            GroupRole::Admin => ADMIN,
            GroupRole::Member => MEMBER,
            _ => return Err(invalid("role", "must be admin or member (the owner changes with a transfer)")),
        };
        if actor == user {
            return Err(invalid("user", "is the owner: transfer the group instead"));
        }
        let changed = in_tx!(state, |tx| async {
            Self::locked_group(&mut tx, group.get()).await?;
            Self::manager(&mut tx, group.get(), actor.get(), false).await?;
            let member =
                tx.fetch_optional::<MemberRow, _>(&store::member(group.get(), user.get())).await?.ok_or_else(|| AppError::not_found("no such member"))?;
            if member.role == name {
                return Ok(false);
            }
            tx.execute(&store::set_role(member.id, name)).await?;
            Ok(true)
        })?;
        if changed {
            self.after(state, ctx, group, actor, user, GroupChange::RoleChanged(role)).await;
        }
        Ok(())
    }

    /// `actor` (the owner) hands the group to a member, and becomes an admin.
    pub async fn transfer(&self, state: &AppState, ctx: &HookCtx, actor: UserId, group: GroupId, user: UserId) -> Result<(), AppError> {
        if actor == user {
            return Err(invalid("user", "is the owner already"));
        }
        in_tx!(state, |tx| async {
            Self::locked_group(&mut tx, group.get()).await?;
            let owner = Self::manager(&mut tx, group.get(), actor.get(), false).await?;
            let member =
                tx.fetch_optional::<MemberRow, _>(&store::member(group.get(), user.get())).await?.ok_or_else(|| AppError::not_found("no such member"))?;
            tx.execute(&store::set_role(member.id, OWNER)).await?;
            tx.execute(&store::set_role(owner.id, ADMIN)).await?;
            tx.execute(&store::set_owner(group.get(), user.get(), state.now().get())).await?;
            Ok(())
        })?;
        self.after(state, ctx, group, actor, user, GroupChange::Transferred).await;
        Ok(())
    }

    // ---- invitations -----------------------------------------------------------------------------

    /// `actor` (the owner or an admin) invites `user`; `true` if the invitation is new. Refused
    /// (403 `forbidden`) when `user` blocked `actor` (with the friends module); the
    /// [`BeforeGroupInvite`] hooks run after the rights check and may refuse. The invited player
    /// gets a `groups.invite` notification. The rate is the route's.
    pub async fn invite(&self, state: &AppState, ctx: &HookCtx, actor: UserId, group: GroupId, user: UserId) -> Result<bool, AppError> {
        if actor == user {
            return Err(invalid("user", "is the caller"));
        }
        Self::check_manager(state, group, actor, true).await?;
        if blocks::blocked(state, user, actor).await? {
            return Err(AppError::forbidden("this player does not take invitations from you"));
        }
        state.hooks().run_before(ctx, BeforeGroupInvite { group, user: actor, invitee: user }).await?;
        let max = u64::from(self.0.config.max_invites);
        let done = in_tx!(state, |tx| async {
            // The invitee's account first, then the group (the order of `join`).
            Self::lock_user(&mut tx, user.get()).await?;
            let row = Self::locked_group(&mut tx, group.get()).await?;
            Self::manager(&mut tx, group.get(), actor.get(), true).await?;
            if tx.fetch_optional::<MemberRow, _>(&store::member(group.get(), user.get())).await?.is_some() {
                return Err(AppError::conflict("this player is a member already"));
            }
            if tx.fetch_optional::<InviteRow, _>(&store::invite(group.get(), user.get())).await?.is_some() {
                return Ok(Done { group: Some(row), change: None, remove: Vec::new() });
            }
            if count(tx.fetch_one::<CountRow, _>(&store::count_invites(group.get())).await?) >= max {
                return Err(quota("the group has too many open invitations"));
            }
            tx.execute(&store::insert_invite(group.get(), user.get(), actor.get(), state.now().get())?).await?;
            Ok(Done { group: Some(row), change: Some(GroupChange::Invited), remove: Vec::new() })
        })?;
        let new = done.change.is_some();
        if let (true, Some(row)) = (new, &done.group) {
            self.notify(state, ctx, user, "groups.invite", actor, row).await;
        }
        self.finish(state, ctx, group, actor, user, done).await;
        Ok(new)
    }

    /// `actor` (the owner or an admin) withdraws `user`'s invitation; `true` if there was one.
    pub async fn revoke_invite(&self, state: &AppState, ctx: &HookCtx, actor: UserId, group: GroupId, user: UserId) -> Result<bool, AppError> {
        let done = in_tx!(state, |tx| async {
            Self::locked_group(&mut tx, group.get()).await?;
            Self::manager(&mut tx, group.get(), actor.get(), true).await?;
            let Some(invite) = tx.fetch_optional::<InviteRow, _>(&store::invite(group.get(), user.get())).await? else { return Ok(Done::none()) };
            tx.execute(&store::delete_invite(invite.id)).await?;
            Ok(Done { group: None, change: Some(GroupChange::InviteRevoked), remove: Vec::new() })
        })?;
        let revoked = done.change.is_some();
        self.finish(state, ctx, group, actor, user, done).await;
        Ok(revoked)
    }

    /// `user` declines its invitation; `true` if there was one.
    pub async fn decline_invite(&self, state: &AppState, ctx: &HookCtx, user: UserId, group: GroupId) -> Result<bool, AppError> {
        let Some(invite) = state.db().fetch_optional::<InviteRow, _>(&store::invite(group.get(), user.get())).await? else { return Ok(false) };
        let declined = state.db().execute(&store::delete_invite(invite.id)).await? > 0;
        if declined {
            self.after(state, ctx, group, user, user, GroupChange::InviteDeclined).await;
        }
        Ok(declined)
    }

    /// A page of `user`'s invitations, newest first.
    pub async fn invites(&self, state: &AppState, user: UserId, page: &PageRequest) -> Result<Page<GroupInvite>, AppError> {
        page.validate()?;
        let limit = u64::from(page.limit_or_default());
        let before = match &page.cursor {
            Some(cursor) => Some(cursor.as_str().parse::<i64>().map_err(|_| AppError::bad_request("the cursor is not valid"))?),
            None => None,
        };
        let mut rows = state.db().fetch_all::<InviteRow, _>(&store::invites_of(user.get(), before, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| Cursor::new(r.id.to_string())) } else { None };
        let ids: Vec<i64> = rows.iter().map(|r| r.group_id).collect();
        let groups: HashMap<i64, GroupRow> = if ids.is_empty() {
            HashMap::new()
        } else {
            state.db().fetch_all::<GroupRow, _>(&store::groups_by_id(&ids)).await?.into_iter().map(|g| (g.id, g)).collect()
        };
        let counts = Self::counts(state, &ids).await?;
        let items = rows
            .iter()
            .filter_map(|r| {
                groups
                    .get(&r.group_id)
                    .map(|g| GroupInvite::new(self.info(g, counts.get(&g.id).copied().unwrap_or(0), None), r.inviter_id.map(UserId), UnixMillis(r.created_at)))
            })
            .collect();
        Ok(Page::new(items, next))
    }

    // ---- upkeep ----------------------------------------------------------------------------------

    /// Groups without an owner (the owner's account was deleted) get the oldest admin, else the
    /// oldest member, as owner; groups with no member left are deleted (with their invitations and
    /// chat rooms); the chat rooms of this module no group names (older than an hour) are deleted.
    /// The number of groups fixed. The module's background task runs it every
    /// `upkeep_interval_secs`.
    pub async fn upkeep(&self, state: &AppState) -> Result<u64, AppError> {
        let ctx = HookCtx::new(state.clone(), None);
        let ids: Vec<i64> = state.db().fetch_all::<IdRow, _>(&store::ownerless_groups(UPKEEP_BATCH)).await?.into_iter().map(|r| r.id).collect();
        let mut fixed = 0u64;
        for id in ids {
            let outcome = in_tx!(state, |tx| async {
                let dialect = tx.dialect();
                let Some(group) = tx.fetch_optional::<GroupRow, _>(&store::lock_group(id, dialect)).await? else { return Ok(Upkeep::Nothing) };
                if tx.fetch_optional::<MemberRow, _>(&store::oldest_with(id, OWNER)).await?.is_some() {
                    return Ok(Upkeep::Nothing);
                }
                let mut next = tx.fetch_optional::<MemberRow, _>(&store::oldest_with(id, ADMIN)).await?;
                if next.is_none() {
                    next = tx.fetch_optional::<MemberRow, _>(&store::oldest_with(id, MEMBER)).await?;
                }
                match next {
                    Some(member) => {
                        tx.execute(&store::set_role(member.id, OWNER)).await?;
                        tx.execute(&store::set_owner(id, member.user_id, state.now().get())).await?;
                        Ok(Upkeep::Owner(member.user_id))
                    }
                    None => {
                        tx.execute(&store::delete_group(id)).await?;
                        Ok(Upkeep::Deleted(group.chat_room))
                    }
                }
            })?;
            let group = GroupId(id);
            let change = match outcome {
                Upkeep::Nothing => continue,
                Upkeep::Owner(user) => AfterGroupChange { group, actor: None, user: Some(UserId(user)), change: GroupChange::Transferred },
                Upkeep::Deleted(room) => {
                    if let Some(room) = room {
                        self.chat_delete(state, room).await;
                    }
                    AfterGroupChange { group, actor: None, user: None, change: GroupChange::Deleted }
                }
            };
            state.hooks().run_after(&ctx, Arc::new(change)).await;
            fixed += 1;
        }
        match self.purge_orphan_rooms(state).await {
            Ok(0) => {}
            Ok(rooms) => tracing::info!(rooms, "groups: deleted chat rooms no group names"),
            Err(error) => tracing::warn!(%error, "groups: deleting chat rooms no group names failed"),
        }
        Ok(fixed)
    }

    // ---- reading (server code) -------------------------------------------------------------------

    /// `user`'s role in `group` (`None`: not a member).
    pub async fn role_of(&self, state: &AppState, group: GroupId, user: UserId) -> Result<Option<GroupRole>, AppError> {
        Ok(Self::member_row(state, group, user).await?.map(|m| role_of(&m.role)))
    }

    /// The members of `group` (at most `max_members`).
    pub async fn members_of(&self, state: &AppState, group: GroupId) -> Result<Vec<UserId>, AppError> {
        let limit = u64::from(self.0.config.max_members);
        Ok(state.db().fetch_all::<UserRow, _>(&store::member_ids(group.get(), limit)).await?.into_iter().map(|r| UserId(r.user_id)).collect())
    }

    // ---- other modules ---------------------------------------------------------------------------

    #[cfg(feature = "notifications")]
    async fn notify(&self, state: &AppState, ctx: &HookCtx, to: UserId, kind: &str, from: UserId, group: &GroupRow) {
        use crate::notifications::{NewNotification, NotificationService};
        if !self.0.config.notify {
            return;
        }
        let Some(notifications) = state.get::<NotificationService>() else { return };
        let note = NewNotification::new(kind).with_sender(from).with_data(serde_json::json!({ "group": group.id, "name": group.name }));
        if let Err(error) = notifications.send_with(state, ctx, to, note).await {
            if error.status().is_server_error() {
                tracing::warn!(%error, kind, "groups: the notification could not be sent (the change is stored)");
            } else {
                tracing::debug!(%error, kind, "groups: the notification was refused (the change is stored)");
            }
        }
    }

    #[cfg(not(feature = "notifications"))]
    async fn notify(&self, _state: &AppState, _ctx: &HookCtx, _to: UserId, _kind: &str, _from: UserId, _group: &GroupRow) {}

    /// A chat group room with the owner, when the chat module is registered and `chat_room` is on.
    #[cfg(feature = "chat")]
    async fn chat_create(&self, state: &AppState, owner: UserId) -> Option<i64> {
        let chat = state.get::<crate::chat::ChatService>().filter(|_| self.0.config.chat_room)?;
        match chat.create_module_room(state, ROOM_ORIGIN, &[owner]).await {
            Ok(room) => Some(room.get()),
            Err(error) => {
                tracing::warn!(%error, "groups: creating the chat room failed (the group has none)");
                None
            }
        }
    }

    #[cfg(feature = "chat")]
    async fn chat_add(&self, state: &AppState, room: i64, user: UserId) {
        if let Some(chat) = state.get::<crate::chat::ChatService>() {
            if let Err(error) = chat.add_member(state, RoomId(room), user).await {
                tracing::warn!(%error, room, "groups: adding a member to the chat room failed");
            }
        }
    }

    #[cfg(feature = "chat")]
    async fn chat_remove(&self, state: &AppState, room: i64, user: UserId) {
        if let Some(chat) = state.get::<crate::chat::ChatService>() {
            if let Err(error) = chat.remove_member(state, RoomId(room), user).await {
                tracing::warn!(%error, room, "groups: removing a member from the chat room failed");
            }
        }
    }

    /// Delete a deleted group's chat room.
    #[cfg(feature = "chat")]
    async fn chat_delete(&self, state: &AppState, room: i64) {
        if let Some(chat) = state.get::<crate::chat::ChatService>() {
            if let Err(error) = chat.delete_group_room(state, RoomId(room)).await {
                tracing::warn!(%error, room, "groups: deleting the chat room failed (the upkeep tries again)");
            }
        }
    }

    /// Delete the chat rooms of this module that no group names (and that are older than
    /// [`ORPHAN_AGE_MS`]); how many.
    #[cfg(feature = "chat")]
    async fn purge_orphan_rooms(&self, state: &AppState) -> Result<u64, AppError> {
        let Some(chat) = state.get::<crate::chat::ChatService>() else { return Ok(0) };
        let before = state.now().get().saturating_sub(ORPHAN_AGE_MS);
        let (mut after, mut deleted) = (0, 0);
        loop {
            let rooms = chat.module_rooms(state, ROOM_ORIGIN, before, after, UPKEEP_BATCH).await?;
            let Some(last) = rooms.last().copied() else { break };
            let named: std::collections::HashSet<i64> =
                state.db().fetch_all::<store::RoomRefRow, _>(&store::groups_with_rooms(&rooms)).await?.into_iter().filter_map(|r| r.chat_room).collect();
            for room in rooms.iter().copied().filter(|r| !named.contains(r)) {
                if chat.delete_group_room(state, RoomId(room)).await? {
                    deleted += 1;
                }
            }
            if (rooms.len() as u64) < UPKEEP_BATCH {
                break;
            }
            after = last;
        }
        Ok(deleted)
    }

    #[cfg(not(feature = "chat"))]
    async fn chat_create(&self, _state: &AppState, _owner: UserId) -> Option<i64> {
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

/// The friends module's blocks (when it is compiled in and registered).
mod blocks {
    use net_backend_protocol::UserId;

    use crate::error::AppError;
    use crate::state::AppState;

    /// Whether `by` blocked `user` (`false` without the friends module).
    pub(super) async fn blocked(state: &AppState, by: UserId, user: UserId) -> Result<bool, AppError> {
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
    fn roles_and_keys() {
        for (name, role) in [(OWNER, GroupRole::Owner), (ADMIN, GroupRole::Admin), (MEMBER, GroupRole::Member)] {
            assert_eq!(role_of(name), role);
        }
        assert_eq!(role_of("x"), GroupRole::Unknown);
        assert_eq!(name_key("  Night OWLS "), "night owls");
        assert_eq!(not_a_member().code(), codes::NOT_A_MEMBER);
        let service = GroupService::new(GroupsConfig::default());
        assert!(service.metadata_bytes(Some(&serde_json::json!("x".repeat(3000)))).is_err());
        assert_eq!(service.metadata_bytes(Some(&Value::Null)).ok(), Some(None));
        let row = GroupRow {
            id: 5,
            name: "Night Owls".into(),
            name_key: "night owls".into(),
            description: None,
            is_open: 1,
            metadata: Some(b"null".to_vec()),
            owner_id: None,
            chat_room: Some(9),
            created_at: 1,
        };
        let info = service.info(&row, 2, None);
        assert_eq!((info.open, info.metadata, info.owner, info.chat_room, info.members), (true, None, None, Some(RoomId(9)), 2));
    }
}
