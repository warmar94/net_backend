//! Errors: [`Error`] for the framework itself (configuration, database, migrations, modules,
//! startup) and [`AppError`] for answers to clients.
//!
//! An [`AppError`] becomes the protocol's error body
//! (`{"error":{"code":"…","message":"…","details":…}}`, [`ErrorBody`]) with the HTTP status of its
//! code ([`http_status_for`](net_backend_protocol::error::http_status_for)). Internal errors (database, I/O, a bug) are logged on the server with
//! the request id and answered as a generic `internal` error: **no SQL, no driver message, no
//! stack trace ever reaches a client.**

use std::fmt;
use std::io;

use axum::response::{IntoResponse, Response};
use http::StatusCode;

use net_backend_protocol::{codes, ApiError, ErrorBody, ValidationDetails};
use serde_json::Value;

use crate::db::DbError;

/// A framework error: configuration, database, migrations, modules, startup, the command line.
///
/// These are for the operator (logs, the console), never sent to a client as they are: when one
/// reaches a request handler it becomes [`AppError::internal`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The configuration is invalid; one message per problem.
    #[error("invalid configuration: {}", .0.join("; "))]
    Config(Vec<String>),
    /// A database error (connecting, a query, the backend not compiled in).
    #[error(transparent)]
    Db(#[from] DbError),
    /// Migrations could not be planned, published or applied.
    #[error("migrations: {0}")]
    Migration(String),
    /// A module or route registration problem (bad or duplicate name, conflicting routes).
    #[error("module: {0}")]
    Module(String),
    /// A start hook or a module's `start` failed.
    #[error("startup: {0}")]
    Startup(String),
    /// An I/O error with what was being done.
    #[error("{context}: {source}")]
    Io {
        /// What was being done (e.g. "binding 127.0.0.1:8080").
        context: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The command line could not be parsed.
    #[error("{0}")]
    Cli(String),
}

impl Error {
    /// An [`Error::Io`] with this context.
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Error::Io { context: context.into(), source }
    }
}

/// Marks a response built from an [`AppError`], so the error-normalising middleware leaves it alone.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ErrorMarker;

/// An error answer to a client: an HTTP status plus the protocol's [`ApiError`] (`code`,
/// `message`, `details`), and, for internal errors, a source that is logged but never sent.
///
/// Return it from handlers (`Result<T, AppError>`); `?` converts [`ApiError`], [`DbError`],
/// [`Error`] and `sqlx::Error` (the last three as `internal`).
pub struct AppError {
    status: StatusCode,
    error: ApiError,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl AppError {
    /// An error with this code and message; the status comes from the code
    /// ([`http_status_for`](net_backend_protocol::error::http_status_for)), 400 for a code the protocol does not define.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::from_api(ApiError::new(code, message))
    }

    /// An error with an explicit status (for a module's or game's own code).
    pub fn with_status(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { status, error: ApiError::new(code, message), source: None }
    }

    /// The same error with these details.
    pub fn with_details(mut self, details: Value) -> Self {
        self.error = self.error.with_details(details);
        self
    }

    /// `bad_request` (400).
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(codes::BAD_REQUEST, message)
    }

    /// `validation_failed` (422) with these field messages.
    pub fn validation(details: ValidationDetails) -> Self {
        Self::from_api(ApiError::validation(details))
    }

    /// `unauthorized` (401).
    pub fn unauthorized() -> Self {
        Self::new(codes::UNAUTHORIZED, "authentication required")
    }

    /// `forbidden` (403).
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(codes::FORBIDDEN, message)
    }

    /// `not_found` (404).
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(codes::NOT_FOUND, message)
    }

    /// `conflict` (409).
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(codes::CONFLICT, message)
    }

    /// `payload_too_large` (413).
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(codes::PAYLOAD_TOO_LARGE, message)
    }

    /// `rate_limited` (429) with `{"retry_after_ms":N}`.
    pub fn rate_limited(retry_after_ms: u64) -> Self {
        Self::new(codes::RATE_LIMITED, "too many requests").with_details(serde_json::json!({ "retry_after_ms": retry_after_ms }))
    }

    /// `unavailable` (503): overloaded, shutting down or a dependency is down; retry later.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(codes::UNAVAILABLE, message)
    }

    /// `internal` (500). The source is logged (with the request id) and never sent.
    pub fn internal(source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, error: internal_body(), source: Some(source.into()) }
    }

    /// `internal` (500) without a source (e.g. a caught panic; the caller logs).
    pub(crate) fn internal_plain() -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, error: internal_body(), source: None }
    }

    /// An error from a status and a protocol error (no source).
    pub(crate) fn from_parts(status: StatusCode, error: ApiError) -> Self {
        Self { status, error, source: None }
    }

    fn from_api(error: ApiError) -> Self {
        let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
        Self { status, error, source: None }
    }

    /// The HTTP status.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The error as clients see it.
    pub fn api_error(&self) -> &ApiError {
        &self.error
    }

    /// The stable code.
    pub fn code(&self) -> &str {
        &self.error.code
    }
}

fn internal_body() -> ApiError {
    ApiError::new(codes::INTERNAL, "internal server error")
}

impl fmt::Debug for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("AppError");
        d.field("status", &self.status.as_u16()).field("code", &self.error.code).field("message", &self.error.message);
        if let Some(source) = &self.source {
            d.field("source", &format_args!("{source}"));
        }
        d.finish()
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status.as_u16(), self.error)?;
        if let Some(source) = &self.source {
            write!(f, " ({source})")?;
        }
        Ok(())
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Inside the request span, so the line carries the request id. One line per failure: an
        // internal error with its cause at ERROR, another 5xx with a cause at WARN; without a cause
        // the code that answered already logged why, and the access log line shows the status.
        if let Some(source) = &self.source {
            if self.status == StatusCode::INTERNAL_SERVER_ERROR {
                tracing::error!(code = %self.error.code, error = %source, "request failed");
            } else if self.status.is_server_error() {
                tracing::warn!(code = %self.error.code, error = %source, "request failed");
            }
        }
        // A server error never carries details a handler might have attached by mistake.
        let error = if self.status.is_server_error() && self.error.code == codes::INTERNAL { internal_body() } else { self.error };
        let mut response = (self.status, axum::Json(ErrorBody::new(error))).into_response();
        response.extensions_mut().insert(ErrorMarker);
        response
    }
}

impl From<ApiError> for AppError {
    fn from(error: ApiError) -> Self {
        Self::from_api(error)
    }
}

impl From<DbError> for AppError {
    fn from(error: DbError) -> Self {
        Self::internal(error)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        Self::internal(DbError::from(error))
    }
}

impl From<Error> for AppError {
    fn from(error: Error) -> Self {
        Self::internal(error)
    }
}

/// The error the server answers instead of a bare HTTP status (a framework rejection, an unknown
/// route, a handler that answered without an [`AppError`]). The status is the protocol's status for
/// the chosen code (axum's 422 for a wrong JSON shape becomes 400 `bad_request`, 408 / 504 become
/// 503 `unavailable`); an unlisted 4xx keeps its status with `bad_request`.
pub(crate) fn default_error_for(status: StatusCode) -> AppError {
    let (code, message) = match status.as_u16() {
        400 => (codes::BAD_REQUEST, "the request is malformed"),
        401 => (codes::UNAUTHORIZED, "authentication required"),
        403 => (codes::FORBIDDEN, "not allowed"),
        404 => (codes::NOT_FOUND, "no such route or object"),
        405 => (codes::METHOD_NOT_ALLOWED, "the method is not allowed on this route"),
        408 => (codes::UNAVAILABLE, "the request took too long"),
        409 => (codes::CONFLICT, "the request conflicts with the current state"),
        413 => (codes::PAYLOAD_TOO_LARGE, "the request body is too large"),
        415 => (codes::UNSUPPORTED_MEDIA_TYPE, "the request body must be JSON (content-type: application/json)"),
        422 => (codes::BAD_REQUEST, "the request body does not match the expected shape"),
        429 => (codes::RATE_LIMITED, "too many requests"),
        503 | 504 => (codes::UNAVAILABLE, "the server is unavailable, retry later"),
        s if s >= 500 => (codes::INTERNAL, "internal server error"),
        _ => return AppError::with_status(status, codes::BAD_REQUEST, "the request was refused"),
    };
    AppError::new(code, message)
}

/// Checks that a code the server sends has the status the protocol assigns to it (used in tests).
#[cfg(test)]
pub(crate) fn protocol_status(code: &str) -> Option<u16> {
    net_backend_protocol::error::http_status_for(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_of(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap_or_default();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn internal_errors_never_leak() {
        let secret = "SELECT password FROM users WHERE id = 7 -- connection refused at 10.0.0.5";
        let error = AppError::internal(io::Error::other(secret));
        assert!(format!("{error:?}").contains("connection refused"), "the source stays in logs");
        let (status, body) = body_of(error.into_response()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, serde_json::json!({"error":{"code":"internal","message":"internal server error"}}));
        assert!(!body.to_string().contains("SELECT"));
    }

    #[tokio::test]
    async fn sqlx_and_db_errors_are_internal() {
        let error: AppError = sqlx::Error::Protocol("table users has no column hash".into()).into();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let (_, body) = body_of(error.into_response()).await;
        assert!(!body.to_string().contains("users"));
        let error: AppError = Error::Migration("x".into()).into();
        assert_eq!(error.code(), codes::INTERNAL);
    }

    #[tokio::test]
    async fn codes_map_to_protocol_statuses() {
        for (error, code) in [
            (AppError::bad_request("x"), codes::BAD_REQUEST),
            (AppError::unauthorized(), codes::UNAUTHORIZED),
            (AppError::forbidden("x"), codes::FORBIDDEN),
            (AppError::not_found("x"), codes::NOT_FOUND),
            (AppError::conflict("x"), codes::CONFLICT),
            (AppError::payload_too_large("x"), codes::PAYLOAD_TOO_LARGE),
            (AppError::rate_limited(1500), codes::RATE_LIMITED),
            (AppError::unavailable("x"), codes::UNAVAILABLE),
            (AppError::internal(io::Error::other("x")), codes::INTERNAL),
            (AppError::validation(ValidationDetails::new()), codes::VALIDATION_FAILED),
        ] {
            assert_eq!(Some(error.status().as_u16()), protocol_status(code), "{code}");
            assert_eq!(error.code(), code);
        }
        let (status, body) = body_of(AppError::rate_limited(1500).into_response()).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["error"]["details"]["retry_after_ms"], 1500);
        // A game's own code: 400 unless a status is given.
        assert_eq!(AppError::new("craft_failed", "x").status(), StatusCode::BAD_REQUEST);
        assert_eq!(AppError::with_status(StatusCode::CONFLICT, "craft_failed", "x").status(), StatusCode::CONFLICT);
    }

    #[test]
    fn default_errors_have_the_protocol_status_of_their_code() {
        for status in [400u16, 401, 403, 404, 405, 408, 409, 413, 415, 422, 429, 500, 502, 503, 504] {
            let error = default_error_for(StatusCode::from_u16(status).unwrap_or(StatusCode::IM_A_TEAPOT));
            assert_eq!(Some(error.status().as_u16()), protocol_status(error.code()), "{status} -> {}", error.code());
        }
        assert_eq!(default_error_for(StatusCode::UNPROCESSABLE_ENTITY).status(), StatusCode::BAD_REQUEST);
        let teapot = default_error_for(StatusCode::IM_A_TEAPOT);
        assert_eq!((teapot.status(), teapot.code()), (StatusCode::IM_A_TEAPOT, codes::BAD_REQUEST));
    }
}
