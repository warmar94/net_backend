# net_backend_protocol

<p>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

> **Status: in development.** Nothing is published yet; the types follow
> [`net_backend_server`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_server) as it is built and are
> released together with its first version.

Shared message types for [`net_backend_server`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_server):
plain Rust + serde, usable from any Rust client.

The server and its clients import the same types, so both sides agree on every request, answer
and push message, and on the exact JSON they become. A mismatch is a compile error instead of a
runtime surprise. The crate contains only data types and pure helpers: **no networking, no async
runtime, no game engine.** The JSON on the wire is the real contract: clients in other languages
speak it directly; this crate is the convenience for Rust and the single source of truth.

## How clients use this

This crate defines **what** is said; a client library decides **how** it is sent.

| Your client is… | Use |
|---|---|
| a **Bevy** game | this crate + [`bevy_net_backend`](https://crates.io/crates/bevy_net_backend) for the connection (with the optional `bevy_net_backend` feature, the types plug straight into its WebSocket requests). |
| **another Rust** app | this crate + the HTTP / WebSocket library you already use (for example reqwest, ureq, tokio-tungstenite). A small ready-made client, [`net_backend_client`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_client), is coming. |
| **not Rust** | not this crate: use the server's API documentation (OpenAPI for HTTP, a WebSocket message reference) and send the same JSON. |

## Contents

- [How clients use this](#how-clients-use-this)
- [What is inside](#what-is-inside)
- [Features](#features)
- [Install](#install)
- [Quick start: a client](#quick-start-a-client)
- [Quick start: a server](#quick-start-a-server)
- [The WebSocket envelope](#the-websocket-envelope)
- [HTTP routes](#http-routes)
- [Errors](#errors)
- [Accounts and sessions](#accounts-and-sessions)
- [Storage (saves)](#storage-saves)
- [Chat](#chat)
- [Ids, timestamps, pages](#ids-timestamps-pages)
- [Versioning and compatibility](#versioning-and-compatibility)
- [Optional integration with bevy_net_backend](#optional-integration-with-bevy_net_backend)
- [API design rules](#api-design-rules)
- [Limits and what it does not do](#limits-and-what-it-does-not-do)
- [Testing](#testing)
- [FAQ](#faq)
- [License](#license)
- [Contributing](#contributing)

## What is inside

| Module | What |
|---|---|
| `envelope` | The WebSocket frames: `WsRequestFrame`, `WsResponseFrame`, `WsPushFrame`, first-message `WsAuth`, `WsAuthOk`, the decoders `WsServerFrame` / `WsClientFrame`, `CloseCode`, the traits `WsCall` (request kind → answer type) and `ServerPush`, `Ack`. |
| `error` | `ApiError { code, message, details }`, the HTTP body `ErrorBody`, the stable `codes`, `ValidationDetails`, `http_status_for`. |
| `ids`, `time`, `page` | `UserId`, `RoomId`, `MessageId` (`i64`), `UnixMillis` (`i64` milliseconds), cursor pagination (`PageRequest`, `Page<T>`, `Cursor`). |
| `auth` | Register, login, Steam login, refresh, logout, account, email verification, password reset; `TokenPair`; redacted `Password`, `AccessToken`, `RefreshToken`, `Secret`. |
| `admin` | Operator routes: list / inspect accounts (`AdminUser`), ban (`BanRequest`, `BanInfo`), revoke sessions, roles, the audit log (`AuditEntry`, `AuditQuery`). |
| `storage` | Per-user key-value objects (save slots): put / get / list / delete / batch with optimistic versions. |
| `chat` | Rooms, direct messages, join / leave / send / history, the pushes `chat.message` and `chat.deleted`. |
| `text` | The character rules behind `validate()`: control, invisible and direction-changing characters. |
| `kinds`, `routes` | Every WebSocket `type` and every `/v1` HTTP path as constants (plus a route table). |
| `version` | `PROTOCOL_VERSION`, `PROTOCOL_HEADER`, `ServerInfo`. |

## Features

| Feature | Default | What |
|---|---|---|
| (none) | yes | serde + serde_json only. |
| `bevy_net_backend` | no | Implements the client crate [`bevy_net_backend`](https://crates.io/crates/bevy_net_backend)'s `WsRequest` / `WsPushMessage` for this crate's WebSocket messages and its `Credentials` for `AccessToken`. Brings that crate and its dependencies; a server never enables it. |

## Install

```toml
[dependencies]
net_backend_protocol = { version = "0.1.0" }

# With the optional client integration:
# net_backend_protocol = { version = "0.1.0", features = ["bevy_net_backend"] }
```

Rust 1.95 or newer.

## Quick start: a client

Build a request, read whatever the server sends back:

```rust
use net_backend_protocol::chat::{ChatMessage, JoinRoom, RoomInfo, SendMessage};
use net_backend_protocol::{RoomId, WsAuth, WsRequestFrame, WsServerFrame};

// First message on a fresh socket (if the handshake carried no `Authorization` header).
let auth = WsAuth::new("the-access-token-from-login").to_message();
assert!(auth.starts_with(r#"{"type":"auth""#));

// A request: {"id":1,"type":"chat.send","data":{"room":12,"text":"hello"}}
let request = WsRequestFrame::call(1, SendMessage::new(RoomId(12), "hello"));
let text = serde_json::to_string(&request).unwrap();
assert_eq!(text, r#"{"id":1,"type":"chat.send","data":{"room":12,"text":"hello"}}"#);
let _join = WsRequestFrame::call(2, JoinRoom::new("world"));

// Whatever the server sends: an answer, a push, auth.ok or auth.failed.
let incoming = r#"{"type":"chat.message","data":{"id":981,"room":12,"sender":42,"text":"hi","sent_at":1790000000000}}"#;
match WsServerFrame::parse(incoming).unwrap() {
    WsServerFrame::Response(answer) => {
        // answer.id is the request's id; answer.result is Ok(data) or Err(ApiError).
        let _room: Option<RoomInfo> = answer.decode::<RoomInfo>().ok().and_then(Result::ok);
    }
    WsServerFrame::Push(push) if push.kind == "chat.message" => {
        let message: ChatMessage = push.data_as().unwrap();
        assert_eq!(message.text, "hi");
    }
    _ => {}
}
```

## Quick start: a server

Decode a client frame, route by kind, answer:

```rust
use net_backend_protocol::chat::{SendAck, SendMessage, DEFAULT_MAX_TEXT_CHARS};
use net_backend_protocol::{codes, kinds, ApiError, MessageId, UnixMillis, WsCall, WsClientFrame, WsResponseFrame};

fn handle(text: &str) -> Option<String> {
    let request = match WsClientFrame::parse(text) {
        Ok(WsClientFrame::Request(request)) => request,
        Ok(WsClientFrame::Auth(_auth)) => return None, // check the token, answer auth.ok / auth.failed
        Ok(_) => return None,
        // A broken frame WITH an id still gets its one answer (`bad_request`); without an id it
        // cannot be answered.
        Err(error) => return error.answer().and_then(|answer| serde_json::to_string(&answer).ok()),
    };
    let answer = match request.kind.as_str() {
        kinds::CHAT_SEND => match request.data_as::<SendMessage>() {
            Ok(send) => match send.validate(DEFAULT_MAX_TEXT_CHARS) {
                Ok(()) => WsResponseFrame::ok_serialize(request.id, &SendAck::new(MessageId(981), UnixMillis::now())).ok()?,
                Err(error) => WsResponseFrame::error(request.id, error),
            },
            Err(_) => WsResponseFrame::error(request.id, ApiError::new(codes::BAD_REQUEST, "bad chat.send payload")),
        },
        _ => WsResponseFrame::error(request.id, ApiError::new(codes::UNKNOWN_TYPE, "unknown type")),
    };
    serde_json::to_string(&answer).ok()
}

let reply = handle(r#"{"id":7,"type":"chat.send","data":{"room":12,"text":"hello"}}"#).unwrap();
assert!(reply.starts_with(r#"{"id":7,"ok":true,"data":{"message_id":981"#));
let refused = handle(r#"{"id":8,"type":"no.such.kind","data":null}"#).unwrap();
assert_eq!(refused, r#"{"id":8,"ok":false,"error":{"code":"unknown_type","message":"unknown type"}}"#);
let malformed = handle(r#"{"id":9,"type":7}"#).unwrap();
assert!(malformed.starts_with(r#"{"id":9,"ok":false,"error":{"code":"bad_request""#));
assert_eq!(<SendMessage as WsCall>::KIND, "chat.send");
```

## The WebSocket envelope

JSON objects in text frames, compatible with the default envelope (`JsonEnvelope`) of the client
crate `bevy_net_backend` 0.1:

| Direction | Frame | JSON |
|---|---|---|
| client → server | request | `{"id":7,"type":"chat.send","data":{…}}` |
| server → client | answer | `{"id":7,"ok":true,"data":{…}}` or `{"id":7,"ok":false,"error":{"code":…,"message":…}}` |
| server → client | push | `{"type":"chat.message","data":{…}}` |
| client → server | first-message auth | `{"type":"auth","data":{"token":"…","protocol":1}}` |
| server → client | auth accepted | `{"type":"auth.ok","data":{"user_id":42,"protocol":1}}` |
| server → client | auth refused | `{"type":"auth.failed","error":{…}}`, then close 4001 |

- `id` is an unsigned 64-bit JSON number, echoed unchanged.
- A frame with a numeric `id` and (an `ok` field or no `type`) is an answer; a missing `ok`
  means `true`, a missing `data` means `null`. Anything else with a `type` is a push or an auth
  result. A push never has an `ok` field, and the server never puts an `id` on a push.
- Key order inside an object carries no meaning.
- A malformed request that has a usable `id` is still answered (`bad_request`,
  `FrameError::id` / `FrameError::answer`); only frames without one cannot be answered.
- Authentication: `Authorization: Bearer <access token>` on the handshake (a bad token is
  refused with HTTP 401 before the upgrade, an expired one with 401 `token_expired`), or the
  first-message `auth` within 5 seconds (`AUTH_TIMEOUT_SECS`; otherwise close 1008).
- **Every `auth` message gets exactly one `auth.ok` or `auth.failed`**, also on a socket already
  authenticated by its header. A later `auth` re-authenticates an open socket with a fresh token;
  a token of another user is refused (`auth.failed`, close 4001).
- An open socket survives the expiry of its access token; only a revocation closes it (4001, or
  4003 for a ban). Client recipe: refresh shortly before expiry (`ACCESS_TOKEN_REFRESH_MARGIN_SECS`);
  after a `Disconnected` with a handshake 401 or close 4001, refresh once and connect again (if the
  refresh fails, log in again).
- Messages are at most 1 MiB (`MAX_MESSAGE_BYTES`) in both directions.

**Close codes** (`CloseCode`). Clients reconnect after every code except 4000–4099:

| Code | Name | Meaning |
|---|---|---|
| 1000 | `NORMAL` | normal closure |
| 1001 | `GOING_AWAY` | server shutdown or redeploy: reconnect |
| 1008 | `POLICY_VIOLATION` | rate limit, no authentication in time, malformed traffic |
| 1009 | `MESSAGE_TOO_BIG` | a message over 1 MiB |
| 1011 | `INTERNAL_ERROR` | unexpected server error: reconnect |
| 1013 | `TRY_AGAIN_LATER` | overloaded, or the connection could not keep up: reconnect later |
| 4001 | `UNAUTHORIZED` | authentication refused or revoked (logout, password change): log in again |
| 4003 | `BANNED` | the account is banned |
| 4009 | `REPLACED` | replaced by a newer connection of the same session |
| 4010 | `UNSUPPORTED_PROTOCOL` | the client's protocol version is not supported |

**WebSocket kinds** (`kinds`): `auth`, `auth.ok`, `auth.failed`, `chat.join`, `chat.leave`,
`chat.send`, `chat.history`, pushes `chat.message` and `chat.deleted`.

## HTTP routes

Everything versioned is under `/v1` (`routes::PREFIX`). "auth" = `Authorization: Bearer <access token>`;
"admin" = the token of an account with the `admin` role (`admin::ADMIN_ROLE`).

| Method | Path | Auth | Body → answer |
|---|---|---|---|
| GET | `/v1/info` | no | → `ServerInfo` |
| POST | `/v1/auth/register` | no | `RegisterRequest` → `AuthSession` |
| POST | `/v1/auth/login` | no | `LoginRequest` → `AuthSession` |
| POST | `/v1/auth/steam` | no | `SteamLoginRequest` → `AuthSession` |
| POST | `/v1/auth/refresh` | no | `RefreshRequest` → `TokenPair` |
| POST | `/v1/auth/logout` | auth or `refresh_token` in the body | `LogoutRequest` → `Ack` |
| POST | `/v1/auth/email/verify` | no | `VerifyEmailRequest` → `Ack` |
| POST | `/v1/auth/email/resend` | auth | (none) → `Ack` |
| POST | `/v1/auth/password/forgot` | no | `ForgotPasswordRequest` → `Ack` (always) |
| POST | `/v1/auth/password/reset` | no | `ResetPasswordRequest` → `Ack` |
| GET | `/v1/account` | auth | → `Account` |
| PATCH | `/v1/account` | auth | `UpdateAccountRequest` → `Account` |
| POST | `/v1/account/password` | auth | `ChangePasswordRequest` → `Ack` |
| DELETE | `/v1/account/identities/{provider}` | auth (recent login) | → `Ack` (unlink a provider) |
| GET | `/v1/storage/{collection}` | auth | query `PageRequest` → `Page<StorageObjectInfo>` (no values) |
| GET | `/v1/storage/{collection}/{key}` | auth | → `StorageObject` |
| PUT | `/v1/storage/{collection}/{key}` | auth | `PutObject` → `ObjectAck` |
| DELETE | `/v1/storage/{collection}/{key}` | auth | query `DeleteObject` → `Ack` |
| POST | `/v1/storage/_batch/get` | auth | `BatchGet` → `BatchObjects` |
| POST | `/v1/storage/_batch/put` | auth | `BatchPut` → `BatchAcks` |
| GET | `/v1/chat/rooms` | auth | query `PageRequest` → `Page<RoomInfo>` |
| GET | `/v1/chat/rooms/{room}/messages` | auth | query `PageRequest` → `Page<ChatMessage>` |
| POST | `/v1/chat/dm` | auth | `OpenDirect` → `RoomInfo` |
| GET | `/v1/chat/dms` | auth | query `PageRequest` → `Page<RoomInfo>` (the caller's DMs, with `peer`) |
| GET | `/v1/admin/users` | admin | query `UserListQuery` (`q`, `cursor`, `limit`) → `Page<AdminUser>` |
| GET | `/v1/admin/users/{user}` | admin | → `AdminUser` |
| POST | `/v1/admin/users/{user}/ban` | admin | `BanRequest` → `Ack` (sessions revoked, sockets close 4003) |
| POST | `/v1/admin/users/{user}/unban` | admin | (none) → `Ack` |
| DELETE | `/v1/admin/users/{user}/sessions` | admin | → `Ack` (every session revoked) |
| DELETE | `/v1/admin/users/{user}/identities/{provider}` | admin | → `Ack` (unlink a provider) |
| PUT / DELETE | `/v1/admin/users/{user}/roles/{role}` | admin | → `Ack` (grant / revoke) |
| GET | `/v1/admin/audit` | admin | query `AuditQuery` (`user`, `action`, `cursor`, `limit`) → `Page<AuditEntry>` |
| GET (upgrade) | `/v1/ws` | header or first message | the WebSocket |

Unversioned: `/healthz` (liveness) and `/readyz` (readiness). `routes::ALL` holds the table as data
(method, path, auth; `Route::new` builds entries for your own routes); `routes::storage_object_path`
and friends fill in path parameters. Request body limits: 64 KiB for JSON routes
(`routes::DEFAULT_BODY_LIMIT_BYTES`), 272 KiB for a storage PUT, 4.06 MiB for a batch put; every
answer stays far below the 10 MiB `bevy_net_backend` accepts by default.

## Errors

Every error is an `ApiError`: a stable `code`, a human-readable `message`, optional `details`.
Over HTTP the body is `{"error":{…}}` (`ErrorBody`); over WebSocket it is the `error` of an
answer or of `auth.failed`.

```rust
use net_backend_protocol::{codes, error::http_status_for, ApiError, ErrorBody, ValidationDetails};

let body: ErrorBody = serde_json::from_str(
    r#"{"error":{"code":"validation_failed","message":"the request is invalid","details":{"fields":{"password":["is too short"]}}}}"#,
)
.unwrap();
assert!(body.error.is(codes::VALIDATION_FAILED));
let fields: ValidationDetails = body.error.details_as().unwrap();
assert_eq!(fields.fields["password"], ["is too short"]);
assert_eq!(http_status_for(codes::VERSION_CONFLICT), Some(409));
assert_eq!(ApiError::new(codes::NOT_FOUND, "no such save").to_string(), "not_found: no such save");
```

Codes (`codes`): `bad_request` 400, `validation_failed` 422, `unauthorized` 401, `forbidden` 403,
`token_expired` 401, `not_found` 404, `method_not_allowed` 405, `unsupported_media_type` 415, `conflict` 409, `version_conflict` 409, `payload_too_large` 413, `rate_limited`
429, `quota_exceeded` 403, `unknown_type` (WebSocket only), `unsupported_protocol` 400,
`invalid_credentials` 401, `refresh_token_reused` 401, `email_taken` 409, `email_not_verified`
403, `invalid_token` 400, `banned` 403, `reauthentication_required` 403, `steam_auth_failed` 401, `room_full` 409, `not_a_member`
403, `hook_timeout` 503, `unavailable` 503, `internal` 500. Codes never change meaning; server
modules and games may add their own, so treat an unknown code like its HTTP status.
`unsupported_protocol` is 400 on plain HTTP routes only (see versioning). `email_taken` tells that
an address has an account: a deliberate trade-off for a clear sign-up answer, bounded by rate limits.

## Accounts and sessions

Register or log in (email + password, or a Steam Web API ticket) and get an `AuthSession`: the
`Account` and a `TokenPair`. The access token lives 1 hour by default
(`DEFAULT_ACCESS_TOKEN_TTL_SECS`), the refresh token 30 days (`DEFAULT_REFRESH_TOKEN_TTL_SECS`);
a server may configure both. Refresh tokens **rotate**: each works once. Presenting the same
refresh token again within 30 seconds of its first use (`REFRESH_REUSE_GRACE_SECS`: an HTTP retry,
two systems refreshing at once) answers the same new pair; a later reuse revokes the whole session
(`refresh_token_reused`). Refresh single-flight anyway and keep the old pair until the new one is
stored. Logout takes the Bearer token or, when it already expired, the refresh token in the body.

```rust
use net_backend_protocol::auth::{LoginRequest, RegisterRequest, TokenPair};

let register = RegisterRequest::new("ada@example.com", "correct horse battery").with_display_name("Ada");
assert!(register.validate().is_ok()); // the same rules the server applies
assert!(RegisterRequest::new("ada@example.com", "short").validate().is_err());

// Secrets never show in Debug output (but do serialize: they must reach the server).
let login = LoginRequest::new("ada@example.com", "my password");
assert!(!format!("{login:?}").contains("my password"));

let tokens: TokenPair = serde_json::from_str(
    r#"{"token_type":"Bearer","access_token":"a","access_expires_at":1,"refresh_token":"r","refresh_expires_at":2}"#,
)
.unwrap();
assert_eq!(tokens.authorization_header(), "Bearer a");
```

Rules shared by both sides: passwords 10 characters to 128 bytes, no control characters, not only
whitespace; emails up to 254 bytes (shape only: the server normalises them and sends a verification
mail); display names up to 32 characters, trimmed. Emails must be a plain `local@domain`
(`auth::is_valid_email`): display names, angle brackets, comments, quoted local parts and address
literals are refused, so a server never re-parses an address with a lenient mail parser. Emails and display names refuse control
characters and invisible or direction-changing characters (bidi controls, zero-width characters,
BOM: `net_backend_protocol::text`), which would allow impersonation. Steam tickets up to 8192 hex
characters (Steam's buffer is 2560 bytes = 5120 hex). Password-reset requests always answer the same way, whether or not the
address has an account. Steam: the game sends the hex ticket from `GetAuthTicketForWebApi` with
its identity string; the server checks it with Steam and creates the account on the first login.

## Storage (saves)

Objects live at `(owner, collection, key)` and hold one JSON value; every route addresses the
caller's own objects. Every write bumps the object's `ObjectVersion` (1 for a new object). The
default is **last write wins**. A write that names the version it expects (`if_version`) gets 409
`version_conflict` with `{"current_version":N}` (`VersionConflict`; for a batch also `"index":i`,
the failing item) if the stored one differs, and nothing changes: only then two devices cannot
silently overwrite each other's save. The server also honours `If-Match: "N"` / `If-None-Match: *`
on single-object PUT and DELETE and always sends an `ETag`. Deleting an object that does not exist
answers `Ack` (unless `if_version` is given).

```rust
use net_backend_protocol::storage::{ObjectVersion, PutObject, DEFAULT_MAX_OBJECT_BYTES};
use net_backend_protocol::routes;

#[derive(serde::Serialize)]
struct Save { level: u32, gold: u64 }

let put = PutObject::from_serialize(&Save { level: 3, gold: 250 }).unwrap().if_version(ObjectVersion(2));
assert!(put.validate(DEFAULT_MAX_OBJECT_BYTES).is_ok());
assert_eq!(serde_json::to_string(&put).unwrap(), r#"{"value":{"gold":250,"level":3},"if_version":2}"#);
assert_eq!(routes::storage_object_path("saves", "slot-1").as_deref(), Some("/v1/storage/saves/slot-1"));
assert_eq!(routes::storage_object_path("saves", "../etc"), None); // not a valid name
let _create_only = PutObject::new(serde_json::json!({})).if_absent(); // only if it does not exist
```

Names: 1–128 bytes of ASCII letters, digits, `_`, `-`, `.`, starting with a letter or digit
(`storage::is_valid_name`). Defaults: 256 KiB per value, 1000 objects per user. Batches: at most 16
distinct objects (`MAX_BATCH`) and 4 MiB of values (`MAX_BATCH_BYTES`), all or nothing; a batch
read over 4 MiB is refused with 413. Collection listings return `StorageObjectInfo` (version, size,
time, no value), so a page stays small. `write` is `owner` or `server`: only server-side game code
sets `server`, and clients then get 403 on writes. Binary data: put it in a string yourself (e.g.
base64, +33 %, which counts against the size limit).

## Chat

Joining, leaving, sending, history and live messages go over the WebSocket; the room list,
history pages and opening a direct-message room are also HTTP routes.

| Kind | Request → answer |
|---|---|
| `chat.join` | `JoinRoom` → `RoomInfo` (by id `{"room":12}` or public key `{"room":"world"}`) |
| `chat.leave` | `LeaveRoom` → `Ack` |
| `chat.send` | `SendMessage` → `SendAck` |
| `chat.history` | `ChatHistory` → `Page<ChatMessage>` (newest first) |
| push `chat.message` | `ChatMessage`, to every member, the sender included |
| push `chat.deleted` | `MessageDeleted` (moderation) |

Public and group rooms: membership lasts as long as the connection; after a reconnect, join
again. **Direct messages** need no join: their `chat.message` goes to every open connection of
both members, and `GET /v1/chat/dms` lists the caller's DM rooms (`RoomInfo::peer`). The sender
gets its own `chat.message` too (the final text after server hooks, also on its other devices); the
`SendAck` answer comes first, games dedupe by `message_id`, and an optional `nonce` on
`SendMessage`, echoed in the push, matches a send whose answer was lost. Text: line breaks and tabs
are allowed; other control characters and invisible / direction-changing characters are refused
(zero-width joiners stay allowed for emoji). Room keys: 1–64 bytes of `a-z`, `0-9`, `_ - .`.
`ChatHistory` is WebSocket-only; over HTTP the room is in the path and the page in the query.
Defaults (a server may configure others): 500 characters per message, 5 messages per 10 seconds per user, 30 days of
history, 200 members per public room, 16 joined rooms per connection. Public rooms are capped on
purpose: one huge room multiplies every message by its member count.

## Ids, timestamps, pages

- Ids are `i64` newtypes (`UserId`, `RoomId`, `MessageId`) and travel as plain JSON numbers.
  Ids from outside (a SteamID64) travel as strings.
- Timestamps are `UnixMillis`: `i64` milliseconds since 1970 UTC, a plain JSON number
  (`UnixMillis::now()`, `from_system_time`, `to_system_time`).
- Lists use **cursor** pagination: `PageRequest { cursor, limit }` (default 50, at most 100) →
  `Page<T> { items, next_cursor }`; the last page has no `next_cursor`. Cursors are opaque (at
  most 512 bytes, `MAX_CURSOR_BYTES`): pass them back unchanged.

## Versioning and compatibility

- **Routes:** `/v1` is stable. A route or kind is never renamed or removed within `/v1`; new ones
  may be added. A breaking change would get `/v2`.
- **Protocol version:** `PROTOCOL_VERSION` (1) grows when messages are added within `/v1`. A
  client names the version it speaks in the `x-net-backend-protocol` header (`PROTOCOL_HEADER`,
  on HTTP requests and the WebSocket handshake) or in the `protocol` field of the first-message
  `auth`; none means 1. The server answers with its own version in the same header and in
  `auth.ok`, lists the accepted range at `GET /v1/info`, and refuses an unsupported version with
  `unsupported_protocol`: HTTP 400 on plain HTTP routes; on `/v1/ws` NEVER 400 (the client would
  retry it forever) but upgrade and close 4010 (`version::WS_REFUSAL_CLOSE`, no reconnect), or 403
  before the upgrade.
- **Forward compatibility:** unknown fields are ignored when decoding (no type uses
  `deny_unknown_fields`), so a newer server may add fields; optional fields may be absent; an
  unknown enum value (a room kind, a write rule) decodes as `Unknown`; unknown error codes and
  provider names are kept as text; a push kind a client does not know is still a push.

## Optional integration with bevy_net_backend

With the feature `bevy_net_backend`, this crate's WebSocket messages implement that client's
`WsRequest` / `WsPushMessage` (same kinds, same answer types) and `AccessToken` implements its
`Credentials` (a `Bearer` header on every request and WebSocket handshake). The client's default
envelope is the one above, so nothing else is needed:

```rust,ignore
use net_backend_protocol::chat::{ChatMessage, JoinRoom, SendMessage};

app.add_ws_request::<JoinRoom>()
    .add_ws_request::<SendMessage>()
    .add_ws_push::<ChatMessage>();

// In a system: `ws: Res<WsClient>`, `credentials: ResMut<BackendCredentials>`.
credentials.set(session.tokens.access_token.clone());
ws.request("main", &JoinRoom::new("world"));
// Answers arrive as `WsResponse<RoomInfo>`, pushes as `WsPush<ChatMessage>`; a refused request is
// `BackendError::Rejected`, whose payload decodes as `ApiError` (`rejection.json::<ApiError>()`).
```

The feature follows the client's minor version: a new client minor release means a new release
of this crate.

## API design rules

- Types a client or server only **reads** (answers, pushes, errors) are `#[non_exhaustive]` with
  public fields, so fields can be added later without a breaking change.
- Types you **build** (requests) are `#[non_exhaustive]` too and have constructors plus `with_*`
  builders: `SendMessage::new(room, text)`, `PutObject::new(value).if_version(v)`. New optional
  fields arrive as new builders, so code written today keeps compiling.
- Nothing that may hold a float (any JSON `Value`) implements `Eq`.
- Every serde attribute is explicit; JSON field names are `snake_case`.
- Secrets (`Password`, `AccessToken`, `RefreshToken`, `Secret`) print `<redacted>` in `Debug`,
  have no `Display` and no `PartialEq` (compare secrets in constant time on the server).
- `validate()` methods carry the shape rules both sides share; the server is still the authority.

## Limits and what it does not do

- No networking, no async runtime, no engine: a client library or a server sends the JSON.
- No OpenAPI schema generation yet (the server documents its API).
- OAuth logins, notifications, friends, leaderboards, groups and lobbies are not in this version;
  they arrive with the server modules that implement them, as additions.
- Storage values are JSON; binary saves are encoded by the game.
- The `validate()` helpers check shapes only; quotas, rate limits and permissions are the
  server's.

## Testing

```sh
cargo test -p net_backend_protocol                              # unit, golden JSON, round trips, redaction, forward compatibility
cargo test -p net_backend_protocol --features bevy_net_backend  # plus the frames through the client's own JsonEnvelope
cargo run -p net_backend_protocol --example print_frames        # prints the JSON of every frame
```

The golden tests pin the exact JSON text of every envelope frame. With the feature on, the tests
run every frame through `bevy_net_backend`'s real encoder and decoder in both directions and
check that both decoders classify the same frames the same way. CI runs fmt, clippy, tests and
docs for every feature combination on Rust 1.96.0.

## FAQ

**Do I need this crate to talk to the server?** No. The JSON is the contract; any HTTP / WebSocket
client in any language works. This crate saves Rust code from re-typing the messages.

**Why `i64` ids and millisecond timestamps instead of UUIDs and RFC 3339 strings?** They map to
one column type on every database the server supports, sort naturally and cost nothing to parse.

**Why cursors and not page numbers?** Lists such as chat history grow while you page through
them; an offset would skip or repeat messages, a cursor does not.

**Why does `chat.send` not resend after a reconnect?** It could post a message twice. Chat
membership also ends with the connection, so a game joins again after reconnecting.

## License

Dual-licensed under MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0
([LICENSE-APACHE](LICENSE-APACHE)), at your option.

## Contributing

This crate lives in the [`net_backend`](https://github.com/warmar94/net_backend) repository
together with the server and the client. Issues and pull requests are welcome. Please run
`cargo fmt --all`, `cargo clippy -p net_backend_protocol --all-targets --all-features -- -D warnings`
and `cargo test -p net_backend_protocol --all-features` before opening a pull request.
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
