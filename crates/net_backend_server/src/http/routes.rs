//! The core routes: health, readiness, server info, the reserved WebSocket path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use net_backend_protocol::ServerInfo;
use serde_json::{json, Value};

use crate::db::Db;
use crate::error::AppError;
use crate::state::AppState;

/// How long the readiness check waits for the database (a refused port fails at once).
const READY_DB_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a readiness result is reused: `/readyz` probes the database at most once per this
/// time, one probe at a time, however many requests ask.
const READY_CACHE: Duration = Duration::from_secs(1);

/// The readiness probe behind `/readyz`: single-flight, its result cached for [`READY_CACHE`]. A
/// failure is logged as a WARN when the state changes (then DEBUG while it lasts), the recovery as
/// INFO.
#[derive(Default)]
pub(crate) struct Readiness {
    last: tokio::sync::Mutex<Option<(Instant, bool)>>,
    probes: AtomicU64,
}

impl Readiness {
    /// Whether the database answers: the cached result when it is fresh, else one probe (callers
    /// arriving meanwhile wait for it and share its result).
    pub(crate) async fn check(&self, db: &Db) -> bool {
        let mut last = self.last.lock().await;
        if let Some((at, ok)) = *last {
            if at.elapsed() < READY_CACHE {
                return ok;
            }
        }
        self.probes.fetch_add(1, Ordering::Relaxed);
        let result = db.check_ready(READY_DB_TIMEOUT).await;
        let before = last.map(|(_, ok)| ok);
        match (&result, before) {
            (Err(error), Some(false)) => tracing::debug!(error = %error.describe(), "readiness: the database check failed again"),
            (Err(error), _) => tracing::warn!(error = %error.describe(), "readiness: the database check failed"),
            (Ok(()), Some(false)) => tracing::info!("readiness: the database answers again"),
            (Ok(()), _) => {}
        }
        let ok = result.is_ok();
        *last = Some((Instant::now(), ok));
        ok
    }

    #[cfg(all(test, feature = "sqlite"))]
    fn probes(&self) -> u64 {
        self.probes.load(Ordering::Relaxed)
    }
}

/// Liveness: 200 while the process runs.
#[utoipa::path(get, path = "/healthz", tag = "operations", operation_id = "health",
    responses((status = 200, description = "The process runs", body = crate::openapi::Status)))]
pub(crate) async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Readiness: 200 when the database answers; 503 while shutting down or without a database. The
/// database is probed at most once per second (one probe at a time); other requests get the last
/// result.
#[utoipa::path(get, path = "/readyz", tag = "operations", operation_id = "ready",
    responses(
        (status = 200, description = "Ready to serve", body = crate::openapi::Status),
        (status = 503, description = "Shutting down or the database is unreachable", body = crate::openapi::ErrorBody),
    ))]
pub(crate) async fn ready(State(state): State<AppState>) -> Response {
    if state.shutdown().is_triggered() {
        return AppError::unavailable("the server is shutting down").into_response();
    }
    if state.readiness().check(state.db()).await {
        Json(json!({ "status": "ready" })).into_response()
    } else {
        AppError::unavailable("the database is unreachable").into_response()
    }
}

/// The server's protocol versions and enabled modules (no authentication).
#[utoipa::path(get, path = "/v1/info", tag = "server", operation_id = "info",
    responses((status = 200, description = "Protocol versions and enabled modules", body = crate::openapi::ServerInfo)))]
pub(crate) async fn info(State(state): State<AppState>) -> Json<ServerInfo> {
    Json(server_info(&state))
}

/// The [`ServerInfo`] of this server: protocol range and the module names, sorted.
pub(crate) fn server_info(state: &AppState) -> ServerInfo {
    let mut modules: Vec<String> = state.modules().iter().map(|m| (*m).to_string()).collect();
    modules.sort();
    ServerInfo::new(modules).with_versions(super::middleware::MIN_PROTOCOL_VERSION, net_backend_protocol::PROTOCOL_VERSION)
}

/// `/v1/ws` until the WebSocket hub exists: 403 (permanent for clients), never 400.
pub(crate) async fn ws_reserved() -> Response {
    AppError::forbidden("the WebSocket hub is not enabled on this server").into_response()
}

/// The browser UI for the OpenAPI document: a third-party viewer script, pinned by URL and
/// Subresource Integrity (both from `[openapi]`, required when the UI is on), and a
/// Content-Security-Policy that allows no other script.
pub(crate) fn docs_page(config: &crate::config::OpenApiConfig) -> Response {
    let url = config.ui_script_url.clone().unwrap_or_default();
    let integrity = config.ui_script_integrity.clone().unwrap_or_default();
    let html = format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>API reference</title>
</head>
<body>
<script id="api-reference" data-url="/v1/openapi.json"></script>
<script src="{url}" integrity="{integrity}" crossorigin="anonymous"></script>
</body>
</html>
"#
    );
    let csp = format!(
        "default-src 'self'; script-src {url}; style-src 'self' 'unsafe-inline' https:; font-src 'self' https: data:; img-src 'self' https: data:; connect-src 'self'; frame-ancestors 'none'"
    );
    let mut response = Html(html).into_response();
    if let Ok(value) = http::HeaderValue::from_str(&csp) {
        response.headers_mut().insert(http::header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

// The test needs a database (SQLite in memory).
#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    /// SF1: many concurrent `/readyz` calls cost one database probe; the result is reused for a
    /// second, then probed again.
    #[tokio::test]
    async fn readiness_probes_once_for_concurrent_callers() {
        let mut config = crate::config::DatabaseConfig::default();
        config.url = crate::config::SecretString::new("sqlite::memory:");
        let db = Db::connect(&config).await.expect("connect");
        let readiness = std::sync::Arc::new(Readiness::default());
        let calls: Vec<_> = (0..50)
            .map(|_| {
                let (readiness, db) = (readiness.clone(), db.clone());
                tokio::spawn(async move { readiness.check(&db).await })
            })
            .collect();
        for call in calls {
            assert!(call.await.expect("join"));
        }
        assert_eq!(readiness.probes(), 1);
        tokio::time::sleep(READY_CACHE + Duration::from_millis(50)).await;
        assert!(readiness.check(&db).await);
        assert_eq!(readiness.probes(), 2);
        // A closed pool fails; the failure is cached too.
        db.close().await;
        tokio::time::sleep(READY_CACHE + Duration::from_millis(50)).await;
        assert!(!readiness.check(&db).await);
        assert!(!readiness.check(&db).await);
        assert_eq!(readiness.probes(), 3);
    }
}
