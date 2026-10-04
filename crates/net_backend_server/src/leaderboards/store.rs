//! The leaderboards module's SQL, as sea-query statements (one statement, three dialects). Every
//! statement runs in the leaderboards suite on SQLite locally and on MySQL / PostgreSQL
//! (env-gated).
//!
//! **Ranking:** rows rank by `(rank_key, achieved_at, user_id)` ascending (`rank_key` is the score
//! on `asc` boards and its negation on `desc` boards), so one index serves every board. A rank is
//! 1 + the number of rows of the same board and period that come before.

use sea_query::{Cond, DeleteStatement, Expr, ExprTrait, InsertStatement, LockType, Order, Query, SelectStatement, UpdateStatement};

use super::migrations::SCORES;
use crate::db::{DbError, Dialect};

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

const COLUMNS: [&str; 5] = ["user_id", "score", "rank_key", "metadata", "achieved_at"];

/// One stored score.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct ScoreRow {
    pub(crate) user_id: i64,
    pub(crate) score: i64,
    pub(crate) rank_key: i64,
    pub(crate) metadata: Option<Vec<u8>>,
    pub(crate) achieved_at: i64,
}

impl ScoreRow {
    /// Its place in the ranking.
    pub(crate) fn position(&self) -> Position {
        Position { rank_key: self.rank_key, achieved_at: self.achieved_at, user: self.user_id }
    }
}

/// A row's place in the ranking: `(rank_key, achieved_at, user_id)`, ascending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) rank_key: i64,
    pub(crate) achieved_at: i64,
    pub(crate) user: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct CountRow {
    pub(crate) n: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    pub(crate) id: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct NameRow {
    pub(crate) id: i64,
    pub(crate) display_name: Option<String>,
}

fn of_period(board: &str, period: i64) -> Cond {
    Cond::all().add(Expr::col("board").eq(board)).add(Expr::col("period_start").eq(period))
}

/// Rows strictly before `p` in the ranking.
fn before(p: Position) -> Cond {
    Cond::any()
        .add(Expr::col("rank_key").lt(p.rank_key))
        .add(Cond::all().add(Expr::col("rank_key").eq(p.rank_key)).add(Expr::col("achieved_at").lt(p.achieved_at)))
        .add(Cond::all().add(Expr::col("rank_key").eq(p.rank_key)).add(Expr::col("achieved_at").eq(p.achieved_at)).add(Expr::col("user_id").lt(p.user)))
}

/// Rows strictly after `p` in the ranking.
fn after(p: Position) -> Cond {
    Cond::any()
        .add(Expr::col("rank_key").gt(p.rank_key))
        .add(Cond::all().add(Expr::col("rank_key").eq(p.rank_key)).add(Expr::col("achieved_at").gt(p.achieved_at)))
        .add(Cond::all().add(Expr::col("rank_key").eq(p.rank_key)).add(Expr::col("achieved_at").eq(p.achieved_at)).add(Expr::col("user_id").gt(p.user)))
}

/// One player's score in a period.
pub(crate) fn score(board: &str, period: i64, user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(COLUMNS).from(SCORES).cond_where(of_period(board, period).add(Expr::col("user_id").eq(user)));
    select
}

/// How many rows come before `p` (its rank is this + 1).
pub(crate) fn count_before(board: &str, period: i64, p: Position) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(SCORES).cond_where(of_period(board, period).add(before(p)));
    select
}

/// How many players have a score in the period.
pub(crate) fn count_all(board: &str, period: i64) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(SCORES).cond_where(of_period(board, period));
    select
}

/// A page of the ranking after `cursor` (best first).
pub(crate) fn page(board: &str, period: i64, cursor: Option<Position>, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    let mut cond = of_period(board, period);
    if let Some(cursor) = cursor {
        cond = cond.add(after(cursor));
    }
    select
        .columns(COLUMNS)
        .from(SCORES)
        .cond_where(cond)
        .order_by("rank_key", Order::Asc)
        .order_by("achieved_at", Order::Asc)
        .order_by("user_id", Order::Asc)
        .limit(limit);
    select
}

/// Up to `limit` rows right before `p`, nearest first.
pub(crate) fn above(board: &str, period: i64, p: Position, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select
        .columns(COLUMNS)
        .from(SCORES)
        .cond_where(of_period(board, period).add(before(p)))
        .order_by("rank_key", Order::Desc)
        .order_by("achieved_at", Order::Desc)
        .order_by("user_id", Order::Desc)
        .limit(limit);
    select
}

/// The display names of accounts.
pub(crate) fn names(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "display_name"]).from("auth_users").and_where(Expr::col("id").is_in(users.iter().copied()));
    select
}

/// The account row, locked for the transaction: every write of a player's scores takes it first,
/// so they run one at a time per player (the read before the write stays true until the commit;
/// no gap locks, so no deadlocks between players on MySQL). MySQL `FOR UPDATE`; PostgreSQL
/// `FOR NO KEY UPDATE`; SQLite's write transaction holds the database lock anyway. `None` if the
/// account does not exist.
pub(crate) fn lock_user(user: i64, dialect: Dialect) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from("auth_users").and_where(Expr::col("id").eq(user));
    match dialect {
        Dialect::Postgres => select.lock(LockType::NoKeyUpdate),
        _ => select.lock_exclusive(),
    };
    select
}

/// What a write stores.
pub(crate) struct Write<'a> {
    pub(crate) board: &'a str,
    pub(crate) period: i64,
    pub(crate) user: i64,
    pub(crate) score: i64,
    pub(crate) rank_key: i64,
    pub(crate) metadata: Option<Vec<u8>>,
    pub(crate) achieved_at: i64,
    pub(crate) now: i64,
}

/// A player's first score in a period.
pub(crate) fn insert(w: Write<'_>) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(SCORES)
        .columns(["board", "period_start", "user_id", "score", "rank_key", "metadata", "submissions", "achieved_at", "updated_at"])
        .values([
            w.board.into(),
            w.period.into(),
            w.user.into(),
            w.score.into(),
            w.rank_key.into(),
            w.metadata.into(),
            1i64.into(),
            w.achieved_at.into(),
            w.now.into(),
        ])
        .map_err(build)?;
    Ok(insert)
}

/// Replace a player's stored score (and count the submission).
pub(crate) fn update(w: Write<'_>) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(SCORES)
        .value("score", w.score)
        .value("rank_key", w.rank_key)
        .value("metadata", w.metadata)
        .value("achieved_at", w.achieved_at)
        .value("submissions", Expr::col("submissions").add(1))
        .value("updated_at", w.now)
        .cond_where(of_period(w.board, w.period).add(Expr::col("user_id").eq(w.user)));
    update
}

/// Count a submission that did not change the stored score.
pub(crate) fn touch(board: &str, period: i64, user: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(SCORES)
        .value("submissions", Expr::col("submissions").add(1))
        .value("updated_at", now)
        .cond_where(of_period(board, period).add(Expr::col("user_id").eq(user)));
    update
}

/// Delete one player's score in a period.
pub(crate) fn delete(board: &str, period: i64, user: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(SCORES).cond_where(of_period(board, period).add(Expr::col("user_id").eq(user)));
    delete
}

/// Up to `limit` ids of a board's rows from periods before `cutoff` (the purge deletes in batches).
pub(crate) fn expired(board: &str, cutoff: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select
        .column("id")
        .from(SCORES)
        .and_where(Expr::col("board").eq(board))
        .and_where(Expr::col("period_start").lt(cutoff))
        .order_by("id", Order::Asc)
        .limit(limit);
    select
}

/// Delete these rows (a batch of [`expired`]).
pub(crate) fn delete_ids(ids: &[i64]) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(SCORES).and_where(Expr::col("id").is_in(ids.iter().copied()));
    delete
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::render_statement;

    #[test]
    fn statements_render() {
        let p = Position { rank_key: -50, achieved_at: 7, user: 3 };
        let count = render_statement(&count_before("hs", 0, p), Dialect::Postgres);
        assert!(count.contains("\"rank_key\" < -50") && count.contains("\"achieved_at\" < 7") && count.contains("\"user_id\" < 3"), "{count}");
        let page = render_statement(&page("hs", 0, Some(p), 11), Dialect::MySql);
        assert!(page.contains("ORDER BY `rank_key` ASC, `achieved_at` ASC, `user_id` ASC") && page.ends_with("LIMIT 11"), "{page}");
        assert!(page.contains("`rank_key` > -50"), "{page}");
        let above = render_statement(&above("hs", 0, p, 5), Dialect::Sqlite);
        assert!(above.contains("ORDER BY \"rank_key\" DESC"), "{above}");
        assert!(render_statement(&lock_user(1, Dialect::Postgres), Dialect::Postgres).ends_with("FOR NO KEY UPDATE"));
        assert!(render_statement(&lock_user(1, Dialect::MySql), Dialect::MySql).ends_with("FOR UPDATE"));
        let w = Write { board: "hs", period: 0, user: 1, score: 5, rank_key: -5, metadata: None, achieved_at: 9, now: 9 };
        assert!(render_statement(&update(w), Dialect::Postgres).contains("\"submissions\" = \"submissions\" + 1"));
    }
}
