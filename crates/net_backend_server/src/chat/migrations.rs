//! The chat module's tables, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `chat_rooms` | public rooms (`room_key`), direct-message rooms (`dm_a` < `dm_b`, both accounts), group rooms; display name, member cap, last activity |
//! | `chat_members` | the members of group rooms (public rooms are open; DM rooms have their two users) |
//! | `chat_messages` | messages: room, sender (+ display name at sending time), body, nonce, time; deletion (moderation) clears the body |

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

pub(crate) const ROOMS: &str = "chat_rooms";
pub(crate) const MEMBERS: &str = "chat_members";
pub(crate) const MESSAGES: &str = "chat_messages";

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
            assert_eq!(list.len(), 12);
            assert!(list.windows(2).all(|w| w[0].version < w[1].version));
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}", migration.sql);
            }
        }
        let mysql = all(Dialect::MySql);
        assert!(mysql[8].sql.contains("`body` varchar(4000) NOT NULL") && mysql[8].sql.contains("utf8mb4_bin"), "{}", mysql[8].sql);
        assert!(all(Dialect::Sqlite)[0].sql.contains("\"dm_a\" integer NULL"), "{}", all(Dialect::Sqlite)[0].sql);
    }
}
