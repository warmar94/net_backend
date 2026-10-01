//! HTTP basics: extractors ([`ApiJson`], [`Ext`], [`RequestId`]), the middleware stack and the
//! core routes.
//!
//! The stack, outermost first: CORS (only when configured) → request id → client address
//! ([`ClientIp`], `http.trusted_proxies`) → tracing → protocol
//! header and version check → error normalising (every 4xx / 5xx is the protocol's error body; an
//! unexpected 5xx never shows its body) → request timeout → panic catching → hard body cap
//! (`http.max_body_bytes`) → body limit → (matched routes only) metrics → rate limit before
//! authentication → authentication → rate limit after authentication → the handler. Below all of
//! it the connection loop enforces the header-read timeout and the shutdown deadline.
//!
//! Core routes: `GET /healthz`, `GET /readyz`, `GET /v1/info`, `GET /v1/openapi.json`
//! (`openapi.enabled`), `GET /v1/docs` (`openapi.ui`), and the
//! reserved `GET /v1/ws` (403 until the WebSocket hub arrives in the next sub-phase; never 400,
//! which the client would retry forever). `GET /metrics` (`metrics.enabled`) is served on its own
//! listener (`metrics.bind`), never on the API port.

pub(crate) mod client_ip;
pub(crate) mod middleware;
pub(crate) mod routes;

pub use client_ip::{ClientIp, FORWARDED_FOR_HEADER};

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use http::request::Parts;
use net_backend_protocol::codes;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub use axum::extract::DefaultBodyLimit;

use crate::error::AppError;
use crate::state::AppState;

/// The request-id header (sent back on every response).
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// A per-route body limit, e.g. `put(upload).layer(body_limit(storage::PUT_BODY_LIMIT_BYTES))`.
/// Applies to the body extractors (`ApiJson`, `Json`, `Bytes`, `String`); a handler reading the
/// raw `Body` stream must limit it itself.
pub fn body_limit(bytes: usize) -> DefaultBodyLimit {
    DefaultBodyLimit::max(bytes)
}

/// The id of one request: in logs (the request span), in the `x-request-id` response header and
/// in [`HookCtx`](crate::hooks::HookCtx).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RequestId(Arc<str>);

impl RequestId {
    /// A new id: a per-process prefix plus a counter (unique per process and start).
    pub fn generate() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        static PREFIX: OnceLock<String> = OnceLock::new();
        let prefix = PREFIX.get_or_init(|| format!("{:x}", net_backend_protocol::UnixMillis::now().get().max(0)));
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(Arc::from(format!("{prefix}-{n:x}")))
    }

    /// A client-supplied id, if it is short and plain (1–64 of `A-Z a-z 0-9 . _ -`).
    pub fn from_client(value: &str) -> Option<Self> {
        let ok = (1..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        ok.then(|| Self(Arc::from(value)))
    }

    /// The id text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestId({})", self.0)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RequestId {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts.extensions.get::<RequestId>().cloned().unwrap_or_else(RequestId::generate))
    }
}

/// A JSON body (request) or answer (response), like `axum::Json`, but every rejection is the
/// protocol's error body: malformed JSON or wrong fields → 400 `bad_request` (with the parser's
/// description of the client's input), too large → 413 `payload_too_large`, no
/// `content-type: application/json` → 415.
#[derive(Clone, Copy, Debug, Default)]
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(json_rejection(rejection)),
        }
    }
}

fn json_rejection(rejection: JsonRejection) -> AppError {
    let status = rejection.status();
    match status.as_u16() {
        413 => AppError::payload_too_large("the request body is too large"),
        415 => AppError::with_status(status, "unsupported_media_type", "the request body must be JSON (content-type: application/json)"),
        // Syntax and shape errors describe the client's own input, never server internals.
        _ => AppError::new(codes::BAD_REQUEST, rejection.body_text()),
    }
}

impl<T: Serialize> IntoResponse for ApiJson<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// A value registered with [`NetBackendServer::state`](crate::NetBackendServer::state), by type.
/// A handler asking for a type that was never registered answers 500 (a programming error, logged).
#[derive(Debug)]
pub struct Ext<T>(pub Arc<T>);

impl<T> std::ops::Deref for Ext<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Send + Sync + 'static> FromRequestParts<AppState> for Ext<T> {
    type Rejection = AppError;

    async fn from_request_parts(_parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        state
            .get::<T>()
            .map(Ext)
            .ok_or_else(|| AppError::internal(std::io::Error::other(format!("no state of type {} was registered", std::any::type_name::<T>()))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids() {
        let a = RequestId::generate();
        let b = RequestId::generate();
        assert_ne!(a, b);
        assert!(RequestId::from_client(a.as_str()).is_some());
        assert!(RequestId::from_client("abc-DEF_1.2").is_some());
        for bad in ["", "a b", "x\ny", "<script>", &"x".repeat(65)] {
            assert!(RequestId::from_client(bad).is_none(), "{bad}");
        }
    }
}
