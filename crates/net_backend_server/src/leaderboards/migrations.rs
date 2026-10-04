//! The leaderboards module's table, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `leaderboard_scores` | one row per board, period and player (`auth_users`, cascade): the stored score, its rank key (the score, negated on `desc` boards, so every board ranks ascending), the metadata's JSON bytes, the number of submissions, when the score was reached, the last submission |
//!
//! `period_start` is the period's start (unix ms) on daily / weekly boards and 0 on all-time
//! boards. The unique index `(board, period_start, user_id)` is a score's identity; the index
//! `(board, period_start, rank_key, achieved_at, user_id)` serves the pages, the ranks (a count of
//! the better rows) and the purge of old periods.

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

/// The table.
pub(crate) const SCORES: &str = "leaderboard_scores";

/// A nullable bytes column (see [`schema::bytes`]).
fn nullable_bytes(name: &'static str, dialect: Dialect) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    match dialect {
        Dialect::MySql => column.custom("LONGBLOB"),
        Dialect::Postgres => column.custom("BYTEA"),
        Dialect::Sqlite => column.custom("BLOB"),
    };
    column.null();
    column
}

fn scores(dialect: Dialect) -> TableCreateStatement {
    let mut t = schema::create_table(SCORES);
    let mut fk = ForeignKey::create();
    fk.name("leaderboard_scores_user_id_fk").from(SCORES, "user_id").to("auth_users", "id").on_delete(ForeignKeyAction::Cascade);
    t.col(schema::id("id"))
        .col(schema::string("board", 64))
        .col(schema::big_int("period_start"))
        .col(schema::foreign_id("user_id"))
        .col(schema::big_int("score"))
        .col(schema::big_int("rank_key"))
        .col(nullable_bytes("metadata", dialect))
        .col(schema::big_int("submissions"))
        .col(schema::unix_millis("achieved_at"))
        .col(schema::unix_millis("updated_at"))
        .foreign_key(&mut fk);
    t
}

fn index(name: &'static str, columns: &[&'static str], unique: bool, dialect: Dialect) -> String {
    let mut index = Index::create();
    index.name(name).table(SCORES);
    for column in columns {
        index.col(*column);
    }
    if unique {
        index.unique();
    }
    render_index(&index, dialect)
}

/// The migrations of the leaderboards module for a dialect (versions `2026100400nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let steps: Vec<(&str, String)> = vec![
        ("create_leaderboard_scores", render_table(&scores(dialect), dialect)),
        ("unique_leaderboard_scores", index("leaderboard_scores_uq", &["board", "period_start", "user_id"], true, dialect)),
        (
            "index_leaderboard_scores_rank",
            index("leaderboard_scores_rank_ix", &["board", "period_start", "rank_key", "achieved_at", "user_id"], false, dialect),
        ),
        ("index_leaderboard_scores_user", index("leaderboard_scores_user_ix", &["user_id"], false, dialect)),
    ];
    steps.into_iter().enumerate().map(|(n, (name, sql))| Migration::new(2026_1004_0000 + n as i64 + 1, name, sql)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_statement_each() {
        for dialect in Dialect::ALL {
            let list = all(*dialect);
            assert_eq!(list.len(), 4);
            assert!(list.windows(2).all(|w| w[0].version < w[1].version));
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}", migration.sql);
            }
        }
        let mysql = all(Dialect::MySql);
        assert!(mysql[0].sql.contains("`metadata` LONGBLOB NULL") && mysql[0].sql.contains("ON DELETE CASCADE"), "{}", mysql[0].sql);
        assert!(mysql[0].sql.contains("utf8mb4_bin"), "{}", mysql[0].sql);
        assert!(all(Dialect::Postgres)[1].sql.starts_with("CREATE UNIQUE INDEX \"leaderboard_scores_uq\""));
        assert!(all(Dialect::Sqlite)[0].sql.contains("\"metadata\" BLOB NULL"), "{}", all(Dialect::Sqlite)[0].sql);
    }
}
