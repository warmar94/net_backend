//! S5 (review): a failed login awaits the same database work for a known and an unknown address.
//! Its own test binary: it installs a GLOBAL tracing subscriber (SQLite runs statements on its own
//! worker thread, which a thread-local subscriber would not see) and counts sqlx's statement events.
#![cfg(feature = "sqlite")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::routes;
use net_backend_server::{AuthService, Config, NetBackendServer, SecretString};
use serde_json::json;

/// S5: a failed login awaits the same database work for a known and an unknown address (one
/// query; the known address's audit row is written in the background).
#[tokio::test(flavor = "current_thread")]
async fn failed_logins_await_the_same_work() {
    #[derive(Clone, Default)]
    struct Buffer(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("sqlx::query=debug"))
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber of this binary");
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.send_verification_on_register = false;
    let mut config = Config::default();
    config.database.url = SecretString::new("sqlite::memory:");
    let prepared = NetBackendServer::new(config).module(Auth::new().with_config(auth).mailer(MemoryMailer::new())).build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let post = |path: &str, body: serde_json::Value| common::post_json(path, body.to_string());
    common::call(&router, post(routes::auth::REGISTER, json!({"email": "known@example.com", "password": "correct horse battery"}))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let statements = |buffer: &Buffer| -> Vec<String> {
        let text = String::from_utf8_lossy(&buffer.0.lock().expect("lock")).into_owned();
        text.lines().filter(|l| l.contains("sqlx::query") && !l.contains("auth_audit_log")).map(String::from).collect()
    };
    buffer.0.lock().expect("lock").clear();
    let (status, _, known) = common::call(&router, post(routes::auth::LOGIN, json!({"email": "known@example.com", "password": "wrong password!!"}))).await;
    let known_work = statements(&buffer);
    buffer.0.lock().expect("lock").clear();
    let (status2, _, unknown) = common::call(&router, post(routes::auth::LOGIN, json!({"email": "unknown@example.com", "password": "wrong password!!"}))).await;
    let unknown_work = statements(&buffer);
    assert_eq!((status, &known), (status2, &unknown));
    assert_eq!(known_work.len(), 1, "{known_work:?}");
    assert_eq!(known_work.len(), unknown_work.len(), "known: {known_work:?}\nunknown: {unknown_work:?}");
    // The known address's failure is still audited (in the background).
    tokio::time::sleep(Duration::from_millis(100)).await;
    let service = prepared.state().get::<AuthService>().expect("service");
    let page =
        service.audit_log(prepared.state(), &net_backend_server::protocol::admin::AuditQuery::new().with_action("auth.login_failed")).await.expect("audit");
    assert_eq!(page.items.len(), 1);
}
