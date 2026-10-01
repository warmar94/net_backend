//! Typed HTTP calls: [`HttpCall`] pairs a request type with its route (method, path template,
//! authentication), its payload (JSON body, query string or nothing) and its answer type, like
//! [`WsCall`](crate::WsCall) does for WebSocket requests. Server, client and any Rust app share
//! the contract, so a path, a method or an answer type cannot drift between them.
//!
//! Every route of [`routes::ALL`](crate::routes::ALL) has exactly one `HttpCall` type:
//!
//! - a route without path parameters uses its body / query type directly (e.g.
//!   [`LoginRequest`](crate::auth::LoginRequest), [`BatchGet`](crate::storage::BatchGet),
//!   [`AuditQuery`](crate::admin::AuditQuery));
//! - a route with path parameters (or without any payload) has a small call type holding the
//!   parameters and the payload (e.g. [`GetObject`](crate::storage::GetObject) for
//!   `GET /v1/storage/{collection}/{key}`, [`WriteObject`](crate::storage::WriteObject) for the
//!   PUT with its [`PutObject`](crate::storage::PutObject) body). Call types are not wire types:
//!   what travels is the path, the payload and the answer.
//!
//! **A client** sends `C::ROUTE.method` to [`path`](HttpCall::path), with
//! [`payload`](HttpCall::payload) as the JSON body ([`PayloadKind::Json`]), as the query string
//! ([`PayloadKind::Query`]) or not at all ([`PayloadKind::Empty`]), plus
//! `Authorization: Bearer <token>` when `C::ROUTE.auth`; a 2xx answer decodes as
//! [`Response`](HttpCall::Response), anything else as the protocol's
//! [`ErrorBody`](crate::ErrorBody) (every error of every route has that shape).
//!
//! **A server** mounts a handler at `C::ROUTE.path` for `C::ROUTE.method` and rebuilds the call
//! with [`from_parts`](HttpCall::from_parts) (path parameters + decoded payload): the shape rules
//! of the path parameters (ids are numbers, storage names are valid names) are checked there.
//!
//! ```
//! use net_backend_protocol::storage::{GetObject, PutObject, WriteObject};
//! use net_backend_protocol::{HttpCall, PayloadKind};
//!
//! let call = WriteObject::new("saves", "slot-1", PutObject::new(serde_json::json!({"level": 3})));
//! assert_eq!(WriteObject::ROUTE.method.as_str(), "PUT");
//! assert_eq!(call.path().as_deref(), Some("/v1/storage/saves/slot-1"));
//! assert_eq!(WriteObject::PAYLOAD, PayloadKind::Json);
//! assert_eq!(serde_json::to_string(call.payload()).ok().as_deref(), Some(r#"{"value":{"level":3}}"#));
//! // A name that would need escaping has no path: nothing is sent.
//! assert_eq!(GetObject::new("saves", "../etc").path(), None);
//! ```

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{codes, ApiError};
use crate::routes::Route;

/// How a call's [`payload`](HttpCall::payload) travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PayloadKind {
    /// Nothing is sent (the payload is [`NoPayload`]).
    Empty,
    /// The JSON body (`content-type: application/json`).
    Json,
    /// The query string (`?cursor=…&limit=50`, urlencoded).
    Query,
}

/// The payload of a call that sends nothing. Serializes as `{}`; decodes from anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub struct NoPayload {}

impl NoPayload {
    /// The empty payload.
    pub const fn new() -> Self {
        Self {}
    }
}

impl<'de> Deserialize<'de> for NoPayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(NoPayload {})
    }
}

/// The empty payload, borrowed for [`HttpCall::payload`].
pub static NO_PAYLOAD: NoPayload = NoPayload {};

/// A typed HTTP request: its route, payload and answer.
///
/// Methods added later always come with a default implementation.
pub trait HttpCall: Sized {
    /// What is sent: the JSON body, the query parameters, or [`NoPayload`].
    type Payload: Serialize + DeserializeOwned;
    /// The answer's JSON on success (2xx).
    type Response: Serialize + DeserializeOwned;
    /// The route: method, path template (`{param}` placeholders) and whether a Bearer token is
    /// required. Always one entry of [`routes::ALL`](crate::routes::ALL).
    const ROUTE: Route;
    /// How the payload travels.
    const PAYLOAD: PayloadKind;

    /// The payload to send.
    fn payload(&self) -> &Self::Payload;

    /// The values of the path's placeholders (none for a route without parameters).
    fn path_params(&self) -> PathParams {
        PathParams::new()
    }

    /// The call from what a server received: the path parameters by name and the decoded
    /// payload. Checks the shape of the path parameters (400 `bad_request` for an id that is not
    /// a number or an invalid name); the payload's own rules are its `validate()`, which the
    /// server runs.
    fn from_parts(params: &PathParams, payload: Self::Payload) -> Result<Self, ApiError>;

    /// The concrete path (`/v1/storage/saves/slot-1`), or `None` if a parameter is missing or
    /// would need escaping (only path-safe values are ever sent).
    fn path(&self) -> Option<String> {
        self.path_params().fill(Self::ROUTE.path)
    }
}

/// Whether a path parameter value can go into a path as it is (RFC 3986 `pchar` without
/// percent-encoding): 1 to 256 characters of the unreserved set (`A-Z a-z 0-9 - . _ ~`), the
/// sub-delimiters (`! $ & ' ( ) * + , ; =`), `:` and `@`; not `.` or `..`. Everything else (`/ ?
/// # % \ " < > ^` the backtick, `{ | }`, spaces, non-ASCII) would need escaping, which some client
/// libraries do differently: such a value is never sent.
pub fn is_path_safe(value: &str) -> bool {
    (1..=256).contains(&value.len())
        && value != "."
        && value != ".."
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'-' | b'.' | b'_' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b':' | b'@')
        })
}

/// The query parameters of a payload (a [`PayloadKind::Query`] call's
/// [`payload`](HttpCall::payload)) as name / value pairs, for a client library to urlencode: absent
/// (`None`) fields are left out (never sent as `null`), numbers and booleans become text. A
/// payload that is not a flat object (a nested object or array): 400 `bad_request`.
///
/// ```
/// use net_backend_protocol::http_call::query_pairs;
/// use net_backend_protocol::PageRequest;
///
/// assert_eq!(query_pairs(&PageRequest::first().with_limit(20)).ok(), Some(vec![("limit".to_string(), "20".to_string())]));
/// ```
pub fn query_pairs<T: Serialize + ?Sized>(payload: &T) -> Result<Vec<(String, String)>, ApiError> {
    let bad = |message: &str| ApiError::new(codes::BAD_REQUEST, message);
    let value = serde_json::to_value(payload).map_err(|_| bad("the query payload cannot be encoded"))?;
    let serde_json::Value::Object(fields) = value else { return Err(bad("a query payload must be an object")) };
    let mut pairs = Vec::with_capacity(fields.len());
    for (name, value) in fields {
        let text = match value {
            serde_json::Value::Null => continue,
            serde_json::Value::String(text) => text,
            serde_json::Value::Bool(flag) => flag.to_string(),
            serde_json::Value::Number(number) => number.to_string(),
            _ => return Err(bad("a query payload's fields must be strings, numbers or booleans")),
        };
        pairs.push((name, text));
    }
    Ok(pairs)
}

/// The values of a route's `{param}` placeholders, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathParams(Vec<(String, String)>);

impl PathParams {
    /// No parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same parameters plus `name` = `value`.
    pub fn with(mut self, name: &str, value: impl ToString) -> Self {
        self.insert(name, value.to_string());
        self
    }

    /// Set `name` (replacing an earlier value).
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let (name, value) = (name.into(), value.into());
        match self.0.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name, value)),
        }
    }

    /// The value of `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// Every parameter, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// The value of `name`, or `bad_request` if it is missing.
    pub fn require(&self, name: &str) -> Result<&str, ApiError> {
        self.get(name).ok_or_else(|| ApiError::new(codes::BAD_REQUEST, format!("the path parameter `{name}` is missing")))
    }

    /// The value of `name` as an id (a signed 64-bit number), or `bad_request`.
    pub fn id<T: From<i64>>(&self, name: &str) -> Result<T, ApiError> {
        self.require(name)?.parse::<i64>().map(T::from).map_err(|_| ApiError::new(codes::BAD_REQUEST, format!("the path parameter `{name}` is not a number")))
    }

    /// The value of `name` checked by `rule`, or `bad_request` naming it with `problem` (a path
    /// segment is not a field: a malformed one is a bad request, like a malformed body).
    pub fn checked(&self, name: &str, rule: impl Fn(&str) -> bool, problem: &str) -> Result<String, ApiError> {
        let value = self.require(name)?;
        if rule(value) {
            Ok(value.to_string())
        } else {
            Err(ApiError::new(codes::BAD_REQUEST, format!("the path parameter `{name}` {problem}")))
        }
    }

    /// The template with every `{name}` replaced by its value, or `None` if a value is missing or
    /// not path-safe ([`is_path_safe`]).
    pub fn fill(&self, template: &str) -> Option<String> {
        let mut out = String::with_capacity(template.len() + 16);
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let close = after.find('}')?;
            let value = self.get(&after[..close])?;
            if !is_path_safe(value) {
                return None;
            }
            out.push_str(value);
            rest = &after[close + 1..];
        }
        out.push_str(rest);
        Some(out)
    }
}

/// The placeholder names of a path template, in order (`/v1/storage/{collection}/{key}` →
/// `collection`, `key`).
pub fn placeholders(template: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        names.push(&after[..close]);
        rest = &after[close + 1..];
    }
    names
}

/// `HttpCall` for a type that IS the payload of a route without path parameters.
macro_rules! payload_call {
    ($ty:ty, $method:ident, $path:expr, $auth:expr, $kind:ident, $response:ty) => {
        impl $crate::http_call::HttpCall for $ty {
            type Payload = Self;
            type Response = $response;
            const ROUTE: $crate::routes::Route = $crate::routes::Route::new($crate::routes::HttpMethod::$method, $path, $auth);
            const PAYLOAD: $crate::http_call::PayloadKind = $crate::http_call::PayloadKind::$kind;

            fn payload(&self) -> &Self {
                self
            }

            fn from_parts(_params: &$crate::http_call::PathParams, payload: Self) -> Result<Self, $crate::error::ApiError> {
                Ok(payload)
            }
        }
    };
}
pub(crate) use payload_call;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_and_safety() {
        let params = PathParams::new().with("collection", "saves").with("key", "slot-1");
        assert_eq!(params.fill("/v1/storage/{collection}/{key}").as_deref(), Some("/v1/storage/saves/slot-1"));
        assert_eq!(params.fill("/v1/info").as_deref(), Some("/v1/info"));
        assert_eq!(PathParams::new().fill("/v1/storage/{collection}"), None);
        assert_eq!(PathParams::new().with("x", "a/b").fill("/{x}"), None);
        for bad in ["", ".", "..", "a b", "a%2F", "a?b", "a#b", "a\\b", "ä", &"x".repeat(257)] {
            assert!(!is_path_safe(bad), "{bad:?}");
        }
        assert!(is_path_safe("slot-1.json") && is_path_safe("42") && is_path_safe("-7"));
        assert_eq!(placeholders("/v1/admin/users/{user}/roles/{role}"), ["user", "role"]);
        let mut replaced = PathParams::new().with("a", 1);
        replaced.insert("a", "2");
        assert_eq!(replaced.get("a"), Some("2"));
        assert_eq!(replaced.iter().count(), 1);
    }

    #[test]
    fn parameter_errors() {
        let params = PathParams::new().with("user", "x").with("name", "../a");
        assert_eq!(params.id::<crate::UserId>("user").err().map(|e| e.code), Some(codes::BAD_REQUEST.to_string()));
        assert_eq!(params.id::<crate::UserId>("missing").err().map(|e| e.code), Some(codes::BAD_REQUEST.to_string()));
        assert_eq!(PathParams::new().with("user", "-3").id::<crate::UserId>("user").ok(), Some(crate::UserId(-3)));
        let error = params.checked("name", crate::storage::is_valid_name, "is not a valid storage name").err();
        assert_eq!(error.as_ref().map(|e| e.code.as_str()), Some(codes::BAD_REQUEST));
        assert_eq!(error.map(|e| e.message).as_deref(), Some("the path parameter `name` is not a valid storage name"));
        assert_eq!(serde_json::to_string(&NoPayload::new()).ok().as_deref(), Some("{}"));
        assert_eq!(serde_json::from_str::<NoPayload>("null").ok(), Some(NoPayload::new()));
    }
}
