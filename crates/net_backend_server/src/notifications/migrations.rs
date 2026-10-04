//! The notifications module's table, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `notifications` | one row per notification: its player (`auth_users`, cascade), kind, text, the data's JSON bytes, the sender (`auth_users`, set to NULL when that account is deleted), when it was read (NULL: unread), when it was created |
//!
//! Indexes: `(user_id, id)` (lists, newest first; the trim to `max_per_user`), `(user_id, read_at)`
//! (unread counts and lists), `(created_at)` (the retention purge), `(sender_id)` (the foreign key).

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

/// The table.
pub(crate) const NOTIFICATIONS: &str = "notifications";

/// The longest stored text, in characters (the protocol's `MAX_TEXT_CHARS`).
const TEXT_CHARS: u32 = net_backend_protocol::notifications::MAX_TEXT_CHARS as u32;

fn nullable(mut column: ColumnDef) -> ColumnDef {
    column.null();
    column
}

fn table(dialect: Dialect) -> TableCreateStatement {
    let mut t = schema::create_table(NOTIFICATIONS);
    let mut user = ForeignKey::create();
    user.name("notifications_user_id_fk").from(NOTIFICATIONS, "user_id").to("auth_users", "id").on_delete(ForeignKeyAction::Cascade);
    let mut sender = ForeignKey::create();
    sender.name("notifications_sender_id_fk").from(NOTIFICATIONS, "sender_id").to("auth_users", "id").on_delete(ForeignKeyAction::SetNull);
    let mut text = ColumnDef::new("text");
    text.string_len(TEXT_CHARS);
    let mut data = ColumnDef::new("data");
    match dialect {
        Dialect::MySql => data.custom("LONGBLOB"),
        Dialect::Postgres => data.custom("BYTEA"),
        Dialect::Sqlite => data.custom("BLOB"),
    };
    let mut sender_id = ColumnDef::new("sender_id");
    sender_id.big_integer();
    let mut read_at = ColumnDef::new("read_at");
    read_at.big_integer();
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::string("kind", 64))
        .col(nullable(text))
        .col(nullable(data))
        .col(nullable(sender_id))
        .col(nullable(read_at))
        .col(schema::unix_millis("created_at"))
        .foreign_key(&mut user)
        .foreign_key(&mut sender);
    t
}

fn index(name: &'static str, columns: &[&'static str], dialect: Dialect) -> String {
    let mut index = Index::create();
    index.name(name).table(NOTIFICATIONS);
    for column in columns {
        index.col(*column);
    }
    render_index(&index, dialect)
}

/// The migrations of the notifications module for a dialect (versions `2026100500nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let steps: Vec<(&str, String)> = vec![
        ("create_notifications", render_table(&table(dialect), dialect)),
        ("index_notifications_user", index("notifications_user_ix", &["user_id", "id"], dialect)),
        ("index_notifications_unread", index("notifications_unread_ix", &["user_id", "read_at"], dialect)),
        ("index_notifications_created", index("notifications_created_ix", &["created_at"], dialect)),
        ("index_notifications_sender", index("notifications_sender_ix", &["sender_id"], dialect)),
    ];
    steps.into_iter().enumerate().map(|(n, (name, sql))| Migration::new(2026_1005_0000 + n as i64 + 1, name, sql)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_statement_each() {
        for dialect in Dialect::ALL {
            let list = all(*dialect);
            assert_eq!(list.len(), 5);
            assert!(list.windows(2).all(|w| w[0].version < w[1].version));
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}", migration.sql);
            }
        }
        let mysql = all(Dialect::MySql);
        assert!(mysql[0].sql.contains("`data` LONGBLOB NULL") && mysql[0].sql.contains("`text` varchar(1000) NULL"), "{}", mysql[0].sql);
        assert!(mysql[0].sql.contains("ON DELETE SET NULL") && mysql[0].sql.contains("ON DELETE CASCADE"), "{}", mysql[0].sql);
        assert!(all(Dialect::Postgres)[0].sql.contains("\"read_at\" bigint NULL"), "{}", all(Dialect::Postgres)[0].sql);
    }
}
