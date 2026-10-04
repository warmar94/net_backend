//! The error body shared by HTTP and WebSocket: [`ApiError`] (`code`, `message`, optional
//! `details`) and the stable error [`codes`].
//!
//! - HTTP: the body of every 4xx / 5xx answer is an [`ErrorBody`]:
//!   `{"error":{"code":"not_found","message":"no such object"}}`.
//! - WebSocket: the `error` of a refused request, `{"id":7,"ok":false,"error":{…}}`, and of
//!   `auth.failed`.
//!
//! Clients branch on `code` (stable, snake_case, ASCII); `message` is human-readable English
//! text that may change at any time; `details` is optional, code-specific JSON (for
//! [`codes::VALIDATION_FAILED`]: `{"fields":{"password":["…"]}}`, see [`ValidationDetails`]).
//! Server modules and games may add their own codes; a client must treat an unknown code like
//! the HTTP status (or, over WebSocket, like a generic failure).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The stable error codes. Codes are part of the public API from 0.1.0: they are never renamed
/// and never change meaning; new ones may be added.
pub mod codes {
    /// The request is malformed (not JSON, wrong types, missing fields). HTTP 400.
    pub const BAD_REQUEST: &str = "bad_request";
    /// The request is well-formed but some fields break a rule; `details` is a
    /// [`ValidationDetails`](super::ValidationDetails). HTTP 422.
    pub const VALIDATION_FAILED: &str = "validation_failed";
    /// No valid credentials (missing, unknown or revoked token). HTTP 401.
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// The access token expired: refresh it and retry (on the WebSocket handshake: refresh, then
    /// connect again). HTTP 401.
    pub const TOKEN_EXPIRED: &str = "token_expired";
    /// Authenticated, but not allowed to do this. HTTP 403.
    pub const FORBIDDEN: &str = "forbidden";
    /// The thing does not exist (or the caller may not know that it does). HTTP 404.
    pub const NOT_FOUND: &str = "not_found";
    /// The HTTP method is not allowed on this route (the `Allow` header lists the allowed ones).
    /// HTTP 405.
    pub const METHOD_NOT_ALLOWED: &str = "method_not_allowed";
    /// The request body has an unsupported media type (JSON routes need
    /// `content-type: application/json`). HTTP 415.
    pub const UNSUPPORTED_MEDIA_TYPE: &str = "unsupported_media_type";
    /// The request conflicts with the current state. HTTP 409.
    pub const CONFLICT: &str = "conflict";
    /// Optimistic concurrency: the object's version is not the expected one; `details` is a
    /// [`VersionConflict`](crate::storage::VersionConflict): `{"current_version":N}` (absent when
    /// the object does not exist), plus `"index":i` for the failing item of a batch. HTTP 409.
    pub const VERSION_CONFLICT: &str = "version_conflict";
    /// The request body (or a WebSocket message, or a stored value) is too large. HTTP 413.
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
    /// Too many requests; `details` may hold `{"retry_after_ms":N}`. HTTP 429.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// A per-user quota is used up (e.g. the number of stored objects). HTTP 403.
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    /// The WebSocket request's `type` is not known to the server. (WebSocket only.)
    pub const UNKNOWN_TYPE: &str = "unknown_type";
    /// The client speaks a protocol version the server does not support; `details` holds
    /// `{"supported_min":N,"supported_max":N}`. HTTP 400 on plain HTTP routes ONLY. On the
    /// WebSocket endpoint never 400 (the client would retry forever): see
    /// [`version`](crate::version) (upgrade, then close 4010).
    pub const UNSUPPORTED_PROTOCOL: &str = "unsupported_protocol";
    /// Email + password did not match an account (deliberately not saying which). HTTP 401.
    pub const INVALID_CREDENTIALS: &str = "invalid_credentials";
    /// The refresh token was already used, outside the grace window
    /// ([`REFRESH_REUSE_GRACE_SECS`](crate::auth::REFRESH_REUSE_GRACE_SECS)): the whole token family
    /// is revoked (possible theft); log in again. Clients refresh single-flight and keep the old
    /// pair until the new one is stored. HTTP 401.
    pub const REFRESH_TOKEN_REUSED: &str = "refresh_token_reused";
    /// The email address is already registered. HTTP 409. A deliberate trade-off: it tells an
    /// attacker that the address has an account (enumeration), but a game needs a clear answer
    /// at sign-up; the server rate-limits registration to bound the damage.
    pub const EMAIL_TAKEN: &str = "email_taken";
    /// The action needs a verified email address. HTTP 403.
    pub const EMAIL_NOT_VERIFIED: &str = "email_not_verified";
    /// A one-time token (email verification, password reset) is unknown, used or expired. HTTP 400.
    pub const INVALID_TOKEN: &str = "invalid_token";
    /// The action needs a recent login (e.g. linking or unlinking a login provider): log in again,
    /// then retry. HTTP 403.
    pub const REAUTHENTICATION_REQUIRED: &str = "reauthentication_required";
    /// The account is banned. HTTP 403; WebSocket close 4003.
    pub const BANNED: &str = "banned";
    /// The Steam ticket was refused by Steam (or Steam could not be asked). HTTP 401.
    pub const STEAM_AUTH_FAILED: &str = "steam_auth_failed";
    /// An OpenID Connect ID token was refused (signature, issuer, audience, expiry, nonce, or a
    /// token used before). HTTP 401.
    pub const OAUTH_FAILED: &str = "oauth_failed";
    /// The chat room is full. HTTP 409.
    pub const ROOM_FULL: &str = "room_full";
    /// The caller is not a member of the chat room (join it first). HTTP 403.
    pub const NOT_A_MEMBER: &str = "not_a_member";
    /// A server hook did not answer in time. HTTP 503.
    pub const HOOK_TIMEOUT: &str = "hook_timeout";
    /// The server is overloaded or shutting down; retry later. HTTP 503.
    pub const UNAVAILABLE: &str = "unavailable";
    /// An unexpected server error; the message never contains internal details. HTTP 500.
    pub const INTERNAL: &str = "internal";
}

/// The HTTP status a server should answer for one of the [`codes`] (`None` for a code this crate
/// does not define, and for [`codes::UNKNOWN_TYPE`], which is WebSocket-only).
pub fn http_status_for(code: &str) -> Option<u16> {
    Some(match code {
        codes::BAD_REQUEST | codes::UNSUPPORTED_PROTOCOL | codes::INVALID_TOKEN => 400,
        codes::UNAUTHORIZED
        | codes::TOKEN_EXPIRED
        | codes::INVALID_CREDENTIALS
        | codes::REFRESH_TOKEN_REUSED
        | codes::STEAM_AUTH_FAILED
        | codes::OAUTH_FAILED => 401,
        codes::FORBIDDEN | codes::QUOTA_EXCEEDED | codes::EMAIL_NOT_VERIFIED | codes::BANNED | codes::NOT_A_MEMBER | codes::REAUTHENTICATION_REQUIRED => 403,
        codes::NOT_FOUND => 404,
        codes::METHOD_NOT_ALLOWED => 405,
        codes::UNSUPPORTED_MEDIA_TYPE => 415,
        codes::CONFLICT | codes::VERSION_CONFLICT | codes::EMAIL_TAKEN | codes::ROOM_FULL => 409,
        codes::PAYLOAD_TOO_LARGE => 413,
        codes::VALIDATION_FAILED => 422,
        codes::RATE_LIMITED => 429,
        codes::INTERNAL => 500,
        codes::HOOK_TIMEOUT | codes::UNAVAILABLE => 503,
        _ => return None,
    })
}

/// An error answer: a stable `code`, a human-readable `message` and optional `details`.
///
/// JSON: `{"code":"not_found","message":"no such object"}` (+ `"details":…` when present).
/// Unknown fields are ignored when decoding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApiError {
    /// The stable code (one of [`codes`], or a module's / game's own).
    pub code: String,
    /// A human-readable message (English; may change; never internal details).
    #[serde(default)]
    pub message: String,
    /// Code-specific details, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ApiError {
    /// An error with this code and message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), details: None }
    }

    /// The same error with these details.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// A [`codes::VALIDATION_FAILED`] error with these field messages as details.
    pub fn validation(details: ValidationDetails) -> Self {
        let details = serde_json::to_value(&details).unwrap_or(Value::Null);
        Self::new(codes::VALIDATION_FAILED, "the request is invalid").with_details(details)
    }

    /// Whether the code is `code`.
    pub fn is(&self, code: &str) -> bool {
        self.code == code
    }

    /// The HTTP status for this error's code ([`http_status_for`]), or 400 for an unknown code.
    pub fn http_status(&self) -> u16 {
        http_status_for(&self.code).unwrap_or(400)
    }

    /// The details decoded as `T` (e.g. [`ValidationDetails`]); `None` if absent or of another shape.
    pub fn details_as<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        self.details.as_ref().and_then(|details| T::deserialize(details).ok())
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            f.write_str(&self.code)
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

impl std::error::Error for ApiError {}

/// The body of an HTTP error answer: `{"error":{…}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ErrorBody {
    /// The error.
    pub error: ApiError,
}

impl ErrorBody {
    /// The body for this error.
    pub fn new(error: ApiError) -> Self {
        Self { error }
    }
}

impl From<ApiError> for ErrorBody {
    fn from(error: ApiError) -> Self {
        Self { error }
    }
}

/// The `details` of a [`codes::VALIDATION_FAILED`] error: messages per field.
///
/// JSON: `{"fields":{"email":["is not an email address"],"password":["is too short"]}}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ValidationDetails {
    /// The messages for each field, by field name (the JSON name, dotted for nested fields).
    #[serde(default)]
    pub fields: BTreeMap<String, Vec<String>>,
}

impl ValidationDetails {
    /// No messages yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a message for `field`.
    pub fn add(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.fields.entry(field.into()).or_default().push(message.into());
    }

    /// Whether there is no message.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// `Ok(())` when empty, otherwise the [`ApiError::validation`] error.
    pub fn into_result(self) -> Result<(), ApiError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(ApiError::validation(self))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_status() {
        let all = [
            codes::BAD_REQUEST,
            codes::VALIDATION_FAILED,
            codes::UNAUTHORIZED,
            codes::TOKEN_EXPIRED,
            codes::FORBIDDEN,
            codes::NOT_FOUND,
            codes::METHOD_NOT_ALLOWED,
            codes::UNSUPPORTED_MEDIA_TYPE,
            codes::CONFLICT,
            codes::VERSION_CONFLICT,
            codes::PAYLOAD_TOO_LARGE,
            codes::RATE_LIMITED,
            codes::QUOTA_EXCEEDED,
            codes::UNSUPPORTED_PROTOCOL,
            codes::INVALID_CREDENTIALS,
            codes::REFRESH_TOKEN_REUSED,
            codes::EMAIL_TAKEN,
            codes::EMAIL_NOT_VERIFIED,
            codes::INVALID_TOKEN,
            codes::BANNED,
            codes::REAUTHENTICATION_REQUIRED,
            codes::STEAM_AUTH_FAILED,
            codes::OAUTH_FAILED,
            codes::ROOM_FULL,
            codes::NOT_A_MEMBER,
            codes::HOOK_TIMEOUT,
            codes::UNAVAILABLE,
            codes::INTERNAL,
        ];
        for code in all {
            assert!(http_status_for(code).is_some(), "{code}");
            assert!(code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'), "{code}");
        }
        assert_eq!(http_status_for(codes::UNKNOWN_TYPE), None);
        assert_eq!(ApiError::new("game_specific", "x").http_status(), 400);
    }

    #[test]
    fn validation_details() {
        let mut details = ValidationDetails::new();
        assert!(details.clone().into_result().is_ok());
        details.add("password", "is too short");
        let error = details.clone().into_result().err().unwrap_or_else(|| ApiError::new("x", "x"));
        assert!(error.is(codes::VALIDATION_FAILED));
        assert_eq!(error.details_as::<ValidationDetails>(), Some(details));
        assert_eq!(error.to_string(), "validation_failed: the request is invalid");
        assert_eq!(ApiError::new("x", "").to_string(), "x");
    }
}
