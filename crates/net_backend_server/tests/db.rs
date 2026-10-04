//! The database layer and migrations against a real database: in-memory / file SQLite always
//! (when compiled in); MySQL and PostgreSQL with `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL`
//! (`cargo test -- --ignored`; CI runs them against service containers). The same suite runs on
//! every backend.
#![cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]

mod common;

use std::path::Path;

use net_backend_server::db::schema;
use net_backend_server::migrate::{MigrationSource, MigrationState};
use net_backend_server::sea_query::{Expr, ExprTrait, Query};
use net_backend_server::{Config, Db, Dialect, Error, Migration, Module, NetBackendServer, PreparedServer, SecretString};

/// A module with two migrations (given out of order) creating `<prefix>_items` and an index.
struct Items {
    name: &'static str,
    table: &'static str,
    extra: bool,
}

impl Module for Items {
    fn name(&self) -> &'static str {
        self.name
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        let mut table = schema::create_table(self.table);
        table
            .col(schema::id("id"))
            .col(schema::foreign_id("owner_id"))
            .col(schema::string("label", 64))
            .col(schema::bytes("payload", dialect))
            .col(schema::unix_millis("created_at"));
        let index = schema::create_index(Box::leak(format!("{}_label", self.table).into_boxed_str()), self.table, &["label"]);
        let mut unique = index.clone();
        unique.unique();
        let mut list = vec![
            Migration::new(202610010002, "index_label", schema::render_index(&unique, dialect)),
            Migration::new(202610010001, "create_items", schema::render_table(&table, dialect)),
        ];
        if self.extra {
            list.push(Migration::new(202610010003, "add_note", format!("ALTER TABLE {} ADD COLUMN note VARCHAR(20)", self.table)));
        }
        list
    }
}

/// A second module, registered first, with one migration.
struct Audit;

impl Module for Audit {
    fn name(&self) -> &'static str {
        "audit"
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        let mut table = schema::create_table("audit_rows");
        table.col(schema::id("id")).col(schema::unix_millis("at"));
        vec![Migration::new(1, "create_audit_rows", schema::render_table(&table, dialect))]
    }
}

#[derive(sqlx::FromRow, Debug, PartialEq)]
struct ItemRow {
    id: i64,
    owner_id: i64,
    label: String,
    payload: Vec<u8>,
    created_at: i64,
}

async fn prepare(url: &str, dir: &Path, extra: bool) -> PreparedServer {
    prepare_with(url, dir, extra, 60).await
}

async fn prepare_with(url: &str, dir: &Path, extra: bool, lock_timeout_secs: u64) -> PreparedServer {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = dir.to_path_buf();
    config.database.max_connections = 4;
    config.database.migrate_lock_timeout_secs = lock_timeout_secs;
    NetBackendServer::new(config).module(Audit).module(Items { name: "items", table: "items", extra }).build().await.expect("build")
}

/// The whole suite on one database URL (a fresh, empty database).
async fn suite(url: &str) {
    let dir = common::temp_dir("db-suite");

    // Ordering: modules in registration order, versions ascending, then the app's own.
    let app_dir = dir.join("app").join(Dialect::from_url(url).expect("dialect").name());
    std::fs::create_dir_all(&app_dir).expect("app dir");
    std::fs::write(app_dir.join("5_app_settings.sql"), "CREATE TABLE app_settings (k VARCHAR(50) NOT NULL PRIMARY KEY, v VARCHAR(200) NOT NULL)")
        .expect("write");
    std::fs::write(app_dir.join("README.txt"), "not a migration").expect("write");
    let server = prepare(url, &dir, false).await;
    let status = server.migration_status().await.expect("status");
    assert!(status.iter().all(|s| s.state == MigrationState::Pending), "{status:?}");
    // `status` is read-only: it did not create the tracking table.
    assert!(server.state().db().execute_script("SELECT * FROM nbs_migrations").await.is_err());
    let report = server.migrate().await.expect("migrate");
    let applied: Vec<String> = report.applied.iter().map(|(m, v, n)| format!("{m}/{v}_{n}")).collect();
    assert_eq!(applied, ["audit/1_create_audit_rows", "items/202610010001_create_items", "items/202610010002_index_label", "app/5_app_settings"]);
    assert!(report.warnings.is_empty());

    // Idempotent.
    assert!(server.migrate().await.expect("again").applied.is_empty());
    let status = server.migration_status().await.expect("status");
    assert_eq!(status.len(), 4);
    assert!(status.iter().all(|s| s.state == MigrationState::Applied && s.applied_at.is_some()), "{status:?}");
    assert_eq!(status[3].source, Some(MigrationSource::App));
    assert_eq!(status[1].source, Some(MigrationSource::Embedded));

    // The DB layer on the migrated tables: insert_id, fetch, update, unique violation, transactions.
    let db = server.state().db().clone();
    db_layer(&db).await;

    // Editing an applied migration is detected and refused.
    std::fs::write(app_dir.join("5_app_settings.sql"), "CREATE TABLE app_settings (k VARCHAR(51) NOT NULL PRIMARY KEY)").expect("write");
    let status = server.migration_status().await.expect("status");
    assert_eq!(status[3].state, MigrationState::Modified);
    let error = server.migrate().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("app/5_app_settings.sql") && error.contains("changed"), "{error}");
    // CRLF line endings are not an edit.
    std::fs::write(app_dir.join("5_app_settings.sql"), "CREATE TABLE app_settings (k VARCHAR(50) NOT NULL PRIMARY KEY, v VARCHAR(200) NOT NULL)")
        .expect("write");
    let crlf = Migration::new(5, "x", "a\r\nb");
    assert_eq!(crlf.checksum(), Migration::new(5, "x", "a\nb").checksum());

    // A failing multi-statement migration: what is left behind is stated exactly, and the
    // documented recovery works. PostgreSQL / SQLite roll it back; MySQL keeps the committed DDL.
    let dialect = Dialect::from_url(url).expect("dialect");
    std::fs::write(app_dir.join("6_two_tables.sql"), "CREATE TABLE half_a (id BIGINT NOT NULL);\nCREATE TABLE half_a (id BIGINT NOT NULL)").expect("write");
    let error = server.migrate().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("app/6_two_tables.sql"), "{error}");
    let half_a_exists = server.state().db().execute_script("SELECT * FROM half_a").await.is_ok();
    let status = server.migration_status().await.expect("status");
    assert_eq!(status.iter().find(|s| s.name == "two_tables").map(|s| s.state), Some(MigrationState::Pending));
    if dialect == Dialect::MySql {
        assert!(error.contains("statement 2 of 2") && error.contains("1. CREATE TABLE half_a") && error.contains("NOT recorded"), "{error}");
        assert!(half_a_exists, "MySQL commits the first CREATE TABLE");
        // Recovery as the message says: delete the applied statement, fix the failing one.
        std::fs::write(app_dir.join("6_two_tables.sql"), "CREATE TABLE half_b (id BIGINT NOT NULL)").expect("write");
    } else {
        assert!(error.contains("rolled back"), "{error}");
        assert!(!half_a_exists, "rolled back completely");
        std::fs::write(app_dir.join("6_two_tables.sql"), "CREATE TABLE half_a (id BIGINT NOT NULL);\nCREATE TABLE half_b (id BIGINT NOT NULL)").expect("write");
    }
    assert_eq!(server.migrate().await.expect("recovered").applied.len(), 1);
    server.state().db().execute_script("SELECT * FROM half_a").await.expect("half_a");
    server.state().db().execute_script("SELECT * FROM half_b").await.expect("half_b");

    // A module upgrade with a new migration: pending, then applied.
    server.state().db().close().await;
    let server = prepare(url, &dir, true).await;
    let status = server.migration_status().await.expect("status");
    assert_eq!(status.iter().filter(|s| s.state == MigrationState::Pending).count(), 1, "{status:?}");
    assert_eq!(server.migrate().await.expect("upgrade").applied.len(), 1);

    // A module that is no longer registered: its rows show as missing.
    server.state().db().close().await;
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = dir.clone();
    let without_audit = NetBackendServer::new(config).module(Items { name: "items", table: "items", extra: true }).build().await.expect("build");
    let status = without_audit.migration_status().await.expect("status");
    assert!(status.iter().any(|s| s.module == "audit" && s.state == MigrationState::Missing && s.source.is_none()), "{status:?}");
    without_audit.state().db().close().await;
}

async fn db_layer(db: &Db) {
    let insert = |label: &str, owner: i64| {
        Query::insert()
            .into_table("items")
            .columns(["owner_id", "label", "payload", "created_at"])
            .values_panic([owner.into(), label.into(), vec![0u8, 159, 146, 150].into(), 1_790_000_000_000i64.into()])
            .to_owned()
    };
    let first = db.insert_id(&insert("sword", 7), "id").await.expect("insert");
    let second = db.insert_id(&insert("shield", 7), "id").await.expect("insert");
    assert!(first > 0 && second > first, "{first} {second}");

    let select = Query::select()
        .columns(["id", "owner_id", "label", "payload", "created_at"])
        .from("items")
        .and_where(Expr::col("owner_id").eq(7))
        .order_by("id", net_backend_server::sea_query::Order::Asc)
        .to_owned();
    let rows: Vec<ItemRow> = db.fetch_all(&select).await.expect("fetch");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], ItemRow { id: first, owner_id: 7, label: "sword".into(), payload: vec![0, 159, 146, 150], created_at: 1_790_000_000_000 });

    let update = Query::update().table("items").value("label", "axe").and_where(Expr::col("id").eq(first)).to_owned();
    assert_eq!(db.execute(&update).await.expect("update"), 1);
    let one: Option<ItemRow> = db.fetch_optional(&select.clone().and_where(Expr::col("id").eq(first)).to_owned()).await.expect("fetch");
    assert_eq!(one.map(|r| r.label).as_deref(), Some("axe"));
    let none: Option<ItemRow> = db.fetch_optional(&select.clone().and_where(Expr::col("id").eq(-1)).to_owned()).await.expect("fetch");
    assert!(none.is_none());

    // The unique index (from the second migration) is portable.
    let duplicate = db.execute(&insert("shield", 8)).await.expect_err("duplicate label");
    assert!(duplicate.is_unique_violation(), "{duplicate}");
    // Unique indexes are case-sensitive on every backend (MySQL: utf8mb4_bin).
    db.execute(&insert("Shield", 8)).await.expect("`Shield` differs from `shield`");

    // An unsigned value above i64::MAX is refused, never a panic.
    let huge = Query::insert()
        .into_table("items")
        .columns(["owner_id", "label", "payload", "created_at"])
        .values_panic([u64::MAX.into(), "huge".into(), Vec::<u8>::new().into(), 0i64.into()])
        .to_owned();
    assert!(matches!(db.execute(&huge).await, Err(net_backend_server::DbError::Build(m)) if m.contains("does not fit")));

    // insert_id / fetch_one inside a transaction.
    let mut tx = db.begin().await.expect("begin");
    let id = tx.insert_id(&insert("lance", 10), "id").await.expect("insert_id in tx");
    let row: ItemRow = tx
        .fetch_one(&Query::select().columns(["id", "owner_id", "label", "payload", "created_at"]).from("items").and_where(Expr::col("id").eq(id)).to_owned())
        .await
        .expect("fetch_one in tx");
    assert_eq!((row.id, row.label.as_str()), (id, "lance"));
    tx.commit().await.expect("commit");
    let one: ItemRow = db
        .fetch_one(&Query::select().columns(["id", "owner_id", "label", "payload", "created_at"]).from("items").and_where(Expr::col("id").eq(id)).to_owned())
        .await
        .expect("fetch_one");
    assert_eq!(one.owner_id, 10);

    // Transactions: rollback discards, commit keeps.
    let mut tx = db.begin().await.expect("begin");
    tx.execute(&insert("bow", 9)).await.expect("insert in tx");
    tx.rollback().await.expect("rollback");
    let mut tx = db.begin().await.expect("begin");
    tx.execute(&insert("staff", 9)).await.expect("insert in tx");
    let inside: Vec<ItemRow> = tx
        .fetch_all(
            &Query::select().columns(["id", "owner_id", "label", "payload", "created_at"]).from("items").and_where(Expr::col("owner_id").eq(9)).to_owned(),
        )
        .await
        .expect("fetch in tx");
    assert_eq!(inside.len(), 1);
    tx.commit().await.expect("commit");
    let after: Vec<ItemRow> = db
        .fetch_all(
            &Query::select().columns(["id", "owner_id", "label", "payload", "created_at"]).from("items").and_where(Expr::col("owner_id").eq(9)).to_owned(),
        )
        .await
        .expect("fetch");
    assert_eq!(after.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["staff"]);
    db.ping().await.expect("ping");
}

/// Publishing: copy, app override wins, unchanged / conflict / force, warnings for unpublished
/// upgrades, a missing dialect directory.
async fn publish_suite(url: &str) {
    let dir = common::temp_dir("db-publish");
    let dialect = Dialect::from_url(url).expect("dialect");
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = dir.clone();
    let builder = || NetBackendServer::new(config.clone()).module(Items { name: "items", table: "items", extra: false });

    let report = builder().publish_migrations("items", &[], false).expect("publish");
    assert_eq!(report.written.len(), 6, "2 migrations x 3 dialects");
    for d in Dialect::ALL {
        assert!(dir.join("items").join(d.name()).join("202610010001_create_items.sql").is_file());
    }
    // The app edits its copy before the first migrate: the edited version is what runs.
    let path = dir.join("items").join(dialect.name()).join("202610010001_create_items.sql");
    let original = std::fs::read_to_string(&path).expect("read");
    let edited = format!("{original};\nCREATE TABLE items_extra (id BIGINT NOT NULL PRIMARY KEY)");
    std::fs::write(&path, &edited).expect("write");

    // Republishing leaves the edited file alone and reports it; the rest is unchanged.
    let again = builder().publish_migrations("items", &[], false).expect("publish again");
    assert_eq!(again.skipped, std::slice::from_ref(&path));
    assert_eq!(again.unchanged.len(), 5);
    assert!(again.written.is_empty());

    let server = builder().build().await.expect("build");
    let report = server.migrate().await.expect("migrate");
    assert_eq!(report.applied.len(), 2);
    let status = server.migration_status().await.expect("status");
    assert!(status.iter().all(|s| s.source == Some(net_backend_server::migrate::MigrationSource::App)));
    server.state().db().execute_script("INSERT INTO items_extra (id) VALUES (1)").await.expect("the edited migration ran");
    server.state().db().close().await;

    // A module upgrade: the app-owned copy lacks the new migration -> a warning, not applied.
    let upgraded = NetBackendServer::new(config.clone()).module(Items { name: "items", table: "items", extra: true }).build().await.expect("build");
    let report = upgraded.migrate().await.expect("migrate");
    assert!(report.applied.is_empty());
    assert!(report.warnings.iter().any(|w| w.contains("202610010003_add_note.sql")), "{:?}", report.warnings);
    upgraded.state().db().close().await;
    // ... and `serve` refuses to start on the old schema, saying what to run.
    let refused = NetBackendServer::new(config.clone()).module(Items { name: "items", table: "items", extra: true }).build().await.expect("build");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let error = refused.serve_with_shutdown(listener, async {}).await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("migrations publish items") && error.contains("202610010003_add_note.sql") && error.contains("migrate"), "{error}");
    // Publishing again adds only the new file and keeps the edited one.
    let added = NetBackendServer::new(config.clone())
        .module(Items { name: "items", table: "items", extra: true })
        .publish_migrations("items", &[dialect], false)
        .expect("publish the upgrade");
    assert_eq!(added.written.len(), 1);
    assert!(added.written[0].ends_with("202610010003_add_note.sql"));
    assert_eq!(added.skipped, std::slice::from_ref(&path));
    assert_eq!(std::fs::read_to_string(&path).expect("read"), edited);
    let upgraded = NetBackendServer::new(config.clone()).module(Items { name: "items", table: "items", extra: true }).build().await.expect("build");
    let report = upgraded.migrate().await.expect("migrate");
    assert_eq!(report.applied.len(), 1);
    assert!(report.warnings.is_empty());
    upgraded.state().db().close().await;

    // A published module without the running dialect's directory is a clear error.
    let lonely = common::temp_dir("db-publish-lonely");
    std::fs::create_dir_all(lonely.join("items").join("nonsense")).expect("dir");
    let mut config2 = config.clone();
    config2.database.migrations_dir = lonely;
    let server = NetBackendServer::new(config2).module(Items { name: "items", table: "items", extra: false }).build().await.expect("build");
    let error = server.migrate().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains(&format!("--dialect {}", dialect.name())), "{error}");
    server.state().db().close().await;

    // Unknown module.
    let error = builder().publish_migrations("chat", &[], false).err();
    assert!(matches!(error, Some(Error::Migration(ref m)) if m.contains("no module `chat`")), "{error:?}");

    // A published file that is not valid UTF-8 (an editor saved Latin-1) is never overwritten
    // without --force.
    let mut latin1 = std::fs::read(&path).expect("read");
    latin1.extend_from_slice(b"\n-- caf\xe9\n");
    std::fs::write(&path, &latin1).expect("write");
    let again = builder().publish_migrations("items", &[dialect], false).expect("republish");
    assert_eq!(again.skipped, std::slice::from_ref(&path));
    assert_eq!(std::fs::read(&path).expect("read"), latin1);

    // --force puts the module's version back over the app's edit.
    let forced = builder().publish_migrations("items", &[dialect], true).expect("force");
    assert_eq!(forced.written, std::slice::from_ref(&path));
    assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_in_memory_migrate_and_queries() {
    let dir = common::temp_dir("db-memory");
    let server = prepare("sqlite::memory:", &dir, false).await;
    assert_eq!(server.migrate().await.expect("migrate").applied.len(), 3);
    db_layer(server.state().db()).await;
    server.state().db().close().await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_file_suite_and_publish() {
    let dir = common::temp_dir("db-sqlite-file");
    let url = format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"));
    suite(&url).await;
    let dir = common::temp_dir("db-sqlite-file-publish");
    let url = format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"));
    publish_suite(&url).await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_suite() {
    let base = common::env_url("NBS_TEST_MYSQL_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
    let (url, name) = common::fresh_database(&base).await;
    publish_suite(&url).await;
    common::drop_database(&base, &name).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_suite() {
    let base = common::env_url("NBS_TEST_POSTGRES_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
    let (url, name) = common::fresh_database(&base).await;
    publish_suite(&url).await;
    common::drop_database(&base, &name).await;
}

/// Two servers migrating one SQLite file at once: `BEGIN IMMEDIATE` + the re-check of the tracking
/// row make one apply each migration and the other skip it; neither fails.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_concurrent_migrate() {
    for round in 0..3 {
        let dir = common::temp_dir("db-sqlite-concurrent");
        let url = format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"));
        let a = prepare(&url, &dir, false).await;
        let b = prepare(&url, &dir, false).await;
        let (ra, rb) = tokio::join!(a.migrate(), b.migrate());
        let (ra, rb) = (ra.expect("a"), rb.expect("b"));
        assert_eq!(ra.applied.len() + rb.applied.len(), 3, "round {round}");
        let status = a.migration_status().await.expect("status");
        assert!(status.iter().all(|s| s.state == MigrationState::Applied), "{status:?}");
        a.state().db().close().await;
        b.state().db().close().await;
    }
}

/// Stress: 8 servers connect to a BRAND-NEW SQLite file at once (the WAL switch race) and migrate
/// it concurrently; 10 rounds, no failure allowed.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_fresh_file_parallel_start_stress() {
    for round in 0..10 {
        let dir = common::temp_dir("db-sqlite-stress");
        let url = format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (url, dir) = (url.clone(), dir.clone());
            tasks.push(tokio::spawn(async move {
                let mut config = Config::default();
                config.database.url = SecretString::new(url);
                config.database.migrations_dir = dir;
                let server = NetBackendServer::new(config).module(Audit).module(Items { name: "items", table: "items", extra: false }).build().await?;
                let report = server.migrate().await;
                server.state().db().close().await;
                report.map(|r| r.applied.len())
            }));
        }
        let mut applied = 0;
        for task in tasks {
            applied += task.await.expect("task").unwrap_or_else(|e| panic!("round {round}: {e}"));
        }
        assert_eq!(applied, 3, "round {round}");
    }
}

/// Two servers migrating the same database at once: the lock makes one apply, the other see
/// nothing to do (MySQL / PostgreSQL).
#[cfg(any(feature = "mysql", feature = "postgres"))]
async fn concurrent_migrate(base: &str) {
    let (url, name) = common::fresh_database(base).await;
    let dir_a = common::temp_dir("db-lock");
    let a = prepare(&url, &dir_a, false).await;
    let b = prepare(&url, &dir_a, false).await;
    let (ra, rb) = tokio::join!(a.migrate(), b.migrate());
    let total = ra.expect("a").applied.len() + rb.expect("b").applied.len();
    assert_eq!(total, 3);
    a.state().db().close().await;
    b.state().db().close().await;
    common::drop_database(base, &name).await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
#[ignore = "needs NBS_TEST_MYSQL_URL"]
async fn mysql_concurrent_migrate() {
    concurrent_migrate(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "needs NBS_TEST_POSTGRES_URL"]
async fn postgres_concurrent_migrate() {
    concurrent_migrate(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}

/// The MySQL lock is per database: a session holding database A's lock does not block database
/// B; database A waits, then fails with the lock-timeout message.
#[cfg(feature = "mysql")]
#[tokio::test]
#[ignore = "needs NBS_TEST_MYSQL_URL"]
async fn mysql_lock_is_per_database_and_times_out() {
    use net_backend_server::sqlx::Connection;
    let base = common::env_url("NBS_TEST_MYSQL_URL");
    let (url_a, name_a) = common::fresh_database(&base).await;
    let (url_b, name_b) = common::fresh_database(&base).await;
    let mut holder = net_backend_server::sqlx::MySqlConnection::connect(&url_a).await.expect("connect");
    let got: Option<i64> = net_backend_server::sqlx::query_scalar("SELECT GET_LOCK(CONCAT('nbs_migrations:', SHA1(IFNULL(DATABASE(), ''))), 0)")
        .fetch_one(&mut holder)
        .await
        .expect("lock");
    assert_eq!(got, Some(1));
    let dir = common::temp_dir("db-mysql-lock");
    // B's lock wait is long (60 s): finishing far below it shows B never waited for A's lock.
    let b = prepare_with(&url_b, &dir, false, 60).await;
    let started = std::time::Instant::now();
    assert_eq!(b.migrate().await.expect("database B is not blocked").applied.len(), 3);
    assert!(started.elapsed() < std::time::Duration::from_secs(30), "{:?}", started.elapsed());
    let a = prepare_with(&url_a, &dir, false, 1).await;
    let error = a.migrate().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("migrations lock") && error.contains("waited 1 s"), "{error}");
    let _ = holder.close().await;
    assert_eq!(a.migrate().await.expect("free again").applied.len(), 3);
    a.state().db().close().await;
    b.state().db().close().await;
    common::drop_database(&base, &name_a).await;
    common::drop_database(&base, &name_b).await;
}

/// The PostgreSQL lock wait is bounded: a held advisory lock makes `migrate` fail after the
/// configured time with an honest message.
#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "needs NBS_TEST_POSTGRES_URL"]
async fn postgres_lock_times_out() {
    use net_backend_server::sqlx::Connection;
    let base = common::env_url("NBS_TEST_POSTGRES_URL");
    let (url, name) = common::fresh_database(&base).await;
    let mut holder = net_backend_server::sqlx::PgConnection::connect(&url).await.expect("connect");
    let got: bool = net_backend_server::sqlx::query_scalar("SELECT pg_try_advisory_lock(7462051993)").fetch_one(&mut holder).await.expect("lock");
    assert!(got);
    let dir = common::temp_dir("db-pg-lock");
    let server = prepare_with(&url, &dir, false, 1).await;
    let started = std::time::Instant::now();
    let error = server.migrate().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("migrations lock") && error.contains("waited 1 s"), "{error}");
    // Bounded by the 1 s wait (generous upper bound for slow runners).
    assert!(started.elapsed() < std::time::Duration::from_secs(30), "{:?}", started.elapsed());
    let _ = holder.close().await;
    assert_eq!(server.migrate().await.expect("free again").applied.len(), 3);
    server.state().db().close().await;
    common::drop_database(&base, &name).await;
}
