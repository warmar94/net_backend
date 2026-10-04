//! The groups module's tables, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `game_groups` | one row per group: name, `name_key` (the name in lower case, unique), description, `is_open` (0 or 1 in a BIGINT: decodes the same on every database), metadata (JSON bytes), owner (`auth_users`, set to NULL when that account is deleted), the chat room id, created / updated |
//! | `game_group_members` | one row per member: group (cascade), account (cascade), role, joined |
//! | `game_group_invites` | one row per open invitation: group (cascade), invited account (cascade), inviter (set to NULL), created |
//!
//! (`groups` is a reserved word on MySQL 8: the tables are `game_groups*`.) Indexes: `name_key`
//! unique (the name search), `(owner_id)`; members `(group_id, user_id)` unique, `(user_id)`;
//! invitations `(group_id, user_id)` unique, `(user_id, id)`, `(inviter_id)`.

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

/// The groups.
pub(crate) const GROUPS: &str = "game_groups";
/// The members.
pub(crate) const MEMBERS: &str = "game_group_members";
/// The open invitations.
pub(crate) const INVITES: &str = "game_group_invites";

/// The longest stored description, in characters (the protocol's `MAX_DESCRIPTION_CHARS`).
const DESCRIPTION_CHARS: u32 = net_backend_protocol::groups::MAX_DESCRIPTION_CHARS as u32;

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

fn groups(dialect: Dialect) -> TableCreateStatement {
    let mut t = schema::create_table(GROUPS);
    let mut description = ColumnDef::new("description");
    description.string_len(DESCRIPTION_CHARS);
    let mut metadata = ColumnDef::new("metadata");
    match dialect {
        Dialect::MySql => metadata.custom("LONGBLOB"),
        Dialect::Postgres => metadata.custom("BYTEA"),
        Dialect::Sqlite => metadata.custom("BLOB"),
    };
    t.col(schema::id("id"))
        .col(schema::string("name", 64))
        .col(schema::string("name_key", 64))
        .col(nullable(description))
        .col(schema::big_int("is_open"))
        .col(nullable(metadata))
        .col(nullable(big("owner_id")))
        .col(nullable(big("chat_room")))
        .col(schema::unix_millis("created_at"))
        .col(schema::unix_millis("updated_at"))
        .foreign_key(&mut fk(GROUPS, "owner_id", "auth_users", ForeignKeyAction::SetNull));
    t
}

fn members() -> TableCreateStatement {
    let mut t = schema::create_table(MEMBERS);
    t.col(schema::id("id"))
        .col(schema::foreign_id("group_id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::string("role", 8))
        .col(schema::unix_millis("joined_at"))
        .foreign_key(&mut fk(MEMBERS, "group_id", GROUPS, ForeignKeyAction::Cascade))
        .foreign_key(&mut fk(MEMBERS, "user_id", "auth_users", ForeignKeyAction::Cascade));
    t
}

fn invites() -> TableCreateStatement {
    let mut t = schema::create_table(INVITES);
    t.col(schema::id("id"))
        .col(schema::foreign_id("group_id"))
        .col(schema::foreign_id("user_id"))
        .col(nullable(big("inviter_id")))
        .col(schema::unix_millis("created_at"))
        .foreign_key(&mut fk(INVITES, "group_id", GROUPS, ForeignKeyAction::Cascade))
        .foreign_key(&mut fk(INVITES, "user_id", "auth_users", ForeignKeyAction::Cascade))
        .foreign_key(&mut fk(INVITES, "inviter_id", "auth_users", ForeignKeyAction::SetNull));
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

/// The migrations of the groups module for a dialect (versions `2026100700nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let table = |t: TableCreateStatement| render_table(&t, dialect);
    let steps: Vec<(&str, String)> = vec![
        ("create_game_groups", table(groups(dialect))),
        ("unique_game_groups_name", index("game_groups_name_uq", GROUPS, &["name_key"], true, dialect)),
        ("index_game_groups_owner", index("game_groups_owner_ix", GROUPS, &["owner_id"], false, dialect)),
        ("create_game_group_members", table(members())),
        ("unique_game_group_members", index("game_group_members_uq", MEMBERS, &["group_id", "user_id"], true, dialect)),
        ("index_game_group_members_user", index("game_group_members_user_ix", MEMBERS, &["user_id"], false, dialect)),
        ("create_game_group_invites", table(invites())),
        ("unique_game_group_invites", index("game_group_invites_uq", INVITES, &["group_id", "user_id"], true, dialect)),
        ("index_game_group_invites_user", index("game_group_invites_user_ix", INVITES, &["user_id", "id"], false, dialect)),
        ("index_game_group_invites_inviter", index("game_group_invites_inviter_ix", INVITES, &["inviter_id"], false, dialect)),
    ];
    steps.into_iter().enumerate().map(|(n, (name, sql))| Migration::new(2026_1007_0000 + n as i64 + 1, name, sql)).collect()
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
        assert!(mysql[0].sql.contains("`metadata` LONGBLOB NULL") && mysql[0].sql.contains("ON DELETE SET NULL"), "{}", mysql[0].sql);
        assert!(mysql[0].sql.contains("`description` varchar(500) NULL"), "{}", mysql[0].sql);
        assert!(all(Dialect::Postgres)[3].sql.contains("ON DELETE CASCADE"), "{}", all(Dialect::Postgres)[3].sql);
    }
}
