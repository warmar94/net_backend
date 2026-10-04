//! The files module's tables, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `stored_files` | one row per file: owner (`auth_users`, cascade), name, content type, size, SHA-256, the store key (unique), visibility, the game's metadata (JSON bytes, nullable), times |
//! | `stored_file_shares` | the accounts a `shared` file is shared with (file and account cascade) |
//!
//! Indexes: `stored_files (storage_key)` unique, `(owner_id, id)` (a player's files, newest first;
//! the quota); `stored_file_shares (file_id, user_id)` unique, `(user_id)` (the foreign key).

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

/// The files.
pub(crate) const FILES: &str = "stored_files";
/// The shares.
pub(crate) const SHARES: &str = "stored_file_shares";

fn fk(table: &'static str, column: &'static str, target: &'static str) -> sea_query::ForeignKeyCreateStatement {
    let mut fk = ForeignKey::create();
    fk.name(format!("{table}_{column}_fk")).from(table, column).to(target, "id").on_delete(ForeignKeyAction::Cascade);
    fk
}

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

fn files(dialect: Dialect) -> TableCreateStatement {
    let mut t = schema::create_table(FILES);
    t.col(schema::id("id"))
        .col(schema::foreign_id("owner_id"))
        .col(schema::string("name", 255))
        .col(schema::string("content_type", 127))
        .col(schema::big_int("size_bytes"))
        .col(schema::string("sha256", 64))
        .col(schema::string("storage_key", 64))
        .col(schema::string("visibility", 8))
        .col(nullable_bytes("metadata", dialect))
        .col(schema::unix_millis("created_at"))
        .col(schema::unix_millis("updated_at"))
        .foreign_key(&mut fk(FILES, "owner_id", "auth_users"));
    t
}

fn shares() -> TableCreateStatement {
    let mut t = schema::create_table(SHARES);
    t.col(schema::id("id"))
        .col(schema::foreign_id("file_id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::unix_millis("created_at"))
        .foreign_key(&mut fk(SHARES, "file_id", FILES))
        .foreign_key(&mut fk(SHARES, "user_id", "auth_users"));
    t
}

fn index(name: &'static str, table: &'static str, columns: &[&'static str], unique: bool, dialect: Dialect) -> String {
    let mut index = Index::create();
    index.name(name).table(table);
    for column in columns {
        index.col(*column);
    }
    if unique {
        index.unique();
    }
    render_index(&index, dialect)
}

/// The migrations of the files module for a dialect (versions `2026101100nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let steps: Vec<(&str, String)> = vec![
        ("create_stored_files", render_table(&files(dialect), dialect)),
        ("unique_stored_files_key", index("stored_files_key_uq", FILES, &["storage_key"], true, dialect)),
        ("index_stored_files_owner", index("stored_files_owner_ix", FILES, &["owner_id", "id"], false, dialect)),
        ("create_stored_file_shares", render_table(&shares(), dialect)),
        ("unique_stored_file_shares", index("stored_file_shares_uq", SHARES, &["file_id", "user_id"], true, dialect)),
        ("index_stored_file_shares_user", index("stored_file_shares_user_ix", SHARES, &["user_id"], false, dialect)),
    ];
    steps.into_iter().enumerate().map(|(n, (name, sql))| Migration::new(2026_1011_0000 + n as i64 + 1, name, sql)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_statement_each() {
        for dialect in Dialect::ALL {
            let list = all(*dialect);
            assert_eq!(list.len(), 6);
            assert!(list.windows(2).all(|w| w[0].version < w[1].version));
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}", migration.sql);
            }
        }
        let mysql = all(Dialect::MySql);
        assert!(mysql[0].sql.contains("`metadata` LONGBLOB NULL") && mysql[0].sql.contains("ON DELETE CASCADE"), "{}", mysql[0].sql);
        assert!(all(Dialect::Postgres)[3].sql.contains("\"stored_files\""), "{}", all(Dialect::Postgres)[3].sql);
    }
}
