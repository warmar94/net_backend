//! The notifications module's SQL, as sea-query statements (one statement, three dialects). Every
//! statement runs in the notifications suite on SQLite locally and on MySQL / PostgreSQL
//! (env-gated).
//!
//! **No ranged writes:** marking, trimming and the purge first read the ids with a plain SELECT and
//! then change exactly those rows by primary key, so MySQL takes record locks only (no gap locks on
//! the user index, no deadlocks between players).

use sea_query::{DeleteStatement, Expr, ExprTrait, InsertStatement, Order, Query, SelectStatement, UpdateStatement};

use super::migrations::NOTIFICATIONS;
use crate::db::DbError;

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

const COLUMNS: [&str; 7] = ["id", "kind", "text", "data", "sender_id", "read_at", "created_at"];

/// One stored notification.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct NotificationRow {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) text: Option<String>,
    pub(crate) data: Option<Vec<u8>>,
    pub(crate) sender_id: Option<i64>,
    pub(crate) read_at: Option<i64>,
    pub(crate) created_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    pub(crate) id: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct CountRow {
    pub(crate) n: i64,
}

/// What a new notification stores.
pub(crate) struct New<'a> {
    pub(crate) user: i64,
    pub(crate) kind: &'a str,
    pub(crate) text: Option<&'a str>,
    pub(crate) data: Option<Vec<u8>>,
    pub(crate) sender: Option<i64>,
    pub(crate) now: i64,
}

pub(crate) fn insert(n: New<'_>) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(NOTIFICATIONS)
        .columns(["user_id", "kind", "text", "data", "sender_id", "created_at"])
        .values([n.user.into(), n.kind.into(), n.text.map(str::to_string).into(), n.data.into(), n.sender.into(), n.now.into()])
        .map_err(build)?;
    Ok(insert)
}

/// The ids of a player's notifications beyond the newest `keep` (oldest last), at most `limit`.
pub(crate) fn overflow(user: i64, keep: u64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(NOTIFICATIONS).and_where(Expr::col("user_id").eq(user)).order_by("id", Order::Desc).limit(limit).offset(keep);
    select
}

/// A page of a player's notifications, newest first, before the id `before`.
pub(crate) fn page(user: i64, before: Option<i64>, unread_only: bool, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(COLUMNS).from(NOTIFICATIONS).and_where(Expr::col("user_id").eq(user));
    if unread_only {
        select.and_where(Expr::col("read_at").is_null());
    }
    if let Some(before) = before {
        select.and_where(Expr::col("id").lt(before));
    }
    select.order_by("id", Order::Desc).limit(limit);
    select
}

/// How many notifications a player has (only the unread ones with `unread_only`).
pub(crate) fn count(user: i64, unread_only: bool) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(NOTIFICATIONS).and_where(Expr::col("user_id").eq(user));
    if unread_only {
        select.and_where(Expr::col("read_at").is_null());
    }
    select
}

/// The ids among `ids` (or all, with `None`) of a player's notifications that are in the other
/// state (unread when marking read, read when marking unread).
pub(crate) fn to_mark(user: i64, ids: Option<&[i64]>, read: bool) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(NOTIFICATIONS).and_where(Expr::col("user_id").eq(user));
    if let Some(ids) = ids {
        select.and_where(Expr::col("id").is_in(ids.iter().copied()));
    }
    if read {
        select.and_where(Expr::col("read_at").is_null());
    } else {
        select.and_where(Expr::col("read_at").is_not_null());
    }
    select
}

/// Set (or clear) `read_at` of these notifications.
pub(crate) fn mark(ids: &[i64], read_at: Option<i64>) -> UpdateStatement {
    let mut update = Query::update();
    update.table(NOTIFICATIONS).value("read_at", read_at).and_where(Expr::col("id").is_in(ids.iter().copied()));
    update
}

/// Delete one of a player's notifications.
pub(crate) fn delete(user: i64, id: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(NOTIFICATIONS).and_where(Expr::col("id").eq(id)).and_where(Expr::col("user_id").eq(user));
    delete
}

/// Up to `limit` ids of notifications created before `cutoff`, oldest first.
pub(crate) fn expired(cutoff: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(NOTIFICATIONS).and_where(Expr::col("created_at").lt(cutoff)).order_by("id", Order::Asc).limit(limit);
    select
}

/// Delete these notifications.
pub(crate) fn delete_ids(ids: &[i64]) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(NOTIFICATIONS).and_where(Expr::col("id").is_in(ids.iter().copied()));
    delete
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{render_statement, Dialect};

    #[test]
    fn statements_render() {
        let page = render_statement(&page(4, Some(90), true, 21), Dialect::Postgres);
        assert!(page.contains("\"read_at\" IS NULL") && page.contains("\"id\" < 90") && page.contains("ORDER BY \"id\" DESC"), "{page}");
        let over = render_statement(&overflow(4, 200, 1000), Dialect::MySql);
        assert!(over.contains("LIMIT 1000 OFFSET 200"), "{over}");
        let over = render_statement(&overflow(4, 200, 1000), Dialect::Sqlite);
        assert!(over.contains("LIMIT 1000 OFFSET 200"), "{over}");
        let mark = render_statement(&to_mark(4, Some(&[1, 2]), false), Dialect::Sqlite);
        assert!(mark.contains("\"read_at\" IS NOT NULL") && mark.contains("IN (1, 2)"), "{mark}");
        let clear = render_statement(&self::mark(&[3], None), Dialect::Postgres);
        assert!(clear.contains("\"read_at\" = NULL"), "{clear}");
    }
}
