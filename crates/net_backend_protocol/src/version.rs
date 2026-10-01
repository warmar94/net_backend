//! Protocol versioning.
//!
//! Two layers:
//! - the **route prefix** `/v1` ([`routes::PREFIX`](crate::routes::PREFIX)): a breaking change
//!   to the API would get `/v2` routes, served next to `/v1` for a while;
//! - the **protocol version** [`PROTOCOL_VERSION`], a number that grows when messages are added
//!   within `/v1` (additive changes only). A client names the version it speaks in the
//!   [`PROTOCOL_HEADER`] (HTTP requests and the WebSocket handshake) or in the `protocol` field of
//!   the first-message `auth`; a client that names none is treated as version 1. The server
//!   answers every HTTP response with the same header (its own version), puts its version into
//!   `auth.ok`, and lists what it accepts at [`routes::INFO`](crate::routes::INFO) ([`ServerInfo`]).
//!
//! **Refusing a version it does not support** (code
//! [`codes::UNSUPPORTED_PROTOCOL`](crate::codes::UNSUPPORTED_PROTOCOL)):
//! - plain HTTP routes: status [`HTTP_REFUSAL_STATUS`] (400) with the error body;
//! - the WebSocket endpoint: NEVER 400 (`bevy_net_backend` retries every handshake status except
//!   401 / 403, so old clients would hammer the server forever). The server accepts the upgrade
//!   and closes at once with [`WS_REFUSAL_CLOSE`] (4010, in the no-reconnect range 4000–4099).
//!   A server that must refuse before the upgrade answers [`WS_HANDSHAKE_REFUSAL_STATUS`] (403),
//!   which the client also treats as permanent.

use serde::{Deserialize, Serialize};

use crate::envelope::CloseCode;

/// The protocol version this crate describes.
pub const PROTOCOL_VERSION: u32 = 1;

/// The header carrying a protocol version (lower-case, as HTTP/2 sends it; HTTP headers are
/// case-insensitive): `x-net-backend-protocol: 1`.
pub const PROTOCOL_HEADER: &str = "x-net-backend-protocol";

/// The status for an unsupported protocol version on a plain HTTP route (never on `/v1/ws`).
pub const HTTP_REFUSAL_STATUS: u16 = 400;

/// How the WebSocket endpoint refuses an unsupported protocol version: upgrade, then close 4010.
pub const WS_REFUSAL_CLOSE: CloseCode = CloseCode::UNSUPPORTED_PROTOCOL;

/// The handshake status if the WebSocket endpoint refuses before the upgrade: 403 (permanent for
/// the client; a 400 would be retried forever).
pub const WS_HANDSHAKE_REFUSAL_STATUS: u16 = 403;

/// What a server tells about itself: `GET /v1/info` (no authentication).
///
/// JSON: `{"protocol":1,"min_protocol":1,"modules":["auth","chat","storage"]}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ServerInfo {
    /// The newest protocol version the server speaks.
    pub protocol: u32,
    /// The oldest protocol version the server still accepts.
    pub min_protocol: u32,
    /// The enabled modules (`"auth"`, `"storage"`, `"chat"`, …).
    #[serde(default)]
    pub modules: Vec<String>,
}

impl ServerInfo {
    /// A server speaking exactly this crate's version with these modules.
    pub fn new(modules: Vec<String>) -> Self {
        Self { protocol: PROTOCOL_VERSION, min_protocol: PROTOCOL_VERSION, modules }
    }

    /// The same info with this accepted version range.
    pub fn with_versions(mut self, min_protocol: u32, protocol: u32) -> Self {
        self.min_protocol = min_protocol;
        self.protocol = protocol;
        self
    }

    /// Whether the server accepts protocol `version`.
    pub fn supports(&self, version: u32) -> bool {
        (self.min_protocol..=self.protocol).contains(&version)
    }

    /// Whether the module `name` is enabled.
    pub fn has_module(&self, name: &str) -> bool {
        self.modules.iter().any(|m| m == name)
    }
}

/// Server facts: `GET /v1/info` (no authentication, no payload) → [`ServerInfo`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GetServerInfo {}

impl GetServerInfo {
    /// The call.
    pub const fn new() -> Self {
        Self {}
    }
}

impl crate::http_call::HttpCall for GetServerInfo {
    type Payload = crate::http_call::NoPayload;
    type Response = ServerInfo;
    const ROUTE: crate::routes::Route = crate::routes::Route::new(crate::routes::HttpMethod::Get, crate::routes::INFO, false);
    const PAYLOAD: crate::http_call::PayloadKind = crate::http_call::PayloadKind::Empty;

    fn payload(&self) -> &crate::http_call::NoPayload {
        &crate::http_call::NO_PAYLOAD
    }

    fn from_parts(_params: &crate::http_call::PathParams, _payload: crate::http_call::NoPayload) -> Result<Self, crate::ApiError> {
        Ok(Self::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info() {
        let info = ServerInfo::new(vec!["chat".into()]).with_versions(1, 3);
        assert!(info.supports(1) && info.supports(3) && !info.supports(4) && !info.supports(0));
        assert!(info.has_module("chat") && !info.has_module("storage"));
        assert!(WS_REFUSAL_CLOSE.is_permanent());
        assert_ne!(WS_HANDSHAKE_REFUSAL_STATUS, HTTP_REFUSAL_STATUS);
    }
}
