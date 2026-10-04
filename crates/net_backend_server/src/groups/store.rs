//! The groups module's SQL, as sea-query statements (one statement, three dialects). Every
//! statement runs in the groups suite on SQLite locally and on MySQL / PostgreSQL (env-gated).
//!
//! **Writes to a group** take its row lock first (MySQL `FOR UPDATE`, PostgreSQL
//! `FOR NO KEY UPDATE`, SQLite's write transaction); a write that also counts a player's groups
//! (create, join) takes the player's account lock BEFORE the group's. Rows change by primary key
//! after a plain read: no gap locks, and the member count (counted, never stored), the per-player
//! count and the invitation count stay exact under concurrent requests.

use sea_query::{DeleteStatement, Expr, ExprTrait, InsertStatement, LikeExpr, LockType, Order, Query, SelectStatement, UpdateStatement};

use super::migrations::{GROUPS, INVITES, MEMBERS};
use crate::db::{DbError, Dialect};

pub(crate) const OWNER: &str = "owner";
pub(crate) const ADMIN: &str = "admin";
pub(crate) const MEMBER: &str = "member";

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

const GROUP_COLUMNS: [&str; 9] = ["id", "name", "name_key", "description", "is_open", "metadata", "owner_id", "chat_room", "created_at"];

/// One group.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct GroupRow {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) name_key: String,
    pub(crate) description: Option<String>,
    pub(crate) is_open: i64,
    pub(crate) metadata: Option<Vec<u8>>,
    pub(crate) owner_id: Option<i64>,
    pub(crate) chat_room: Option<i64>,
    pub(crate) created_at: i64,
}

/// One membership.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct MemberRow {
    pub(crate) id: i64,
    pub(crate) group_id: i64,
    pub(crate) user_id: i64,
    pub(crate) role: String,
    pub(crate) joined_at: i64,
}

/// One invitation.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct InviteRow {
    pub(crate) id: i64,
    pub(crate) group_id: i64,
    pub(crate) inviter_id: Option<i64>,
    pub(crate) created_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    #[allow(dead_code)]
    pub(crate) id: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct CountRow {
    pub(crate) n: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct NameRow {
    pub(crate) id: i64,
    pub(crate) display_name: Option<String>,
}

const MEMBER_COLUMNS: [&str; 5] = ["id", "group_id", "user_id", "role", "joined_at"];
const INVITE_COLUMNS: [&str; 4] = ["id", "group_id", "inviter_id", "created_at"];

fn locked(mut select: SelectStatement, dialect: Dialect) -> SelectStatement {
    match dialect {
        Dialect::Postgres => select.lock(LockType::NoKeyUpdate),
        _ => select.lock_exclusive(),
    };
    select
}

/// The account row, locked for the transaction (`None` if the account does not exist).
pub(crate) fn lock_user(user: i64, dialect: Dialect) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from("auth_users").and_where(Expr::col("id").eq(user));
    locked(select, dialect)
}

/// The group row, locked for the transaction.
pub(crate) fn lock_group(group: i64, dialect: Dialect) -> SelectStatement {
    locked(group_by_id(group), dialect)
}

pub(crate) fn group_by_id(group: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(GROUP_COLUMNS).from(GROUPS).and_where(Expr::col("id").eq(group));
    select
}

pub(crate) fn groups_by_id(groups: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(GROUP_COLUMNS).from(GROUPS).and_where(Expr::col("id").is_in(groups.iter().copied()));
    select
}

/// Escape `%`, `_` and `\` for a LIKE pattern with `\` as the escape character.
fn like_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A page of groups by `name_key`, after `after`, whose `name_key` starts with `prefix`.
pub(crate) fn search(prefix: Option<&str>, after: Option<&str>, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(GROUP_COLUMNS).from(GROUPS);
    if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
        select.and_where(Expr::col("name_key").like(LikeExpr::new(format!("{}%", like_escape(prefix))).escape('\\')));
    }
    if let Some(after) = after {
        select.and_where(Expr::col("name_key").gt(after));
    }
    select.order_by("name_key", Order::Asc).limit(limit);
    select
}

pub(crate) struct NewGroup<'a> {
    pub(crate) name: &'a str,
    pub(crate) name_key: &'a str,
    pub(crate) description: Option<&'a str>,
    pub(crate) open: bool,
    pub(crate) metadata: Option<Vec<u8>>,
    pub(crate) owner: i64,
    pub(crate) now: i64,
}

pub(crate) fn insert_group(g: NewGroup<'_>) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(GROUPS)
        .columns(["name", "name_key", "description", "is_open", "metadata", "owner_id", "created_at", "updated_at"])
        .values([
            g.name.into(),
            g.name_key.into(),
            g.description.map(str::to_string).into(),
            i64::from(g.open).into(),
            g.metadata.into(),
            g.owner.into(),
            g.now.into(),
            g.now.into(),
        ])
        .map_err(build)?;
    Ok(insert)
}

/// What an update sets (absent: unchanged; `Some(None)`: cleared).
pub(crate) struct GroupUpdate<'a> {
    pub(crate) name: Option<(&'a str, &'a str)>,
    pub(crate) description: Option<Option<&'a str>>,
    pub(crate) open: Option<bool>,
    pub(crate) metadata: Option<Option<Vec<u8>>>,
    pub(crate) now: i64,
}

pub(crate) fn update_group(group: i64, u: GroupUpdate<'_>) -> UpdateStatement {
    let mut update = Query::update();
    update.table(GROUPS).value("updated_at", u.now).and_where(Expr::col("id").eq(group));
    if let Some((name, key)) = u.name {
        update.value("name", name).value("name_key", key);
    }
    if let Some(description) = u.description {
        update.value("description", description.map(str::to_string));
    }
    if let Some(open) = u.open {
        update.value("is_open", i64::from(open));
    }
    if let Some(metadata) = u.metadata {
        update.value("metadata", metadata);
    }
    update
}

/// A group's id and its chat room.
#[cfg(feature = "chat")]
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct RoomRefRow {
    #[allow(dead_code)]
    pub(crate) id: i64,
    pub(crate) chat_room: Option<i64>,
}

/// The groups that name one of these chat rooms.
#[cfg(feature = "chat")]
pub(crate) fn groups_with_rooms(rooms: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "chat_room"]).from(GROUPS).and_where(Expr::col("chat_room").is_in(rooms.iter().copied()));
    select
}

pub(crate) fn set_chat_room(group: i64, room: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(GROUPS).value("chat_room", room).and_where(Expr::col("id").eq(group));
    update
}

/// How many members a group has.
pub(crate) fn count_members(group: i64) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(MEMBERS).and_where(Expr::col("group_id").eq(group));
    select
}

/// The member counts of these groups (groups without members are absent).
pub(crate) fn member_counts(groups: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select
        .column("group_id")
        .expr_as(Expr::col("id").count(), "n")
        .from(MEMBERS)
        .and_where(Expr::col("group_id").is_in(groups.iter().copied()))
        .group_by_col("group_id");
    select
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct GroupCountRow {
    pub(crate) group_id: i64,
    pub(crate) n: i64,
}

pub(crate) fn set_owner(group: i64, owner: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(GROUPS).value("owner_id", owner).value("updated_at", now).and_where(Expr::col("id").eq(group));
    update
}

pub(crate) fn delete_group(group: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(GROUPS).and_where(Expr::col("id").eq(group));
    delete
}

pub(crate) fn member(group: i64, user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("group_id").eq(group)).and_where(Expr::col("user_id").eq(user));
    select
}

/// A player's memberships, oldest first.
pub(crate) fn memberships(user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("user_id").eq(user)).order_by("id", Order::Asc);
    select
}

/// A page of a group's members in the order they joined, after the row id `after`.
pub(crate) fn members(group: i64, after: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("group_id").eq(group));
    if let Some(after) = after {
        select.and_where(Expr::col("id").gt(after));
    }
    select.order_by("id", Order::Asc).limit(limit);
    select
}

/// Every member id of a group (at most `limit`).
pub(crate) fn member_ids(group: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.column("user_id").from(MEMBERS).and_where(Expr::col("group_id").eq(group)).limit(limit);
    select
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct UserRow {
    pub(crate) user_id: i64,
}

pub(crate) fn insert_member(group: i64, user: i64, role: &str, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(MEMBERS)
        .columns(["group_id", "user_id", "role", "joined_at"])
        .values([group.into(), user.into(), role.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

pub(crate) fn set_role(id: i64, role: &str) -> UpdateStatement {
    let mut update = Query::update();
    update.table(MEMBERS).value("role", role).and_where(Expr::col("id").eq(id));
    update
}

pub(crate) fn delete_member(id: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(MEMBERS).and_where(Expr::col("id").eq(id));
    delete
}

/// How many groups a player belongs to.
pub(crate) fn count_memberships(user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(MEMBERS).and_where(Expr::col("user_id").eq(user));
    select
}

pub(crate) fn invite(group: i64, user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(INVITE_COLUMNS).from(INVITES).and_where(Expr::col("group_id").eq(group)).and_where(Expr::col("user_id").eq(user));
    select
}

pub(crate) fn count_invites(group: i64) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(INVITES).and_where(Expr::col("group_id").eq(group));
    select
}

pub(crate) fn insert_invite(group: i64, user: i64, inviter: i64, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(INVITES)
        .columns(["group_id", "user_id", "inviter_id", "created_at"])
        .values([group.into(), user.into(), inviter.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

pub(crate) fn delete_invite(id: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(INVITES).and_where(Expr::col("id").eq(id));
    delete
}

/// A page of a player's invitations, newest first, before the row id `before`.
pub(crate) fn invites_of(user: i64, before: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(INVITE_COLUMNS).from(INVITES).and_where(Expr::col("user_id").eq(user));
    if let Some(before) = before {
        select.and_where(Expr::col("id").lt(before));
    }
    select.order_by("id", Order::Desc).limit(limit);
    select
}

/// Groups without an owner (the owner's account was deleted), at most `limit`.
pub(crate) fn ownerless_groups(limit: u64) -> SelectStatement {
    let mut owners = Query::select();
    owners
        .column((MEMBERS, "id"))
        .from(MEMBERS)
        .and_where(Expr::col((MEMBERS, "group_id")).equals((GROUPS, "id")))
        .and_where(Expr::col((MEMBERS, "role")).eq(OWNER));
    let mut select = Query::select();
    select.column((GROUPS, "id")).from(GROUPS).and_where(Expr::not_exists(owners)).order_by((GROUPS, "id"), Order::Asc).limit(limit);
    select
}

/// The member of a group with this role who joined first.
pub(crate) fn oldest_with(group: i64, role: &str) -> SelectStatement {
    let mut select = Query::select();
    select
        .columns(MEMBER_COLUMNS)
        .from(MEMBERS)
        .and_where(Expr::col("group_id").eq(group))
        .and_where(Expr::col("role").eq(role))
        .order_by("id", Order::Asc)
        .limit(1);
    select
}

/// The display names of accounts.
pub(crate) fn names(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "display_name"]).from("auth_users").and_where(Expr::col("id").is_in(users.iter().copied()));
    select
}

/// The caller's memberships among these groups.
pub(crate) fn roles_in(user: i64, groups: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MEMBER_COLUMNS).from(MEMBERS).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("group_id").is_in(groups.iter().copied()));
    select
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::render_statement;

    #[test]
    fn statements_render() {
        let search = render_statement(&search(Some("50%_a"), Some("4"), 21), Dialect::MySql);
        assert!(search.contains("LIKE '50\\\\%\\\\_a%'") && search.contains("`name_key` > '4'") && search.contains("ORDER BY `name_key` ASC"), "{search}");
        assert!(render_statement(&lock_group(1, Dialect::Postgres), Dialect::Postgres).ends_with("FOR NO KEY UPDATE"));
        assert!(render_statement(&lock_user(1, Dialect::MySql), Dialect::MySql).ends_with("FOR UPDATE"));
        let update =
            render_statement(&update_group(3, GroupUpdate { name: None, description: Some(None), open: Some(true), metadata: None, now: 9 }), Dialect::Sqlite);
        assert!(update.contains("\"description\" = NULL") && update.contains("\"is_open\" = 1") && !update.contains("\"name\""), "{update}");
        assert_eq!(like_escape("50%_a\\b"), "50\\%\\_a\\\\b");
        for dialect in Dialect::ALL {
            let ownerless = render_statement(&ownerless_groups(100), *dialect);
            assert!(ownerless.contains("NOT EXISTS") && ownerless.contains("'owner'") && ownerless.contains("LIMIT 100"), "{ownerless}");
            let oldest = render_statement(&oldest_with(4, ADMIN), *dialect);
            assert!(oldest.contains("'admin'") && oldest.contains("ASC") && oldest.contains("LIMIT 1"), "{oldest}");
        }
    }
}
