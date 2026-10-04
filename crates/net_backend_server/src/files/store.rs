//! The files module's SQL, as sea-query statements (one statement, three dialects). Every statement
//! runs in the files suite on SQLite locally and on MySQL / PostgreSQL (env-gated).
//!
//! An upload's row is written under the owner's account lock (MySQL `FOR UPDATE`, PostgreSQL
//! `FOR NO KEY UPDATE`, SQLite's write transaction) after a fresh count of the owner's files, so
//! the quotas are exact; rows change by primary key only.

use sea_query::{Cond, DeleteStatement, Expr, ExprTrait, Func, InsertStatement, LockType, Order, Query, SelectStatement, UpdateStatement, Value};

use super::migrations::{FILES, SHARES};
use crate::db::{DbError, Dialect};

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

/// One file row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct FileRow {
    pub(crate) id: i64,
    pub(crate) owner_id: i64,
    pub(crate) name: String,
    pub(crate) content_type: String,
    pub(crate) size_bytes: i64,
    pub(crate) sha256: String,
    pub(crate) storage_key: String,
    pub(crate) visibility: String,
    pub(crate) metadata: Option<Vec<u8>>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

const FILE_COLUMNS: [&str; 11] =
    ["id", "owner_id", "name", "content_type", "size_bytes", "sha256", "storage_key", "visibility", "metadata", "created_at", "updated_at"];

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    #[allow(dead_code)]
    pub(crate) id: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct UserRow {
    pub(crate) user_id: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct CountRow {
    pub(crate) n: i64,
}

/// A player's files and their bytes.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct UsageRow {
    pub(crate) n: i64,
    pub(crate) bytes: i64,
}

/// The account row, locked for the transaction (`None` if the account does not exist).
pub(crate) fn lock_user(user: i64, dialect: Dialect) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from("auth_users").and_where(Expr::col("id").eq(user));
    match dialect {
        Dialect::Postgres => select.lock(LockType::NoKeyUpdate),
        _ => select.lock_exclusive(),
    };
    select
}

/// How many files a player owns and their bytes (`SUM` cast back to a BIGINT on MySQL /
/// PostgreSQL).
pub(crate) fn usage(owner: i64, dialect: Dialect) -> SelectStatement {
    let sum = Func::coalesce([Expr::from(Func::sum(Expr::col("size_bytes"))), Expr::val(0i64)]);
    let bytes: Expr = match dialect {
        Dialect::MySql => Func::cast_as(sum, "SIGNED").into(),
        Dialect::Postgres => Func::cast_as(sum, "BIGINT").into(),
        _ => sum.into(),
    };
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").expr_as(bytes, "bytes").from(FILES).and_where(Expr::col("owner_id").eq(owner));
    select
}

/// What a new file row holds.
pub(crate) struct NewFile<'a> {
    pub(crate) owner: i64,
    pub(crate) name: &'a str,
    pub(crate) content_type: &'a str,
    pub(crate) size: i64,
    pub(crate) sha256: &'a str,
    pub(crate) key: &'a str,
    pub(crate) visibility: &'a str,
    pub(crate) metadata: Option<Vec<u8>>,
    pub(crate) now: i64,
}

pub(crate) fn insert_file(f: NewFile<'_>) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(FILES)
        .columns(["owner_id", "name", "content_type", "size_bytes", "sha256", "storage_key", "visibility", "metadata", "created_at", "updated_at"])
        .values([
            f.owner.into(),
            f.name.into(),
            f.content_type.into(),
            f.size.into(),
            f.sha256.into(),
            f.key.into(),
            f.visibility.into(),
            Value::Bytes(f.metadata).into(),
            f.now.into(),
            f.now.into(),
        ])
        .map_err(build)?;
    Ok(insert)
}

/// The file row's id, locked for the transaction (`None`: the file is gone).
pub(crate) fn lock_file(id: i64, dialect: Dialect) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(FILES).and_where(Expr::col("id").eq(id));
    match dialect {
        Dialect::Postgres => select.lock(LockType::NoKeyUpdate),
        _ => select.lock_exclusive(),
    };
    select
}

pub(crate) fn file(id: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(FILE_COLUMNS).from(FILES).and_where(Expr::col("id").eq(id));
    select
}

/// A page of `owner`'s files, newest first (ids below `before`). With `reader` (another player):
/// only the visibilities in `shown`, plus the `shared` files shared with `reader`.
pub(crate) fn page(owner: i64, before: Option<i64>, limit: u64, reader: Option<(i64, &[&str])>) -> SelectStatement {
    let mut select = Query::select();
    select.columns(FILE_COLUMNS).from(FILES).and_where(Expr::col("owner_id").eq(owner));
    if let Some(before) = before {
        select.and_where(Expr::col("id").lt(before));
    }
    if let Some((reader, shown)) = reader {
        let mut shared = Query::select();
        shared.column("file_id").from(SHARES).and_where(Expr::col("user_id").eq(reader));
        let mut any = Cond::any().add(Cond::all().add(Expr::col("visibility").eq("shared")).add(Expr::col("id").in_subquery(shared)));
        if !shown.is_empty() {
            any = any.add(Expr::col("visibility").is_in(shown.iter().copied()));
        }
        select.cond_where(any);
    }
    select.order_by("id", Order::Desc).limit(limit);
    select
}

/// The accounts a file is shared with.
pub(crate) fn shares_of(file: i64) -> SelectStatement {
    let mut select = Query::select();
    select.column("user_id").from(SHARES).and_where(Expr::col("file_id").eq(file)).order_by("user_id", Order::Asc);
    select
}

/// Whether a file is shared with `user` (a count of 0 or 1).
pub(crate) fn is_shared(file: i64, user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(SHARES).and_where(Expr::col("file_id").eq(file)).and_where(Expr::col("user_id").eq(user));
    select
}

/// How many of these accounts exist.
pub(crate) fn count_accounts(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from("auth_users").and_where(Expr::col("id").is_in(users.iter().copied()));
    select
}

pub(crate) fn insert_share(file: i64, user: i64, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(SHARES).columns(["file_id", "user_id", "created_at"]).values([file.into(), user.into(), now.into()]).map_err(build)?;
    Ok(insert)
}

pub(crate) fn delete_shares(file: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(SHARES).and_where(Expr::col("file_id").eq(file));
    delete
}

/// A file's settings change (name, visibility, metadata; `None` keeps a field).
pub(crate) fn update_file(id: i64, name: Option<&str>, visibility: Option<&str>, metadata: Option<Option<Vec<u8>>>, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(FILES).value("updated_at", now).and_where(Expr::col("id").eq(id));
    if let Some(name) = name {
        update.value("name", name);
    }
    if let Some(visibility) = visibility {
        update.value("visibility", visibility);
    }
    if let Some(metadata) = metadata {
        update.value("metadata", Value::Bytes(metadata));
    }
    update
}

/// A stored key that a file row names.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct KeyRow {
    pub(crate) storage_key: String,
}

/// Which of these store keys a file row names (the unique key index).
pub(crate) fn known_keys(keys: &[String]) -> SelectStatement {
    let mut select = Query::select();
    select.column("storage_key").from(FILES).and_where(Expr::col("storage_key").is_in(keys.iter().map(String::as_str)));
    select
}

pub(crate) fn delete_file(id: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(FILES).and_where(Expr::col("id").eq(id));
    delete
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::render_statement;

    #[test]
    fn statements_render_everywhere() {
        let page_sql = render_statement(&page(4, Some(90), 11, Some((7, &["public", "friends"]))), Dialect::MySql);
        assert!(page_sql.contains("`owner_id` = 4") && page_sql.contains("`id` < 90") && page_sql.contains("ORDER BY `id` DESC"), "{page_sql}");
        assert!(
            page_sql.contains("`visibility` IN ('public', 'friends')") && page_sql.contains("SELECT `file_id` FROM `stored_file_shares` WHERE `user_id` = 7"),
            "{page_sql}"
        );
        let mine = render_statement(&page(4, None, 11, None), Dialect::Postgres);
        assert!(mine.contains("WHERE \"owner_id\" = 4 ORDER BY"), "{mine}");
        let used = render_statement(&usage(1, Dialect::Postgres), Dialect::Postgres);
        assert!(used.contains("CAST(COALESCE(SUM(\"size_bytes\"), 0) AS BIGINT)"), "{used}");
        let lock = render_statement(&lock_user(1, Dialect::Postgres), Dialect::Postgres);
        assert!(lock.ends_with("FOR NO KEY UPDATE"), "{lock}");
        let clear = render_statement(&update_file(3, None, Some("public"), Some(None), 5), Dialect::Sqlite);
        let keys = render_statement(&known_keys(&["ab".into(), "cd".into()]), Dialect::MySql);
        assert_eq!(keys, "SELECT `storage_key` FROM `stored_files` WHERE `storage_key` IN ('ab', 'cd')");
        assert!(clear.contains("\"metadata\" = NULL") && clear.contains("\"visibility\" = 'public'") && !clear.contains("\"name\""), "{clear}");
    }
}
