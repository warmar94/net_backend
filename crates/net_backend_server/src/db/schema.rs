//! Portable column types for framework and module tables, built with sea-query's schema builder.
//!
//! The rule (owner decision): ids are `BIGINT` (auto-increment primary keys), timestamps are
//! `BIGINT` unix milliseconds ([`UnixMillis`](net_backend_protocol::UnixMillis)), JSON and binary
//! payloads are bytes (`LONGBLOB` / `BYTEA` / `BLOB`), strings are `VARCHAR(n)` (`utf8mb4` with the
//! case-sensitive `utf8mb4_bin` collation on MySQL, see [`render_table`]). No native timestamp, JSON or enum types: the same row decodes the same way everywhere.
//!
//! These helpers are for writing (and generating) a module's migration SQL; migrations themselves
//! are plain SQL files per dialect ([`crate::migrate`]).

use sea_query::{ColumnDef, Index, IndexCreateStatement, Table, TableCreateStatement};

use super::Dialect;

/// `BIGINT` auto-increment primary key (SQLite: `INTEGER PRIMARY KEY AUTOINCREMENT`, the 64-bit rowid).
pub fn id(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.big_integer().not_null().auto_increment().primary_key();
    column
}

/// A `BIGINT NOT NULL` reference to another table's id.
pub fn foreign_id(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.big_integer().not_null();
    column
}

/// A `BIGINT NOT NULL` timestamp in unix milliseconds.
pub fn unix_millis(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.big_integer().not_null();
    column
}

/// A `BIGINT NOT NULL` counter or score.
pub fn big_int(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.big_integer().not_null();
    column
}

/// A `VARCHAR(len) NOT NULL` string.
pub fn string(name: &'static str, len: u32) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.string_len(len).not_null();
    column
}

/// A `BOOLEAN NOT NULL` (MySQL `TINYINT(1)`).
pub fn boolean(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.boolean().not_null();
    column
}

/// A bytes column for payloads up to the backend's limit: MySQL `LONGBLOB` (4 GiB; a plain MySQL
/// `BLOB` stops at 64 KiB), PostgreSQL `BYTEA`, SQLite `BLOB`. JSON is stored as its UTF-8 bytes.
pub fn bytes(name: &'static str, dialect: Dialect) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    match dialect {
        Dialect::MySql => column.custom("LONGBLOB"),
        Dialect::Postgres => column.custom("BYTEA"),
        Dialect::Sqlite => column.custom("BLOB"),
    };
    column.not_null();
    column
}

/// A new `CREATE TABLE IF NOT EXISTS` statement.
pub fn create_table(name: &'static str) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(name).if_not_exists();
    table
}

/// The SQL text of a `CREATE TABLE` in a dialect; on MySQL with `ENGINE=InnoDB`, `utf8mb4` and the
/// binary collation `utf8mb4_bin`: string comparisons and unique indexes are case- and
/// accent-sensitive as on PostgreSQL and SQLite (MySQL's default collations would treat `Sword`
/// and `sword` as duplicates). One difference stays: `utf8mb4_bin` ignores trailing spaces in
/// comparisons (PAD SPACE), PostgreSQL and SQLite do not. Normalise in code (trim, lower-case
/// emails) instead of relying on the collation.
pub fn render_table(table: &TableCreateStatement, dialect: Dialect) -> String {
    match dialect {
        Dialect::MySql => {
            let mut table = table.clone();
            table.engine("InnoDB").character_set("utf8mb4").collate("utf8mb4_bin");
            table.to_string(sea_query::MysqlQueryBuilder)
        }
        Dialect::Postgres => table.to_string(sea_query::PostgresQueryBuilder),
        Dialect::Sqlite => table.to_string(sea_query::SqliteQueryBuilder),
    }
}

/// A new `CREATE INDEX` statement on `table(columns…)`.
pub fn create_index(name: &'static str, table: &'static str, columns: &[&'static str]) -> IndexCreateStatement {
    let mut index = Index::create();
    index.name(name).table(table);
    for column in columns {
        index.col(*column);
    }
    index
}

/// The SQL text of a `CREATE INDEX` in a dialect.
pub fn render_index(index: &IndexCreateStatement, dialect: Dialect) -> String {
    match dialect {
        Dialect::MySql => index.to_string(sea_query::MysqlQueryBuilder),
        Dialect::Postgres => index.to_string(sea_query::PostgresQueryBuilder),
        Dialect::Sqlite => index.to_string(sea_query::SqliteQueryBuilder),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(dialect: Dialect) -> String {
        let mut table = create_table("scores");
        table.col(id("id")).col(foreign_id("user_id")).col(big_int("points")).col(bytes("data", dialect)).col(unix_millis("created_at"));
        render_table(&table, dialect)
    }

    /// Snapshot of the portable types in each dialect (changes here change every migration).
    #[test]
    fn portable_types_per_dialect() {
        let mysql = example(Dialect::MySql);
        assert!(mysql.starts_with("CREATE TABLE IF NOT EXISTS `scores`"), "{mysql}");
        assert!(mysql.contains("`id` bigint NOT NULL PRIMARY KEY AUTO_INCREMENT"), "{mysql}");
        assert!(mysql.contains("`data` LONGBLOB NOT NULL"), "{mysql}");
        assert!(mysql.contains("ENGINE=InnoDB") && mysql.contains("COLLATE=utf8mb4_bin"), "{mysql}");

        let pg = example(Dialect::Postgres);
        assert!(pg.contains("\"id\" bigint GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY"), "{pg}");
        assert!(pg.contains("\"data\" BYTEA NOT NULL"), "{pg}");
        assert!(pg.contains("\"created_at\" bigint NOT NULL"), "{pg}");

        let sqlite = example(Dialect::Sqlite);
        assert!(sqlite.contains("\"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT"), "{sqlite}");
        assert!(sqlite.contains("\"data\" BLOB NOT NULL"), "{sqlite}");
        // SQLite's INTEGER is 64-bit: the portable BIGINT.
        assert!(sqlite.contains("\"created_at\" integer NOT NULL"), "{sqlite}");

        let index = create_index("scores_user", "scores", &["user_id", "points"]);
        assert!(render_index(&index, Dialect::Postgres).contains("CREATE INDEX \"scores_user\" ON \"scores\" (\"user_id\", \"points\")"));
    }
}
