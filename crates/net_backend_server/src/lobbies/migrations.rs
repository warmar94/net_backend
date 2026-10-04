//! The lobbies module's tables, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `lobbies` | one row per lobby: the join code (unique), visibility, state, host (`auth_users`, set to NULL when that account is deleted), the most players, the chat room id, created / updated |
//! | `lobby_members` | one row per member: lobby (cascade), account (cascade), the ready flag (0 or 1 in a BIGINT: decodes the same on every database), joined |
//! | `lobby_metadata` | one row per metadata key: lobby (cascade), key, value |
//!
//! Indexes: `code` unique; `(state, visibility, id)` (the search); `(host_id)`; members
//! `(lobby_id, user_id)` unique, `(user_id)`; metadata `(lobby_id, meta_key)` unique,
//! `(meta_key, meta_value, lobby_id)` (the metadata filters).

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

/// The lobbies.
pub(crate) const LOBBIES: &str = "lobbies";
/// The members.
pub(crate) const MEMBERS: &str = "lobby_members";
/// The metadata.
pub(crate) const METADATA: &str = "lobby_metadata";

/// The longest stored metadata value, in characters (the protocol's `MAX_META_VALUE_CHARS`).
const VALUE_CHARS: u32 = net_backend_protocol::lobbies::MAX_META_VALUE_CHARS as u32;
/// The longest metadata key, in bytes (the protocol's `MAX_META_KEY_BYTES`).
const KEY_BYTES: u32 = net_backend_protocol::lobbies::MAX_META_KEY_BYTES as u32;

fn nullable(mut column: ColumnDef) -> ColumnDef {
    column.null();
    column
}

fn big(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.big_integer();
    column
}

fn fk(table: &'static str, column: &'static str, to: &'static str, action: ForeignKeyAction) -> sea_query::ForeignKeyCreateStatement {
    let mut fk = ForeignKey::create();
    fk.name(format!("{table}_{column}_fk")).from(table, column).to(to, "id").on_delete(action);
    fk
}

fn lobbies() -> TableCreateStatement {
    let mut t = schema::create_table(LOBBIES);
    t.col(schema::id("id"))
        .col(schema::string("code", 8))
        .col(schema::string("visibility", 8))
        .col(schema::string("state", 8))
        .col(nullable(big("host_id")))
        .col(schema::big_int("max_players"))
        .col(nullable(big("chat_room")))
        .col(schema::unix_millis("created_at"))
        .col(schema::unix_millis("updated_at"))
        .foreign_key(&mut fk(LOBBIES, "host_id", "auth_users", ForeignKeyAction::SetNull));
    t
}

fn members() -> TableCreateStatement {
    let mut t = schema::create_table(MEMBERS);
    t.col(schema::id("id"))
        .col(schema::foreign_id("lobby_id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::big_int("ready"))
        .col(schema::unix_millis("joined_at"))
        .foreign_key(&mut fk(MEMBERS, "lobby_id", LOBBIES, ForeignKeyAction::Cascade))
        .foreign_key(&mut fk(MEMBERS, "user_id", "auth_users", ForeignKeyAction::Cascade));
    t
}

fn metadata() -> TableCreateStatement {
    let mut t = schema::create_table(METADATA);
    t.col(schema::id("id"))
        .col(schema::foreign_id("lobby_id"))
        .col(schema::string("meta_key", KEY_BYTES))
        .col(schema::string("meta_value", VALUE_CHARS))
        .foreign_key(&mut fk(METADATA, "lobby_id", LOBBIES, ForeignKeyAction::Cascade));
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

/// The migrations of the lobbies module for a dialect (versions `2026101000nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let table = |t: TableCreateStatement| render_table(&t, dialect);
    let steps: Vec<(&str, String)> = vec![
        ("create_lobbies", table(lobbies())),
        ("unique_lobbies_code", index("lobbies_code_uq", LOBBIES, &["code"], true, dialect)),
        ("index_lobbies_search", index("lobbies_search_ix", LOBBIES, &["state", "visibility", "id"], false, dialect)),
        ("index_lobbies_host", index("lobbies_host_ix", LOBBIES, &["host_id"], false, dialect)),
        ("create_lobby_members", table(members())),
        ("unique_lobby_members", index("lobby_members_uq", MEMBERS, &["lobby_id", "user_id"], true, dialect)),
        ("index_lobby_members_user", index("lobby_members_user_ix", MEMBERS, &["user_id"], false, dialect)),
        ("create_lobby_metadata", table(metadata())),
        ("unique_lobby_metadata", index("lobby_metadata_uq", METADATA, &["lobby_id", "meta_key"], true, dialect)),
        ("index_lobby_metadata_filter", index("lobby_metadata_filter_ix", METADATA, &["meta_key", "meta_value", "lobby_id"], false, dialect)),
    ];
    steps.into_iter().enumerate().map(|(n, (name, sql))| Migration::new(2026_1010_0000 + n as i64 + 1, name, sql)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_statement_each() {
        for dialect in Dialect::ALL {
            let list = all(*dialect);
            assert_eq!(list.len(), 10);
            assert!(list.windows(2).all(|w| w[0].version < w[1].version));
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}", migration.sql);
            }
        }
        let mysql = all(Dialect::MySql);
        assert!(mysql[0].sql.contains("`code` varchar(8)") && mysql[0].sql.contains("ON DELETE SET NULL"), "{}", mysql[0].sql);
        assert!(mysql[7].sql.contains("`meta_value` varchar(256)"), "{}", mysql[7].sql);
        assert!(all(Dialect::Postgres)[4].sql.contains("ON DELETE CASCADE"), "{}", all(Dialect::Postgres)[4].sql);
    }
}
