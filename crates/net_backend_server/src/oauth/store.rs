//! The OpenID Connect module's SQL, as sea-query statements (one statement, three dialects).

use sea_query::{DeleteStatement, Expr, ExprTrait, InsertStatement, Order, Query, SelectStatement};

use super::migrations::USED;
use crate::db::DbError;

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdRow {
    pub(crate) id: i64,
}

/// Record a used nonce / token (a unique violation = used before).
pub(crate) fn insert_used(provider: &str, token_key: &str, expires_at: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(USED)
        .columns(["provider", "token_key", "expires_at"])
        .values([provider.into(), token_key.into(), expires_at.into()])
        .map_err(|e| DbError::Build(e.to_string()))?;
    Ok(insert)
}

/// Up to `limit` expired rows (a plain read; the delete goes by id: no ranged locking statement).
pub(crate) fn expired(now: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(USED).and_where(Expr::col("expires_at").lt(now)).order_by("id", Order::Asc).limit(limit);
    select
}

/// Delete rows by id.
pub(crate) fn delete_ids(ids: &[i64]) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(USED).and_where(Expr::col("id").is_in(ids.iter().copied()));
    delete
}
