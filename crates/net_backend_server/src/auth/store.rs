//! The auth module's SQL, as sea-query statements (one statement, three dialects). Callers run
//! them on the pool or inside a transaction. Every statement here runs in the auth test suite on
//! SQLite locally and on MySQL / PostgreSQL in CI and on the test server.

use sea_query::{DeleteStatement, Expr, ExprTrait, Func, InsertStatement, LikeExpr, LockType, Order, Query, SelectStatement, UpdateStatement};

use crate::db::{DbError, Dialect};

pub(crate) const USERS: &str = "auth_users";
pub(crate) const CREDENTIALS: &str = "auth_credentials";
pub(crate) const IDENTITIES: &str = "auth_identities";
pub(crate) const SESSIONS: &str = "auth_sessions";
pub(crate) const ACCESS: &str = "auth_access_tokens";
pub(crate) const REFRESH: &str = "auth_refresh_tokens";
pub(crate) const EMAIL_TOKENS: &str = "auth_email_tokens";
pub(crate) const ROLES: &str = "auth_user_roles";
pub(crate) const AUDIT: &str = "auth_audit_log";
pub(crate) const SECRETS: &str = "auth_secrets";

/// Email-token purposes.
pub(crate) const PURPOSE_VERIFY: &str = "verify";
pub(crate) const PURPOSE_RESET: &str = "reset";

fn build(error: sea_query::error::Error) -> DbError {
    DbError::Build(error.to_string())
}

#[derive(Clone, sqlx::FromRow)]
pub(crate) struct UserRow {
    pub(crate) id: i64,
    pub(crate) email: Option<String>,
    pub(crate) email_normalized: Option<String>,
    pub(crate) email_verified_at: Option<i64>,
    pub(crate) display_name: Option<String>,
    pub(crate) banned_at: Option<i64>,
    pub(crate) banned_until: Option<i64>,
    pub(crate) ban_reason: Option<String>,
    pub(crate) created_at: i64,
}

/// Personal data stays out of `Debug` (a future `debug!(?row)` must not log an email address).
impl std::fmt::Debug for UserRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserRow")
            .field("id", &self.id)
            .field("email", &self.email.as_ref().map(|_| "<hidden>"))
            .field("banned_at", &self.banned_at)
            .finish_non_exhaustive()
    }
}

impl UserRow {
    /// Whether a ban is in force at `now`.
    pub(crate) fn is_banned(&self, now: i64) -> bool {
        self.banned_at.is_some() && self.banned_until.is_none_or(|until| until > now)
    }
}

const USER_COLUMNS: [&str; 9] =
    ["id", "email", "email_normalized", "email_verified_at", "display_name", "banned_at", "banned_until", "ban_reason", "created_at"];

fn select_users() -> SelectStatement {
    let mut select = Query::select();
    for column in USER_COLUMNS {
        select.column((USERS, column));
    }
    select.from(USERS);
    select
}

pub(crate) fn user_by_id(id: i64) -> SelectStatement {
    let mut select = select_users();
    select.and_where(Expr::col((USERS, "id")).eq(id));
    select
}

pub(crate) fn user_by_email(normalized: &str) -> SelectStatement {
    let mut select = select_users();
    select.and_where(Expr::col((USERS, "email_normalized")).eq(normalized));
    select
}

pub(crate) fn user_by_identity(provider: &str, subject: &str) -> SelectStatement {
    let mut select = select_users();
    select
        .inner_join(IDENTITIES, Expr::col((IDENTITIES, "user_id")).equals((USERS, "id")))
        .and_where(Expr::col((IDENTITIES, "provider")).eq(provider))
        .and_where(Expr::col((IDENTITIES, "subject")).eq(subject));
    select
}

pub(crate) fn insert_user(
    email: Option<&str>,
    normalized: Option<&str>,
    display_name: Option<&str>,
    verified_at: Option<i64>,
    now: i64,
) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(USERS)
        .columns(["email", "email_normalized", "email_verified_at", "display_name", "created_at", "updated_at"])
        .values([
            email.map(str::to_string).into(),
            normalized.map(str::to_string).into(),
            verified_at.into(),
            display_name.map(str::to_string).into(),
            now.into(),
            now.into(),
        ])
        .map_err(build)?;
    Ok(insert)
}

#[derive(sqlx::FromRow)]
pub(crate) struct CredentialRow {
    pub(crate) password_hash: String,
}

impl std::fmt::Debug for CredentialRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialRow(<redacted>)")
    }
}

/// An account with its password hash (if any), in ONE query: a login costs the same round trips
/// whether or not the address has an account.
#[derive(sqlx::FromRow)]
pub(crate) struct LoginRow {
    #[sqlx(flatten)]
    pub(crate) user: UserRow,
    pub(crate) password_hash: Option<String>,
}

impl std::fmt::Debug for LoginRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRow").field("user", &self.user).field("password_hash", &"<redacted>").finish()
    }
}

/// The account of a normalised email address with its password hash.
pub(crate) fn login_by_email(normalized: &str) -> SelectStatement {
    let mut select = select_users();
    select
        .column((CREDENTIALS, "password_hash"))
        .left_join(CREDENTIALS, Expr::col((CREDENTIALS, "user_id")).equals((USERS, "id")))
        .and_where(Expr::col((USERS, "email_normalized")).eq(normalized));
    select
}

pub(crate) fn credentials(user: i64) -> SelectStatement {
    let mut select = Query::select();
    select.column("password_hash").from(CREDENTIALS).and_where(Expr::col("user_id").eq(user));
    select
}

pub(crate) fn insert_credentials(user: i64, hash: &str, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(CREDENTIALS).columns(["user_id", "password_hash", "updated_at"]).values([user.into(), hash.into(), now.into()]).map_err(build)?;
    Ok(insert)
}

pub(crate) fn update_credentials(user: i64, hash: &str, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(CREDENTIALS).value("password_hash", hash).value("updated_at", now).and_where(Expr::col("user_id").eq(user));
    update
}

pub(crate) fn insert_identity(user: i64, provider: &str, subject: &str, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(IDENTITIES)
        .columns(["user_id", "provider", "subject", "created_at"])
        .values([user.into(), provider.into(), subject.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct IdentityRow {
    pub(crate) user_id: i64,
    pub(crate) provider: String,
    pub(crate) subject: String,
}

pub(crate) fn identities_of(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["user_id", "provider", "subject"]).from(IDENTITIES).and_where(Expr::col("user_id").is_in(users.iter().copied())).order_by("id", Order::Asc);
    select
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct RoleRow {
    pub(crate) user_id: i64,
    pub(crate) role: String,
}

pub(crate) fn roles_of(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["user_id", "role"]).from(ROLES).and_where(Expr::col("user_id").is_in(users.iter().copied())).order_by("role", Order::Asc);
    select
}

pub(crate) fn insert_role(user: i64, role: &str, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(ROLES).columns(["user_id", "role", "created_at"]).values([user.into(), role.into(), now.into()]).map_err(build)?;
    Ok(insert)
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct CountRow {
    pub(crate) n: i64,
}

/// How many accounts hold `role`.
pub(crate) fn count_role(role: &str) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(ROLES).and_where(Expr::col("role").eq(role));
    select
}

/// How many identities an account has (of one provider, or all).
pub(crate) fn count_identities(user: i64, provider: Option<&str>) -> SelectStatement {
    let mut select = Query::select();
    select.expr_as(Expr::col("id").count(), "n").from(IDENTITIES).and_where(Expr::col("user_id").eq(user));
    if let Some(provider) = provider {
        select.and_where(Expr::col("provider").eq(provider));
    }
    select
}

/// Unlink an account's identities of one provider, or all.
pub(crate) fn delete_identities(user: i64, provider: Option<&str>) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(IDENTITIES).and_where(Expr::col("user_id").eq(user));
    if let Some(provider) = provider {
        delete.and_where(Expr::col("provider").eq(provider));
    }
    delete
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct RevokedRow {
    pub(crate) id: i64,
    pub(crate) user_id: i64,
    pub(crate) revoked_at: Option<i64>,
    pub(crate) revoke_reason: Option<String>,
}

/// Sessions revoked after the position `(at, id)` in `(revoked_at, id)` order, oldest first: one
/// keyset page of the revocation poll (`id = i64::MIN` starts at `at` itself).
pub(crate) fn revoked_after(at: i64, id: i64, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select
        .columns(["id", "user_id", "revoked_at", "revoke_reason"])
        .from(SESSIONS)
        .and_where(Expr::col("revoked_at").gt(at).or(Expr::col("revoked_at").eq(at).and(Expr::col("id").gt(id))))
        .order_by("revoked_at", Order::Asc)
        .order_by("id", Order::Asc)
        .limit(limit);
    select
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct SessionIdRow {
    pub(crate) id: i64,
}

/// A user's live sessions (not revoked, not expired) beyond the newest `keep`, newest first (the
/// per-user session cap revokes them).
pub(crate) fn sessions_over_cap(user: i64, now: i64, keep: u64) -> SelectStatement {
    let mut select = Query::select();
    select
        .column("id")
        .from(SESSIONS)
        .and_where(Expr::col("user_id").eq(user))
        .and_where(Expr::col("revoked_at").is_null())
        .and_where(Expr::col("expires_at").gt(now))
        .order_by("id", Order::Desc)
        .limit(1000)
        .offset(keep);
    select
}

/// Revoke these sessions (those not revoked yet).
pub(crate) fn revoke_session_ids(ids: &[i64], reason: &str, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(SESSIONS)
        .value("revoked_at", now)
        .value("revoke_reason", reason)
        .and_where(Expr::col("id").is_in(ids.iter().copied()))
        .and_where(Expr::col("revoked_at").is_null());
    update
}

pub(crate) fn delete_role(user: i64, role: &str) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(ROLES).and_where(Expr::col("user_id").eq(user)).and_where(Expr::col("role").eq(role));
    delete
}

pub(crate) fn insert_session(
    user: i64,
    method: &str,
    ip: Option<String>,
    user_agent: Option<String>,
    now: i64,
    expires_at: i64,
) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(SESSIONS)
        .columns(["user_id", "method", "ip", "user_agent", "created_at", "last_used_at", "expires_at"])
        .values([user.into(), method.into(), ip.into(), user_agent.into(), now.into(), now.into(), expires_at.into()])
        .map_err(build)?;
    Ok(insert)
}

pub(crate) fn insert_token(table: &'static str, session: i64, user: i64, hash: &str, expires_at: i64, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(table)
        .columns(["session_id", "user_id", "token_hash", "expires_at", "created_at"])
        .values([session.into(), user.into(), hash.into(), expires_at.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

/// An access token with what authentication needs: its session and the user's ban.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct AccessRow {
    pub(crate) user_id: i64,
    pub(crate) session_id: i64,
    pub(crate) expires_at: i64,
    pub(crate) session_revoked_at: Option<i64>,
    pub(crate) session_last_used_at: i64,
    pub(crate) session_created_at: i64,
    pub(crate) banned_at: Option<i64>,
    pub(crate) banned_until: Option<i64>,
}

pub(crate) fn access_by_hash(hash: &str) -> SelectStatement {
    let mut select = Query::select();
    select
        .column((ACCESS, "user_id"))
        .column((ACCESS, "session_id"))
        .column((ACCESS, "expires_at"))
        .expr_as(Expr::col((SESSIONS, "revoked_at")), "session_revoked_at")
        .expr_as(Expr::col((SESSIONS, "last_used_at")), "session_last_used_at")
        .expr_as(Expr::col((SESSIONS, "created_at")), "session_created_at")
        .column((USERS, "banned_at"))
        .column((USERS, "banned_until"))
        .from(ACCESS)
        .inner_join(SESSIONS, Expr::col((SESSIONS, "id")).equals((ACCESS, "session_id")))
        .inner_join(USERS, Expr::col((USERS, "id")).equals((ACCESS, "user_id")))
        .and_where(Expr::col((ACCESS, "token_hash")).eq(hash));
    select
}

/// A token row (access or refresh) by hash, for the grace answer.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct TokenRow {
    pub(crate) session_id: i64,
    pub(crate) expires_at: i64,
}

pub(crate) fn token_by_hash(table: &'static str, hash: &str) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["session_id", "expires_at"]).from(table).and_where(Expr::col("token_hash").eq(hash));
    select
}

/// A refresh token with its session's state.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct RefreshRow {
    pub(crate) id: i64,
    pub(crate) session_id: i64,
    pub(crate) user_id: i64,
    pub(crate) expires_at: i64,
    pub(crate) used_at: Option<i64>,
    pub(crate) session_revoked_at: Option<i64>,
}

pub(crate) fn refresh_by_hash(hash: &str) -> SelectStatement {
    let mut select = Query::select();
    select
        .column((REFRESH, "id"))
        .column((REFRESH, "session_id"))
        .column((REFRESH, "user_id"))
        .column((REFRESH, "expires_at"))
        .column((REFRESH, "used_at"))
        .expr_as(Expr::col((SESSIONS, "revoked_at")), "session_revoked_at")
        .from(REFRESH)
        .inner_join(SESSIONS, Expr::col((SESSIONS, "id")).equals((REFRESH, "session_id")))
        .and_where(Expr::col((REFRESH, "token_hash")).eq(hash));
    select
}

#[derive(sqlx::FromRow)]
pub(crate) struct UsedAtRow {
    pub(crate) used_at: Option<i64>,
    pub(crate) rotation_nonce: Option<String>,
}

pub(crate) fn refresh_used_at(id: i64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["used_at", "rotation_nonce"]).from(REFRESH).and_where(Expr::col("id").eq(id));
    select
}

/// Mark a refresh token used with its rotation nonce, only if it was not (the affected-row count
/// tells who won).
pub(crate) fn mark_refresh_used(id: i64, now: i64, nonce: &str) -> UpdateStatement {
    let mut update = Query::update();
    update.table(REFRESH).value("used_at", now).value("rotation_nonce", nonce).and_where(Expr::col("id").eq(id)).and_where(Expr::col("used_at").is_null());
    update
}

/// Forget the rotation nonces of tokens used before `cutoff` (and at or after `since`: the sweep
/// only touches the rows that left the grace window since the last sweep).
pub(crate) fn clear_nonces(since: i64, cutoff: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(REFRESH)
        .value("rotation_nonce", Option::<String>::None)
        .and_where(Expr::col("used_at").gte(since))
        .and_where(Expr::col("used_at").lt(cutoff))
        .and_where(Expr::col("rotation_nonce").is_not_null());
    update
}

/// Forget the rotation nonces of a session's tokens used before `cutoff` (at each rotation).
pub(crate) fn clear_session_nonces(session: i64, cutoff: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(REFRESH)
        .value("rotation_nonce", Option::<String>::None)
        .and_where(Expr::col("session_id").eq(session))
        .and_where(Expr::col("used_at").lt(cutoff))
        .and_where(Expr::col("rotation_nonce").is_not_null());
    update
}

/// Forget the rotation nonce of one token (a reuse after the grace window).
pub(crate) fn clear_nonce(id: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(REFRESH).value("rotation_nonce", Option::<String>::None).and_where(Expr::col("id").eq(id));
    update
}

pub(crate) fn touch_session(session: i64, now: i64, expires_at: Option<i64>) -> UpdateStatement {
    let mut update = Query::update();
    update.table(SESSIONS).value("last_used_at", now).and_where(Expr::col("id").eq(session));
    if let Some(expires_at) = expires_at {
        update.value("expires_at", expires_at);
    }
    update
}

/// Revoke a user's sessions that are not revoked yet: one (`only`), all but one (`except`), or all.
pub(crate) fn revoke_sessions(user: i64, only: Option<i64>, except: Option<i64>, reason: &str, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(SESSIONS)
        .value("revoked_at", now)
        .value("revoke_reason", reason)
        .and_where(Expr::col("user_id").eq(user))
        .and_where(Expr::col("revoked_at").is_null());
    if let Some(only) = only {
        update.and_where(Expr::col("id").eq(only));
    }
    if let Some(except) = except {
        update.and_where(Expr::col("id").ne(except));
    }
    update
}

pub(crate) fn update_display_name(user: i64, name: Option<&str>, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(USERS).value("display_name", name.map(str::to_string)).value("updated_at", now).and_where(Expr::col("id").eq(user));
    update
}

pub(crate) fn set_email_verified(user: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update
        .table(USERS)
        .value("email_verified_at", now)
        .value("updated_at", now)
        .and_where(Expr::col("id").eq(user))
        .and_where(Expr::col("email_verified_at").is_null());
    update
}

pub(crate) fn set_ban(user: i64, ban: Option<(i64, Option<i64>, Option<String>)>, now: i64) -> UpdateStatement {
    let (at, until, reason) = match ban {
        Some((at, until, reason)) => (Some(at), until, reason),
        None => (None, None, None),
    };
    let mut update = Query::update();
    update
        .table(USERS)
        .value("banned_at", at)
        .value("banned_until", until)
        .value("ban_reason", reason)
        .value("updated_at", now)
        .and_where(Expr::col("id").eq(user));
    update
}

pub(crate) fn insert_email_token(user: i64, purpose: &str, normalized: &str, hash: &str, expires_at: i64, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert
        .into_table(EMAIL_TOKENS)
        .columns(["user_id", "purpose", "email_normalized", "token_hash", "expires_at", "created_at"])
        .values([user.into(), purpose.into(), normalized.into(), hash.into(), expires_at.into(), now.into()])
        .map_err(build)?;
    Ok(insert)
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct EmailTokenRow {
    pub(crate) id: i64,
    pub(crate) user_id: i64,
    pub(crate) email_normalized: String,
    pub(crate) expires_at: i64,
    pub(crate) used_at: Option<i64>,
}

pub(crate) fn email_token(purpose: &str, hash: &str) -> SelectStatement {
    let mut select = Query::select();
    select
        .columns(["id", "user_id", "email_normalized", "expires_at", "used_at"])
        .from(EMAIL_TOKENS)
        .and_where(Expr::col("purpose").eq(purpose))
        .and_where(Expr::col("token_hash").eq(hash));
    select
}

pub(crate) fn use_email_token(id: i64, now: i64) -> UpdateStatement {
    let mut update = Query::update();
    update.table(EMAIL_TOKENS).value("used_at", now).and_where(Expr::col("id").eq(id)).and_where(Expr::col("used_at").is_null());
    update
}

/// The account row, locked for the transaction (a primary-key record lock, never a gap): new email
/// tokens of one account are made one at a time. MySQL `FOR UPDATE`; PostgreSQL `FOR NO KEY UPDATE`
/// (compatible with the `FOR KEY SHARE` of the token insert's foreign-key check); SQLite's write
/// transaction holds the database lock anyway.
pub(crate) fn lock_user(user: i64, dialect: Dialect) -> SelectStatement {
    let mut select = Query::select();
    select.column("id").from(USERS).and_where(Expr::col("id").eq(user));
    match dialect {
        Dialect::Postgres => {
            select.lock(LockType::NoKeyUpdate);
        }
        Dialect::Sqlite => {}
        _ => {
            select.lock_exclusive();
        }
    }
    select
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct EmailTokenIdRow {
    pub(crate) id: i64,
}

/// The ids of a user's unused tokens of a purpose (a plain read: no gap locks on MySQL).
pub(crate) fn unused_email_token_ids(user: i64, purpose: &str) -> SelectStatement {
    let mut select = Query::select();
    select
        .column("id")
        .from(EMAIL_TOKENS)
        .and_where(Expr::col("user_id").eq(user))
        .and_where(Expr::col("purpose").eq(purpose))
        .and_where(Expr::col("used_at").is_null());
    select
}

/// Delete unused tokens by id (MySQL locks only these rows, never a gap of the user index).
pub(crate) fn delete_unused_email_tokens_by_id(ids: &[i64]) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(EMAIL_TOKENS).and_where(Expr::col("id").is_in(ids.iter().copied())).and_where(Expr::col("used_at").is_null());
    delete
}

/// Delete a user's unused tokens of a purpose (a new one replaces them).
pub(crate) fn delete_unused_email_tokens(user: i64, purpose: &str) -> DeleteStatement {
    let mut delete = Query::delete();
    delete
        .from_table(EMAIL_TOKENS)
        .and_where(Expr::col("user_id").eq(user))
        .and_where(Expr::col("purpose").eq(purpose))
        .and_where(Expr::col("used_at").is_null());
    delete
}

/// Escape `%`, `_` and `\` for a LIKE pattern with `\` as the escape character.
fn like_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A page of users, newest first: ids below `before`, matching `q`, at most `limit` rows.
pub(crate) fn list_users(q: Option<&str>, before: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = select_users();
    if let Some(before) = before {
        select.and_where(Expr::col((USERS, "id")).lt(before));
    }
    if let Some(q) = q.map(str::trim).filter(|q| !q.is_empty()) {
        let email = LikeExpr::new(format!("%{}%", like_escape(&q.to_lowercase()))).escape('\\');
        let name = LikeExpr::new(format!("%{}%", like_escape(&q.to_lowercase()))).escape('\\');
        // LOWER() on both sides: case-insensitive on every database (MySQL utf8mb4_bin and PostgreSQL
        // LIKE are case-sensitive, SQLite's is not).
        let mut condition = Expr::col((USERS, "email_normalized")).like(email).or(Expr::expr(Func::lower(Expr::col((USERS, "display_name")))).like(name));
        if let Ok(id) = q.parse::<i64>() {
            condition = condition.or(Expr::col((USERS, "id")).eq(id));
        }
        select.and_where(condition);
    }
    select.order_by((USERS, "id"), Order::Desc).limit(limit);
    select
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct SessionStatRow {
    pub(crate) user_id: i64,
    pub(crate) n: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct LastSeenRow {
    pub(crate) user_id: i64,
    pub(crate) last_seen: Option<i64>,
}

/// Open (not revoked, not expired) sessions per user.
pub(crate) fn active_sessions_of(users: &[i64], now: i64) -> SelectStatement {
    let mut select = Query::select();
    select
        .column("user_id")
        .expr_as(Expr::col("id").count(), "n")
        .from(SESSIONS)
        .and_where(Expr::col("user_id").is_in(users.iter().copied()))
        .and_where(Expr::col("revoked_at").is_null())
        .and_where(Expr::col("expires_at").gt(now))
        .group_by_col("user_id");
    select
}

/// The last use of any session per user.
pub(crate) fn last_seen_of(users: &[i64]) -> SelectStatement {
    let mut select = Query::select();
    select
        .column("user_id")
        .expr_as(Expr::col("last_used_at").max(), "last_seen")
        .from(SESSIONS)
        .and_where(Expr::col("user_id").is_in(users.iter().copied()))
        .group_by_col("user_id");
    select
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct AuditRow {
    pub(crate) id: i64,
    pub(crate) actor_user_id: Option<i64>,
    pub(crate) action: String,
    pub(crate) target_type: Option<String>,
    pub(crate) target_id: Option<String>,
    pub(crate) ip: Option<String>,
    pub(crate) request_id: Option<String>,
    pub(crate) data: Option<String>,
    pub(crate) created_at: i64,
}

/// A page of audit entries, newest first.
pub(crate) fn list_audit(user: Option<i64>, action: Option<&str>, before: Option<i64>, limit: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns(["id", "actor_user_id", "action", "target_type", "target_id", "ip", "request_id", "data", "created_at"]).from(AUDIT);
    if let Some(before) = before {
        select.and_where(Expr::col("id").lt(before));
    }
    if let Some(user) = user {
        select.and_where(Expr::col("actor_user_id").eq(user).or(Expr::col("target_type").eq("user").and(Expr::col("target_id").eq(user.to_string()))));
    }
    if let Some(action) = action {
        if action.ends_with('.') {
            select.and_where(Expr::col("action").like(LikeExpr::new(format!("{}%", like_escape(action))).escape('\\')));
        } else {
            select.and_where(Expr::col("action").eq(action));
        }
    }
    select.order_by("id", Order::Desc).limit(limit);
    select
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct SecretRow {
    pub(crate) value: String,
}

pub(crate) fn secret(name: &str) -> SelectStatement {
    let mut select = Query::select();
    select.column("value").from(SECRETS).and_where(Expr::col("name").eq(name));
    select
}

pub(crate) fn insert_secret(name: &str, value: &str, now: i64) -> Result<InsertStatement, DbError> {
    let mut insert = Query::insert();
    insert.into_table(SECRETS).columns(["name", "value", "created_at"]).values([name.into(), value.into(), now.into()]).map_err(build)?;
    Ok(insert)
}

/// Delete rows of `table` whose `column` lies before `cutoff`.
pub(crate) fn delete_before(table: &'static str, column: &'static str, cutoff: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(table).and_where(Expr::col(column).lt(cutoff));
    delete
}

/// Delete sessions that ended (revoked or expired) before `cutoff` (their tokens go with them).
pub(crate) fn delete_old_sessions(cutoff: i64) -> DeleteStatement {
    let mut delete = Query::delete();
    delete.from_table(SESSIONS).and_where(Expr::col("expires_at").lt(cutoff).or(Expr::col("revoked_at").lt(cutoff)));
    delete
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{render_statement, Dialect};

    #[test]
    fn like_patterns_are_escaped() {
        assert_eq!(like_escape("50%_a\\b"), "50\\%\\_a\\\\b");
        let sql = render_statement(&list_users(Some("Ada_"), Some(10), 51), Dialect::Postgres);
        // PostgreSQL's escape-string literal doubles the backslash: E'%ada\\_%'.
        assert!(sql.contains(r"LIKE E'%ada\\_%' ESCAPE"), "{sql}");
        assert!(sql.contains("ORDER BY \"auth_users\".\"id\" DESC LIMIT 51"), "{sql}");
    }

    #[test]
    fn email_token_statements_take_no_ranged_lock() {
        assert!(render_statement(&lock_user(7, Dialect::MySql), Dialect::MySql).ends_with("FOR UPDATE"));
        assert!(render_statement(&lock_user(7, Dialect::Postgres), Dialect::Postgres).ends_with("FOR NO KEY UPDATE"));
        assert!(!render_statement(&lock_user(7, Dialect::Sqlite), Dialect::Sqlite).contains("FOR "));
        let delete = render_statement(&delete_unused_email_tokens_by_id(&[3, 4]), Dialect::MySql);
        assert!(delete.contains("`id` IN (3, 4)") && !delete.contains("user_id"), "{delete}");
    }

    #[test]
    fn joins_render_on_every_dialect() {
        for dialect in Dialect::ALL {
            let sql = render_statement(&access_by_hash("abc"), *dialect);
            assert!(sql.contains("session_revoked_at") && sql.contains("JOIN"), "{sql}");
        }
    }
}
