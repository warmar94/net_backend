//! The chat module's tables, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `chat_rooms` | public rooms (`room_key`), direct-message rooms (`dm_a` < `dm_b`, both accounts), group rooms; display name, member cap, last activity |
//! | `chat_members` | the members of group rooms (public rooms are open; DM rooms have their two users) |
//! | `chat_messages` | messages: room, sender (+ display name at sending time), body, nonce, time; deletion (moderation) clears the body; `edited_at` / `edited_by` after an edit |
//! | `chat_reads` | read markers: "user read room R up to message M" (one row per user and room) |
//!
//! Added by the chat extras (steps 13+, existing rows keep working): `chat_rooms.is_public` (player rooms: 1 =
//! public), `chat_members.role` (`owner` / `moderator` / `member` / `invited` / `banned`; group rooms'
//! rows are `member`), the edit columns, `chat_reads`. Then `chat_rooms.origin` (steps 20+): the module that
//! created a group room (`lobbies`, `groups`; NULL for the rooms of server code), so a module's upkeep
//! finds the rooms its lobbies or groups no longer name.

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, Table, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

pub(crate) const ROOMS: &str = "chat_rooms";
pub(crate) const MEMBERS: &str = "chat_members";
pub(crate) const MESSAGES: &str = "chat_messages";
pub(crate) const READS: &str = "chat_reads";

/// The longest stored message body, in characters (the `max_text_chars` setting is at most this).
pub(crate) const BODY_CHARS: u32 = 4000;

fn nullable_string(name: &'static str, len: u32) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.string_len(len).null();
    column
}

fn nullable_big(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.big_integer().null();
    column
}

fn fk(table: &'static str, column: &'static str, to: &'static str) -> sea_query::ForeignKeyCreateStatement {
    let mut fk = ForeignKey::create();
    fk.name(format!("{table}_{column}_fk")).from(table, column).to(to, "id").on_delete(ForeignKeyAction::Cascade);
    fk
}

fn rooms() -> TableCreateStatement {
    let mut t = schema::create_table(ROOMS);
    t.col(schema::id("id"))
        .col(schema::string("kind", 8))
        .col(nullable_string("room_key", 64))
        .col(nullable_string("name", 64))
        .col(nullable_big("max_members"))
        .col(nullable_big("dm_a"))
        .col(nullable_big("dm_b"))
        .col(schema::unix_millis("created_at"))
        .col(schema::unix_millis("last_activity_at"))
        .foreign_key(&mut fk(ROOMS, "dm_a", "auth_users"))
        .foreign_key(&mut fk(ROOMS, "dm_b", "auth_users"));
    t
}

fn members() -> TableCreateStatement {
    let mut t = schema::create_table(MEMBERS);
    t.col(schema::id("id"))
        .col(schema::foreign_id("room_id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::unix_millis("created_at"))
        .foreign_key(&mut fk(MEMBERS, "room_id", ROOMS))
        .foreign_key(&mut fk(MEMBERS, "user_id", "auth_users"));
    t
}

fn messages() -> TableCreateStatement {
    let mut t = schema::create_table(MESSAGES);
    t.col(schema::id("id"))
        .col(schema::foreign_id("room_id"))
        .col(schema::foreign_id("sender_id"))
        .col(nullable_string("sender_name", 64))
        .col(schema::string("body", BODY_CHARS))
        .col(nullable_string("nonce", 64))
        .col(schema::unix_millis("created_at"))
        .col(nullable_big("deleted_at"))
        .col(nullable_big("deleted_by"))
        .foreign_key(&mut fk(MESSAGES, "room_id", ROOMS))
        .foreign_key(&mut fk(MESSAGES, "sender_id", "auth_users"));
    t
}

fn reads() -> TableCreateStatement {
    let mut t = schema::create_table(READS);
    t.col(schema::id("id"))
        .col(schema::foreign_id("room_id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::big_int("message_id"))
        .col(schema::unix_millis("read_at"))
        .foreign_key(&mut fk(READS, "room_id", ROOMS))
        .foreign_key(&mut fk(READS, "user_id", "auth_users"));
    t
}

/// A column added to an existing table (one `ALTER TABLE`, no foreign key: SQLite allows none
/// with a non-NULL default).
fn add_column(table: &'static str, column: ColumnDef, dialect: Dialect) -> String {
    let mut alter = Table::alter();
    alter.table(table).add_column(column);
    match dialect {
        Dialect::MySql => alter.to_string(sea_query::MysqlQueryBuilder),
        Dialect::Postgres => alter.to_string(sea_query::PostgresQueryBuilder),
        Dialect::Sqlite => alter.to_string(sea_query::SqliteQueryBuilder),
    }
}

fn defaulted_big(name: &'static str, value: i64) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.big_integer().not_null().default(value);
    column
}

fn defaulted_string(name: &'static str, len: u32, value: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.string_len(len).not_null().default(value);
    column
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

/// The migrations of the chat module for a dialect (versions `2026100300nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let table = |t: TableCreateStatement| render_table(&t, dialect);
    let steps: Vec<(&str, String)> = vec![
        ("create_chat_rooms", table(rooms())),
        ("unique_chat_rooms_key", index("chat_rooms_key_uq", ROOMS, &["room_key"], true, dialect)),
        ("unique_chat_rooms_dm", index("chat_rooms_dm_uq", ROOMS, &["dm_a", "dm_b"], true, dialect)),
        ("index_chat_rooms_dm_b", index("chat_rooms_dm_b_ix", ROOMS, &["dm_b"], false, dialect)),
        ("index_chat_rooms_kind", index("chat_rooms_kind_ix", ROOMS, &["kind", "id"], false, dialect)),
        ("create_chat_members", table(members())),
        ("unique_chat_members", index("chat_members_uq", MEMBERS, &["room_id", "user_id"], true, dialect)),
        ("index_chat_members_user", index("chat_members_user_ix", MEMBERS, &["user_id"], false, dialect)),
        ("create_chat_messages", table(messages())),
        ("index_chat_messages_room", index("chat_messages_room_ix", MESSAGES, &["room_id", "id"], false, dialect)),
        ("index_chat_messages_created", index("chat_messages_created_ix", MESSAGES, &["created_at"], false, dialect)),
        ("index_chat_messages_sender", index("chat_messages_sender_ix", MESSAGES, &["sender_id"], false, dialect)),
        // Chat extras: player rooms, roles, editing, read markers.
        ("add_chat_rooms_is_public", add_column(ROOMS, defaulted_big("is_public", 0), dialect)),
        ("add_chat_members_role", add_column(MEMBERS, defaulted_string("role", 16, "member"), dialect)),
        ("add_chat_messages_edited_at", add_column(MESSAGES, nullable_big("edited_at"), dialect)),
        ("add_chat_messages_edited_by", add_column(MESSAGES, nullable_big("edited_by"), dialect)),
        ("create_chat_reads", table(reads())),
        ("unique_chat_reads", index("chat_reads_uq", READS, &["room_id", "user_id"], true, dialect)),
        ("index_chat_reads_user", index("chat_reads_user_ix", READS, &["user_id"], false, dialect)),
        // The group rooms of the lobbies and groups modules.
        ("add_chat_rooms_origin", add_column(ROOMS, nullable_string("origin", 16), dialect)),
        ("index_chat_rooms_origin", index("chat_rooms_origin_ix", ROOMS, &["origin", "id"], false, dialect)),
    ];
    steps.into_iter().enumerate().map(|(n, (name, sql))| Migration::new(2026_1003_0000 + n as i64 + 1, name, sql)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_statement_each() {
        for dialect in Dialect::ALL {
            let list = all(*dialect);
            assert_eq!(list.len(), 21);
            assert!(list.windows(2).all(|w| w[0].version < w[1].version));
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}", migration.sql);
            }
        }
        let mysql = all(Dialect::MySql);
        assert!(mysql[8].sql.contains("`body` varchar(4000) NOT NULL") && mysql[8].sql.contains("utf8mb4_bin"), "{}", mysql[8].sql);
        assert!(all(Dialect::Sqlite)[0].sql.contains("\"dm_a\" integer NULL"), "{}", all(Dialect::Sqlite)[0].sql);
        for dialect in Dialect::ALL {
            let list = all(*dialect);
            assert_eq!(list[12].version, 2026_1003_0013);
            assert!(list[12].sql.starts_with("ALTER TABLE") && list[12].sql.contains("is_public") && list[12].sql.contains("DEFAULT 0"), "{}", list[12].sql);
            assert!(list[13].sql.contains("role") && list[13].sql.contains("'member'"), "{}", list[13].sql);
            assert!(list[14].sql.contains("edited_at") && !list[14].sql.contains("NOT NULL"), "{}", list[14].sql);
            assert_eq!((list[19].version, list[20].version), (2026_1003_0020, 2026_1003_0021));
            assert!(list[19].sql.starts_with("ALTER TABLE") && list[19].sql.contains("origin") && !list[19].sql.contains("NOT NULL"), "{}", list[19].sql);
            assert!(list[20].sql.contains("chat_rooms_origin_ix"), "{}", list[20].sql);
        }
    }
}
