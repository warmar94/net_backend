//! The auth module's tables, one statement per migration (MySQL commits DDL one statement at a
//! time; a failure then leaves nothing half done). Portable types only: `BIGINT` ids and unix-ms
//! timestamps, `VARCHAR` text; nullable timestamps instead of booleans.
//!
//! | Table | What |
//! |---|---|
//! | `auth_users` | account: email (as entered + normalised, unique), verification, display name, ban |
//! | `auth_credentials` | the argon2id PHC string per user (absent for Steam-only accounts) |
//! | `auth_identities` | linked login providers (`steam`, SteamID64), unique per provider |
//! | `auth_sessions` | one per login = one refresh-token family; revocation |
//! | `auth_access_tokens` / `auth_refresh_tokens` | SHA-256 of each token, expiry, use (+ the rotation nonce during the grace window) |
//! | `auth_email_tokens` | one-time verification / reset tokens (SHA-256), expiry, use |
//! | `auth_user_roles` | roles per user |
//! | `auth_audit_log` | who did what, when, from where (no secrets) |
//! | `auth_secrets` | the server's refresh-derivation key (generated once) |

use sea_query::{ColumnDef, ForeignKey, ForeignKeyAction, Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

fn nullable_string(name: &'static str, len: u32) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.string_len(len).null();
    column
}

fn nullable_millis(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.big_integer().null();
    column
}

fn user_fk(table: &'static str, column: &'static str) -> sea_query::ForeignKeyCreateStatement {
    let mut fk = ForeignKey::create();
    fk.name(format!("{table}_{column}_fk")).from(table, column).to("auth_users", "id").on_delete(ForeignKeyAction::Cascade);
    fk
}

fn session_fk(table: &'static str) -> sea_query::ForeignKeyCreateStatement {
    let mut fk = ForeignKey::create();
    fk.name(format!("{table}_session_fk")).from(table, "session_id").to("auth_sessions", "id").on_delete(ForeignKeyAction::Cascade);
    fk
}

fn users() -> TableCreateStatement {
    let mut t = schema::create_table("auth_users");
    t.col(schema::id("id"))
        .col(nullable_string("email", 254))
        .col(nullable_string("email_normalized", 254))
        .col(nullable_millis("email_verified_at"))
        .col(nullable_string("display_name", 64))
        .col(nullable_millis("banned_at"))
        .col(nullable_millis("banned_until"))
        .col(nullable_string("ban_reason", 255))
        .col(schema::unix_millis("created_at"))
        .col(schema::unix_millis("updated_at"));
    t
}

fn credentials() -> TableCreateStatement {
    let mut t = schema::create_table("auth_credentials");
    let mut user_id = ColumnDef::new("user_id");
    user_id.big_integer().not_null().primary_key();
    t.col(user_id).col(schema::string("password_hash", 255)).col(schema::unix_millis("updated_at")).foreign_key(&mut user_fk("auth_credentials", "user_id"));
    t
}

fn identities() -> TableCreateStatement {
    let mut t = schema::create_table("auth_identities");
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::string("provider", 32))
        .col(schema::string("subject", 255))
        .col(schema::unix_millis("created_at"))
        .foreign_key(&mut user_fk("auth_identities", "user_id"));
    t
}

fn sessions() -> TableCreateStatement {
    let mut t = schema::create_table("auth_sessions");
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::string("method", 16))
        .col(nullable_string("ip", 45))
        .col(nullable_string("user_agent", 255))
        .col(schema::unix_millis("created_at"))
        .col(schema::unix_millis("last_used_at"))
        .col(schema::unix_millis("expires_at"))
        .col(nullable_millis("revoked_at"))
        .col(nullable_string("revoke_reason", 32))
        .foreign_key(&mut user_fk("auth_sessions", "user_id"));
    t
}

fn token_table(name: &'static str, with_used: bool) -> TableCreateStatement {
    let mut t = schema::create_table(name);
    t.col(schema::id("id"))
        .col(schema::foreign_id("session_id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::string("token_hash", 64))
        .col(schema::unix_millis("expires_at"))
        .col(schema::unix_millis("created_at"));
    if with_used {
        // The random nonce of the rotation that used this token (the grace answer derives the
        // replacement pair from it); cleared once the grace window has passed.
        t.col(nullable_millis("used_at")).col(nullable_string("rotation_nonce", 64));
    }
    t.foreign_key(&mut session_fk(name));
    t
}

fn email_tokens() -> TableCreateStatement {
    let mut t = schema::create_table("auth_email_tokens");
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::string("purpose", 16))
        .col(schema::string("email_normalized", 254))
        .col(schema::string("token_hash", 64))
        .col(schema::unix_millis("expires_at"))
        .col(schema::unix_millis("created_at"))
        .col(nullable_millis("used_at"))
        .foreign_key(&mut user_fk("auth_email_tokens", "user_id"));
    t
}

fn user_roles() -> TableCreateStatement {
    let mut t = schema::create_table("auth_user_roles");
    t.col(schema::id("id"))
        .col(schema::foreign_id("user_id"))
        .col(schema::string("role", 64))
        .col(schema::unix_millis("created_at"))
        .foreign_key(&mut user_fk("auth_user_roles", "user_id"));
    t
}

fn audit_log() -> TableCreateStatement {
    let mut t = schema::create_table("auth_audit_log");
    let mut actor = ColumnDef::new("actor_user_id");
    actor.big_integer().null();
    t.col(schema::id("id"))
        .col(actor)
        .col(schema::string("action", 64))
        .col(nullable_string("target_type", 32))
        .col(nullable_string("target_id", 64))
        .col(nullable_string("ip", 45))
        .col(nullable_string("request_id", 64))
        .col(nullable_string("data", 2048))
        .col(schema::unix_millis("created_at"));
    t
}

fn secrets() -> TableCreateStatement {
    let mut t = schema::create_table("auth_secrets");
    let mut name = ColumnDef::new("name");
    name.string_len(32).not_null().primary_key();
    t.col(name).col(schema::string("value", 128)).col(schema::unix_millis("created_at"));
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

/// The migrations of the auth module for a dialect (versions `2026100100nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let table = |t: TableCreateStatement| render_table(&t, dialect);
    let steps: Vec<(&str, String)> = vec![
        ("create_auth_users", table(users())),
        ("unique_auth_users_email", index("auth_users_email_uq", "auth_users", &["email_normalized"], true, dialect)),
        ("create_auth_credentials", table(credentials())),
        ("create_auth_identities", table(identities())),
        ("unique_auth_identities_subject", index("auth_identities_subject_uq", "auth_identities", &["provider", "subject"], true, dialect)),
        ("index_auth_identities_user", index("auth_identities_user_ix", "auth_identities", &["user_id"], false, dialect)),
        ("create_auth_sessions", table(sessions())),
        ("index_auth_sessions_user", index("auth_sessions_user_ix", "auth_sessions", &["user_id"], false, dialect)),
        ("create_auth_access_tokens", table(token_table("auth_access_tokens", false))),
        ("unique_auth_access_tokens_hash", index("auth_access_tokens_hash_uq", "auth_access_tokens", &["token_hash"], true, dialect)),
        ("index_auth_access_tokens_expiry", index("auth_access_tokens_expires_ix", "auth_access_tokens", &["expires_at"], false, dialect)),
        ("create_auth_refresh_tokens", table(token_table("auth_refresh_tokens", true))),
        ("unique_auth_refresh_tokens_hash", index("auth_refresh_tokens_hash_uq", "auth_refresh_tokens", &["token_hash"], true, dialect)),
        ("index_auth_refresh_tokens_expiry", index("auth_refresh_tokens_expires_ix", "auth_refresh_tokens", &["expires_at"], false, dialect)),
        ("create_auth_email_tokens", table(email_tokens())),
        ("unique_auth_email_tokens_hash", index("auth_email_tokens_hash_uq", "auth_email_tokens", &["token_hash"], true, dialect)),
        ("index_auth_email_tokens_user", index("auth_email_tokens_user_ix", "auth_email_tokens", &["user_id", "purpose"], false, dialect)),
        ("create_auth_user_roles", table(user_roles())),
        ("unique_auth_user_roles", index("auth_user_roles_uq", "auth_user_roles", &["user_id", "role"], true, dialect)),
        ("create_auth_audit_log", table(audit_log())),
        ("index_auth_audit_actor", index("auth_audit_log_actor_ix", "auth_audit_log", &["actor_user_id"], false, dialect)),
        ("index_auth_audit_target", index("auth_audit_log_target_ix", "auth_audit_log", &["target_type", "target_id"], false, dialect)),
        ("index_auth_audit_action", index("auth_audit_log_action_ix", "auth_audit_log", &["action"], false, dialect)),
        ("create_auth_secrets", table(secrets())),
        // PostgreSQL and SQLite do not index foreign keys: the session cascade (purge) needs these.
        ("index_auth_access_tokens_session", index("auth_access_tokens_session_ix", "auth_access_tokens", &["session_id"], false, dialect)),
        ("index_auth_refresh_tokens_session", index("auth_refresh_tokens_session_ix", "auth_refresh_tokens", &["session_id"], false, dialect)),
        ("index_auth_refresh_tokens_used", index("auth_refresh_tokens_used_ix", "auth_refresh_tokens", &["used_at"], false, dialect)),
        ("index_auth_sessions_revoked", index("auth_sessions_revoked_ix", "auth_sessions", &["revoked_at"], false, dialect)),
        ("index_auth_sessions_expiry", index("auth_sessions_expires_ix", "auth_sessions", &["expires_at"], false, dialect)),
    ];
    steps.into_iter().enumerate().map(|(n, (name, sql))| Migration::new(2026_1001_0000 + n as i64 + 1, name, sql)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_statement_each_and_ordered() {
        for dialect in Dialect::ALL {
            let list = all(*dialect);
            assert_eq!(list.len(), 29);
            assert!(list.windows(2).all(|w| w[0].version < w[1].version));
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}: {}", migration.name, migration.sql);
            }
        }
        let mysql = all(Dialect::MySql);
        assert!(mysql[0].sql.contains("`email_normalized` varchar(254) NULL") && mysql[0].sql.contains("utf8mb4_bin"), "{}", mysql[0].sql);
        assert!(mysql[1].sql.starts_with("CREATE UNIQUE INDEX `auth_users_email_uq`"), "{}", mysql[1].sql);
        let pg = all(Dialect::Postgres);
        assert!(pg[2].sql.contains("ON DELETE CASCADE"), "{}", pg[2].sql);
        let sqlite = all(Dialect::Sqlite);
        assert!(sqlite[23].sql.contains("\"name\" varchar(32) NOT NULL PRIMARY KEY"), "{}", sqlite[23].sql);
        assert!(sqlite[11].sql.contains("\"rotation_nonce\" varchar(64) NULL"), "{}", sqlite[11].sql);
    }
}
