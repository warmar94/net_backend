//! The database layer: one [`Db`] handle over the sqlx pool of the configured backend, with
//! statements built by [sea-query](sea_query) so the same code runs on MySQL, PostgreSQL and
//! SQLite.
//!
//! - **No `AnyPool`, no generics:** [`Db`] is an enum over `MySqlPool` / `PgPool` / `SqlitePool`
//!   (only the compiled-in backends exist). Framework queries are sea-query statements rendered
//!   for the pool's [`Dialect`] and bound through `sea-query-sqlx`.
//! - **Rows:** `#[derive(sqlx::FromRow)]` structs decode from every backend ([`FromDbRow`]).
//! - **Raw access:** an app with one database can use its pool directly (`db.mysql()`,
//!   `db.postgres()`, `db.sqlite()`), including sqlx's compile-checked `query!` macros.
//! - **Portable column types** ([`schema`]): `BIGINT` ids, `BIGINT` unix-millisecond timestamps,
//!   bytes for JSON / binary payloads.
//!
//! ```no_run
//! # async fn demo(db: net_backend_server::db::Db) -> Result<(), net_backend_server::db::DbError> {
//! use net_backend_server::sea_query::{Expr, ExprTrait, Query};
//!
//! #[derive(sqlx::FromRow)]
//! struct Score { user_id: i64, points: i64 }
//!
//! let top: Vec<Score> = db
//!     .fetch_all(Query::select().columns(["user_id", "points"]).from("scores")
//!         .and_where(Expr::col("points").gt(100)).limit(10))
//!     .await?;
//! # Ok(()) }
//! ```

// Without any backend `Db` has no variants: the bodies below are unreachable by construction.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

pub mod schema;

use std::fmt;
use std::time::Duration;

use sea_query::{InsertStatement, QueryStatementWriter};
use sea_query_sqlx::SqlxValues;

use crate::config::DatabaseConfig;

/// A SQL dialect. All three always exist as values (for planning and publishing migrations);
/// only the compiled-in ones can be connected ([`is_enabled`](Dialect::is_enabled)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Dialect {
    /// MySQL 8+ / MariaDB.
    MySql,
    /// PostgreSQL.
    Postgres,
    /// SQLite 3.35+ (bundled).
    Sqlite,
}

impl Dialect {
    /// Every dialect.
    pub const ALL: &'static [Dialect] = &[Dialect::MySql, Dialect::Postgres, Dialect::Sqlite];

    /// The short name, also the cargo feature and the migrations sub-directory: `mysql`,
    /// `postgres`, `sqlite`.
    pub const fn name(self) -> &'static str {
        match self {
            Dialect::MySql => "mysql",
            Dialect::Postgres => "postgres",
            Dialect::Sqlite => "sqlite",
        }
    }

    /// The product name: `MySQL`, `PostgreSQL`, `SQLite`.
    pub const fn display_name(self) -> &'static str {
        match self {
            Dialect::MySql => "MySQL",
            Dialect::Postgres => "PostgreSQL",
            Dialect::Sqlite => "SQLite",
        }
    }

    /// Whether this backend is compiled in (its cargo feature is on).
    pub const fn is_enabled(self) -> bool {
        match self {
            Dialect::MySql => cfg!(feature = "mysql"),
            Dialect::Postgres => cfg!(feature = "postgres"),
            Dialect::Sqlite => cfg!(feature = "sqlite"),
        }
    }

    /// The compiled-in dialects.
    pub fn enabled() -> Vec<Dialect> {
        Self::ALL.iter().copied().filter(|d| d.is_enabled()).collect()
    }

    /// The dialect of a connection URL, by its scheme.
    pub fn from_url(url: &str) -> Option<Dialect> {
        let scheme = url.split(':').next()?.to_ascii_lowercase();
        match scheme.as_str() {
            "mysql" | "mariadb" => Some(Dialect::MySql),
            "postgres" | "postgresql" => Some(Dialect::Postgres),
            "sqlite" => Some(Dialect::Sqlite),
            _ => None,
        }
    }

    /// The dialect named `name` (`mysql`, `postgres`, `sqlite`).
    pub fn from_name(name: &str) -> Option<Dialect> {
        Self::ALL.iter().copied().find(|d| d.name() == name)
    }

    /// Whether DDL (`CREATE TABLE` …) can be rolled back in a transaction. MySQL commits DDL
    /// implicitly, so a failed MySQL migration may be half applied.
    pub const fn transactional_ddl(self) -> bool {
        !matches!(self, Dialect::MySql)
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A database error. Its text may contain SQL or driver details: it is for logs only; handlers
/// turn it into a generic `internal` answer ([`AppError`](crate::AppError)).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbError {
    /// An error from sqlx (connection, query, decoding).
    #[error("database: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// A sea-query statement could not be built.
    #[error("query building: {0}")]
    Build(String),
    /// The URL names a backend whose feature is not compiled in.
    #[error("the `{}` feature of net_backend_server is not enabled ({} URL)", .0.name(), .0.display_name())]
    BackendDisabled(Dialect),
    /// No backend is compiled in at all.
    #[error("no database backend is compiled in: enable one of the features `mysql`, `postgres`, `sqlite`")]
    NoBackend,
    /// The URL is empty or its scheme is not recognised.
    #[error("the database URL is missing or not a mysql://, postgres:// or sqlite: URL")]
    BadUrl,
    /// Another process holds the migrations lock of this database.
    #[error("another process holds the migrations lock of this database (waited {waited_secs} s); if no migration is running, a stuck database session holds it: end that session")]
    LockTimeout {
        /// How long it waited.
        waited_secs: u64,
    },
}

impl DbError {
    /// A one-line description for operators: for a database error the server's message and code
    /// (SQLSTATE / MySQL error number), without the driver's decorations.
    pub fn describe(&self) -> String {
        match self {
            DbError::Sqlx(sqlx::Error::Database(e)) => match e.code() {
                Some(code) => format!("{} (code {code})", e.message()),
                None => e.message().to_string(),
            },
            other => other.to_string(),
        }
    }

    /// Whether this is a unique-constraint violation (e.g. a duplicate email), on every backend.
    pub fn is_unique_violation(&self) -> bool {
        match self {
            DbError::Sqlx(sqlx::Error::Database(e)) => e.is_unique_violation(),
            _ => false,
        }
    }

    /// Whether this is a foreign-key violation, on every backend.
    pub fn is_foreign_key_violation(&self) -> bool {
        match self {
            DbError::Sqlx(sqlx::Error::Database(e)) => e.is_foreign_key_violation(),
            _ => false,
        }
    }

    /// Whether the database aborted the transaction to resolve a conflict between concurrent
    /// transactions, so that running the whole transaction again may succeed: a deadlock (MySQL
    /// error 1213, SQLSTATE `40001`; PostgreSQL `40P01`), a serialization failure (PostgreSQL
    /// `40001`) or a busy / locked SQLite database. A lock-wait timeout (MySQL 1205) is not
    /// retryable (another wait would only add to it).
    pub fn is_retryable(&self) -> bool {
        match self {
            DbError::Sqlx(error) => sqlx_retryable(error),
            _ => false,
        }
    }
}

fn sqlx_retryable(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(e) = error else { return false };
    #[cfg(feature = "sqlite")]
    if e.try_downcast_ref::<sqlx::sqlite::SqliteError>().is_some() {
        return is_sqlite_busy(e.code().as_deref());
    }
    #[cfg(feature = "mysql")]
    if let Some(mysql) = e.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
        return mysql.number() == 1213;
    }
    matches!(e.code().as_deref(), Some("40001" | "40P01"))
}

/// Whether `error` (or an error in its source chain, e.g. the [`DbError`] inside an
/// [`AppError`](crate::AppError)) is [retryable](DbError::is_retryable).
pub fn is_retryable_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(e) = current {
        if let Some(db) = e.downcast_ref::<DbError>() {
            return db.is_retryable();
        }
        if let Some(sqlx) = e.downcast_ref::<sqlx::Error>() {
            return sqlx_retryable(sqlx);
        }
        current = e.source();
    }
    false
}

/// Bounded retries of a write transaction that the database aborted as a deadlock / serialization
/// failure / busy database ([`DbError::is_retryable`]). Use it only around a transaction whose
/// work can run again from the start: everything it did was rolled back, and nothing outside the
/// database happened inside it.
///
/// ```no_run
/// # async fn demo(db: net_backend_server::db::Db) -> Result<(), net_backend_server::AppError> {
/// use net_backend_server::db::Retry;
///
/// let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
/// loop {
///     let mut tx = db.begin_write().await?;
///     let result: Result<(), net_backend_server::AppError> = async {
///         // ... the transaction's statements on `tx` ...
///         Ok(())
///     }
///     .await;
///     match tx.finish(result).await {
///         Err(error) if retry.again(&error).await => continue,
///         other => return other,
///     }
/// }
/// # }
/// ```
#[derive(Debug)]
pub struct Retry {
    attempt: u32,
    max: u32,
}

impl Retry {
    /// The attempts the framework's own write transactions make in all (the first + 2 retries).
    pub const DEFAULT_ATTEMPTS: u32 = 3;

    /// At most `max_attempts` attempts in all (at least 1).
    pub fn new(max_attempts: u32) -> Self {
        Self { attempt: 1, max: max_attempts.max(1) }
    }

    /// The attempt running now (1 = the first).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// After a failed attempt: `true` (after a short, jittered pause) if `error` is
    /// [retryable](is_retryable_error) and attempts remain; the caller then runs the transaction
    /// again from the start. `false`: give up and answer the error.
    pub fn again(&mut self, error: &(dyn std::error::Error + 'static)) -> impl std::future::Future<Output = bool> + Send + 'static {
        // Decided now: the future holds no reference to the error.
        let attempt = self.attempt;
        let go = attempt < self.max && is_retryable_error(error);
        if go {
            self.attempt += 1;
            tracing::debug!(attempt, "retrying a transaction the database aborted (deadlock / serialization / busy)");
        }
        async move {
            if go {
                // 5-25 ms per attempt so far, spread by the clock's sub-millisecond digits (no RNG crate).
                let spread = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::from(d.subsec_micros()) % 21);
                tokio::time::sleep(Duration::from_millis((5 + spread).saturating_mul(u64::from(attempt)))).await;
            }
            go
        }
    }
}

// Row decoding bounds per backend. Each is a blanket-implemented marker that is empty when the
// backend is not compiled in, so `FromDbRow` means "decodes from every compiled-in backend".
#[cfg(feature = "mysql")]
mod row_mysql {
    /// Decodes from a MySQL row.
    pub trait FromMySqlRow: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> {}
    impl<T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>> FromMySqlRow for T {}
}
#[cfg(not(feature = "mysql"))]
mod row_mysql {
    /// Decodes from a MySQL row (the backend is not compiled in: always true).
    pub trait FromMySqlRow {}
    impl<T> FromMySqlRow for T {}
}
#[cfg(feature = "postgres")]
mod row_postgres {
    /// Decodes from a PostgreSQL row.
    pub trait FromPgRow: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> {}
    impl<T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>> FromPgRow for T {}
}
#[cfg(not(feature = "postgres"))]
mod row_postgres {
    /// Decodes from a PostgreSQL row (the backend is not compiled in: always true).
    pub trait FromPgRow {}
    impl<T> FromPgRow for T {}
}
#[cfg(feature = "sqlite")]
mod row_sqlite {
    /// Decodes from a SQLite row.
    pub trait FromSqliteRow: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> {}
    impl<T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>> FromSqliteRow for T {}
}
#[cfg(not(feature = "sqlite"))]
mod row_sqlite {
    /// Decodes from a SQLite row (the backend is not compiled in: always true).
    pub trait FromSqliteRow {}
    impl<T> FromSqliteRow for T {}
}

pub use row_mysql::FromMySqlRow;
pub use row_postgres::FromPgRow;
pub use row_sqlite::FromSqliteRow;

/// A row type that decodes from every compiled-in backend. Implemented automatically for
/// `#[derive(sqlx::FromRow)]` structs whose fields are portable (`i64`, `String`, `Vec<u8>`,
/// `bool`, `Option<…>`).
pub trait FromDbRow: FromMySqlRow + FromPgRow + FromSqliteRow + Send + Unpin + 'static {}
impl<T: FromMySqlRow + FromPgRow + FromSqliteRow + Send + Unpin + 'static> FromDbRow for T {}

/// Runs `$body` with `$pool` bound to the concrete pool (or transaction) and `$qb` to the
/// matching sea-query builder.
macro_rules! dispatch {
    ($value:expr, $ty:ident, |$pool:ident, $qb:ident| $body:expr) => {
        match $value {
            #[cfg(feature = "mysql")]
            $ty::MySql(ref $pool) => {
                #[allow(unused_variables)]
                let $qb = sea_query::MysqlQueryBuilder;
                $body
            }
            #[cfg(feature = "postgres")]
            $ty::Postgres(ref $pool) => {
                #[allow(unused_variables)]
                let $qb = sea_query::PostgresQueryBuilder;
                $body
            }
            #[cfg(feature = "sqlite")]
            $ty::Sqlite(ref $pool) => {
                #[allow(unused_variables)]
                let $qb = sea_query::SqliteQueryBuilder;
                $body
            }
        }
    };
    (mut $value:expr, $ty:ident, |$pool:ident, $qb:ident| $body:expr) => {
        match $value {
            #[cfg(feature = "mysql")]
            $ty::MySql(ref mut $pool) => {
                #[allow(unused_variables)]
                let $qb = sea_query::MysqlQueryBuilder;
                $body
            }
            #[cfg(feature = "postgres")]
            $ty::Postgres(ref mut $pool) => {
                #[allow(unused_variables)]
                let $qb = sea_query::PostgresQueryBuilder;
                $body
            }
            #[cfg(feature = "sqlite")]
            $ty::Sqlite(ref mut $pool) => {
                #[allow(unused_variables)]
                let $qb = sea_query::SqliteQueryBuilder;
                $body
            }
        }
    };
}

/// The database: the connection pool of the configured backend. Cheap to clone (the pools are
/// reference-counted). Reach it from handlers with `State<Db>` or through
/// [`AppState::db`](crate::AppState::db).
#[derive(Clone)]
#[non_exhaustive]
pub enum Db {
    /// A MySQL / MariaDB pool.
    #[cfg(feature = "mysql")]
    MySql(sqlx::MySqlPool),
    /// A PostgreSQL pool.
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    /// A SQLite pool.
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
}

impl fmt::Debug for Db {
    // sqlx's own Debug prints the connect options; never risk a password in a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Db({})", self.dialect())
    }
}

/// The statement text and its bound values. Refuses an unsigned value above `i64::MAX`: every
/// portable integer column is a signed `BIGINT`, and sea-query-sqlx would panic converting it on
/// SQLite and PostgreSQL.
#[cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(dead_code))]
fn build<S: QueryStatementWriter>(statement: &S, builder: impl sea_query::QueryBuilder) -> Result<(sqlx::AssertSqlSafe<String>, SqlxValues), DbError> {
    let (sql, values) = statement.build(builder);
    check_values(&values)?;
    Ok((sqlx::AssertSqlSafe(sql), SqlxValues(values)))
}

/// Every bound value fits the portable column types.
pub(crate) fn check_values(values: &sea_query::Values) -> Result<(), DbError> {
    for value in &values.0 {
        if let sea_query::Value::BigUnsigned(Some(n)) = value {
            if i64::try_from(*n).is_err() {
                return Err(DbError::Build(format!("the unsigned value {n} does not fit a BIGINT (above i64::MAX)")));
            }
        }
    }
    Ok(())
}

/// Opens one plain connection with the pool's options (an honest first error; probes).
#[allow(unused_macros)]
macro_rules! direct_connection {
    ($conn:ty, $pool:expr) => {
        <$conn as sqlx::Connection>::connect_with(&*$pool.connect_options())
    };
}

impl Db {
    /// Connect with these settings (eagerly, or lazily with `connect_lazy`).
    pub async fn connect(config: &DatabaseConfig) -> Result<Db, DbError> {
        let url = config.url.expose();
        let dialect = Dialect::from_url(url).ok_or(DbError::BadUrl)?;
        if !dialect.is_enabled() {
            return Err(if Dialect::enabled().is_empty() { DbError::NoBackend } else { DbError::BackendDisabled(dialect) });
        }
        #[allow(unused_variables)]
        let acquire = Duration::from_secs(config.acquire_timeout_secs.max(1));
        match dialect {
            #[cfg(feature = "mysql")]
            Dialect::MySql => {
                use std::str::FromStr;
                let options = sqlx::mysql::MySqlConnectOptions::from_str(url)?;
                let pool = sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(config.max_connections.max(1))
                    .min_connections(config.min_connections)
                    .acquire_timeout(acquire);
                if !config.connect_lazy {
                    // One plain connection first: the pool would only report "timed out" for a
                    // refused port or a wrong password.
                    first_connection(<sqlx::MySqlConnection as sqlx::Connection>::connect_with(&options), acquire).await?;
                }
                let pool = if config.connect_lazy { pool.connect_lazy_with(options) } else { pool.connect_with(options).await? };
                Ok(Db::MySql(pool))
            }
            #[cfg(feature = "postgres")]
            Dialect::Postgres => {
                use std::str::FromStr;
                let options = sqlx::postgres::PgConnectOptions::from_str(url)?;
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(config.max_connections.max(1))
                    .min_connections(config.min_connections)
                    .acquire_timeout(acquire);
                if !config.connect_lazy {
                    first_connection(<sqlx::PgConnection as sqlx::Connection>::connect_with(&options), acquire).await?;
                }
                let pool = if config.connect_lazy { pool.connect_lazy_with(options) } else { pool.connect_with(options).await? };
                Ok(Db::Postgres(pool))
            }
            #[cfg(feature = "sqlite")]
            Dialect::Sqlite => {
                use std::str::FromStr;
                let in_memory = url.contains(":memory:") || url.contains("mode=memory");
                let mut options =
                    sqlx::sqlite::SqliteConnectOptions::from_str(url)?.create_if_missing(true).foreign_keys(true).busy_timeout(Duration::from_secs(5));
                let mut pool = sqlx::sqlite::SqlitePoolOptions::new().acquire_timeout(acquire);
                if in_memory {
                    // Every connection to `:memory:` is its own empty database: keep exactly one,
                    // forever, or tables vanish.
                    pool = pool.max_connections(1).min_connections(1).idle_timeout(None).max_lifetime(None);
                } else {
                    pool = pool.max_connections(config.max_connections.max(1)).min_connections(config.min_connections);
                    if config.connect_lazy {
                        options = options.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
                    } else {
                        // Switching a new file to WAL needs an exclusive lock that busy_timeout does
                        // not wait for: do it once here, retried, instead of in every pooled
                        // connection (two processes opening a fresh file raced on it).
                        sqlite_enable_wal(&options, acquire.max(Duration::from_secs(10))).await?;
                    }
                }
                let pool = if config.connect_lazy { pool.connect_lazy_with(options) } else { pool.connect_with(options).await? };
                Ok(Db::Sqlite(pool))
            }
            #[allow(unreachable_patterns)]
            _ => Err(DbError::BackendDisabled(dialect)),
        }
    }

    /// The backend's dialect.
    pub fn dialect(&self) -> Dialect {
        match *self {
            #[cfg(feature = "mysql")]
            Db::MySql(_) => Dialect::MySql,
            #[cfg(feature = "postgres")]
            Db::Postgres(_) => Dialect::Postgres,
            #[cfg(feature = "sqlite")]
            Db::Sqlite(_) => Dialect::Sqlite,
        }
    }

    /// The MySQL pool, if this is MySQL.
    #[cfg(feature = "mysql")]
    pub fn mysql(&self) -> Option<&sqlx::MySqlPool> {
        match self {
            Db::MySql(pool) => Some(pool),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The PostgreSQL pool, if this is PostgreSQL.
    #[cfg(feature = "postgres")]
    pub fn postgres(&self) -> Option<&sqlx::PgPool> {
        match self {
            Db::Postgres(pool) => Some(pool),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The SQLite pool, if this is SQLite.
    #[cfg(feature = "sqlite")]
    pub fn sqlite(&self) -> Option<&sqlx::SqlitePool> {
        match self {
            Db::Sqlite(pool) => Some(pool),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// Check that the database answers (`SELECT 1`).
    pub async fn ping(&self) -> Result<(), DbError> {
        self.execute_script("SELECT 1").await
    }

    /// Readiness probe that fails fast: with an idle pooled connection, `SELECT 1` on it;
    /// otherwise a fresh plain connection (a refused port fails at once instead of waiting for
    /// the pool's acquire timeout). Bounded by `limit`.
    pub async fn check_ready(&self, limit: Duration) -> Result<(), DbError> {
        let probe = async {
            match *self {
                #[cfg(feature = "mysql")]
                Db::MySql(ref pool) => {
                    if pool.num_idle() > 0 {
                        sqlx::raw_sql("SELECT 1").execute(pool).await?;
                    } else {
                        let mut conn = direct_connection!(sqlx::MySqlConnection, pool).await?;
                        sqlx::raw_sql("SELECT 1").execute(&mut conn).await?;
                        let _ = sqlx::Connection::close(conn).await;
                    }
                }
                #[cfg(feature = "postgres")]
                Db::Postgres(ref pool) => {
                    if pool.num_idle() > 0 {
                        sqlx::raw_sql("SELECT 1").execute(pool).await?;
                    } else {
                        let mut conn = direct_connection!(sqlx::PgConnection, pool).await?;
                        sqlx::raw_sql("SELECT 1").execute(&mut conn).await?;
                        let _ = sqlx::Connection::close(conn).await;
                    }
                }
                #[cfg(feature = "sqlite")]
                Db::Sqlite(ref pool) => {
                    sqlx::raw_sql("SELECT 1").execute(pool).await?;
                }
            }
            Ok::<(), DbError>(())
        };
        match tokio::time::timeout(limit, probe).await {
            Ok(result) => result,
            Err(_) => Err(DbError::Sqlx(sqlx::Error::PoolTimedOut)),
        }
    }

    /// Run a query and decode exactly one row (`RowNotFound` if there is none).
    pub async fn fetch_one<T: FromDbRow, S: QueryStatementWriter>(&self, statement: &S) -> Result<T, DbError> {
        dispatch!(*self, Db, |pool, qb| {
            let (sql, values) = build(statement, qb)?;
            Ok(sqlx::query_as_with::<_, T, _>(sql, values).fetch_one(pool).await?)
        })
    }

    /// Whether a table exists in the current database / schema.
    pub(crate) async fn table_exists(&self, table: &str) -> Result<bool, DbError> {
        #[allow(unused_variables)]
        let table = table.to_string();
        let count: i64 = match *self {
            #[cfg(feature = "mysql")]
            Db::MySql(ref pool) => {
                sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = ?")
                    .bind(table)
                    .fetch_one(pool)
                    .await?
            }
            #[cfg(feature = "postgres")]
            Db::Postgres(ref pool) => {
                sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = $1")
                    .bind(table)
                    .fetch_one(pool)
                    .await?
            }
            #[cfg(feature = "sqlite")]
            Db::Sqlite(ref pool) => {
                sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?").bind(table).fetch_one(pool).await?
            }
        };
        Ok(count > 0)
    }

    /// A transaction for one migration. SQLite: `BEGIN IMMEDIATE` (takes the write lock at once,
    /// so two processes migrating one file serialise), retried until `wait` runs out.
    pub(crate) async fn begin_migration(&self, wait: Duration) -> Result<DbTx, DbError> {
        match *self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite(ref pool) => {
                let deadline = tokio::time::Instant::now() + wait;
                loop {
                    match pool.begin_with("BEGIN IMMEDIATE").await {
                        Ok(tx) => return Ok(DbTx::Sqlite(tx)),
                        Err(sqlx::Error::Database(e)) if tokio::time::Instant::now() < deadline && is_sqlite_busy(e.code().as_deref()) => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        Err(sqlx::Error::Database(e)) if is_sqlite_busy(e.code().as_deref()) => {
                            return Err(DbError::LockTimeout { waited_secs: wait.as_secs() });
                        }
                        Err(other) => return Err(other.into()),
                    }
                }
            }
            #[allow(unreachable_patterns)]
            _ => {
                let _ = wait;
                self.begin().await
            }
        }
    }

    /// Run an INSERT / UPDATE / DELETE; returns the number of affected rows.
    pub async fn execute<S: QueryStatementWriter>(&self, statement: &S) -> Result<u64, DbError> {
        dispatch!(*self, Db, |pool, qb| {
            let (sql, values) = build(statement, qb)?;
            Ok(sqlx::query_with(sql, values).execute(pool).await?.rows_affected())
        })
    }

    /// Run a query and decode every row.
    pub async fn fetch_all<T: FromDbRow, S: QueryStatementWriter>(&self, statement: &S) -> Result<Vec<T>, DbError> {
        dispatch!(*self, Db, |pool, qb| {
            let (sql, values) = build(statement, qb)?;
            Ok(sqlx::query_as_with::<_, T, _>(sql, values).fetch_all(pool).await?)
        })
    }

    /// Run a query and decode the first row, if any.
    pub async fn fetch_optional<T: FromDbRow, S: QueryStatementWriter>(&self, statement: &S) -> Result<Option<T>, DbError> {
        dispatch!(*self, Db, |pool, qb| {
            let (sql, values) = build(statement, qb)?;
            Ok(sqlx::query_as_with::<_, T, _>(sql, values).fetch_optional(pool).await?)
        })
    }

    /// Insert one row and return its generated `BIGINT` id (`RETURNING` on PostgreSQL and
    /// SQLite, `LAST_INSERT_ID()` on MySQL).
    pub async fn insert_id(&self, statement: &InsertStatement, id_column: &'static str) -> Result<i64, DbError> {
        let mut returning = statement.clone();
        returning.returning_col(id_column);
        #[allow(unused_variables)]
        let returning = returning;
        match *self {
            #[cfg(feature = "mysql")]
            Db::MySql(ref pool) => {
                let (sql, values) = build(statement, sea_query::MysqlQueryBuilder)?;
                let result = sqlx::query_with(sql, values).execute(pool).await?;
                i64::try_from(result.last_insert_id()).map_err(|_| DbError::Build("insert id out of range".into()))
            }
            #[cfg(feature = "postgres")]
            Db::Postgres(ref pool) => {
                let (sql, values) = build(&returning, sea_query::PostgresQueryBuilder)?;
                Ok(sqlx::query_scalar_with::<_, i64, _>(sql, values).fetch_one(pool).await?)
            }
            #[cfg(feature = "sqlite")]
            Db::Sqlite(ref pool) => {
                let (sql, values) = build(&returning, sea_query::SqliteQueryBuilder)?;
                Ok(sqlx::query_scalar_with::<_, i64, _>(sql, values).fetch_one(pool).await?)
            }
        }
    }

    /// Run SQL text as it is: several statements, no bound values (migrations, DDL). Never put
    /// untrusted input into it.
    pub async fn execute_script(&self, sql: impl Into<String>) -> Result<(), DbError> {
        #[allow(unused_variables)]
        let sql = sql.into();
        dispatch!(*self, Db, |pool, _qb| {
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).execute(pool).await?;
            Ok(())
        })
    }

    /// Start a transaction that will write. On SQLite it takes the write lock at once
    /// (`BEGIN IMMEDIATE`, waiting up to the busy timeout), so a transaction that reads before it
    /// writes cannot fail with `SQLITE_BUSY` when another connection wrote in between; elsewhere
    /// it is [`begin`](Self::begin).
    pub async fn begin_write(&self) -> Result<DbTx, DbError> {
        match *self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite(ref pool) => Ok(DbTx::Sqlite(pool.begin_with("BEGIN IMMEDIATE").await?)),
            #[allow(unreachable_patterns)]
            _ => self.begin().await,
        }
    }

    /// Start a transaction.
    pub async fn begin(&self) -> Result<DbTx, DbError> {
        Ok(match *self {
            #[cfg(feature = "mysql")]
            Db::MySql(ref pool) => DbTx::MySql(pool.begin().await?),
            #[cfg(feature = "postgres")]
            Db::Postgres(ref pool) => DbTx::Postgres(pool.begin().await?),
            #[cfg(feature = "sqlite")]
            Db::Sqlite(ref pool) => DbTx::Sqlite(pool.begin().await?),
        })
    }

    /// The SQL text a statement becomes on this backend (for logs and tests).
    pub fn render<S: QueryStatementWriter>(&self, statement: &S) -> String {
        render_statement(statement, self.dialect())
    }

    /// Close the pool: waits for checked-out connections to come back.
    pub async fn close(&self) {
        dispatch!(*self, Db, |pool, _qb| pool.close().await)
    }

    /// Whether the pool is closed.
    pub fn is_closed(&self) -> bool {
        dispatch!(*self, Db, |pool, _qb| pool.is_closed())
    }

    /// Take the migrations lock of THIS database, waiting at most `wait`; held until the guard is
    /// released (its dedicated session ends).
    /// - MySQL: `GET_LOCK('nbs_migrations:' || SHA1(DATABASE()), 0)`, polled every 250 ms (named
    ///   per database: MySQL lock names are server-wide).
    /// - PostgreSQL: `pg_try_advisory_lock(7462051993)`, polled every 250 ms (advisory locks are
    ///   per database).
    /// - SQLite: no session lock; each migration takes the write lock with `BEGIN IMMEDIATE` and
    ///   re-checks the tracking row ([`begin_migration`](Self::begin_migration)).
    ///
    /// Polling (instead of a blocking wait inside one statement) keeps sqlx's slow-statement
    /// warnings out of the log; one INFO line says that the process waits.
    pub(crate) async fn lock_migrations(&self, wait: Duration) -> Result<MigrationLock, DbError> {
        #[allow(unused_variables)]
        let deadline = tokio::time::Instant::now() + wait;
        match *self {
            #[cfg(feature = "mysql")]
            Db::MySql(ref pool) => {
                let mut conn = pool.acquire().await?.detach();
                let mut announced = false;
                loop {
                    let got: Option<i64> =
                        sqlx::query_scalar("SELECT GET_LOCK(CONCAT('nbs_migrations:', SHA1(IFNULL(DATABASE(), ''))), 0)").fetch_one(&mut conn).await?;
                    if got == Some(1) {
                        return Ok(MigrationLock::MySql(conn));
                    }
                    if tokio::time::Instant::now() >= deadline {
                        let _ = sqlx::Connection::close(conn).await;
                        return Err(DbError::LockTimeout { waited_secs: wait.as_secs() });
                    }
                    if !announced {
                        tracing::info!(">>> NBS: waiting for the migrations lock (another process is migrating)");
                        announced = true;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            #[cfg(feature = "postgres")]
            Db::Postgres(ref pool) => {
                let mut conn = pool.acquire().await?.detach();
                let mut announced = false;
                loop {
                    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(7462051993)").fetch_one(&mut conn).await?;
                    if got {
                        return Ok(MigrationLock::Postgres(conn));
                    }
                    if tokio::time::Instant::now() >= deadline {
                        let _ = sqlx::Connection::close(conn).await;
                        return Err(DbError::LockTimeout { waited_secs: wait.as_secs() });
                    }
                    if !announced {
                        tracing::info!(">>> NBS: waiting for the migrations lock (another process is migrating)");
                        announced = true;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            #[cfg(feature = "sqlite")]
            Db::Sqlite(_) => Ok(MigrationLock::None),
        }
    }
}

/// The first plain connection of an eager connect, bounded by the acquire timeout.
#[allow(dead_code)]
async fn first_connection<C: sqlx::Connection>(connect: impl std::future::Future<Output = Result<C, sqlx::Error>>, limit: Duration) -> Result<(), DbError> {
    match tokio::time::timeout(limit, connect).await {
        Ok(Ok(conn)) => {
            let _ = conn.close().await;
            Ok(())
        }
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err(DbError::Sqlx(sqlx::Error::PoolTimedOut)),
    }
}

/// Put a SQLite file into WAL mode (persistent for the file), retrying while another connection
/// holds the lock, until `wait` runs out.
#[cfg(feature = "sqlite")]
async fn sqlite_enable_wal(options: &sqlx::sqlite::SqliteConnectOptions, wait: Duration) -> Result<(), DbError> {
    use sqlx::Connection;
    let deadline = tokio::time::Instant::now() + wait;
    let mut attempt: u64 = 0;
    loop {
        let result = async {
            let mut conn = sqlx::SqliteConnection::connect_with(options).await?;
            let switched = sqlx::raw_sql("PRAGMA journal_mode = WAL").execute(&mut conn).await.map(|_| ());
            let _ = conn.close().await;
            switched
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(sqlx::Error::Database(e)) if is_sqlite_busy(e.code().as_deref()) && tokio::time::Instant::now() < deadline => {
                attempt += 1;
                // Spread competing processes apart.
                tokio::time::sleep(Duration::from_millis(10 + (attempt * 17) % 40)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// SQLITE_BUSY (5) and its extended codes (e.g. 517 SQLITE_BUSY_SNAPSHOT), SQLITE_LOCKED (6).
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
fn is_sqlite_busy(code: Option<&str>) -> bool {
    code.and_then(|c| c.parse::<i32>().ok()).is_some_and(|c| matches!(c & 0xff, 5 | 6))
}

/// The SQL text of a statement in a dialect (values inlined; for logs, tests and snapshots only).
pub fn render_statement<S: QueryStatementWriter>(statement: &S, dialect: Dialect) -> String {
    match dialect {
        Dialect::MySql => statement.to_string(sea_query::MysqlQueryBuilder),
        Dialect::Postgres => statement.to_string(sea_query::PostgresQueryBuilder),
        Dialect::Sqlite => statement.to_string(sea_query::SqliteQueryBuilder),
    }
}

/// The migrations lock: a dedicated connection that holds it; closing the connection releases it.
pub(crate) enum MigrationLock {
    #[cfg(feature = "mysql")]
    MySql(sqlx::MySqlConnection),
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgConnection),
    #[allow(dead_code)]
    None,
}

impl MigrationLock {
    /// Release the lock by ending the connection's session.
    pub(crate) async fn release(self) {
        match self {
            #[cfg(feature = "mysql")]
            MigrationLock::MySql(conn) => {
                let _ = sqlx::Connection::close(conn).await;
            }
            #[cfg(feature = "postgres")]
            MigrationLock::Postgres(conn) => {
                let _ = sqlx::Connection::close(conn).await;
            }
            MigrationLock::None => {}
        }
    }
}

/// A transaction on the [`Db`]. Commit it with [`commit`](DbTx::commit); dropping it rolls back.
#[non_exhaustive]
pub enum DbTx {
    /// A MySQL transaction.
    #[cfg(feature = "mysql")]
    MySql(sqlx::Transaction<'static, sqlx::MySql>),
    /// A PostgreSQL transaction.
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    /// A SQLite transaction.
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
}

impl fmt::Debug for DbTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DbTx({})", self.dialect())
    }
}

impl DbTx {
    /// The backend's dialect.
    pub fn dialect(&self) -> Dialect {
        match *self {
            #[cfg(feature = "mysql")]
            DbTx::MySql(_) => Dialect::MySql,
            #[cfg(feature = "postgres")]
            DbTx::Postgres(_) => Dialect::Postgres,
            #[cfg(feature = "sqlite")]
            DbTx::Sqlite(_) => Dialect::Sqlite,
        }
    }

    /// Run an INSERT / UPDATE / DELETE inside the transaction; returns the affected rows.
    pub async fn execute<S: QueryStatementWriter>(&mut self, statement: &S) -> Result<u64, DbError> {
        dispatch!(mut *self, DbTx, |tx, qb| {
            let (sql, values) = build(statement, qb)?;
            Ok(sqlx::query_with(sql, values).execute(&mut **tx).await?.rows_affected())
        })
    }

    /// Run a query inside the transaction and decode every row.
    pub async fn fetch_all<T: FromDbRow, S: QueryStatementWriter>(&mut self, statement: &S) -> Result<Vec<T>, DbError> {
        dispatch!(mut *self, DbTx, |tx, qb| {
            let (sql, values) = build(statement, qb)?;
            Ok(sqlx::query_as_with::<_, T, _>(sql, values).fetch_all(&mut **tx).await?)
        })
    }

    /// Run a query inside the transaction and decode the first row, if any.
    pub async fn fetch_optional<T: FromDbRow, S: QueryStatementWriter>(&mut self, statement: &S) -> Result<Option<T>, DbError> {
        dispatch!(mut *self, DbTx, |tx, qb| {
            let (sql, values) = build(statement, qb)?;
            Ok(sqlx::query_as_with::<_, T, _>(sql, values).fetch_optional(&mut **tx).await?)
        })
    }

    /// Run a query inside the transaction and decode exactly one row.
    pub async fn fetch_one<T: FromDbRow, S: QueryStatementWriter>(&mut self, statement: &S) -> Result<T, DbError> {
        dispatch!(mut *self, DbTx, |tx, qb| {
            let (sql, values) = build(statement, qb)?;
            Ok(sqlx::query_as_with::<_, T, _>(sql, values).fetch_one(&mut **tx).await?)
        })
    }

    /// Insert one row inside the transaction and return its generated `BIGINT` id (`RETURNING`
    /// on PostgreSQL and SQLite, `LAST_INSERT_ID()` on MySQL, which is per connection and so
    /// correct inside the transaction).
    pub async fn insert_id(&mut self, statement: &InsertStatement, id_column: &'static str) -> Result<i64, DbError> {
        let mut returning = statement.clone();
        returning.returning_col(id_column);
        #[allow(unused_variables)]
        let returning = returning;
        match *self {
            #[cfg(feature = "mysql")]
            DbTx::MySql(ref mut tx) => {
                let (sql, values) = build(statement, sea_query::MysqlQueryBuilder)?;
                let result = sqlx::query_with(sql, values).execute(&mut **tx).await?;
                i64::try_from(result.last_insert_id()).map_err(|_| DbError::Build("insert id out of range".into()))
            }
            #[cfg(feature = "postgres")]
            DbTx::Postgres(ref mut tx) => {
                let (sql, values) = build(&returning, sea_query::PostgresQueryBuilder)?;
                Ok(sqlx::query_scalar_with::<_, i64, _>(sql, values).fetch_one(&mut **tx).await?)
            }
            #[cfg(feature = "sqlite")]
            DbTx::Sqlite(ref mut tx) => {
                let (sql, values) = build(&returning, sea_query::SqliteQueryBuilder)?;
                Ok(sqlx::query_scalar_with::<_, i64, _>(sql, values).fetch_one(&mut **tx).await?)
            }
        }
    }

    /// Run SQL text as it is inside the transaction (several statements, no bound values).
    pub async fn execute_script(&mut self, sql: impl Into<String>) -> Result<(), DbError> {
        #[allow(unused_variables)]
        let sql = sql.into();
        dispatch!(mut *self, DbTx, |tx, _qb| {
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).execute(&mut **tx).await?;
            Ok(())
        })
    }

    /// Commit.
    pub async fn commit(self) -> Result<(), DbError> {
        match self {
            #[cfg(feature = "mysql")]
            DbTx::MySql(tx) => tx.commit().await?,
            #[cfg(feature = "postgres")]
            DbTx::Postgres(tx) => tx.commit().await?,
            #[cfg(feature = "sqlite")]
            DbTx::Sqlite(tx) => tx.commit().await?,
        }
        Ok(())
    }

    /// Commit if `result` is `Ok` (a failed commit becomes the error), roll back otherwise; the
    /// result.
    pub async fn finish<T, E: From<DbError>>(self, result: Result<T, E>) -> Result<T, E> {
        match result {
            Ok(value) => {
                self.commit().await?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.rollback().await;
                Err(error)
            }
        }
    }

    /// Roll back (dropping the transaction does the same).
    pub async fn rollback(self) -> Result<(), DbError> {
        match self {
            #[cfg(feature = "mysql")]
            DbTx::MySql(tx) => tx.rollback().await?,
            #[cfg(feature = "postgres")]
            DbTx::Postgres(tx) => tx.rollback().await?,
            #[cfg(feature = "sqlite")]
            DbTx::Sqlite(tx) => tx.rollback().await?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialects() {
        assert_eq!(Dialect::from_url("mysql://u@h/db"), Some(Dialect::MySql));
        assert_eq!(Dialect::from_url("MariaDB://u@h/db"), Some(Dialect::MySql));
        assert_eq!(Dialect::from_url("postgresql://u@h/db"), Some(Dialect::Postgres));
        assert_eq!(Dialect::from_url("sqlite::memory:"), Some(Dialect::Sqlite));
        assert_eq!(Dialect::from_url("redis://h"), None);
        assert_eq!(Dialect::from_url(""), None);
        assert_eq!(Dialect::from_name("postgres"), Some(Dialect::Postgres));
        assert!(Dialect::Postgres.transactional_ddl() && !Dialect::MySql.transactional_ddl());
        for d in Dialect::enabled() {
            assert!(d.is_enabled());
        }
    }

    #[test]
    fn unsigned_values_above_i64_are_refused() {
        let ok = sea_query::Values(vec![sea_query::Value::BigUnsigned(Some(i64::MAX as u64)), sea_query::Value::BigUnsigned(None)]);
        assert!(check_values(&ok).is_ok());
        let bad = sea_query::Values(vec![sea_query::Value::BigUnsigned(Some(u64::MAX))]);
        assert!(matches!(check_values(&bad), Err(DbError::Build(m)) if m.contains("does not fit")));
        assert!(is_sqlite_busy(Some("5")) && is_sqlite_busy(Some("517")) && is_sqlite_busy(Some("6")) && !is_sqlite_busy(Some("19")) && !is_sqlite_busy(None));
    }

    #[tokio::test]
    async fn only_conflicts_are_retried() {
        let plain = DbError::Build("x".into());
        assert!(!plain.is_retryable());
        let mut retry = Retry::new(3);
        assert!(!retry.again(&plain).await, "not a conflict");
        assert_eq!(retry.attempt(), 1);
        let wrapped = crate::AppError::internal(DbError::Sqlx(sqlx::Error::PoolTimedOut));
        assert!(!is_retryable_error(&wrapped));
        assert_eq!(Retry::new(0).attempt(), 1);
    }

    #[tokio::test]
    async fn disabled_or_bad_urls_are_clear_errors() {
        let mut config = DatabaseConfig::default();
        config.url = crate::config::SecretString::new("redis://h");
        assert!(matches!(Db::connect(&config).await, Err(DbError::BadUrl)));
        for dialect in Dialect::ALL.iter().copied().filter(|d| !d.is_enabled()) {
            config.url = crate::config::SecretString::new(format!("{}://u:p@127.0.0.1/x", dialect.name()));
            let error = Db::connect(&config).await.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(error.contains("not enabled") || error.contains("no database backend"), "{error}");
            assert!(!error.contains("u:p"), "{error}");
        }
    }
}
