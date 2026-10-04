//! The friends module's tables, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `friend_links` | one row per player and other player: `friend`, `sent` (a request the player sent), `received` (one it received) or `blocked`; both accounts `auth_users`, cascade; since when |
//! | `friend_profiles` | one row per player that used the module: its friend code (unique), until when it counts as online, when it was last seen |
//! | `friends_settings` | one row per player that changed a setting: since when it is hidden from Steam ID lookups (`NULL`: findable), when it changed |
//! | `friend_presence` | one row per player and server instance with a WebSocket connection of that player: the instance, until when the row counts (moved forward while connected) |
//!
//! A friendship or a request is two rows (one per side); a block is the blocker's row only.
//! Indexes: `(user_id, other_id)` unique, `(user_id, state, id)` (the lists and counts),
//! `(other_id)` (the foreign key); `friend_profiles (user_id)` and `(code)` unique; and
//! `auth_users (display_name)` (adding a friend by name); `friends_settings (user_id)` unique;
//! `friend_presence (user_id, instance_id)` unique and `(online_until)` (the purge). No settings row =
//! the defaults (findable).

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

/// The relations.
pub(crate) const LINKS: &str = "friend_links";
/// The friend codes and online times.
pub(crate) const PROFILES: &str = "friend_profiles";
/// The players' settings (the Steam findable flag).
pub(crate) const SETTINGS: &str = "friends_settings";
/// Which server instances hold a WebSocket connection of a player.
pub(crate) const PRESENCE: &str = "friend_presence";

fn nullable_big(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.big_integer().null();
    column
}

fn fk(table: &'static str, column: &'static str) -> sea_query::ForeignKeyCreateStatement {
    let mut fk = ForeignKey::create();
    fk.name(format!("{table}_{column}_fk")).from(table, column).to("auth_users", "id").on_delete(ForeignKeyAction::Cascade);
    fk
}

fn links() -> TableCreateStatement {
    let mut t = schema::create_table(LINKS);
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::foreign_id("other_id"))
        .col(schema::string("state", 8))
        .col(schema::unix_millis("created_at"))
        .col(schema::unix_millis("updated_at"))
        .foreign_key(&mut fk(LINKS, "user_id"))
        .foreign_key(&mut fk(LINKS, "other_id"));
    t
}

fn profiles() -> TableCreateStatement {
    let mut t = schema::create_table(PROFILES);
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::string("code", 8))
        .col(nullable_big("online_until"))
        .col(nullable_big("last_seen_at"))
        .col(schema::unix_millis("created_at"))
        .foreign_key(&mut fk(PROFILES, "user_id"));
    t
}

fn settings() -> TableCreateStatement {
    let mut t = schema::create_table(SETTINGS);
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(nullable_big("steam_hidden_at"))
        .col(schema::unix_millis("updated_at"))
        .foreign_key(&mut fk(SETTINGS, "user_id"));
    t
}

fn presence() -> TableCreateStatement {
    let mut t = schema::create_table(PRESENCE);
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::big_int("instance_id"))
        .col(schema::unix_millis("online_until"))
        .foreign_key(&mut fk(PRESENCE, "user_id"));
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

/// The migrations of the friends module for a dialect (versions `2026100600nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let table = |t: TableCreateStatement| render_table(&t, dialect);
    let steps: Vec<(&str, String)> = vec![
        ("create_friend_links", table(links())),
        ("unique_friend_links", index("friend_links_uq", LINKS, &["user_id", "other_id"], true, dialect)),
        ("index_friend_links_state", index("friend_links_state_ix", LINKS, &["user_id", "state", "id"], false, dialect)),
        ("index_friend_links_other", index("friend_links_other_ix", LINKS, &["other_id"], false, dialect)),
        ("create_friend_profiles", table(profiles())),
        ("unique_friend_profiles_user", index("friend_profiles_user_uq", PROFILES, &["user_id"], true, dialect)),
        ("unique_friend_profiles_code", index("friend_profiles_code_uq", PROFILES, &["code"], true, dialect)),
        ("index_auth_users_display_name", index("friends_auth_users_name_ix", "auth_users", &["display_name"], false, dialect)),
        ("create_friends_settings", table(settings())),
        ("unique_friends_settings_user", index("friends_settings_user_uq", SETTINGS, &["user_id"], true, dialect)),
        ("create_friend_presence", table(presence())),
        ("unique_friend_presence", index("friend_presence_uq", PRESENCE, &["user_id", "instance_id"], true, dialect)),
        ("index_friend_presence_until", index("friend_presence_until_ix", PRESENCE, &["online_until"], false, dialect)),
    ];
    steps.into_iter().enumerate().map(|(n, (name, sql))| Migration::new(2026_1006_0000 + n as i64 + 1, name, sql)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_statement_each() {
        for dialect in Dialect::ALL {
            let list = all(*dialect);
            assert_eq!(list.len(), 13);
            assert_eq!(list[12].version, 2026_1006_0013);
            assert_eq!(list[8].version, 2026_1006_0009);
            assert!(list.windows(2).all(|w| w[0].version < w[1].version));
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}", migration.sql);
            }
        }
        let mysql = all(Dialect::MySql);
        assert!(mysql[0].sql.contains("`state` varchar(8) NOT NULL") && mysql[0].sql.contains("ON DELETE CASCADE"), "{}", mysql[0].sql);
        assert!(all(Dialect::Postgres)[4].sql.contains("\"online_until\" bigint NULL"), "{}", all(Dialect::Postgres)[4].sql);
        assert!(all(Dialect::Sqlite)[7].sql.contains("\"auth_users\""), "{}", all(Dialect::Sqlite)[7].sql);
        let presence = &all(Dialect::Postgres)[10].sql;
        assert!(presence.contains("\"instance_id\" bigint NOT NULL") && presence.contains("ON DELETE CASCADE"), "{presence}");
        let settings = &all(Dialect::MySql)[8].sql;
        assert!(settings.contains("`steam_hidden_at` bigint NULL") && settings.contains("ON DELETE CASCADE"), "{settings}");
    }
}
