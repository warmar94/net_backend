//! Migrations: plain SQL per dialect, ordered, tracked in a table, namespaced per module, and
//! publishable into the app (the Laravel way).
//!
//! **Sources.** Each module embeds its SQL per dialect ([`Module::migrations`]). The app's own
//! migrations live in `<migrations_dir>/app/<dialect>/<version>_<name>.sql`.
//!
//! **Publishing.** `migrations publish <module>` copies the module's SQL into
//! `<migrations_dir>/<module>/<dialect>/<version>_<name>.sql` (every dialect, or one with
//! `--dialect`); existing files are never overwritten without `--force`. From then on the app **owns** that module's migrations: when the directory
//! `<migrations_dir>/<module>/` exists, its files are used and the embedded ones are ignored, so
//! the app can edit them. Edit before the first `migrate`; an applied migration is checksummed
//! and a later edit is an error (write a new migration instead). When a module upgrade brings new
//! migrations, `migrate` warns until they are published too (publishing again adds only new files).
//!
//! **Order.** Modules in registration order, then `app`; inside a namespace by version. Versions
//! are positive integers, by convention `YYYYMMDDnnnn` (`202610010001`).
//!
//! **Tracking.** The table `nbs_migrations (module, version, name, checksum, applied_at)`; the
//! checksum is the SHA-256 of the SQL with line endings normalised to LF (a CRLF checkout does not
//! count as an edit). `status` only reads (without the table everything is pending).
//!
//! **Transactions.** Each migration runs in a transaction with its tracking row: PostgreSQL and
//! SQLite roll a failed migration back completely. MySQL commits DDL at once, so on MySQL the
//! statements of a migration run one by one ([`split_statements`] rules: `;` outside quotes and
//! comments) and a failure names exactly which statements already took effect, says that the
//! migration is not recorded and how to recover. A migration starting with
//! [`NO_TRANSACTION_MARKER`] runs without a transaction. MySQL `BEGIN … END` bodies (triggers,
//! procedures, events): [`SINGLE_STATEMENT_MARKER`] sends the whole file as one statement;
//! [`STATEMENT_BEGIN_MARKER`] / [`STATEMENT_END_MARKER`] lines mark one such block inside a file
//! ([`mysql_statements`]). On MySQL, once a DDL statement of a failing migration ran, every
//! statement before the failure stays applied (implicit commit, then autocommit).
//!
//! **Concurrency.** MySQL: `GET_LOCK('nbs_migrations:' || SHA1(DATABASE()))` (per database);
//! PostgreSQL: an advisory lock (per database); both polled up to
//! `database.migrate_lock_timeout_secs`, then [`DbError::LockTimeout`]. SQLite: each migration
//! takes the write lock with `BEGIN IMMEDIATE` (retried up to the same limit). Everywhere each
//! migration re-checks its tracking row inside its transaction and is skipped if another process
//! applied it meanwhile.
//!
//! [`Module::migrations`]: crate::Module::migrations

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use net_backend_protocol::UnixMillis;
use sea_query::{ColumnDef, Expr, ExprTrait, Index, Query, Table};
use sha2::{Digest, Sha256};

use crate::db::{Db, DbError, Dialect};
use crate::error::Error;
use crate::module::ModuleSet;

/// The namespace of the app's own migrations (`<migrations_dir>/app/<dialect>/`).
pub const APP_NAMESPACE: &str = "app";

/// The tracking table.
pub const MIGRATIONS_TABLE: &str = "nbs_migrations";

/// One migration: a version, a name and the SQL (one or more statements).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Migration {
    /// A positive version, unique inside its module (`YYYYMMDDnnnn` by convention).
    pub version: i64,
    /// A short name, `[a-z0-9_]+` (it becomes part of the file name).
    pub name: Cow<'static, str>,
    /// The SQL text.
    pub sql: Cow<'static, str>,
}

impl Migration {
    /// A migration.
    pub fn new(version: i64, name: impl Into<Cow<'static, str>>, sql: impl Into<Cow<'static, str>>) -> Self {
        Self { version, name: name.into(), sql: sql.into() }
    }

    /// The SQL with line endings normalised to LF.
    pub fn normalized_sql(&self) -> String {
        normalize(&self.sql)
    }

    /// The checksum stored when applied: SHA-256 (hex) of [`normalized_sql`](Migration::normalized_sql).
    pub fn checksum(&self) -> String {
        let digest = Sha256::digest(self.normalized_sql().as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The file name when published: `<version>_<name>.sql`.
    pub fn file_name(&self) -> String {
        format!("{}_{}.sql", self.version, self.name)
    }
}

fn normalize(sql: &str) -> String {
    sql.replace("\r\n", "\n")
}

/// Where a planned migration comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MigrationSource {
    /// Embedded in the module.
    Embedded,
    /// A file in the app's migrations directory (published or the app's own).
    App,
}

/// The state of one migration in [`status`](crate::PreparedServer::migration_status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MigrationState {
    /// Applied and unchanged.
    Applied,
    /// Not applied yet.
    Pending,
    /// Applied, but the SQL changed since (an error for `migrate`).
    Modified,
    /// Recorded as applied, but no module or file provides it any more.
    Missing,
}

impl fmt::Display for MigrationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MigrationState::Applied => "applied",
            MigrationState::Pending => "pending",
            MigrationState::Modified => "MODIFIED",
            MigrationState::Missing => "missing",
        })
    }
}

/// One line of the migration status.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MigrationStatus {
    /// The module (or `app`).
    pub module: String,
    /// The version.
    pub version: i64,
    /// The name.
    pub name: String,
    /// Where it comes from (`None` for a [`Missing`](MigrationState::Missing) one).
    pub source: Option<MigrationSource>,
    /// Its state.
    pub state: MigrationState,
    /// When it was applied.
    pub applied_at: Option<UnixMillis>,
}

/// What [`migrate`](crate::PreparedServer::migrate) did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MigrateReport {
    /// The applied migrations, in order: `(module, version, name)`.
    pub applied: Vec<(String, i64, String)>,
    /// Warnings (e.g. a published module with newer, unpublished migrations).
    pub warnings: Vec<String>,
}

/// What [`publish`](crate::NetBackendServer::publish_migrations) wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublishReport {
    /// Files written (new, or replaced with `force`).
    pub written: Vec<PathBuf>,
    /// Files already there with the same content.
    pub unchanged: Vec<PathBuf>,
    /// Files already there with other content (the app edited them), left alone (use `force`
    /// to overwrite).
    pub skipped: Vec<PathBuf>,
}

/// One migration in the plan.
#[derive(Clone, Debug)]
pub(crate) struct Planned {
    pub(crate) module: String,
    pub(crate) migration: Migration,
    pub(crate) source: MigrationSource,
}

/// The ordered list of every known migration for one dialect, plus warnings.
#[derive(Clone, Debug, Default)]
pub(crate) struct Plan {
    pub(crate) items: Vec<Planned>,
    pub(crate) warnings: Vec<String>,
}

fn validate_set(namespace: &str, migrations: &mut [Migration]) -> Result<(), Error> {
    migrations.sort_by_key(|m| m.version);
    let mut problems = Vec::new();
    let mut seen = HashSet::new();
    for m in migrations.iter() {
        if m.version <= 0 {
            problems.push(format!("{namespace}: version {} must be positive", m.version));
        }
        if !seen.insert(m.version) {
            problems.push(format!("{namespace}: version {} appears twice", m.version));
        }
        if m.name.is_empty() || m.name.len() > 200 || !m.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            problems.push(format!("{namespace}: migration {} has an invalid name `{}` (expected [a-z0-9_]+)", m.version, m.name));
        }
        if m.sql.trim().is_empty() {
            problems.push(format!("{namespace}: migration {} is empty", m.version));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::Migration(problems.join("; ")))
    }
}

/// Parse `<version>_<name>.sql`.
fn parse_file_name(file_name: &str) -> Option<(i64, String)> {
    let stem = file_name.strip_suffix(".sql")?;
    let (version, name) = stem.split_once('_')?;
    if version.is_empty() || version.len() > 18 || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((version.parse().ok()?, name.to_string()))
}

/// Read every `*.sql` file of a directory as migrations (other files are ignored).
fn read_dir_migrations(namespace: &str, dir: &Path) -> Result<Vec<Migration>, Error> {
    let entries = std::fs::read_dir(dir).map_err(|e| Error::io(format!("reading {}", dir.display()), e))?;
    let mut migrations = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(format!("reading {}", dir.display()), e))?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        if !file_name.ends_with(".sql") || !path.is_file() {
            continue;
        }
        let (version, name) =
            parse_file_name(&file_name).ok_or_else(|| Error::Migration(format!("{namespace}: `{}` is not named <version>_<name>.sql", path.display())))?;
        let sql = std::fs::read_to_string(&path).map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
        migrations.push(Migration::new(version, name, sql));
    }
    validate_set(namespace, &mut migrations)?;
    Ok(migrations)
}

/// Build the plan for one dialect: modules in registration order (embedded, or the app's copy
/// when published), then the app's own.
pub(crate) fn plan(modules: &ModuleSet, dialect: Dialect, dir: &Path) -> Result<Plan, Error> {
    let mut plan = Plan::default();
    for module in modules.iter() {
        let name = module.name();
        let mut embedded = module.migrations(dialect);
        validate_set(name, &mut embedded)?;
        let published = dir.join(name);
        let (migrations, source) = if published.is_dir() {
            let dialect_dir = published.join(dialect.name());
            if !dialect_dir.is_dir() {
                return Err(Error::Migration(format!(
                    "module `{name}` is published to {} but has no `{}` directory: run `migrations publish {name} --dialect {}`",
                    published.display(),
                    dialect.name(),
                    dialect.name()
                )));
            }
            let owned = read_dir_migrations(name, &dialect_dir)?;
            let known: HashSet<i64> = owned.iter().map(|m| m.version).collect();
            let newer: Vec<String> = embedded.iter().filter(|m| !known.contains(&m.version)).map(|m| m.file_name()).collect();
            if !newer.is_empty() {
                plan.warnings
                    .push(format!("module `{name}` has migrations that are not published ({}): run `migrations publish {name}` to add them", newer.join(", ")));
            }
            (owned, MigrationSource::App)
        } else {
            (embedded, MigrationSource::Embedded)
        };
        plan.items.extend(migrations.into_iter().map(|migration| Planned { module: name.to_string(), migration, source }));
    }
    let app_dir = dir.join(APP_NAMESPACE).join(dialect.name());
    if app_dir.is_dir() {
        let own = read_dir_migrations(APP_NAMESPACE, &app_dir)?;
        plan.items.extend(own.into_iter().map(|migration| Planned { module: APP_NAMESPACE.to_string(), migration, source: MigrationSource::App }));
    }
    Ok(plan)
}

#[derive(sqlx::FromRow)]
struct AppliedRow {
    module: String,
    version: i64,
    name: String,
    checksum: String,
    applied_at: i64,
}

async fn ensure_table(db: &Db) -> Result<(), Error> {
    let mut table = Table::create();
    table
        .table(MIGRATIONS_TABLE)
        .if_not_exists()
        .col(ColumnDef::new("module").string_len(64).not_null())
        .col(ColumnDef::new("version").big_integer().not_null())
        .col(ColumnDef::new("name").string_len(255).not_null())
        .col(ColumnDef::new("checksum").string_len(64).not_null())
        .col(ColumnDef::new("applied_at").big_integer().not_null())
        .primary_key(Index::create().col("module").col("version"));
    let sql = crate::db::schema::render_table(&table, db.dialect());
    db.execute_script(sql).await?;
    Ok(())
}

async fn applied(db: &Db) -> Result<BTreeMap<(String, i64), AppliedRow>, Error> {
    let select = Query::select().columns(["module", "version", "name", "checksum", "applied_at"]).from(MIGRATIONS_TABLE).to_owned();
    let rows: Vec<AppliedRow> = db.fetch_all(&select).await?;
    Ok(rows.into_iter().map(|r| ((r.module.clone(), r.version), r)).collect())
}

/// The status of every known and recorded migration. Read-only: without a tracking table every
/// migration is pending (the table is created by the first `migrate`, under the lock).
pub(crate) async fn status(db: &Db, plan: &Plan) -> Result<Vec<MigrationStatus>, Error> {
    let mut applied = if db.table_exists(MIGRATIONS_TABLE).await? { applied(db).await? } else { BTreeMap::new() };
    let mut out = Vec::new();
    for item in &plan.items {
        let row = applied.remove(&(item.module.clone(), item.migration.version));
        let state = match &row {
            None => MigrationState::Pending,
            Some(row) if row.checksum == item.migration.checksum() => MigrationState::Applied,
            Some(_) => MigrationState::Modified,
        };
        out.push(MigrationStatus {
            module: item.module.clone(),
            version: item.migration.version,
            name: item.migration.name.to_string(),
            source: Some(item.source),
            state,
            applied_at: row.map(|r| UnixMillis(r.applied_at)),
        });
    }
    for ((module, version), row) in applied {
        out.push(MigrationStatus {
            module,
            version,
            name: row.name,
            source: None,
            state: MigrationState::Missing,
            applied_at: Some(UnixMillis(row.applied_at)),
        });
    }
    Ok(out)
}

/// The marker (first line of a migration) that runs it outside a transaction, e.g. for
/// PostgreSQL's `CREATE INDEX CONCURRENTLY`.
pub const NO_TRANSACTION_MARKER: &str = "-- nbs:no-transaction";

/// The marker (in the leading comment lines of a migration) that sends the whole file to the
/// database as ONE statement, unsplit: a MySQL trigger, procedure or event with a `BEGIN … END`
/// body. MySQL's client-side `DELIMITER` command is not SQL and is not supported.
pub const SINGLE_STATEMENT_MARKER: &str = "-- nbs:single-statement";

/// A line that starts a block sent as ONE statement (until [`STATEMENT_END_MARKER`]), for a file
/// that mixes a `BEGIN … END` body with ordinary statements.
pub const STATEMENT_BEGIN_MARKER: &str = "-- nbs:statement-begin";

/// The line that ends a [`STATEMENT_BEGIN_MARKER`] block.
pub const STATEMENT_END_MARKER: &str = "-- nbs:statement-end";

/// Whether one of the leading comment lines (before the first SQL line) is `marker`.
fn has_directive(sql: &str, marker: &str) -> bool {
    sql.lines().map(str::trim).filter(|l| !l.is_empty()).take_while(|l| l.starts_with("--")).any(|l| l.eq_ignore_ascii_case(marker))
}

fn wants_no_transaction(sql: &str) -> bool {
    has_directive(sql, NO_TRANSACTION_MARKER)
}

/// The statements of a MySQL migration: the whole file with [`SINGLE_STATEMENT_MARKER`], else
/// [`split_statements`] outside `statement-begin` / `statement-end` blocks, each block as one
/// statement (an unclosed block runs to the end of the file).
pub fn mysql_statements(sql: &str) -> Vec<String> {
    if has_directive(sql, SINGLE_STATEMENT_MARKER) {
        return if has_code(sql) { vec![sql.trim().to_string()] } else { Vec::new() };
    }
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut block: Option<String> = None;
    for line in sql.lines() {
        let marker = line.trim();
        match block.as_mut() {
            Some(body) if marker.eq_ignore_ascii_case(STATEMENT_END_MARKER) => {
                let body = std::mem::take(body);
                if has_code(&body) {
                    out.push(body.trim().to_string());
                }
                block = None;
            }
            Some(body) => {
                body.push_str(line);
                body.push('\n');
            }
            None if marker.eq_ignore_ascii_case(STATEMENT_BEGIN_MARKER) => {
                out.extend(split_statements(&std::mem::take(&mut plain)));
                block = Some(String::new());
            }
            None => {
                plain.push_str(line);
                plain.push('\n');
            }
        }
    }
    if let Some(body) = block {
        if has_code(&body) {
            out.push(body.trim().to_string());
        }
    }
    out.extend(split_statements(&plain));
    out
}

/// Apply every pending migration in plan order, under the migrations lock (waiting at most
/// `lock_wait` for another process).
pub(crate) async fn run(db: &Db, plan: &Plan, now: impl Fn() -> UnixMillis, lock_wait: Duration) -> Result<MigrateReport, Error> {
    let lock = db.lock_migrations(lock_wait).await.map_err(|e| Error::Migration(e.describe()))?;
    let result = run_locked(db, plan, now, lock_wait).await;
    lock.release().await;
    result
}

#[derive(sqlx::FromRow)]
struct ChecksumRow {
    checksum: String,
}

async fn run_locked(db: &Db, plan: &Plan, now: impl Fn() -> UnixMillis, lock_wait: Duration) -> Result<MigrateReport, Error> {
    // Under the lock: concurrent `CREATE TABLE IF NOT EXISTS` can still collide on PostgreSQL.
    ensure_table(db).await?;
    let applied = applied(db).await?;
    let modified: Vec<String> = plan
        .items
        .iter()
        .filter(|item| applied.get(&(item.module.clone(), item.migration.version)).is_some_and(|row| row.checksum != item.migration.checksum()))
        .map(|item| format!("{}/{}", item.module, item.migration.file_name()))
        .collect();
    if !modified.is_empty() {
        return Err(Error::Migration(format!(
            "applied migrations were changed afterwards: {} (restore them and add a new migration instead)",
            modified.join(", ")
        )));
    }
    let mut report = MigrateReport { applied: Vec::new(), warnings: plan.warnings.clone() };
    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }
    let dialect = db.dialect();
    for item in plan.items.iter().filter(|item| !applied.contains_key(&(item.module.clone(), item.migration.version))) {
        let m = &item.migration;
        let file = format!("{}/{}", item.module, m.file_name());
        let mut insert = Query::insert();
        insert
            .into_table(MIGRATIONS_TABLE)
            .columns(["module", "version", "name", "checksum", "applied_at"])
            .values([Expr::val(item.module.clone()), Expr::val(m.version), Expr::val(m.name.to_string()), Expr::val(m.checksum()), Expr::val(now().get())])
            .map_err(|e| Error::Migration(format!("building the tracking row: {e}")))?;
        let recheck = Query::select()
            .column("checksum")
            .from(MIGRATIONS_TABLE)
            .and_where(Expr::col("module").eq(item.module.clone()))
            .and_where(Expr::col("version").eq(m.version))
            .to_owned();
        let sql = m.normalized_sql();
        // MySQL commits DDL at once, so the statements run one by one: on a failure the error
        // names exactly which ones already took effect.
        let statements = if dialect == Dialect::MySql { mysql_statements(&sql) } else { vec![sql.clone()] };

        if wants_no_transaction(&sql) {
            if let Some(index) = run_statements_on_pool(db, &statements).await.err() {
                return Err(failure(&file, dialect, &statements, index, false));
            }
            db.execute(&insert).await.map_err(|e| Error::Migration(format!("{file}: applied, but recording it failed: {}", e.describe())))?;
        } else {
            let mut tx = db.begin_migration(lock_wait).await.map_err(|e| Error::Migration(format!("{file}: {}", e.describe())))?;
            // Another process may have applied it meanwhile (SQLite has no session lock; this is
            // also a cheap guard everywhere else).
            let done: Option<ChecksumRow> = tx.fetch_optional(&recheck).await?;
            if let Some(row) = done {
                let _ = tx.rollback().await;
                if row.checksum != m.checksum() {
                    return Err(Error::Migration(format!("{file} was applied by another process with different SQL")));
                }
                continue;
            }
            let mut failed = None;
            for (index, statement) in statements.iter().enumerate() {
                if let Err(error) = tx.execute_script(statement.clone()).await {
                    failed = Some((index, error));
                    break;
                }
            }
            if let Some((index, error)) = failed {
                let _ = tx.rollback().await;
                return Err(failure_with(&file, dialect, &statements, index, &error, true));
            }
            if let Err(error) = tx.execute(&insert).await {
                let _ = tx.rollback().await;
                return Err(Error::Migration(format!("{file}: recording it failed: {}", error.describe())));
            }
            tx.commit().await.map_err(|e| Error::Migration(format!("{file}: commit failed: {}", e.describe())))?;
        }
        tracing::info!(module = %item.module, version = m.version, name = %m.name, ">>> NBS: migration applied");
        report.applied.push((item.module.clone(), m.version, m.name.to_string()));
    }
    Ok(report)
}

/// Run statements outside a transaction; the index of the failing one with its error.
async fn run_statements_on_pool(db: &Db, statements: &[String]) -> Result<(), (usize, DbError)> {
    for (index, statement) in statements.iter().enumerate() {
        db.execute_script(statement.clone()).await.map_err(|e| (index, e))?;
    }
    Ok(())
}

fn failure(file: &str, dialect: Dialect, statements: &[String], (index, error): (usize, DbError), in_tx: bool) -> Error {
    failure_with(file, dialect, statements, index, &error, in_tx)
}

/// How many of the statements that ran before a MySQL failure stay applied. Without a transaction:
/// all of them. In a transaction: none if only DML ran (rolled back); but once a DDL statement ran,
/// MySQL committed implicitly and left the transaction, so every later statement autocommitted
/// too: then all of them.
pub(crate) fn mysql_statements_kept(before: &[String], in_tx: bool) -> usize {
    if !in_tx || before.iter().any(|s| is_mysql_ddl(s)) {
        before.len()
    } else {
        0
    }
}

/// Statements that MySQL commits implicitly.
fn is_mysql_ddl(statement: &str) -> bool {
    let first = statement.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
    matches!(first.as_str(), "CREATE" | "ALTER" | "DROP" | "RENAME" | "TRUNCATE")
}

fn first_line(statement: &str) -> String {
    let line = statement.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("--")).unwrap_or_default();
    let mut short: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        short.push('…');
    }
    short
}

/// The error for a failed migration: what failed, what is left behind, how to recover.
fn failure_with(file: &str, dialect: Dialect, statements: &[String], index: usize, error: &DbError, in_tx: bool) -> Error {
    let total = statements.len();
    let cause = error.describe();
    if dialect != Dialect::MySql {
        let left = if in_tx {
            "nothing of it was applied (rolled back)".to_string()
        } else {
            "it ran without a transaction (no-transaction marker): undo what it did by hand if needed".to_string()
        };
        return Error::Migration(format!("{file} failed: {cause}; {left}; fix the file and run `migrate` again"));
    }
    let position = format!("statement {} of {total}", index + 1);
    let before = &statements[..index.min(total)];
    let kept = mysql_statements_kept(before, in_tx);
    if kept == 0 {
        return Error::Migration(format!("{file} failed at {position}: {cause}; nothing of it was applied; fix the file and run `migrate` again"));
    }
    let list: Vec<String> = before[..kept].iter().enumerate().map(|(i, s)| format!("  {}. {}", i + 1, first_line(s))).collect();
    Error::Migration(format!(
        "{file} failed at {position}: {cause}\nMySQL commits DDL at once: these statements of it stay applied:\n{}\nThe migration is NOT recorded. To recover, either undo them by hand, or delete them from the file \
         (they are applied) and fix the failing statement; then run `migrate` again. Prefer one DDL statement per MySQL migration.",
        list.join("\n")
    ))
}

/// Split SQL text into statements at `;` outside quotes and comments (`'…'`, `"…"`, `` `…` ``,
/// `-- …`, `# …`, `/* … */`, PostgreSQL `$tag$…$tag$`). Empty and comment-only pieces are dropped.
pub fn split_statements(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut current = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                current.push(c);
                i += 1;
                while i < chars.len() {
                    let d = chars[i];
                    current.push(d);
                    i += 1;
                    if d == '\\' && c != '`' && i < chars.len() {
                        current.push(chars[i]);
                        i += 1;
                    } else if d == c {
                        if i < chars.len() && chars[i] == c {
                            current.push(c);
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    current.push(chars[i]);
                    i += 1;
                }
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    current.push(chars[i]);
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                current.push_str("/*");
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    current.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    current.push_str("*/");
                    i += 2;
                }
            }
            '$' => {
                // A dollar-quote tag: `$$` or `$name$`.
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '$' {
                    let tag: String = chars[i..=j].iter().collect();
                    current.push_str(&tag);
                    i = j + 1;
                    let rest: String = chars[i..].iter().collect();
                    match rest.find(&tag) {
                        Some(end) => {
                            current.push_str(&rest[..end + tag.len()]);
                            i += rest[..end + tag.len()].chars().count();
                        }
                        None => {
                            current.push_str(&rest);
                            i = chars.len();
                        }
                    }
                } else {
                    current.push(c);
                    i += 1;
                }
            }
            ';' => {
                out.push(std::mem::take(&mut current));
                i += 1;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }
    out.push(current);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| has_code(s)).collect()
}

/// Whether a piece holds more than whitespace and comments.
fn has_code(piece: &str) -> bool {
    let mut rest = piece.trim();
    loop {
        if rest.is_empty() {
            return false;
        }
        if rest.starts_with("--") || rest.starts_with('#') {
            rest = rest.split_once('\n').map_or("", |(_, r)| r).trim();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim();
        } else {
            return true;
        }
    }
}

/// Copy a module's migrations into `<dir>/<module>/<dialect>/`. A file already there is never
/// overwritten unless `force`: byte-identical (after line-ending normalisation) counts as
/// unchanged, anything else (an edit, a non-UTF-8 file) as skipped; a file that cannot be read
/// is an error. Publishing again after a module upgrade therefore adds only the new files.
pub(crate) fn publish(modules: &ModuleSet, module: &str, dir: &Path, dialects: &[Dialect], force: bool) -> Result<PublishReport, Error> {
    let Some(found) = modules.get(module) else {
        let known = modules.names().join(", ");
        return Err(Error::Migration(format!("no module `{module}` is registered (registered: {known})")));
    };
    let mut files = Vec::new();
    for dialect in dialects {
        let mut migrations = found.migrations(*dialect);
        validate_set(module, &mut migrations)?;
        for m in migrations {
            files.push((dir.join(module).join(dialect.name()).join(m.file_name()), m.normalized_sql()));
        }
    }
    if files.is_empty() {
        return Err(Error::Migration(format!("module `{module}` has no migrations to publish")));
    }
    let mut report = PublishReport::default();
    for (path, sql) in files {
        match std::fs::read(&path) {
            Ok(existing) => {
                let same = String::from_utf8(existing).is_ok_and(|text| normalize(&text) == sql);
                if same {
                    report.unchanged.push(path);
                    continue;
                }
                if !force {
                    report.skipped.push(path);
                    continue;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(format!("reading {}", path.display()), error)),
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(format!("creating {}", parent.display()), e))?;
        }
        std::fs::write(&path, sql.as_bytes()).map_err(|e| Error::io(format!("writing {}", path.display()), e))?;
        report.written.push(path);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_ignores_line_endings() {
        let lf = Migration::new(1, "a", "CREATE TABLE a (x BIGINT);\nCREATE TABLE b (y BIGINT);\n");
        let crlf = Migration::new(1, "a", "CREATE TABLE a (x BIGINT);\r\nCREATE TABLE b (y BIGINT);\r\n");
        assert_eq!(lf.checksum(), crlf.checksum());
        assert_eq!(lf.checksum().len(), 64);
        assert_ne!(lf.checksum(), Migration::new(1, "a", "CREATE TABLE a (x INT);").checksum());
        assert_eq!(lf.file_name(), "1_a.sql");
    }

    #[test]
    fn file_names() {
        assert_eq!(parse_file_name("202610010001_create_users.sql"), Some((202610010001, "create_users".into())));
        assert_eq!(parse_file_name("1_x.sql"), Some((1, "x".into())));
        assert_eq!(parse_file_name("create.sql"), None);
        assert_eq!(parse_file_name("1x_y.sql"), None);
        assert_eq!(parse_file_name("1_y.txt"), None);
        assert_eq!(parse_file_name("1234567890123456789_y.sql"), None);
    }

    #[test]
    fn set_validation() {
        let mut ok = vec![Migration::new(2, "b", "x"), Migration::new(1, "a", "x")];
        assert!(validate_set("m", &mut ok).is_ok());
        assert_eq!(ok[0].version, 1, "sorted by version");
        let mut bad = vec![Migration::new(1, "a", "x"), Migration::new(1, "B-x", " "), Migration::new(0, "c", "x")];
        let error = validate_set("m", &mut bad).err().map(|e| e.to_string()).unwrap_or_default();
        for needle in ["appears twice", "invalid name", "is empty", "must be positive"] {
            assert!(error.contains(needle), "{needle}: {error}");
        }
    }

    #[test]
    fn statements_split_outside_quotes_and_comments() {
        let sql = "CREATE TABLE a (x VARCHAR(5) DEFAULT 'a;b');
-- note; here
INSERT INTO a VALUES ('it''s;'); /* c; */ # x;

CREATE TABLE `we;ird` (y INT);
-- only a comment;
";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(parts[0].ends_with("'a;b')"));
        assert!(parts[1].contains("'it''s;'"));
        assert!(parts[2].contains("`we;ird`"));
        assert_eq!(split_statements("SELECT $$a;b$$; SELECT $t$c;$t$"), ["SELECT $$a;b$$", "SELECT $t$c;$t$"]);
        assert_eq!(
            split_statements(
                "  ;; -- x
"
            ),
            Vec::<String>::new()
        );
        assert_eq!(split_statements("SELECT 1"), ["SELECT 1"]);
    }

    #[test]
    fn failure_messages_say_what_is_left() {
        let error = DbError::Build("Table 'half_a' already exists".into());
        let stmts: Vec<String> = ["CREATE TABLE half_a (id BIGINT)", "INSERT INTO t VALUES (1)", "CREATE TABLE half_a (id BIGINT)"].map(String::from).to_vec();
        let mysql = failure_with("app/2_bad.sql", Dialect::MySql, &stmts, 2, &error, true).to_string();
        assert!(mysql.contains("statement 3 of 3") && mysql.contains("1. CREATE TABLE half_a") && mysql.contains("2. INSERT"), "{mysql}");
        assert!(mysql.contains("NOT recorded") && mysql.contains("run `migrate` again"), "{mysql}");
        let first = failure_with("app/2_bad.sql", Dialect::MySql, &stmts, 0, &error, true).to_string();
        assert!(first.contains("nothing of it was applied"), "{first}");
        let dml_only: Vec<String> = ["INSERT INTO t VALUES (1)", "INSERT INTO t VALUES (x)"].map(String::from).to_vec();
        assert!(failure_with("f", Dialect::MySql, &dml_only, 1, &error, true).to_string().contains("nothing of it was applied"));
        assert!(failure_with("f", Dialect::MySql, &dml_only, 1, &error, false).to_string().contains("1. INSERT"));
        let pg = failure_with("app/2_bad.sql", Dialect::Postgres, &stmts, 0, &error, true).to_string();
        assert!(pg.contains("rolled back"), "{pg}");
        assert!(wants_no_transaction(
            "
-- nbs:no-transaction
CREATE INDEX CONCURRENTLY i ON t (x)"
        ));
        assert!(!wants_no_transaction(
            "CREATE TABLE t (x INT)
-- nbs:no-transaction"
        ));
    }

    #[test]
    fn mysql_kept_statements_after_implicit_commit() {
        let v = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // DDL then DML: the DDL committed, the INSERT autocommitted after it (the live repro).
        assert_eq!(mysql_statements_kept(&v(&["CREATE TABLE m4 (id BIGINT PRIMARY KEY)", "INSERT INTO m4 VALUES (1)"]), true), 2);
        // DML then DDL: the DDL committed the DML before it.
        assert_eq!(mysql_statements_kept(&v(&["INSERT INTO t VALUES (1)", "ALTER TABLE t ADD c INT"]), true), 2);
        // Only DML in a transaction: rolled back.
        assert_eq!(mysql_statements_kept(&v(&["INSERT INTO t VALUES (1)", "UPDATE t SET c = 2"]), true), 0);
        // Without a transaction: everything that ran.
        assert_eq!(mysql_statements_kept(&v(&["INSERT INTO t VALUES (1)"]), false), 1);
        assert_eq!(mysql_statements_kept(&[], true), 0);
    }

    #[test]
    fn mysql_begin_end_bodies_stay_whole() {
        let trigger = "-- nbs:single-statement\nCREATE TRIGGER t_bi BEFORE INSERT ON t FOR EACH ROW\nBEGIN\n  SET NEW.a = 1;\n  SET NEW.b = 2;\nEND\n";
        let parts = mysql_statements(trigger);
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert!(parts[0].contains("SET NEW.a = 1;") && parts[0].ends_with("END"));
        // A block inside a file with ordinary statements around it.
        let mixed = "CREATE TABLE t (a INT, b INT);\n-- nbs:statement-begin\nCREATE PROCEDURE p()\nBEGIN\n  INSERT INTO t VALUES (1, 2);\n  SELECT COUNT(*) FROM t;\nEND\n-- nbs:statement-end\nINSERT INTO t VALUES (3, 4);\n";
        let parts = mysql_statements(mixed);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(parts[0].starts_with("CREATE TABLE t"));
        assert!(parts[1].starts_with("CREATE PROCEDURE p()") && parts[1].contains("SELECT COUNT(*) FROM t;") && parts[1].ends_with("END"));
        assert_eq!(parts[2], "INSERT INTO t VALUES (3, 4)");
        // Without a directive the splitter would cut the body (documented).
        assert!(mysql_statements("CREATE TRIGGER x BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; END").len() > 1);
        // Directives are read only from the leading comment lines.
        assert!(has_directive("-- note\n-- nbs:no-transaction\nCREATE INDEX i ON t (a)", NO_TRANSACTION_MARKER));
        assert!(!has_directive("CREATE TABLE t (a INT);\n-- nbs:single-statement", SINGLE_STATEMENT_MARKER));
        assert!(mysql_statements("-- nbs:single-statement\n-- only comments\n").is_empty());
    }
}
