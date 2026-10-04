//! The OpenID Connect module's table, one statement per migration (portable types only).
//!
//! | Table | What |
//! |---|---|
//! | `oauth_used_tokens` | the nonces (or, for a token without a nonce, the tokens) already used for a login, per provider, until the token expires: a second login with the same one is refused on every instance |
//!
//! Indexes: `(provider, token_key)` unique (the replay check is the insert), `(expires_at)` (the purge).

use sea_query::{Index, TableCreateStatement};

use crate::db::schema::{self, render_index, render_table};
use crate::db::Dialect;
use crate::migrate::Migration;

/// The used nonces / tokens.
pub(crate) const USED: &str = "oauth_used_tokens";

fn used() -> TableCreateStatement {
    let mut t = schema::create_table(USED);
    t.col(schema::id("id")).col(schema::string("provider", 32)).col(schema::string("token_key", 64)).col(schema::unix_millis("expires_at"));
    t
}

fn index(name: &'static str, columns: &[&'static str], unique: bool, dialect: Dialect) -> String {
    let mut index = Index::create();
    index.name(name).table(USED);
    for column in columns {
        index.col(*column);
    }
    if unique {
        index.unique();
    }
    render_index(&index, dialect)
}

/// The migrations of the OpenID Connect module for a dialect (versions `2026101000nn`).
pub(crate) fn all(dialect: Dialect) -> Vec<Migration> {
    let steps: Vec<(&str, String)> = vec![
        ("create_oauth_used_tokens", render_table(&used(), dialect)),
        ("unique_oauth_used_tokens", index("oauth_used_tokens_uq", &["provider", "token_key"], true, dialect)),
        ("index_oauth_used_tokens_expires", index("oauth_used_tokens_expires_ix", &["expires_at"], false, dialect)),
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
            assert_eq!(list.len(), 3);
            assert!(list.windows(2).all(|w| w[0].version < w[1].version));
            for migration in &list {
                assert_eq!(crate::migrate::split_statements(&migration.sql).len(), 1, "{}", migration.sql);
            }
        }
        assert!(all(Dialect::MySql)[0].sql.contains("`token_key` varchar(64) NOT NULL"), "{}", all(Dialect::MySql)[0].sql);
        assert!(all(Dialect::Postgres)[1].sql.contains("UNIQUE"), "{}", all(Dialect::Postgres)[1].sql);
    }
}
