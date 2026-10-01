//! The middleware functions (assembled in `app.rs`).

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::header::{ALLOW, CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE};
use http::{HeaderValue, StatusCode};
use net_backend_protocol::{codes, routes, ErrorBody, PROTOCOL_HEADER, PROTOCOL_VERSION};

use super::{ClientIp, RequestId, REQUEST_ID_HEADER};
use crate::auth::{AuthContext, AuthFailure, Authenticator};
use crate::error::{default_error_for, AppError, ErrorMarker};
use crate::rate_limit::{RateDecision, RateLimitKey, RateLimitStage, RateLimiter};
use crate::state::AppState;

/// The oldest protocol version this server accepts.
pub(crate) const MIN_PROTOCOL_VERSION: u32 = PROTOCOL_VERSION;

/// The largest game-made JSON 4xx body the error normaliser inspects (larger ones are replaced).
const MAX_INSPECTED_ERROR_BODY: usize = 64 * 1024;

/// What the middleware needs besides the app state.
#[derive(Clone)]
pub(crate) struct Mw {
    pub(crate) state: AppState,
    pub(crate) authenticators: Arc<[Arc<dyn Authenticator>]>,
    pub(crate) rate_limiters: Arc<[Arc<dyn RateLimiter>]>,
}

/// Attach a request id (the client's when trusted and plain, else a new one) and echo it.
pub(crate) async fn request_id(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let client =
        state.config().http.trust_request_id.then(|| req.headers().get(REQUEST_ID_HEADER).and_then(|v| v.to_str().ok()).and_then(RequestId::from_client));
    let id = client.flatten().unwrap_or_else(RequestId::generate);
    req.extensions_mut().insert(id.clone());
    let mut response = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(id.as_str()) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// Make every error answer the protocol's error body, and never let an unexpected 5xx body out.
///
/// Passes: answers built from an [`AppError`], and a game's own JSON 4xx whose body already is a
/// protocol error body (`{"error":{"code":"…",…}}`). Everything else with status ≥ 400 is
/// replaced by [`default_error_for`] (plain-text rejections, empty bodies, other JSON shapes, any
/// 5xx that did not come from an `AppError`).
pub(crate) async fn normalize_errors(req: Request, next: Next) -> Response {
    let response = next.run(req).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) || response.extensions().get::<ErrorMarker>().is_some() {
        return response;
    }
    let is_json = response.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"));
    let (parts, body) = response.into_parts();
    if status.is_client_error() && is_json {
        if let Ok(bytes) = axum::body::to_bytes(body, MAX_INSPECTED_ERROR_BODY).await {
            if serde_json::from_slice::<ErrorBody>(&bytes).is_ok_and(|b| !b.error.code.is_empty()) {
                return Response::from_parts(parts, Body::from(bytes));
            }
        }
    } else if status.is_server_error() {
        tracing::error!(status = status.as_u16(), "a handler answered a server error without AppError; the body was replaced");
    }
    let mut replaced = default_error_for(status).into_response();
    for name in [ALLOW, RETRY_AFTER, WWW_AUTHENTICATE] {
        if let Some(value) = parts.headers.get(&name) {
            replaced.headers_mut().insert(name, value.clone());
        }
    }
    replaced
}

/// Send the protocol version on every answer; refuse an unsupported client version on plain
/// HTTP routes (400 `unsupported_protocol`). `/v1/ws` is never refused with 400: the WebSocket
/// hub closes with 4010 instead.
pub(crate) async fn protocol(req: Request, next: Next) -> Response {
    let refused = req.uri().path() != routes::WS
        && req.headers().get(PROTOCOL_HEADER).is_some_and(|v| {
            let version = v.to_str().ok().and_then(|s| s.trim().parse::<u32>().ok());
            !version.is_some_and(|v| (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&v))
        });
    let mut response = if refused {
        AppError::new(codes::UNSUPPORTED_PROTOCOL, "this protocol version is not supported")
            .with_details(serde_json::json!({ "supported_min": MIN_PROTOCOL_VERSION, "supported_max": PROTOCOL_VERSION }))
            .into_response()
    } else {
        next.run(req).await
    };
    response.headers_mut().insert(PROTOCOL_HEADER, HeaderValue::from(PROTOCOL_VERSION));
    response
}

/// The per-request time limit (503 `unavailable` after it).
pub(crate) async fn timeout(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let limit = Duration::from_secs(state.config().http.request_timeout_secs.max(1));
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(limit_secs = limit.as_secs(), "request timed out");
            AppError::unavailable("the request took too long").into_response()
        }
    }
}

/// The answer for a panicking handler: 500 `internal`, no details.
pub(crate) fn panic_response(panic: Box<dyn Any + Send + 'static>) -> Response {
    let message = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
    tracing::error!(panic = %message, "handler panicked");
    AppError::internal_plain().into_response()
}

/// When the authenticators ran for a request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AuthCheckedAt(pub(crate) Instant);

/// Ask the authenticators in order: the first `Ok(Some(..))` attaches its context; an `Err` is
/// remembered as an [`AuthFailure`] (answered by handlers that need a user) and stops the chain.
pub(crate) async fn authenticate(State(mw): State<Mw>, req: Request, next: Next) -> Response {
    if mw.authenticators.is_empty() {
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    // When the credentials were checked (the WebSocket hub refuses a socket whose session was
    // revoked after this moment).
    parts.extensions.insert(AuthCheckedAt(Instant::now()));
    for authenticator in mw.authenticators.iter() {
        match authenticator.authenticate(&parts, &mw.state).await {
            Ok(Some(context)) => {
                parts.extensions.insert(context);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                parts.extensions.insert(AuthFailure::from_error(error));
                break;
            }
        }
    }
    next.run(Request::from_parts(parts, body)).await
}

fn limit(mw: &Mw, req: &Request, stage: RateLimitStage) -> Option<Response> {
    if mw.rate_limiters.is_empty() {
        return None;
    }
    let user = match stage {
        RateLimitStage::BeforeAuth => None,
        _ => req.extensions().get::<AuthContext>().map(|a| a.user_id),
    };
    let key = RateLimitKey::new(
        req.extensions().get::<ClientIp>().and_then(|c| c.0),
        req.extensions().get::<MatchedPath>().map(|p| p.as_str().to_string()),
        user,
        stage,
    );
    for limiter in mw.rate_limiters.iter() {
        if let RateDecision::Deny { retry_after_ms } = limiter.check(&key) {
            return Some(rate_limited_response(retry_after_ms));
        }
    }
    None
}

/// The 429 answer with `Retry-After` (whole seconds, at least 1).
pub(crate) fn rate_limited_response(retry_after_ms: u64) -> Response {
    let mut response = AppError::rate_limited(retry_after_ms).into_response();
    response.headers_mut().insert(RETRY_AFTER, HeaderValue::from(retry_after_ms.div_ceil(1000).max(1)));
    response
}

/// Ask the rate limiter before authentication (address + route).
pub(crate) async fn rate_limit_before_auth(State(mw): State<Mw>, req: Request, next: Next) -> Response {
    match limit(&mw, &req, RateLimitStage::BeforeAuth) {
        Some(refused) => refused,
        None => next.run(req).await,
    }
}

/// Ask the rate limiter after authentication (with the user).
pub(crate) async fn rate_limit_after_auth(State(mw): State<Mw>, req: Request, next: Next) -> Response {
    match limit(&mw, &req, RateLimitStage::AfterAuth) {
        Some(refused) => refused,
        None => next.run(req).await,
    }
}

/// Count requests and their duration per route pattern.
pub(crate) async fn track(req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_string();
    let route = req.extensions().get::<MatchedPath>().map_or_else(|| "unmatched".to_string(), |p| p.as_str().to_string());
    let started = Instant::now();
    let response = next.run(req).await;
    let status = response.status().as_u16().to_string();
    metrics::counter!("nbs_http_requests_total", "method" => method.clone(), "route" => route.clone(), "status" => status).increment(1);
    metrics::histogram!("nbs_http_request_duration_seconds", "method" => method, "route" => route).record(started.elapsed().as_secs_f64());
    response
}

/// The JSON 404 for unknown routes.
pub(crate) async fn not_found() -> Response {
    default_error_for(StatusCode::NOT_FOUND).into_response()
}
