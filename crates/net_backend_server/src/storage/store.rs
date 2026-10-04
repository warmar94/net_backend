//! The storage module's SQL, as sea-query statements (one statement, three dialects). Every
//! statement runs in the storage suite on SQLite locally and on MySQL / PostgreSQL (env-gated).

use sea_query::{Cond, DeleteStatement, Expr, ExprTrait, Func, InsertStatement, LockType, Order, Query, SelectStatement, UpdateStatement};

use super::migrations::OBJECTS;
use crate::db::{DbError, Dialect};

/// `write_access` of an owner-writable object.
pub(crate) const WRITE_OWNER: &str = "owner";
/// `write_access` of a server-locked object.
pub(crate) const WRITE_SERVER: &str = "server";

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

/// One object with its value.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct ObjectRow {
    pub(crate) collection: String,
    pub(crate) object_key: String,
    pub(crate) value: Vec<u8>,
    pub(crate) version: i64,
    pub(crate) write_access: String,
    pub(crate) visibility: String,
    pub(crate) updated_at: i64,
}

/// One object without its value.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct InfoRow {
    pub(crate) object_key: String,
    pub(crate) version: i64,
    pub(crate) write_access: String,
    pub(crate) visibility: String,
    pub(crate) size_bytes: i64,
    pub(crate) updated_at: i64,
}

/// An object's version, lock and size (read under the account lock before a write or delete).
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct StateRow {
    pub(crate) version: i64,
    pub(crate) write_access: String,
    pub(crate) size_bytes: i64,
}

/// What a user's objects use: how many, and the bytes of their values.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct UsageRow {
    pub(crate) n: i64,
    pub(crate) bytes: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    #[allow(dead_code)]
    pub(crate) id: i64,
}

fn identity(user: i64, collection: &str, key: &str) -> Cond {
    Cond::all().add(Expr::col("user_id").eq(user)).add(Expr::col("collection").eq(collection)).add(Expr::col("object_key").eq(key))
}

/// One object with its value.
pub(crate) fn object(user: i64, collection: &str, key: &str) -> SelectStatement {
    let mut select = Query::select();
    select
        .columns(["collection", "object_key", "value", "version", "write_access", "visibility", "updated_at"])
        .from(OBJECTS)
        .cond_where(identity(user, collection, key));
    select
}

/// Several objects of one user (at most a batch: the condition is a short OR list).
pub(crate) fn objects(user: i64, names: &[(&str, &str)]) -> SelectStatement {
    let mut any = Cond::any();
    for (collection, key) in names {
        any = any.add(Cond::all().add(Expr::col("collection").eq(*collection)).add(Expr::col("object_key").eq(*key)));
    }
    let mut select = Query::select();
    select
        .columns(["collection", "object_key", "value", "version", "write_access", "visibility", "updated_at"])
        .from(OBJECTS)
        .cond_where(Cond::all().add(Expr::col("user_id").eq(user)).add(any));
    select
}

/// An object's version, lock and size (a plain read: no gap locks on MySQL; exact because every
/// writer of the user's objects holds the account lock).
pub(crate) fn state(user: i64, collection: &str, key: &str) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["version", "write_access", "size_bytes"]).from(OBJECTS).cond_where(identity(user, collection, key));
    select
}

/// A page of one collection, ordered by key, after `after` (the previous page's last key); with
/// `visible`, only objects of those visibilities (another player's view).
pub(crate) fn list(user: i64, collection: &str, after: Option<&str>, limit: u64, visible: Option<&[&str]>) -> SelectStatement {
    let mut select = Query::select();
    select
        .columns(["object_key", "version", "write_access", "visibility", "size_bytes", "updated_at"])
        .from(OBJECTS)
        .and_where(Expr::col("user_id").eq(user))
        .and_where(Expr::col("collection").eq(collection));
    if let Some(visible) = visible {
        select.and_where(Expr::col("visibility").is_in(visible.iter().copied()));
    }
    if let Some(after) = after {
        select.and_where(Expr::col("object_key").gt(after));
    }
    select.order_by("object_key", Order::Asc).limit(limit);
    select
}

/// How many objects a user owns and the bytes of their values (`SUM` is a DECIMAL / NUMERIC on
/// MySQL / PostgreSQL: cast back to a BIGINT).
pub(crate) fn usage(user: i64, dialect: Dialect) -> SelectStatement {
    let sum = Func::coalesce([Expr::from(Func::sum(Expr::col("size_bytes"))), Expr::val(0i64)]);
    let bytes: Expr = match dialect {
        Dialect::MySql => Func::cast_as(sum, "SIGNED").into(),
        Dialect::Postgres => Func::cast_as(sum, "BIGINT").into(),
        _ => sum.into(),
    };
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").expr_as(bytes, "bytes").from(OBJECTS).and_where(Expr::col("user_id").eq(user));
    select
}

/// The account row, locked for the transaction: every write and delete of a user's objects takes
/// it first, so they run one at a time per user (exact quotas; the state read before a write
/// stays true until the commit; no gap locks, so no deadlocks between different users on MySQL).
/// MySQL `FOR UPDATE`; PostgreSQL `FOR NO KEY UPDATE` (it does not block the `FOR KEY SHARE` of
/// foreign-key checks, e.g. a chat message by the same user); SQLite's write transaction holds the
/// database lock anyway. `None` if the account does not exist.
pub(crate) fn lock_user(user: i64, dialect: Dialect) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from("auth_users").and_where(Expr::col("id").eq(user));
    match dialect {
        Dialect::Postgres => select.lock(LockType::NoKeyUpdate),
        _ => select.lock_exclusive(),
    };
    select
}

/// What an update writes and what it requires.
pub(crate) struct Update<'a> {
    pub(crate) user: i64,
    pub(crate) collection: &'a str,
    pub(crate) key: &'a str,
    pub(crate) value: Vec<u8>,
    pub(crate) now: i64,
    /// Only if the stored version is this one.
    pub(crate) if_version: Option<i64>,
    /// Only if the object is owner-writable (client writes).
    pub(crate) owner_only: bool,
    /// Set the lock (server / admin writes).
    pub(crate) write: Option<&'a str>,
    /// Set the visibility.
    pub(crate) visibility: Option<&'a str>,
}

/// Overwrite an object and bump its version (0 rows: absent, another version, or locked).
pub(crate) fn update(u: Update<'_>) -> UpdateStatement {
    let size = i64::try_from(u.value.len()).unwrap_or(i64::MAX);
    let mut update = Query::update();
    update
        .table(OBJECTS)
        .value("value", u.value)
        .value("version", Expr::col("version").add(1))
        .value("size_bytes", size)
        .value("updated_at", u.now)
        .cond_where(identity(u.user, u.collection, u.key));
    if let Some(write) = u.write {
        update.value("write_access", write);
    }
    if let Some(visibility) = u.visibility {
        update.value("visibility", visibility);
    }
    if let Some(version) = u.if_version {
        update.and_where(Expr::col("version").eq(version));
    }
    if u.owner_only {
        update.and_where(Expr::col("write_access").eq(WRITE_OWNER));
    }
    update
}

/// A new object, version 1.
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert(user: i64, collection: &str, key: &str, value: Vec<u8>, write: &str, visibility: &str, now: i64) -> Result<InsertStatement, DbError> {
    let size = i64::try_from(value.len()).unwrap_or(i64::MAX);
    let mut insert = Query::insert();
    insert
        .into_table(OBJECTS)
        .columns(["user_id", "collection", "object_key", "value", "version", "write_access", "visibility", "size_bytes", "created_at", "updated_at"])
        .values([user.into(), collection.into(), key.into(), value.into(), 1i64.into(), write.into(), visibility.into(), size.into(), now.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

/// Delete an object (0 rows: absent, another version, or locked).
pub(crate) fn delete(user: i64, collection: &str, key: &str, if_version: Option<i64>, owner_only: bool) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(OBJECTS).cond_where(identity(user, collection, key));
    if let Some(version) = if_version {
        delete.and_where(Expr::col("version").eq(version));
    }
    if owner_only {
        delete.and_where(Expr::col("write_access").eq(WRITE_OWNER));
    }
    delete
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{render_statement, Dialect};

    #[test]
    fn statements_render_everywhere() {
        let update = update(Update {
            user: 1,
            collection: "saves",
            key: "a",
            value: b"{}".to_vec(),
            now: 5,
            if_version: Some(3),
            owner_only: true,
            write: None,
            visibility: Some("public"),
        });
        let sql = render_statement(&update, Dialect::Postgres);
        assert!(sql.contains("\"version\" = \"version\" + 1") && sql.contains("\"version\" = 3") && sql.contains("\"write_access\" = 'owner'"), "{sql}");
        let lock = render_statement(&lock_user(1, Dialect::MySql), Dialect::MySql);
        assert!(lock.ends_with("FOR UPDATE"), "{lock}");
        let lock = render_statement(&lock_user(1, Dialect::Postgres), Dialect::Postgres);
        assert!(lock.ends_with("FOR NO KEY UPDATE"), "{lock}");
        assert!(!render_statement(&lock_user(1, Dialect::Sqlite), Dialect::Sqlite).contains("FOR "));
        let used = render_statement(&usage(1, Dialect::MySql), Dialect::MySql);
        assert!(used.contains("CAST(COALESCE(SUM(`size_bytes`), 0) AS SIGNED) AS `bytes`"), "{used}");
        let used = render_statement(&usage(1, Dialect::Postgres), Dialect::Postgres);
        assert!(used.contains("CAST(COALESCE(SUM(\"size_bytes\"), 0) AS BIGINT)"), "{used}");
        let many = render_statement(&objects(1, &[("s", "a"), ("s", "b")]), Dialect::Sqlite);
        assert!(many.contains(" OR "), "{many}");
        let page = render_statement(&list(1, "s", Some("k"), 11, None), Dialect::MySql);
        assert!(page.contains("`object_key` > 'k'") && page.contains("ORDER BY `object_key` ASC") && page.contains("LIMIT 11"), "{page}");
        let shown = render_statement(&list(1, "s", None, 11, Some(&["public", "friends"])), Dialect::Postgres);
        assert!(shown.contains("\"visibility\" IN ('public', 'friends')"), "{shown}");
    }
}
