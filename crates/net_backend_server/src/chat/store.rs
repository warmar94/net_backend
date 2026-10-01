//! The chat module's SQL, as sea-query statements (one statement, three dialects). Every
//! statement runs in the chat suite on SQLite locally and on MySQL / PostgreSQL (env-gated).

use sea_query::{Cond, Expr, ExprTrait, InsertStatement, Order, Query, SelectStatement, UpdateStatement};

use super::migrations::{MEMBERS, MESSAGES, ROOMS};
use crate::db::DbError;

pub(crate) const KIND_ROOM: &str = "room";
pub(crate) const KIND_DM: &str = "dm";
pub(crate) const KIND_GROUP: &str = "group";

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

const ROOM_COLUMNS: [&str; 8] = ["id", "kind", "room_key", "name", "max_members", "dm_a", "dm_b", "last_activity_at"];

#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct RoomRow {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) room_key: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) max_members: Option<i64>,
    pub(crate) dm_a: Option<i64>,
    pub(crate) dm_b: Option<i64>,
    pub(crate) last_activity_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct MessageRow {
    pub(crate) id: i64,
    pub(crate) room_id: i64,
    pub(crate) sender_id: i64,
    pub(crate) sender_name: Option<String>,
    pub(crate) body: String,
    pub(crate) nonce: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) deleted_at: Option<i64>,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct NameRow {
    #[allow(dead_code)]
    pub(crate) id: i64,
    pub(crate) display_name: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    pub(crate) id: i64,
}

fn rooms() -> SelectStatement {
    let mut select = Query::select();
    select.columns(ROOM_COLUMNS).from(ROOMS);
    select
}

pub(crate) fn room_by_id(id: i64) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("id").eq(id));
    select
}

pub(crate) fn room_by_key(key: &str) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("room_key").eq(key));
    select
}

/// The direct-message room of two users (`a` < `b`).
pub(crate) fn dm_room(a: i64, b: i64) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("dm_a").eq(a)).and_where(Expr::col("dm_b").eq(b));
    select
}

/// A page of public rooms by id.
pub(crate) fn public_rooms(after: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("kind").eq(KIND_ROOM));
    if let Some(after) = after {
        select.and_where(Expr::col("id").gt(after));
    }
    select.order_by("id", Order::Asc).limit(limit);
    select
}

/// A page of a user's direct-message rooms, newest activity first, after the cursor
/// `(activity, id)`.
pub(crate) fn dms_of(user: i64, before: Option<(i64, i64)>, limit: u64) -> SelectStatement {
    let mut select = rooms();
    select.and_where(Expr::col("kind").eq(KIND_DM)).cond_where(Cond::any().add(Expr::col("dm_a").eq(user)).add(Expr::col("dm_b").eq(user)));
    if let Some((activity, id)) = before {
        select.cond_where(
            Cond::any()
                .add(Expr::col("last_activity_at").lt(activity))
                .add(Cond::all().add(Expr::col("last_activity_at").eq(activity)).add(Expr::col("id").lt(id))),
        );
    }
    select.order_by("last_activity_at", Order::Desc).order_by("id", Order::Desc).limit(limit);
    select
}

/// A new room.
pub(crate) fn insert_room(
    kind: &str,
    key: Option<&str>,
    name: Option<&str>,
    max_members: Option<i64>,
    dm: Option<(i64, i64)>,
    now: i64,
) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(ROOMS)
        .columns(["kind", "room_key", "name", "max_members", "dm_a", "dm_b", "created_at", "last_activity_at"])
        .values([
            kind.into(),
            key.map(str::to_string).into(),
            name.map(str::to_string).into(),
            max_members.into(),
            dm.map(|d| d.0).into(),
            dm.map(|d| d.1).into(),
            now.into(),
            now.into(),
        ])
        .map_err(build)?;
    Ok(insert)
}

/// Change a public room's name and cap.
pub(crate) fn update_room(id: i64, name: Option<&str>, max_members: Option<i64>) -> UpdateStatement {
    let mut update = Query::update();
    update.table(ROOMS).value("name", name.map(str::to_string)).value("max_members", max_members).and_where(Expr::col("id").eq(id));
    update
}

/// A message makes the room the newest in its members' lists.
pub(crate) fn touch_room(id: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(ROOMS).value("last_activity_at", now).and_where(Expr::col("id").eq(id));
    update
}

pub(crate) fn member(room: i64, user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(MEMBERS).and_where(Expr::col("room_id").eq(room)).and_where(Expr::col("user_id").eq(user));
    select
}

pub(crate) fn insert_member(room: i64, user: i64, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(MEMBERS).columns(["room_id", "user_id", "created_at"]).values([room.into(), user.into(), now.into()]).map_err(build)?;
    Ok(insert)
}

pub(crate) fn delete_member(room: i64, user: i64) -> sea_query::DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(MEMBERS).and_where(Expr::col("room_id").eq(room)).and_where(Expr::col("user_id").eq(user));
    delete
}

/// The display names of accounts (absent rows: no such account).
pub(crate) fn names(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "display_name"]).from("auth_users").and_where(Expr::col("id").is_in(users.iter().copied()));
    select
}

pub(crate) fn insert_message(room: i64, sender: i64, sender_name: Option<&str>, body: &str, nonce: Option<&str>, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(MESSAGES)
        .columns(["room_id", "sender_id", "sender_name", "body", "nonce", "created_at"])
        .values([room.into(), sender.into(), sender_name.map(str::to_string).into(), body.into(), nonce.map(str::to_string).into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

const MESSAGE_COLUMNS: [&str; 8] = ["id", "room_id", "sender_id", "sender_name", "body", "nonce", "created_at", "deleted_at"];

/// A page of a room's history: newest first, before the message id `before`, not deleted, not
/// older than `since`.
pub(crate) fn history(room: i64, before: Option<i64>, since: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MESSAGE_COLUMNS).from(MESSAGES).and_where(Expr::col("room_id").eq(room)).and_where(Expr::col("deleted_at").is_null());
    if let Some(before) = before {
        select.and_where(Expr::col("id").lt(before));
    }
    if let Some(since) = since {
        select.and_where(Expr::col("created_at").gte(since));
    }
    select.order_by("id", Order::Desc).limit(limit);
    select
}

pub(crate) fn message(room: i64, id: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(MESSAGE_COLUMNS).from(MESSAGES).and_where(Expr::col("room_id").eq(room)).and_where(Expr::col("id").eq(id));
    select
}

/// Delete a message (moderation): the body is cleared, the row stays (ids keep growing).
/// 0 rows: deleted already.
pub(crate) fn delete_message(id: i64, by: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(MESSAGES)
        .value("body", "")
        .value("deleted_at", now)
        .value("deleted_by", by)
        .and_where(Expr::col("id").eq(id))
        .and_where(Expr::col("deleted_at").is_null());
    update
}

/// Up to `limit` ids of messages older than the retention, oldest first (the purge deletes in
/// batches: one huge DELETE would hold its locks for long).
pub(crate) fn expired(cutoff: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(MESSAGES).and_where(Expr::col("created_at").lt(cutoff)).order_by("id", Order::Asc).limit(limit);
    select
}

/// Delete these messages (a batch of [`expired`]).
pub(crate) fn delete_messages(ids: &[i64]) -> sea_query::DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(MESSAGES).and_where(Expr::col("id").is_in(ids.iter().copied()));
    delete
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{render_statement, Dialect};

    #[test]
    fn statements_render() {
        let page = render_statement(&dms_of(4, Some((10, 7)), 21), Dialect::Postgres);
        assert!(page.contains("ORDER BY \"last_activity_at\" DESC, \"id\" DESC") && page.contains("\"dm_a\" = 4 OR \"dm_b\" = 4"), "{page}");
        let expired = render_statement(&expired(77, 1000), Dialect::Postgres);
        assert!(expired.contains("\"created_at\" < 77") && expired.ends_with("LIMIT 1000"), "{expired}");
        let history = render_statement(&history(1, Some(50), Some(9), 11), Dialect::MySql);
        assert!(history.contains("`deleted_at` IS NULL") && history.contains("`id` < 50") && history.contains("`created_at` >= 9"), "{history}");
    }
}
