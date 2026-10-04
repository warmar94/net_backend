//! The WebSocket envelope: JSON objects in text frames, exactly as `bevy_net_backend`'s
//! `JsonEnvelope` speaks them.
//!
//! | Direction | Frame | JSON |
//! |---|---|---|
//! | client → server | request ([`WsRequestFrame`]) | `{"id":7,"type":"chat.send","data":{…}}` |
//! | server → client | answer ([`WsResponseFrame`]) | `{"id":7,"ok":true,"data":{…}}` or `{"id":7,"ok":false,"error":{"code":…,"message":…}}` |
//! | server → client | push ([`WsPushFrame`]) | `{"type":"chat.message","data":{…}}` (never an `ok` field) |
//! | client → server | first-message auth ([`WsAuth`]) | `{"type":"auth","data":{"token":"…","protocol":1}}` (no `id`) |
//! | server → client | auth accepted | `{"type":"auth.ok","data":{"user_id":42,"protocol":1}}` |
//! | server → client | auth refused | `{"type":"auth.failed","error":{…}}`, then close 4001 |
//!
//! Rules the client relies on (and [`WsServerFrame`] decodes the same way):
//! - `id` is a JSON number, an unsigned 64-bit integer, echoed unchanged in the answer.
//! - A frame with a numeric `id` AND (an `ok` field OR no `type`) is an answer; a missing `ok`
//!   means `true`, a missing `data` means `null`.
//! - Anything else with a `type` is a push, `auth.ok` or `auth.failed`. A push therefore never has
//!   an `ok` field, and the server never puts an `id` on a push (message ids go inside `data`).
//! - Binary frames and non-JSON text are not part of the protocol.
//! - The order of keys inside an object carries no meaning.
//!
//! Authentication: either `Authorization: Bearer <token>` on the handshake (a bad token is a 401
//! before the upgrade, which the client does not retry; an expired one is 401 `token_expired`),
//! or the first-message `auth` within [`AUTH_TIMEOUT_SECS`] (otherwise close 1008). Rules:
//! - **Every `auth` message gets exactly one answer**, `auth.ok` or `auth.failed`, also on a
//!   socket already authenticated by its handshake header (a client using both, with the
//!   client's `with_auth_ack`, must not wait forever). The one exception: a TEMPORARY server
//!   failure (its database is down, overloaded) closes the socket with 1013 without an answer,
//!   so the client reconnects and tries again; `auth.failed` is always a definitive refusal.
//! - A later `auth` on an open socket **re-authenticates** it with a fresh token (`auth.ok`); a
//!   token of a DIFFERENT user is refused (`auth.failed`, then close 4001).
//! - An open socket survives the expiry of its access token; only a revocation closes it (4001,
//!   or 4003 for a ban). See [`auth`](crate::auth) for the refresh recipe.
//! - `auth.failed` is followed by close 4001; the client does not reconnect by itself.
//!
//! Malformed requests: a frame that has a usable `id` (an unsigned integer) is always answered,
//! with `bad_request` if the rest is broken ([`FrameError::id`]); only frames without a usable id
//! (not JSON, not an object, no / bad `id`) cannot be answered and are dropped (and counted).
//!
//! Close codes: [`CloseCode`].

use std::fmt;

use serde::de::{DeserializeOwned, Error as _};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::auth::AccessToken;
use crate::error::{codes, ApiError};
use crate::ids::UserId;
use crate::kinds;
use crate::version::PROTOCOL_VERSION;

/// The largest WebSocket message, in bytes (both directions; the same as the client's default
/// message limit). A bigger one is closed with 1009.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// How long a socket may stay unauthenticated (no handshake token, no `auth` message yet) before
/// the server closes it with 1008.
pub const AUTH_TIMEOUT_SECS: u64 = 5;

/// A WebSocket request kind with its answer type: implemented by every request of this crate
/// (e.g. [`SendMessage`](crate::chat::SendMessage) → [`SendAck`](crate::chat::SendAck)), so a
/// server can route by [`KIND`](WsCall::KIND) and a client knows what comes back.
pub trait WsCall: Serialize + DeserializeOwned {
    /// The answer's `data`.
    type Response: Serialize + DeserializeOwned;
    /// The `type` on the wire (one of [`kinds`]).
    const KIND: &'static str;
}

/// A server push kind: implemented by every push of this crate (e.g.
/// [`ChatMessage`](crate::chat::ChatMessage) for `chat.message`).
pub trait ServerPush: Serialize + DeserializeOwned {
    /// The `type` on the wire (one of [`kinds`]).
    const KIND: &'static str;
}

/// An empty success answer: `{}` on the wire. Decoding accepts anything (also `null` or an
/// object with fields a newer server added).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Ack {}

impl Ack {
    /// The acknowledgement.
    pub const fn new() -> Self {
        Self {}
    }
}

impl<'de> Deserialize<'de> for Ack {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(Ack {})
    }
}

/// A WebSocket close code. The server uses the named ones; the client reconnects after every code
/// except 4000–4099 ([`is_permanent`](CloseCode::is_permanent)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CloseCode(pub u16);

impl CloseCode {
    /// 1000: normal closure (e.g. the client logged out).
    pub const NORMAL: CloseCode = CloseCode(1000);
    /// 1001: the server is shutting down or redeploying; reconnect.
    pub const GOING_AWAY: CloseCode = CloseCode(1001);
    /// 1008: policy violation (rate limit, no authentication in time, malformed traffic).
    pub const POLICY_VIOLATION: CloseCode = CloseCode(1008);
    /// 1009: a message over [`MAX_MESSAGE_BYTES`].
    pub const MESSAGE_TOO_BIG: CloseCode = CloseCode(1009);
    /// 1011: an unexpected server error; reconnect.
    pub const INTERNAL_ERROR: CloseCode = CloseCode(1011);
    /// 1013: overloaded, or this connection could not keep up with its messages; reconnect later
    /// (and resync).
    pub const TRY_AGAIN_LATER: CloseCode = CloseCode(1013);
    /// 4001: authentication refused or revoked (logout, password change, admin): log in again.
    pub const UNAUTHORIZED: CloseCode = CloseCode(4001);
    /// 4003: the account is banned.
    pub const BANNED: CloseCode = CloseCode(4003);
    /// 4009: replaced by a newer connection of the same user or session (e.g. the user opened more
    /// connections than the server allows; the oldest goes).
    pub const REPLACED: CloseCode = CloseCode(4009);
    /// 4010: the client's protocol version is not supported (see [`crate::version`]).
    pub const UNSUPPORTED_PROTOCOL: CloseCode = CloseCode(4010);

    /// Every code named above.
    pub const ALL: &'static [CloseCode] = &[
        Self::NORMAL,
        Self::GOING_AWAY,
        Self::POLICY_VIOLATION,
        Self::MESSAGE_TOO_BIG,
        Self::INTERNAL_ERROR,
        Self::TRY_AGAIN_LATER,
        Self::UNAUTHORIZED,
        Self::BANNED,
        Self::REPLACED,
        Self::UNSUPPORTED_PROTOCOL,
    ];

    /// The number.
    pub const fn get(self) -> u16 {
        self.0
    }

    /// Whether this means "do not come back": 4000–4099, the range the client never retries.
    pub const fn is_permanent(self) -> bool {
        self.0 >= 4000 && self.0 < 4100
    }
}

impl fmt::Display for CloseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<u16> for CloseCode {
    fn from(code: u16) -> Self {
        CloseCode(code)
    }
}

impl From<CloseCode> for u16 {
    fn from(code: CloseCode) -> Self {
        code.0
    }
}

/// `{"type":…,"data":…}` with a borrowed kind and payload (serializing only).
#[derive(Serialize)]
struct TypedFrame<'a, D: Serialize> {
    #[serde(rename = "type")]
    kind: &'a str,
    data: &'a D,
}

/// An error payload as an [`ApiError`]; a payload of another shape (a server that does not follow
/// this protocol) becomes `fallback_code` with the raw payload as `details`.
fn error_from(value: Option<Value>, fallback_code: &str) -> ApiError {
    match value {
        Some(value) => match ApiError::deserialize(&value) {
            Ok(error) => error,
            Err(_) => ApiError::new(fallback_code, "the error payload is not an API error").with_details(value),
        },
        None => ApiError::new(fallback_code, ""),
    }
}

/// Whether a JSON object is an answer by the client's rule: a numeric `id` and (`ok` or no `type`).
fn response_id(object: &Map<String, Value>) -> Option<u64> {
    let id = object.get("id").and_then(Value::as_u64)?;
    (object.contains_key("ok") || object.get("type").and_then(Value::as_str).is_none()).then_some(id)
}

/// A request from the client: `{"id":7,"type":"chat.send","data":{…}}`.
///
/// `T` is the payload: [`Value`] for "any" (a server decoding before it knows the kind), or a
/// typed request ([`WsRequestFrame::call`]). A missing `data` decodes as `null`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct WsRequestFrame<T = Value> {
    /// The request id (unique per client process), echoed in the answer.
    pub id: u64,
    /// The kind (one of [`kinds`], or a game's own).
    #[serde(rename = "type")]
    pub kind: String,
    /// The payload.
    pub data: T,
}

impl<T> WsRequestFrame<T> {
    /// A request of `kind` with this payload.
    pub fn new(id: u64, kind: impl Into<String>, data: T) -> Self {
        Self { id, kind: kind.into(), data }
    }
}

impl<C: WsCall> WsRequestFrame<C> {
    /// A typed request with its kind ([`WsCall::KIND`]).
    pub fn call(id: u64, call: C) -> Self {
        Self { id, kind: C::KIND.to_string(), data: call }
    }
}

impl WsRequestFrame<Value> {
    /// The payload decoded as `T` (e.g. the request type for this frame's kind). On an error,
    /// answer `bad_request` to [`id`](WsRequestFrame::id): the frame is still a request.
    pub fn data_as<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(&self.data)
    }
}

#[derive(Deserialize)]
struct RawRequest {
    id: u64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: Value,
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for WsRequestFrame<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawRequest::deserialize(deserializer)?;
        let data = T::deserialize(raw.data).map_err(D::Error::custom)?;
        Ok(Self { id: raw.id, kind: raw.kind, data })
    }
}

/// The answer to a request: `{"id":7,"ok":true,"data":…}` or `{"id":7,"ok":false,"error":{…}}`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct WsResponseFrame<T = Value> {
    /// The request's id.
    pub id: u64,
    /// The payload (`ok: true`) or the error (`ok: false`).
    pub result: Result<T, ApiError>,
}

impl<T> WsResponseFrame<T> {
    /// A success answer.
    pub fn ok(id: u64, data: T) -> Self {
        Self { id, result: Ok(data) }
    }

    /// An error answer.
    pub fn error(id: u64, error: ApiError) -> Self {
        Self { id, result: Err(error) }
    }

    /// Whether it is a success.
    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }
}

impl WsResponseFrame<Value> {
    /// The answer to the request `id` with a payload of any serializable type.
    pub fn ok_serialize<T: Serialize + ?Sized>(id: u64, data: &T) -> Result<Self, serde_json::Error> {
        Ok(Self::ok(id, serde_json::to_value(data)?))
    }

    /// The payload decoded as `T` (the error stays an error).
    pub fn decode<T: DeserializeOwned>(&self) -> Result<Result<T, ApiError>, serde_json::Error> {
        match &self.result {
            Ok(data) => T::deserialize(data).map(Ok),
            Err(error) => Ok(Err(error.clone())),
        }
    }
}

impl<T: Serialize> Serialize for WsResponseFrame<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut frame = serializer.serialize_struct("WsResponseFrame", 3)?;
        frame.serialize_field("id", &self.id)?;
        match &self.result {
            Ok(data) => {
                frame.serialize_field("ok", &true)?;
                frame.serialize_field("data", data)?;
            }
            Err(error) => {
                frame.serialize_field("ok", &false)?;
                frame.serialize_field("error", error)?;
            }
        }
        frame.end()
    }
}

fn response_from_object<T: DeserializeOwned>(mut object: Map<String, Value>) -> Result<WsResponseFrame<T>, String> {
    let id = response_id(&object).ok_or("not an answer: it needs a numeric `id` and an `ok` field or no `type`")?;
    let ok = object.get("ok").and_then(Value::as_bool).unwrap_or(true);
    let result = if ok {
        let data = object.remove("data").unwrap_or(Value::Null);
        Ok(T::deserialize(data).map_err(|e| e.to_string())?)
    } else {
        Err(error_from(object.remove("error"), codes::INTERNAL))
    };
    Ok(WsResponseFrame { id, result })
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for WsResponseFrame<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Map::<String, Value>::deserialize(deserializer)?;
        response_from_object(object).map_err(D::Error::custom)
    }
}

/// A server push: `{"type":"chat.message","data":{…}}`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct WsPushFrame<T = Value> {
    /// The kind (one of [`kinds`], or a game's own; never `auth.ok` / `auth.failed`).
    pub kind: String,
    /// The payload.
    pub data: T,
}

impl<T> WsPushFrame<T> {
    /// A push of `kind` with this payload. The kind is NOT checked: never use `auth`, `auth.ok`
    /// or `auth.failed` (see [`checked`](WsPushFrame::checked)).
    pub fn new(kind: impl Into<String>, data: T) -> Self {
        Self { kind: kind.into(), data }
    }
}

impl<T> WsPushFrame<T> {
    /// A push of `kind`, or `None` if `kind` is reserved for authentication (`auth`, `auth.ok`,
    /// `auth.failed`: the client would read such a "push" as an auth result) or empty.
    /// [`new`](WsPushFrame::new) does not check; use this for kinds that come from outside code.
    pub fn checked(kind: impl Into<String>, data: T) -> Option<Self> {
        let kind = kind.into();
        (!kind.is_empty() && !kinds::is_reserved(&kind)).then_some(Self { kind, data })
    }
}

impl<P: ServerPush> WsPushFrame<P> {
    /// A typed push with its kind ([`ServerPush::KIND`]).
    pub fn push(data: P) -> Self {
        Self { kind: P::KIND.to_string(), data }
    }
}

impl WsPushFrame<Value> {
    /// The payload decoded as `T`.
    pub fn data_as<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(&self.data)
    }
}

impl<T: Serialize> Serialize for WsPushFrame<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        TypedFrame { kind: &self.kind, data: &self.data }.serialize(serializer)
    }
}

fn push_from_object<T: DeserializeOwned>(mut object: Map<String, Value>) -> Result<WsPushFrame<T>, String> {
    if response_id(&object).is_some() {
        return Err("not a push: it is an answer".into());
    }
    let kind = match object.get("type").and_then(Value::as_str) {
        Some(kinds::AUTH_OK | kinds::AUTH_FAILED) => return Err("not a push: it is an auth result".into()),
        Some(kind) => kind.to_string(),
        None => return Err("not a push: no `type`".into()),
    };
    let data = T::deserialize(object.remove("data").unwrap_or(Value::Null)).map_err(|e| e.to_string())?;
    Ok(WsPushFrame { kind, data })
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for WsPushFrame<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Map::<String, Value>::deserialize(deserializer)?;
        push_from_object(object).map_err(D::Error::custom)
    }
}

/// First-message authentication, the `data` of `{"type":"auth","data":{…}}`. Send it as the very
/// first frame (with `bevy_net_backend`: return [`to_message`](WsAuth::to_message) from
/// `Credentials::ws_auth_message`). `Debug` never shows the token.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WsAuth {
    /// The access token.
    pub token: AccessToken,
    /// The protocol version the client speaks (absent = 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
}

impl WsAuth {
    /// Authentication with `token`, speaking this crate's [`PROTOCOL_VERSION`].
    pub fn new(token: impl Into<AccessToken>) -> Self {
        Self { token: token.into(), protocol: Some(PROTOCOL_VERSION) }
    }

    /// The whole frame as JSON text: `{"type":"auth","data":{"token":"…","protocol":1}}`. It
    /// contains the token: never log it.
    pub fn to_message(&self) -> String {
        // Strings and an integer: serializing cannot fail.
        serde_json::to_string(&TypedFrame { kind: kinds::AUTH, data: self }).unwrap_or_default()
    }
}

/// The `data` of `auth.ok`: who the socket belongs to and the server's protocol version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WsAuthOk {
    /// The authenticated user.
    pub user_id: UserId,
    /// The server's protocol version.
    pub protocol: u32,
}

impl WsAuthOk {
    /// An acceptance for `user_id` at this crate's [`PROTOCOL_VERSION`].
    pub fn new(user_id: UserId) -> Self {
        Self { user_id, protocol: PROTOCOL_VERSION }
    }
}

/// Any frame the server sends, decoded by the same rules as the client (see the module docs).
/// Use [`WsServerFrame::parse`] on a text frame.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum WsServerFrame {
    /// The answer to a request.
    Response(WsResponseFrame),
    /// A push.
    Push(WsPushFrame),
    /// First-message authentication accepted (`data` is optional on the wire: the client only
    /// looks at the `type`).
    AuthOk(Option<WsAuthOk>),
    /// First-message authentication refused; the server closes with 4001.
    AuthFailed(ApiError),
}

impl WsServerFrame {
    /// Decode a text frame. An error means the frame is not part of the protocol (the client
    /// ignores such frames).
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Encode as JSON text.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

impl Serialize for WsServerFrame {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            WsServerFrame::Response(frame) => frame.serialize(serializer),
            WsServerFrame::Push(frame) => frame.serialize(serializer),
            WsServerFrame::AuthOk(Some(ok)) => TypedFrame { kind: kinds::AUTH_OK, data: ok }.serialize(serializer),
            WsServerFrame::AuthOk(None) => {
                let mut frame = serializer.serialize_struct("WsServerFrame", 1)?;
                frame.serialize_field("type", kinds::AUTH_OK)?;
                frame.end()
            }
            WsServerFrame::AuthFailed(error) => {
                let mut frame = serializer.serialize_struct("WsServerFrame", 2)?;
                frame.serialize_field("type", kinds::AUTH_FAILED)?;
                frame.serialize_field("error", error)?;
                frame.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for WsServerFrame {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut object = Map::<String, Value>::deserialize(deserializer)?;
        if response_id(&object).is_some() {
            return response_from_object(object).map(WsServerFrame::Response).map_err(D::Error::custom);
        }
        match object.get("type").and_then(Value::as_str) {
            Some(kinds::AUTH_OK) => Ok(WsServerFrame::AuthOk(object.remove("data").and_then(|data| WsAuthOk::deserialize(data).ok()))),
            Some(kinds::AUTH_FAILED) => Ok(WsServerFrame::AuthFailed(error_from(object.remove("error"), codes::UNAUTHORIZED))),
            Some(_) => push_from_object(object).map(WsServerFrame::Push).map_err(D::Error::custom),
            None => Err(D::Error::custom("not a protocol frame: neither an answer nor a `type`")),
        }
    }
}

/// Any frame a client sends: a request, or the first-message `auth`. Use
/// [`WsClientFrame::parse`] on a text frame.
///
/// Rule: a frame with an `id` is a request (the `id` must be an unsigned integer, the `type` a
/// string); a frame without one must be `{"type":"auth",…}`. A request whose kind happens to be
/// `auth` (it has an `id`) is a request like any other.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum WsClientFrame {
    /// A request.
    Request(WsRequestFrame),
    /// First-message authentication.
    Auth(WsAuth),
}

impl WsClientFrame {
    /// Decode a text frame. On an error, [`FrameError::id`] is the request id if the frame had a
    /// usable one: answer it with `bad_request` (the client waits for exactly one answer). With
    /// no id the frame cannot be answered.
    pub fn parse(text: &str) -> Result<Self, FrameError> {
        let value: Value = serde_json::from_str(text).map_err(|e| FrameError { id: None, message: e.to_string() })?;
        let id = value.as_object().and_then(|object| object.get("id")).and_then(Value::as_u64);
        WsClientFrame::deserialize(value).map_err(|e| FrameError { id, message: e.to_string() })
    }

    /// The request id of a text frame, leniently: the `id` of a JSON object when it is an unsigned
    /// integer, whatever else the frame contains (`None` for non-JSON, non-objects, a missing,
    /// negative, fractional or string id).
    pub fn request_id(text: &str) -> Option<u64> {
        let value: Value = serde_json::from_str(text).ok()?;
        value.as_object()?.get("id")?.as_u64()
    }
}

/// Why a client frame could not be decoded, and the request id if it had one.
///
/// The message is serde_json's (it may quote a payload value, but never a secret: the secret
/// types decode with a fixed message).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FrameError {
    /// The request id, when the frame was a JSON object with an unsigned-integer `id`: the
    /// server answers `{"id":…,"ok":false,"error":{"code":"bad_request",…}}`.
    pub id: Option<u64>,
    /// What was wrong.
    pub message: String,
}

impl FrameError {
    /// The `bad_request` answer for this error, if the frame had an id.
    pub fn answer(&self) -> Option<WsResponseFrame> {
        self.id.map(|id| WsResponseFrame::error(id, ApiError::new(codes::BAD_REQUEST, "the request is malformed")))
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.id {
            Some(id) => write!(f, "malformed request {id}: {}", self.message),
            None => write!(f, "not a request: {}", self.message),
        }
    }
}

impl std::error::Error for FrameError {}

impl Serialize for WsClientFrame {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            WsClientFrame::Request(frame) => frame.serialize(serializer),
            WsClientFrame::Auth(auth) => TypedFrame { kind: kinds::AUTH, data: auth }.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for WsClientFrame {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut object = Map::<String, Value>::deserialize(deserializer)?;
        if object.contains_key("id") {
            return WsRequestFrame::deserialize(Value::Object(object)).map(WsClientFrame::Request).map_err(D::Error::custom);
        }
        match object.get("type").and_then(Value::as_str) {
            Some(kinds::AUTH) => {
                let data = object.remove("data").unwrap_or(Value::Null);
                WsAuth::deserialize(data).map(WsClientFrame::Auth).map_err(D::Error::custom)
            }
            _ => Err(D::Error::custom("not a request: no `id`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes() {
        assert!(CloseCode::UNAUTHORIZED.is_permanent() && CloseCode(4099).is_permanent());
        assert!(!CloseCode::GOING_AWAY.is_permanent() && !CloseCode(4100).is_permanent() && !CloseCode(3999).is_permanent());
        assert_eq!(u16::from(CloseCode::TRY_AGAIN_LATER), 1013);
        assert_eq!(CloseCode::from(1000), CloseCode::NORMAL);
        assert_eq!(CloseCode::REPLACED.to_string(), "4009");
        assert_eq!(CloseCode::BANNED.get(), 4003);
    }

    #[test]
    fn ack_accepts_anything() {
        for json in ["{}", "null", r#"{"later":1}"#, "true"] {
            assert_eq!(serde_json::from_str::<Ack>(json).ok(), Some(Ack::new()), "{json}");
        }
        assert_eq!(serde_json::to_string(&Ack::new()).ok().as_deref(), Some("{}"));
    }

    #[test]
    fn error_payload_fallbacks() {
        let frame = WsServerFrame::parse(r#"{"id":1,"ok":false,"error":"nope"}"#).ok();
        let Some(WsServerFrame::Response(WsResponseFrame { result: Err(error), .. })) = frame else {
            panic!("not an error answer: {frame:?}");
        };
        assert!(error.is(codes::INTERNAL));
        assert_eq!(error.details, Some(Value::String("nope".into())));
        let frame = WsServerFrame::parse(r#"{"type":"auth.failed"}"#).ok();
        assert!(matches!(frame, Some(WsServerFrame::AuthFailed(e)) if e.is(codes::UNAUTHORIZED)));
    }
}
