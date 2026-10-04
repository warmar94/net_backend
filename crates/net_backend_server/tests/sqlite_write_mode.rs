//! SQLite's write mode: `database.sqlite_synchronous` reaches every pooled connection, and the
//! busy timeout follows `database.acquire_timeout_secs`.
#![cfg(feature = "sqlite")]

mod common;

use std::time::{Duration, Instant};

use net_backend_server::config::SqliteSynchronous;
use net_backend_server::{Config, Db, SecretString};
use sqlx::Connection;

/// A file database in a fresh directory, with these settings.
fn file_config(name: &str, synchronous: Option<SqliteSynchronous>, acquire_secs: u64, lazy: bool) -> (Config, String) {
    let dir = common::temp_dir(name);
    let url = format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"));
    let mut config = Config::default();
    config.database.url = SecretString::new(url.clone());
    config.database.acquire_timeout_secs = acquire_secs;
    config.database.connect_lazy = lazy;
    if let Some(synchronous) = synchronous {
        config.database.sqlite_synchronous = synchronous;
    }
    (config, url)
}

/// `PRAGMA <name>` read back on pooled connections (all `max_connections` of them at once, so
/// every connection the pool opens is checked, not only the first).
async fn pragma_on_every_connection(db: &Db, name: &str, connections: u32) -> Vec<i64> {
    let pool = db.sqlite().expect("a SQLite pool");
    let mut held = Vec::new();
    let mut values = Vec::new();
    for _ in 0..connections {
        let mut conn = pool.acquire().await.expect("a pooled connection");
        let value: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("PRAGMA {name}"))).fetch_one(&mut *conn).await.expect("pragma");
        values.push(value);
        held.push(conn);
    }
    values
}

// SQLite's codes: 1 = NORMAL, 2 = FULL.
const NORMAL: i64 = 1;
const FULL: i64 = 2;

#[tokio::test]
async fn default_is_normal_on_every_connection() {
    for lazy in [false, true] {
        let (mut config, _) = file_config("sqlite-sync-default", None, 5, lazy);
        config.database.max_connections = 3;
        let db = Db::connect(&config.database).await.expect("connect");
        assert_eq!(pragma_on_every_connection(&db, "synchronous", 3).await, [NORMAL; 3], "lazy = {lazy}");
        // WAL is still on for a file database.
        let pool = db.sqlite().unwrap();
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode").fetch_one(pool).await.unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        db.close().await;
    }
}

#[tokio::test]
async fn full_is_applied_on_every_connection() {
    for lazy in [false, true] {
        let (mut config, _) = file_config("sqlite-sync-full", Some(SqliteSynchronous::Full), 5, lazy);
        config.database.max_connections = 3;
        let db = Db::connect(&config.database).await.expect("connect");
        assert_eq!(pragma_on_every_connection(&db, "synchronous", 3).await, [FULL; 3], "lazy = {lazy}");
        db.close().await;
    }
}

#[tokio::test]
async fn setting_from_the_config_file_reaches_the_pool() {
    let dir = common::temp_dir("sqlite-sync-file");
    let url = format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"));
    let toml = format!("[database]\nurl = \"{url}\"\nsqlite_synchronous = \"full\"\nacquire_timeout_secs = 9\n");
    let config = Config::from_toml_str(&toml).expect("config");
    let db = Db::connect(&config.database).await.expect("connect");
    assert_eq!(pragma_on_every_connection(&db, "synchronous", 1).await, [FULL]);
    assert_eq!(pragma_on_every_connection(&db, "busy_timeout", 1).await, [9_000]);
    db.close().await;
}

#[tokio::test]
async fn in_memory_database_connects_with_both_settings() {
    for synchronous in [SqliteSynchronous::Normal, SqliteSynchronous::Full] {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config.database.sqlite_synchronous = synchronous;
        let db = Db::connect(&config.database).await.expect("connect");
        let expected = if synchronous == SqliteSynchronous::Full { FULL } else { NORMAL };
        assert_eq!(pragma_on_every_connection(&db, "synchronous", 1).await, [expected]);
        db.close().await;
    }
}

#[tokio::test]
async fn busy_timeout_is_the_acquire_timeout() {
    for secs in [1_u64, 7, 30] {
        let (mut config, _) = file_config("sqlite-busy-pragma", None, secs, false);
        config.database.max_connections = 2;
        let db = Db::connect(&config.database).await.expect("connect");
        let millis = i64::try_from(secs * 1000).unwrap();
        assert_eq!(pragma_on_every_connection(&db, "busy_timeout", 2).await, [millis; 2], "{secs} s");
        db.close().await;
    }
}

/// A write behind another connection's write lock waits the configured time (not a fixed 5 s),
/// then fails as a retryable busy error.
#[tokio::test]
async fn blocked_write_waits_the_configured_time() {
    for secs in [1_u64, 2] {
        let (config, url) = file_config("sqlite-busy-wait", None, secs, false);
        let db = Db::connect(&config.database).await.expect("connect");
        db.execute_script("CREATE TABLE t (x BIGINT)").await.expect("create");

        // Another connection takes the write lock and keeps it.
        let mut holder = sqlx::SqliteConnection::connect(&url).await.expect("holder");
        sqlx::raw_sql("BEGIN IMMEDIATE").execute(&mut holder).await.expect("write lock");

        let started = Instant::now();
        let error = db.execute_script("INSERT INTO t (x) VALUES (1)").await.expect_err("the write lock is held");
        let waited = started.elapsed();
        assert!(error.is_retryable(), "a busy error: {error}");
        let limit = Duration::from_secs(secs);
        assert!(waited >= limit - Duration::from_millis(100), "{secs} s: gave up after {waited:?}");
        assert!(waited < limit + Duration::from_millis(2500), "{secs} s: waited {waited:?}");

        // Once the lock is free, the same write goes through.
        sqlx::raw_sql("ROLLBACK").execute(&mut holder).await.expect("release");
        holder.close().await.expect("close");
        db.execute_script("INSERT INTO t (x) VALUES (1)").await.expect("write after the lock is free");
        db.close().await;
    }
}
