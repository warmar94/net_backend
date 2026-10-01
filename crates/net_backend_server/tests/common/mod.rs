//! Shared helpers for the integration tests.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::Router;
use http::{HeaderMap, Request, StatusCode};
use net_backend_server::{Config, Dialect, SecretString};
use serde_json::Value;
use tower::ServiceExt;

/// A fresh directory under `target/tmp/it/<name>-<pid>-<n>`.
pub fn temp_dir(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp").join("it").join(format!(
        "{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Whether any database backend is compiled in.
pub fn any_backend() -> bool {
    !Dialect::enabled().is_empty()
}

/// A configuration that builds without a reachable database server: in-memory SQLite when
/// compiled in, else a lazy pool towards a closed port (only `/readyz` notices).
pub fn http_config() -> Config {
    let mut config = Config::default();
    let url = if Dialect::Sqlite.is_enabled() {
        "sqlite::memory:".to_string()
    } else if Dialect::MySql.is_enabled() {
        "mysql://test:test@127.0.0.1:9/test".to_string()
    } else {
        "postgres://test:test@127.0.0.1:9/test".to_string()
    };
    config.database.url = SecretString::new(url);
    config.database.connect_lazy = !Dialect::Sqlite.is_enabled();
    config.database.acquire_timeout_secs = 1;
    config.database.migrations_dir = temp_dir("migrations");
    config
}

/// One request through the router; the answer's status, headers and JSON body (`Null` if not JSON).
pub async fn call(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = router.clone().oneshot(request).await.expect("infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 16 << 20).await.expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, headers, json)
}

/// `GET path`.
pub fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).expect("request")
}

/// `POST path` with a JSON body.
pub fn post_json(path: &str, body: impl Into<String>) -> Request<Body> {
    Request::post(path).header("content-type", "application/json").body(Body::from(body.into())).expect("request")
}

/// The database URLs to run the DB suite against: in-memory SQLite (when compiled in) is always
/// included by the SQLite tests; MySQL / PostgreSQL come from `NBS_TEST_MYSQL_URL` /
/// `NBS_TEST_POSTGRES_URL` in their own (ignored by default) tests.
pub fn env_url(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("set {var} to run this test (e.g. in CI with a service container)"))
}

/// A fresh, empty database next to the one in `base_url` (MySQL / PostgreSQL), created with the
/// base URL's account. Returns the URL of the new database and its name.
#[cfg(any(feature = "mysql", feature = "postgres"))]
pub async fn fresh_database(base_url: &str) -> (String, String) {
    static N: AtomicU32 = AtomicU32::new(0);
    let name =
        format!("nbs_t_{}_{}_{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed), net_backend_server::protocol::UnixMillis::now().get() % 1_000_000);
    let mut config = Config::default();
    config.database.url = SecretString::new(base_url);
    let admin = net_backend_server::Db::connect(&config.database).await.expect("connect to the test server");
    admin.execute_script(format!("CREATE DATABASE {name}")).await.expect("create database");
    admin.close().await;
    let (head, query) = match base_url.split_once('?') {
        Some((head, query)) => (head, format!("?{query}")),
        None => (base_url, String::new()),
    };
    let scheme_end = head.find("://").map_or(0, |i| i + 3);
    let path_start = head[scheme_end..].find('/').map_or(head.len(), |i| scheme_end + i);
    (format!("{}/{name}{query}", &head[..path_start]), name)
}

/// Drop a database made by [`fresh_database`] (best effort).
#[cfg(any(feature = "mysql", feature = "postgres"))]
pub async fn drop_database(base_url: &str, name: &str) {
    let mut config = Config::default();
    config.database.url = SecretString::new(base_url);
    if let Ok(admin) = net_backend_server::Db::connect(&config.database).await {
        let _ = admin.execute_script(format!("DROP DATABASE IF EXISTS {name}")).await;
        admin.close().await;
    }
}
