# net_backend API reference

The complete HTTP + WebSocket + JSON API of a [`net_backend_server`](crates/net_backend_server/README.md)
server, for clients that talk to it directly: web pages, JavaScript / TypeScript, C#, GDScript,
Python, anything with an HTTP and a WebSocket library.

Rust clients have ready-made libraries that already speak everything below:

| Your client is… | Use |
|---|---|
| a **Rust** app | [`net_backend_client`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_client) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) |
| a **Bevy** game | [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) |
| **anything else** | this document |

## Contents

1. [Overview](#1-overview)
2. [Conventions](#2-conventions)
3. [Errors](#3-errors)
4. [Rate limits and sizes](#4-rate-limits-and-sizes)
5. [Accounts and authentication](#5-accounts-and-authentication)
6. [HTTP route reference](#6-http-route-reference)
7. [WebSocket](#7-websocket)
8. [Storage flows](#8-storage-flows)
9. [Chat flows](#9-chat-flows)
10. [Examples: curl](#10-examples-curl)
11. [Examples: browser JavaScript](#11-examples-browser-javascript)
12. [Building a client: checklist](#12-building-a-client-checklist)

---

## 1. Overview

A `net_backend_server` is a game backend you run yourself. Every server built with the framework
speaks the same API; which parts are present depends on the modules the server registers. The
reference server (the one the repository's deployment files install) registers all of them:

| Module | What it adds |
|---|---|
| core | `GET /v1/info`, `/healthz`, `/readyz`, the WebSocket endpoint `/v1/ws`, the API documents |
| `auth` | accounts, logins (email + password, Steam), tokens, sessions, email verification, password reset, roles, admin routes |
| `storage` | per-player JSON objects (save slots, settings) with versions |
| `chat` | public rooms, direct messages, history, presence, moderation |

`GET /v1/info` lists the modules a server runs. A game's own server may add its own routes and
WebSocket kinds on top; they follow the same conventions and appear in the server's API documents.

### Base URL

Throughout this document the server is `https://your-server.example`. Every API path starts with
`/v1/` (two health routes do not). WebSocket: `wss://your-server.example/v1/ws`.

Production servers run behind a reverse proxy that terminates TLS: use `https://` and `wss://`.

### Versioning

- **Paths:** `/v1` is stable: within `/v1` routes, fields and WebSocket kinds keep their names
  and stay.
- **Protocol version:** an integer, currently `1`. A client may name the version it speaks in
  the `x-net-backend-protocol` header (HTTP requests and the WebSocket handshake) or in the
  `protocol` field of the WebSocket `auth` message. Without it the server assumes `1`. Every
  HTTP answer (except CORS preflights) carries the server's own version in the same header, and
  `GET /v1/info` lists the accepted range:

```json
{"protocol":1,"min_protocol":1,"modules":["auth","chat","storage"]}
```

- An unsupported version is refused with `unsupported_protocol` and the details
  `{"supported_min":1,"supported_max":1}`: HTTP 400 on normal routes; on `/v1/ws` never 400
  (see [Version mismatch](#version-mismatch)).

### Machine-readable documents

| Document | Where | What |
|---|---|---|
| OpenAPI 3.1 | `GET /v1/openapi.json` | every documented HTTP route with its request and answer schemas; feed it to an OpenAPI generator for typed clients (TypeScript, C#, …) |
| AsyncAPI 3.0 | `GET /v1/asyncapi.json` | the WebSocket endpoint: the envelope, `auth` / `auth.ok` / `auth.failed`, every request kind with its answer, every push, the close codes (also as `x-close-codes`) |
| Browser UI | `GET /v1/docs` | an interactive viewer of the OpenAPI document, only when the operator enables it |

The operator decides whether the documents are public (`openapi.enabled`, on by default). The
admin routes are left out of the OpenAPI document unless the operator lists them
(`admin_in_openapi` in the `auth` and `storage` module settings). This document covers all of
them.

### Health

| Route | Answer |
|---|---|
| `GET /healthz` | `200 {"status":"ok"}` while the process runs (liveness) |
| `GET /readyz` | `200 {"status":"ready"}` when the database answers; `503` (error body) while shutting down or when the database is unreachable (readiness) |

---

## 2. Conventions

### JSON everywhere

- Request bodies are JSON with `Content-Type: application/json` (otherwise 415
  `unsupported_media_type`). Routes with a JSON body need one even when every field is optional:
  send `{}`.
- Every success is **HTTP 200** with a JSON body. Routes that return nothing return `{}`
  (called `Ack` below).
- Every 4xx / 5xx carries the [error body](#3-errors).
- Field names are `snake_case`. Key order means nothing.
- **Forward compatibility:** ignore fields you do not know (a newer server may add fields).
  Optional fields may be absent or `null`; treat both the same. Unknown enum values (a room
  `kind`, a storage `write` rule), unknown error codes and unknown push kinds must not break a
  client.

### Headers

| Header | Direction | Meaning |
|---|---|---|
| `Authorization: Bearer <access token>` | request | the caller (see [Accounts and authentication](#5-accounts-and-authentication)) |
| `Content-Type: application/json` | request | on every request with a body |
| `x-net-backend-protocol: 1` | both | the protocol version (optional on requests) |
| `x-request-id` | answer | the request's id; quote it when reporting a problem |
| `ETag: "3"` | answer | a storage object's version (single-object `GET` / `PUT`) |
| `If-Match: "3"` / `If-None-Match: *` | request | conditional storage writes (alternative to `if_version`) |
| `Retry-After: <seconds>` | answer | on some 429 / 503 answers |
| `Allow` | answer | on 405, the allowed methods |

### Ids

User, room, message and audit ids are integers (64-bit signed, plain JSON numbers). They are
assigned by the database, start at 1 and grow, so they fit JavaScript numbers in practice. Ids
from other systems travel as **strings** (a Steam id: `"76561197960287930"`). WebSocket request
ids are chosen by the client (unsigned integers; keep them below 2^53 in JavaScript).

### Timestamps

Unix time in **milliseconds** (UTC), a plain JSON number: `1790000000000`. In JavaScript:
`new Date(ms)`; `Date.now()` gives the same unit.

### Pagination

Lists use cursors:

- Request (HTTP query string, or the same fields in a WebSocket request's `data`): `cursor`
  (optional; the previous page's `next_cursor`) and `limit` (optional; default 50, clamped to
  1–100).
- Answer: `{"items":[…],"next_cursor":"…"}`. `next_cursor` is absent (or `null`) on the last page.
- Cursors are opaque strings of at most 512 bytes: pass them back unchanged (URL-encoded in a
  query string), never build or parse one.

```text
GET /v1/chat/rooms?limit=20
GET /v1/chat/rooms?limit=20&cursor=<next_cursor of the previous page, URL-encoded>
```

### Text rules

The server refuses text that could impersonate or break layouts. Requests that break a rule get
422 `validation_failed` with the field named in `details.fields`.

| Field | Rule |
|---|---|
| email | a plain `local@domain` (no display name, no angle brackets, comments, quoted local parts or address literals), at most 254 bytes; unique per server, case-insensitive |
| password | 10 characters to 128 bytes (UTF-8), no control characters, not only white space |
| display name | at most 32 characters, trimmed; no control characters and no invisible or direction-changing characters (bidi controls, zero-width characters, BOM, …) |
| storage collection / key | 1–128 bytes of `A-Z a-z 0-9 _ - .`, starting with a letter or digit |
| chat room key | 1–64 bytes of `a-z 0-9 _ - .`, starting with a letter or digit |
| chat text | at most 500 characters (server setting); line breaks (`\n`) and tabs allowed, `\r` and other control characters refused; invisible / direction-changing characters refused except the zero-width joiner and non-joiner (emoji need them); must show something visible; at most 8 combining marks in a row |
| chat nonce | 1–64 visible ASCII characters |
| role | `[a-z][a-z0-9_.-]*`, at most 64 bytes |

### CORS (browser pages on another origin)

CORS is **off** unless the server operator sets `cors.allowed_origins` (for example
`["https://your-site.example"]`, or `["*"]`). With it on, the server allows the methods `GET`,
`POST`, `PUT`, `PATCH`, `DELETE`, the request headers `Authorization`, `Content-Type`,
`If-Match`, `If-None-Match`, `x-net-backend-protocol`, `x-request-id`, and lets the page read the
answer headers `x-net-backend-protocol`, `x-request-id`, `ETag` and `Retry-After`, so a page on
another origin uses the API exactly like any other client.

A page served from the same origin as the API (for example through the same reverse proxy) needs
no CORS at all. WebSocket connections are not subject to CORS.

Tokens travel only in the `Authorization` header or the WebSocket `auth` message, never in
cookies, so cross-site request forgery does not apply.

---

## 3. Errors

Every error over HTTP is:

```json
{"error":{"code":"validation_failed","message":"the request is invalid","details":{"fields":{"password":["is shorter than 10 characters"]}}}}
```

- `code`: stable, `snake_case`. **Branch on it.**
- `message`: human-readable English; may change at any time; never contains internal details.
- `details`: optional, code-specific JSON.

Over the WebSocket the same object is the `error` of a refused request or of `auth.failed`.

### Every error code

| Code | HTTP | Meaning | Client action |
|---|---|---|---|
| `bad_request` | 400 | malformed request: not JSON, wrong types, missing fields, invalid path parameter | fix the request |
| `validation_failed` | 422 | well-formed but breaks a rule; `details.fields` maps each field to its problems | show the field messages |
| `unauthorized` | 401 | no valid credentials: missing, unknown, malformed or revoked token | log in again (after one refresh attempt for a revoked access token) |
| `token_expired` | 401 | the access token expired | refresh, then retry once |
| `forbidden` | 403 | authenticated but not allowed (missing role, server-locked object, registration closed, a server rule) | do not retry |
| `not_found` | 404 | no such route or object (or the caller may not know it exists) | |
| `method_not_allowed` | 405 | wrong HTTP method; `Allow` lists the allowed ones | fix the request |
| `unsupported_media_type` | 415 | a JSON route without `Content-Type: application/json` | send the header |
| `conflict` | 409 | the request conflicts with the current state | |
| `version_conflict` | 409 | storage: the stored version is not the expected one; `details`: `{"current_version":N}` (absent when the object does not exist), plus `"index"` in a batch | reload, merge, write again |
| `payload_too_large` | 413 | body (or WebSocket answer, or stored value) too large | send less |
| `rate_limited` | 429 | too many requests; `details`: `{"retry_after_ms":N}` | wait that long, then retry |
| `quota_exceeded` | 403 | a per-user quota is used up (stored objects / bytes, rooms per WebSocket) | free something up |
| `unknown_type` | – | WebSocket only: the request `type` is not known to this server | |
| `unsupported_protocol` | 400 | the client's protocol version is not supported; `details`: `{"supported_min":N,"supported_max":N}` | update the client |
| `invalid_credentials` | 401 | email + password do not match (never says which part) | |
| `refresh_token_reused` | 401 | a refresh token was used a second time after the grace window: the whole session is revoked | log in again |
| `email_taken` | 409 | registration: the address already has an account | offer login / password reset |
| `email_not_verified` | 403 | the action needs a verified email address | verify first |
| `invalid_token` | 400 | a one-time mail token (verification, reset) is unknown, used or expired | ask for a new mail |
| `banned` | 403 | the account is banned; `details`: `{"until":<unix ms>}` for a timed ban (absent: until lifted) | show the ban; do not retry |
| `reauthentication_required` | 403 | the action needs a recent login (linking / unlinking a login provider) | log in again, then retry |
| `steam_auth_failed` | 401 | Steam refused the ticket (or could not be asked) | |
| `room_full` | 409 | the chat room is at its member cap | try later |
| `not_a_member` | 403 | not a member of the chat room (join it first; group / DM rooms: members only) | |
| `hook_timeout` | 503 | a server rule did not answer in time | retry later |
| `unavailable` | 503 | overloaded, shutting down, or the request took longer than the server's limit | retry later with backoff |
| `internal` | 500 | unexpected server error (never with details) | retry later; report with `x-request-id` |

Servers and their games may add their own codes: treat an unknown code like its HTTP status.

Notes:

- An unknown route answers 404 `{"error":{"code":"not_found","message":"no such route or object"}}`.
- A request that takes longer than the server's request timeout (default 30 s) answers 503
  `unavailable`.
- `email_taken` reveals that an address has an account: a deliberate choice for a clear sign-up
  answer, bounded by the registration rate limit. Login, password reset and the failed-login
  limits never reveal it.

---

## 4. Rate limits and sizes

All numbers are the server's defaults; an operator may change them. Every 429 answer carries
`details.retry_after_ms`; answers from the request-level limiters also carry `Retry-After` (whole
seconds).

### HTTP rate limits

| What | Limit |
|---|---|
| logins (password and Steam), per client address and route | 10 per minute |
| registrations, per client address | 10 per hour |
| refreshes, per client address | 60 per minute |
| forgot / reset / verify / resend / password change, per client address and route | 10 per minute |
| mails per account (verification, reset) | 3 per hour (a further reset request answers the same and sends nothing; a further resend answers 429) |
| failed logins per email address and client network | 5, then one more try every 3 minutes |
| failed logins per email address, from everywhere | 50 per hour; above it only networks that logged in to the account before may try |
| storage writes (`PUT`, `DELETE`, a batch counts once), per user | 60 per 60 s (a burst of 60, then one per second) |
| opening a direct-message room, per user | 20 per 600 s (a burst of 20, then one every 30 s) |

Client networks are single IPv4 addresses and IPv6 /64 blocks. The failed-login limits count
whether or not the address has an account.

### WebSocket limits

| What | Limit |
|---|---|
| frames per connection | 20 per second, burst 40 (text, binary and ping frames count); over it requests are answered `rate_limited`; a client that keeps flooding is closed with 1008 |
| chat messages per user | a burst of 5, then one every 2 s (`rate_limited` with `retry_after_ms`) |
| message size | 1 MiB (1048576 bytes) in both directions; bigger: close 1009 |
| connections per user | 5; a 6th closes the oldest with 4009 (the same session's first) |
| connections per client address | 100 (429 at the handshake) |
| handshakes per client address | 60 per minute (429 at the handshake) |
| rooms per connection | 16 (`quota_exceeded`) |
| connections per public room | 200 (`room_full`) |
| time to authenticate | 5 s after the upgrade (else close 1008) |

### Body and value sizes

| What | Limit |
|---|---|
| JSON request bodies | 64 KiB (413 `payload_too_large` above) |
| storage `PUT` body | 272 KiB (a value of up to 256 KiB plus JSON) |
| storage batch `PUT` body | 4 MiB + 64 KiB |
| one stored value | 256 KiB of JSON (422 `validation_failed` above) |
| stored objects per user | 1000 (403 `quota_exceeded`) |
| stored bytes per user | 4 MiB of values (403 `quota_exceeded`; a write that does not grow an object always passes) |
| batch | 1–16 distinct objects, at most 4 MiB of values together; a batch read over 4 MiB answers 413 |
| Steam ticket | 8192 hex characters |
| ban reason | 255 characters |

---

## 5. Accounts and authentication

### Tokens

A login answers an `AuthSession`: the account and a token pair.

```json
{
  "account": {
    "id": 42,
    "email": "player@example.com",
    "email_verified": false,
    "display_name": "Player One",
    "roles": [],
    "identities": [],
    "created_at": 1790000000000
  },
  "tokens": {
    "token_type": "Bearer",
    "access_token": "nbsa_…",
    "access_expires_at": 1790003600000,
    "refresh_token": "nbsr_…",
    "refresh_expires_at": 1792592000000
  }
}
```

| Token | Lifetime (default) | Use |
|---|---|---|
| access token | 1 hour (`access_expires_at`) | `Authorization: Bearer <access token>` on HTTP requests and the WebSocket handshake, or the WebSocket `auth` message |
| refresh token | 30 days (`refresh_expires_at`; every refresh starts a new one) | `POST /v1/auth/refresh` only |

- Tokens are opaque strings (`nbsa_` / `nbsr_` plus 64 hex characters). Store and send them
  unchanged; never parse them. Use the `*_expires_at` fields, not the token text, for timing.
- **Refresh before expiry:** when less than 60 seconds of the access token are left, refresh.
- **Rotation:** every refresh token works **once** and is replaced by the answer's new pair.
  Presenting the same refresh token again **within 30 seconds** of its first use (an HTTP retry,
  two tabs refreshing at once) answers the **same** new pair. Presenting it again **later** revokes
  the whole session (`refresh_token_reused`: the token was probably stolen), and the player must log
  in again. So: refresh single-flight, store the new pair before using it, keep the old pair until
  the new one is stored, and share tokens between tabs (see the
  [checklist](#12-building-a-client-checklist)).
- **Sessions:** one per login. Logout revokes one session or all of them; a password change revokes
  the other sessions; a password reset or a ban revokes all. Revocation takes effect on the next
  request; open WebSockets of a revoked session are closed (4001, or 4003 for a ban).
- **Expired or revoked tokens:** an expired access token answers 401 `token_expired` (refresh and
  retry); an unknown, malformed or revoked one 401 `unauthorized`; a banned account's token or
  refresh 403 `banned`. Routes that do not need a caller (register, login, refresh, logout,
  forgot / reset, verify) ignore a stale `Authorization` header, so a client that always sends its
  last token still works there. `POST /v1/auth/steam` is the exception: there a Bearer token that is
  invalid or expired is refused.

### Register, log in, refresh, log out

| Step | Request | Answer |
|---|---|---|
| register | `POST /v1/auth/register` `{"email","password","display_name"?}` | `AuthSession`; a verification mail is sent |
| log in | `POST /v1/auth/login` `{"email","password"}` | `AuthSession` |
| refresh | `POST /v1/auth/refresh` `{"refresh_token"}` | `TokenPair` |
| log out this session | `POST /v1/auth/logout` `{}` with the Bearer token, or `{"refresh_token":"…"}` when the access token expired | `{}` |
| log out everywhere | `POST /v1/auth/logout` `{"everywhere":true}` (+ Bearer or `refresh_token`) | `{}` |

Login answers the same 401 `invalid_credentials` for an unknown address and a wrong password. A
server may require a verified address for password logins (403 `email_not_verified`) or close
registration (403 `forbidden`).

### Email verification and password reset (website pages)

Mails contain a one-time token, or a link the operator configured with the token in it, for
example `https://your-site.example/verify?token=…` and `https://your-site.example/reset?token=…`.
A website implements those two pages:

| Page | What it does |
|---|---|
| `/verify?token=…` | `POST /v1/auth/email/verify` `{"token":"<token from the URL>"}` → `{}`, or 400 `invalid_token` (unknown, used or expired: offer "send again", which is `POST /v1/auth/email/resend` with the player's Bearer token) |
| `/reset?token=…` | a form for the new password, then `POST /v1/auth/password/reset` `{"token":"…","new_password":"…"}` → `{}`; every session of the account is revoked, so log in afterwards |
| "forgot password" form | `POST /v1/auth/password/forgot` `{"email":"…"}` → always `{}` (whether or not the address has an account) |

Verification tokens expire after 24 hours, reset tokens after 1 hour; both are single-use. A reset
also unlinks login providers (Steam) by default.

### Steam

A game gets a ticket with Steamworks' `GetAuthTicketForWebApi(identity)` and posts it hex-encoded
with the identity string: `POST /v1/auth/steam` `{"ticket_hex":"…","identity":"…"}` → `AuthSession`.
The server checks the ticket with Steam; the first login creates an account (its `email` is
`null`; `identities` holds `{"provider":"steam","subject":"<SteamID64 as a string>"}`). With the
Bearer token of a login younger than 10 minutes, the same request links Steam to that account
instead (otherwise 403 `reauthentication_required`). The route answers 404 when the server has no
Steam login configured. Steam login is for game clients with the Steam SDK; web pages use email +
password.

### Bans

A banned account gets 403 `banned` on login, refresh and every authenticated request, with
`details.until` (unix ms) for a timed ban. Its sessions are revoked and its WebSockets closed with
4003. Show the ban and do not retry automatically.

### Roles

Roles are strings in `account.roles` (`admin`, `moderator`, a game's own); a normal player has
none. The admin routes need `admin`; deleting other players' chat messages needs `admin` or
`moderator` (server setting).

---

## 6. HTTP route reference

"Auth" means `Authorization: Bearer <access token>` is required; a request without a valid token
answers 401 (`unauthorized` / `token_expired`) or 403 `banned` before anything else happens.
Every route can also answer 429 `rate_limited`, 500 `internal` and 503 `unavailable`; the tables
list the other answers. Schemas are written as JSON with `?` marking optional fields.

### Shared shapes

```text
Ack                {}
Account            {"id":int, "email":string|null, "email_verified":bool, "display_name":string|null,
                    "roles":[string], "identities":[LinkedIdentity], "created_at":ms}
LinkedIdentity     {"provider":"steam", "subject":string}
TokenPair          {"token_type":"Bearer", "access_token":string, "access_expires_at":ms,
                    "refresh_token":string, "refresh_expires_at":ms}
AuthSession        {"account":Account, "tokens":TokenPair}
StorageObject      {"collection":string, "key":string, "owner":int, "value":JSON, "version":int,
                    "write":"owner"|"server", "updated_at":ms}
StorageObjectInfo  {"collection":string, "key":string, "version":int, "write":"owner"|"server",
                    "size_bytes":int, "updated_at":ms}
ObjectAck          {"collection":string, "key":string, "version":int, "updated_at":ms}
RoomInfo           {"id":int, "kind":"room"|"dm"|"group", "key"?:string, "name"?:string,
                    "member_count"?:int, "max_members"?:int, "peer"?:int}
ChatMessage        {"id":int, "room":int, "sender":int, "sender_name"?:string|null, "text":string,
                    "sent_at":ms, "nonce"?:string}
Page<T>            {"items":[T], "next_cursor"?:string}
```

- `RoomInfo.kind`: `room` (public), `dm` (direct messages, with `peer` = the other user), `group`
  (members only). `member_count` counts users online in the room now; `max_members` is the cap in
  connections.
- `StorageObject.write`: `owner` (the player may write it) or `server` (server-locked: player
  writes get 403).

### Server

| Method | Path | Auth | Request | Answer |
|---|---|---|---|---|
| GET | `/v1/info` | no | – | `{"protocol":1,"min_protocol":1,"modules":[string]}` |
| GET | `/healthz` | no | – | `{"status":"ok"}` |
| GET | `/readyz` | no | – | `{"status":"ready"}`; 503 when not ready |
| GET | `/v1/openapi.json` | no | – | the OpenAPI document (when enabled) |
| GET | `/v1/asyncapi.json` | no | – | the AsyncAPI document (when enabled) |
| GET (upgrade) | `/v1/ws` | handshake header or first message | – | the [WebSocket](#7-websocket) |

### Auth

#### `POST /v1/auth/register`

Create an account with email and password; it is logged in at once.

- Auth: no.
- Body: `{"email":string, "password":string, "display_name"?:string}`.
- 200: `AuthSession`. A verification mail is sent.
- Errors: 403 `forbidden` (registration closed, or refused by a server rule); 409 `email_taken`;
  422 `validation_failed` (details per field); 429 `rate_limited`.

#### `POST /v1/auth/login`

- Auth: no.
- Body: `{"email":string, "password":string}`.
- 200: `AuthSession`.
- Errors: 401 `invalid_credentials`; 403 `banned` (details `until`), `email_not_verified`, or a
  server rule's refusal; 429 `rate_limited` (too many attempts from this address or for this
  account); 503 `unavailable` (password hashing is saturated: retry shortly).

#### `POST /v1/auth/steam`

- Auth: no; optional Bearer of a recent login to **link** Steam to that account.
- Body: `{"ticket_hex":string, "identity":string}` (the ticket hex-encoded, at most 8192
  characters; the identity string the ticket was requested for).
- 200: `AuthSession`.
- Errors: 401 `steam_auth_failed`, or the Bearer token sent is `token_expired` / `unauthorized`;
  403 `banned`, a refused borrowed copy, or `reauthentication_required` (linking needs a login
  younger than 10 minutes); 404 Steam login is not enabled on this server; 409 `conflict` (the
  Steam account is linked to another account, or this account already has one).

#### `POST /v1/auth/refresh`

- Auth: no.
- Body: `{"refresh_token":string}`.
- 200: `TokenPair` (a new pair; the old refresh token is used up).
- Errors: 401 `unauthorized` (invalid, expired or revoked) or `refresh_token_reused` (the session
  was revoked); 403 `banned`.

#### `POST /v1/auth/logout`

Revoke this session, or every session of the account.

- Auth: the Bearer token **or** `refresh_token` in the body.
- Body: `{"everywhere"?:bool, "refresh_token"?:string}` (send `{}` for "this session, by Bearer").
- 200: `{}`.
- Errors: 401 `unauthorized` (neither a valid access token nor a valid refresh token).

#### `POST /v1/auth/email/verify`

- Auth: no.
- Body: `{"token":string}` (the one-time token from the mail).
- 200: `{}`.
- Errors: 400 `invalid_token`.

#### `POST /v1/auth/email/resend`

Send the verification mail again (nothing happens when the address is confirmed already).

- Auth: yes. No body.
- 200: `{}`.
- Errors: 401; 429 `rate_limited`.

#### `POST /v1/auth/password/forgot`

- Auth: no.
- Body: `{"email":string}`.
- 200: `{}`, always the same whether or not the address has an account (a mail is sent if it does).
- Errors: 429 `rate_limited`.

#### `POST /v1/auth/password/reset`

- Auth: no.
- Body: `{"token":string, "new_password":string}`.
- 200: `{}`. Every session of the account is revoked.
- Errors: 400 `invalid_token`; 422 `validation_failed`.

### Account

#### `GET /v1/account`

- Auth: yes.
- 200: `Account`.

#### `PATCH /v1/account`

Change the caller's account (absent fields stay).

- Auth: yes.
- Body: `{"display_name"?:string}`.
- 200: the changed `Account`.
- Errors: 422 `validation_failed`.

#### `POST /v1/account/password`

Change the password, knowing the current one; the **other** sessions are revoked (this one stays).

- Auth: yes.
- Body: `{"current_password":string, "new_password":string}`.
- 200: `{}`.
- Errors: 401 `invalid_credentials` (the current password is wrong); 422 `validation_failed`.

#### `DELETE /v1/account/identities/{provider}`

Unlink a login provider (`steam`) from the caller's account.

- Auth: yes, with a login younger than 10 minutes.
- 200: `{}`.
- Errors: 403 `reauthentication_required` (log in again first); 404 `not_found` (no such linked
  provider); 409 `conflict` (it is the account's only way to log in).

### Storage

Every route addresses the caller's own objects. See [Storage flows](#8-storage-flows).

#### `GET /v1/storage/{collection}`

A page of the caller's objects in a collection, **without values**, ordered by key.

- Auth: yes.
- Query: `cursor`?, `limit`? (1–100, default 50).
- 200: `Page<StorageObjectInfo>`.
- Errors: 400 `bad_request` (an invalid name or cursor).

#### `GET /v1/storage/{collection}/{key}`

- Auth: yes.
- 200: `StorageObject`; header `ETag: "<version>"`.
- Errors: 404 `not_found`.

#### `PUT /v1/storage/{collection}/{key}`

Write one object. Without a condition the last write wins.

- Auth: yes.
- Body: `{"value":JSON, "if_version"?:int}`. `if_version: N` writes only over version N;
  `if_version: 0` writes only if the object does not exist.
- Headers (alternative to `if_version`): `If-Match: "N"`, `If-None-Match: *`.
- 200: `ObjectAck` (with the new `version`); header `ETag: "<version>"`.
- Errors: 400 `bad_request` (an invalid name, a malformed condition header, or a header that
  disagrees with `if_version`); 403 `forbidden` (the object or its collection is written by the
  server only), `quota_exceeded` (too many objects or bytes), or a server rule's refusal; 409
  `version_conflict` (details `{"current_version":N}`); 413 `payload_too_large`; 422
  `validation_failed` (the value is too large); 429 `rate_limited` (details `retry_after_ms`).

#### `DELETE /v1/storage/{collection}/{key}`

Deleting an object that does not exist answers `{}` too, unless a version is named.

- Auth: yes.
- Query: `if_version`? (only if the stored version is this one). Header alternative: `If-Match: "N"`.
- 200: `{}`.
- Errors: 403 `forbidden` (server-locked) or a server rule's refusal; 409 `version_conflict`; 429
  `rate_limited`.

#### `POST /v1/storage/_batch/get`

Read several objects at once.

- Auth: yes.
- Body: `{"objects":[{"collection":string, "key":string}]}` (1–16 distinct objects).
- 200: `{"objects":[StorageObject]}`; missing objects are simply absent.
- Errors: 413 `payload_too_large` (more than 4 MiB of values: read fewer per batch); 422
  `validation_failed`.

#### `POST /v1/storage/_batch/put`

Write several objects in one transaction: all or nothing.

- Auth: yes.
- Body: `{"objects":[{"collection":string, "key":string, "value":JSON, "if_version"?:int}]}`
  (1–16 distinct objects, at most 4 MiB of values together).
- 200: `{"objects":[ObjectAck]}`, in request order.
- Errors: 403 `forbidden` / `quota_exceeded` / a server rule's refusal (details: `index` of the
  failing item); 409 `version_conflict` (details: `index` and `current_version` of the first
  failing item); 422 `validation_failed`; 429 `rate_limited` (a batch counts as one write).

### Chat

Joining, sending and live messages use the [WebSocket](#chat-kinds); these HTTP routes list rooms,
read history, open direct messages and delete messages. See [Chat flows](#9-chat-flows).

#### `GET /v1/chat/rooms`

The public rooms, with online counts and caps.

- Auth: yes.
- Query: `cursor`?, `limit`?.
- 200: `Page<RoomInfo>`.

#### `GET /v1/chat/rooms/{room}/messages`

A page of a room's history, **newest first**.

- Auth: yes. Public rooms: any player; group and DM rooms: their members.
- Query: `cursor`? (older messages), `limit`?.
- 200: `Page<ChatMessage>`.
- Errors: 403 `not_a_member`; 404 `not_found`.

#### `DELETE /v1/chat/rooms/{room}/messages/{message}`

Delete a message: its sender, or a moderator (roles `admin` / `moderator` by default). The room
gets a `chat.deleted` push and the message leaves the history.

- Auth: yes.
- 200: `{}`.
- Errors: 403 `forbidden` (neither its sender nor a moderator); 404 `not_found` (no such message,
  or deleted already).

#### `POST /v1/chat/dm`

Open (or find) the direct-message room with another user.

- Auth: yes.
- Body: `{"user":int}`.
- 200: `RoomInfo` (`kind: "dm"`, with `peer`).
- Errors: 400 `bad_request` (yourself); 403 a server rule's refusal (for example a block); 404
  `not_found` (no such account); 429 `rate_limited` (details `retry_after_ms`).

#### `GET /v1/chat/dms`

The caller's direct-message rooms, newest activity first.

- Auth: yes.
- Query: `cursor`?, `limit`?.
- 200: `Page<RoomInfo>` (each with `peer`).

### Admin (role `admin` only)

Every admin route needs the Bearer token of an account with the `admin` role; others get 403
`forbidden`. Every admin action is recorded in the audit log. These routes are for operator tools,
never for player-facing pages.

```text
AdminUser   {"account":Account, "active_sessions":int, "last_seen_at"?:ms, "ban"?:BanInfo}
BanInfo     {"banned_at":ms, "until"?:ms, "reason"?:string}
AuditEntry  {"id":int, "action":string, "actor"?:int, "target_type"?:string, "target_id"?:string,
             "ip"?:string, "request_id"?:string, "data"?:object, "created_at":ms}
```

| Method | Path | Request | Answer | Errors |
|---|---|---|---|---|
| GET | `/v1/admin/users` | query `q`? (part of the email, case-insensitive, or display name, or an id), `cursor`?, `limit`? | `Page<AdminUser>`, newest first | |
| GET | `/v1/admin/users/{user}` | – | `AdminUser` | 404 |
| POST | `/v1/admin/users/{user}/ban` | `{"until"?:ms (in the future; absent = until lifted), "reason"?:string (≤ 255 characters)}` | `{}`; sessions revoked, sockets closed with 4003 | 404; 409 `conflict` (yourself, or an admin: remove the role first); 422 |
| POST | `/v1/admin/users/{user}/unban` | – | `{}` | 404 |
| DELETE | `/v1/admin/users/{user}/sessions` | – | `{}`; every session revoked | 404 |
| DELETE | `/v1/admin/users/{user}/identities/{provider}` | – | `{}` (unlink, e.g. `steam`) | 404 |
| PUT | `/v1/admin/users/{user}/roles/{role}` | – | `{}` (granted, or already held) | 404 |
| DELETE | `/v1/admin/users/{user}/roles/{role}` | – | `{}` (revoked, or not held) | 409 `conflict` (your own admin role, or the last admin) |
| GET | `/v1/admin/audit` | query `user`? (acted or was the target), `action`? (one action, or a prefix ending in `.` such as `admin.`), `cursor`?, `limit`? | `Page<AuditEntry>`, newest first | |
| GET | `/v1/admin/users/{user}/storage/{collection}` | query `cursor`?, `limit`? | `Page<StorageObjectInfo>` | |
| GET | `/v1/admin/users/{user}/storage/{collection}/{key}` | – | `StorageObject` | 404 |
| PUT | `/v1/admin/users/{user}/storage/{collection}/{key}` | `{"value":JSON, "if_version"?:int, "write"?:"owner"\|"server"}` (`write` absent: unchanged; new objects `owner`) | `ObjectAck` | 404 (no such account); 409 `version_conflict`; 422 |
| DELETE | `/v1/admin/users/{user}/storage/{collection}/{key}` | query `if_version`? | `{}` (also for a server-locked object) | 409 `version_conflict` |

An admin storage write ignores the owner's write lock and the owner's quotas.

---

## 7. WebSocket

One connection per client carries requests, answers and server pushes as **JSON objects in text
frames**. Binary frames are not part of the protocol and are ignored.

```text
wss://your-server.example/v1/ws
```

### The envelope

| Direction | Frame | JSON |
|---|---|---|
| client → server | request | `{"id":7,"type":"chat.send","data":{…}}` |
| server → client | answer (success) | `{"id":7,"ok":true,"data":{…}}` |
| server → client | answer (error) | `{"id":7,"ok":false,"error":{"code":"…","message":"…","details"?:…}}` |
| server → client | push | `{"type":"chat.message","data":{…}}` |
| client → server | authenticate | `{"type":"auth","data":{"token":"<access token>","protocol":1}}` |
| server → client | authenticated | `{"type":"auth.ok","data":{"user_id":42,"protocol":1}}` |
| server → client | refused | `{"type":"auth.failed","error":{…}}`, then the server closes |

Rules:

- `id` is an unsigned integer you choose (a counter per connection is enough), echoed unchanged in
  the answer. **Every request gets exactly one answer.** Answers to different requests arrive in the
  order the requests were sent (one socket's requests are handled one after another).
- `data` may be left out (it then means `null`). In an answer, a missing `ok` means `true` and a
  missing `data` means `null`.
- **Telling frames apart:** a frame with a numeric `id` and (an `ok` field or no `type`) is an
  answer. Anything else with a `type` is a push or an auth result (`auth.ok`, `auth.failed`). A push
  never has an `id` or `ok` field.
- A malformed frame that has an unsigned-integer `id` is answered `bad_request`; one without a
  usable `id` cannot be answered and is dropped.
- Request errors: `bad_request` (malformed frame or `data`), `unauthorized` (not authenticated
  yet), `unknown_type`, `rate_limited` (`details.retry_after_ms`), `payload_too_large` (the answer
  would exceed the message limit), `unavailable` (the handler took longer than 10 s), `internal`,
  plus each kind's own codes.

### Connecting and authenticating

| How | Use it when | What happens |
|---|---|---|
| `Authorization: Bearer <access token>` on the handshake | your WebSocket library can set headers (C#, Godot, native clients) | checked before the upgrade; a bad token is refused with an HTTP status (below) |
| first message `{"type":"auth","data":{"token":"…","protocol":1}}` within **5 s** | **browsers** (the browser `WebSocket` API cannot set headers), or any client | answered `auth.ok` or `auth.failed`; no `auth` in time: close 1008 |
| `?token=<access token>` (or `?access_token=`) on the URL | only if the operator enabled `ws.query_token` (off by default) | like the header; avoid it: proxies log URLs |

- Without a header the upgrade succeeds anonymously; the socket must send `auth` within 5 seconds.
  Requests sent before `auth.ok` are answered `unauthorized`, and pushes only reach authenticated
  sockets. **Wait for `auth.ok` before sending requests.**
- Every `auth` message gets exactly one `auth.ok` or `auth.failed`, also on a socket the handshake
  header already authenticated.
- A later `auth` with a fresh token of the **same** user re-authenticates an open socket (answered
  `auth.ok`); a token of another user is refused (`auth.failed`, close 4001).
- `auth.failed` is final for that socket: the server closes it right after (4001; 4003 when banned;
  4010 for an unsupported `protocol`). Its `error.code` says why (`token_expired`, `unauthorized`,
  `banned`, `unsupported_protocol`).
- A **temporary** server failure while checking `auth` (database down, overload) closes with 1013
  **without** `auth.failed`: reconnect with backoff and try again.
- An open socket **survives the expiry of its access token**. Only a revocation closes it (logout,
  password change or reset, admin action, refresh-token reuse: 4001; a ban: 4003). Re-sending
  `auth` with a fresh token is not required.

Handshake answers (HTTP, before the upgrade):

| Status | Meaning | Client action |
|---|---|---|
| 101 | upgraded | |
| 401 `unauthorized` / `token_expired` | the handshake token is invalid / expired | `token_expired`: refresh, then connect again; `unauthorized`: refresh once, else log in |
| 403 | `banned`, an unsupported protocol version on a request that is not an upgrade, or a server rule | do not retry |
| 426 | a plain GET without an upgrade | |
| 429 + `Retry-After` | too many handshakes or sockets from this address | wait, then retry |
| 503 + `Retry-After` | the server is full, shutting down, or temporarily failing | wait, then retry |

The endpoint never answers 400. Note that a browser does not show the status of a refused
handshake (the `WebSocket` just closes with code 1006); with first-message `auth` the reason
arrives as `auth.failed` instead.

### Version mismatch

Name your version in the `x-net-backend-protocol` handshake header or the `protocol` field of
`auth`. An unsupported version: the server upgrades, then closes with **4010** (with first-message
auth: `auth.failed` `unsupported_protocol` + 4010). A non-upgrade request with an unsupported
version gets 403. Do not reconnect; the client needs an update.

### Heartbeats

The server sends a WebSocket ping every 20 s and answers the client's pings. A connection that
sent nothing (not even a pong) for 60 s is dropped. Browsers and most libraries answer pings
automatically. Clients may send their own pings (they count against the frame rate).

### Close codes

| Code | Meaning | Reconnect? |
|---|---|---|
| 1000 | normal closure | yes |
| 1001 | the server is shutting down or redeploying | yes (soon) |
| 1006 | (set by the client library) the connection dropped without a close frame: network loss, or a refused handshake in a browser | yes, with backoff |
| 1008 | no `auth` in time, or still flooding after the rate limit refused requests | yes, with backoff (fix the cause) |
| 1009 | a message over 1 MiB | yes |
| 1011 | an unexpected server error | yes, with backoff |
| 1013 | overloaded, or this socket could not keep up with its pushes | yes, later; then resync |
| 4001 | authentication refused or revoked (logout, password change, admin, refresh-token reuse) | **no** (see below) |
| 4003 | the account is banned | **no** |
| 4009 | replaced: the user opened more connections than allowed (5); the oldest goes first | **no** |
| 4010 | the client's protocol version is not supported | **no** |

**Never reconnect automatically after 4000–4099.** The server uses that range only for "do not come
back with these credentials". For 4001: refresh the tokens once; if the refresh succeeds, connect
once more with the new access token; if the refresh fails (or the new connection gets 4001 again),
show the login screen. For 4009: tell the player another window or device took over; reconnect only
on a user action.

For every other code, reconnect with exponential backoff and jitter (for example 1 s, 2 s, 4 s, …
up to 30 s, each ±25 %); reset the backoff after `auth.ok`. After a reconnect: authenticate again,
**join your chat rooms again** (membership ends with the connection) and reload anything you may
have missed (chat history newer than your last message, storage objects).

### Kinds registered by the reference server

| Kind | Direction | `data` → answer `data` |
|---|---|---|
| `auth` | client → server | `{"token":string, "protocol"?:int}` → `auth.ok` / `auth.failed` |
| `auth.ok` | server → client | `{"user_id":int, "protocol":int}` |
| `auth.failed` | server → client | (no `data`; `error`: ApiError) |
| `chat.join` | request | `{"room":int \| string}` → `RoomInfo` |
| `chat.leave` | request | `{"room":int}` → `{}` |
| `chat.send` | request | `{"room":int, "text":string, "nonce"?:string}` → `{"message_id":int, "sent_at":ms}` |
| `chat.history` | request | `{"room":int, "cursor"?:string, "limit"?:int}` → `Page<ChatMessage>` (newest first) |
| `chat.members` | request | `{"room":int}` → `{"room":int, "members":[{"user":int, "name"?:string}], "count":int, "truncated"?:bool}` |
| `chat.message` | push | `ChatMessage` |
| `chat.deleted` | push | `{"id":int, "room":int}` |
| `chat.presence` | push | `{"room":int, "user":int, "event":"joined" \| "left", "name"?:string, "count"?:int}` |

A game's own server may register more kinds; its `/v1/asyncapi.json` lists them all. An unknown
request kind is answered `unknown_type`; ignore push kinds you do not know.

<a id="chat-kinds"></a>
#### Chat kinds in detail

- **`chat.join`** — by room id (`{"room":12}`) or a public room's key (`{"room":"world"}`). Joining
  a room already joined is not an error. Membership lasts as long as the connection. Group rooms
  need membership; DM rooms need no join (joining answers their info). Errors: `not_found`,
  `not_a_member`, `room_full` (the room's cap counts connections), `quota_exceeded` (16 rooms on
  this connection), `validation_failed` (an invalid key).
- **`chat.leave`** — leaving a room not joined is not an error.
- **`chat.send`** — to a room joined **on this connection**, or to one of your DM rooms (no join
  needed). The answer `{"message_id","sent_at"}` comes **before** your own `chat.message` echo.
  Dedupe by `message_id` (answer) = `id` (push), or match the `nonce`. The text in the push is the
  final text after server rules (it may differ from what you sent). Errors: `validation_failed`
  (text rules), `rate_limited` (`details.retry_after_ms`, about 2000 right after a burst),
  `not_a_member`, or a server rule's refusal (for example `forbidden` from a word filter).
- **`chat.history`** — the same as the HTTP history route, over the socket. Public rooms: anyone;
  group and DM rooms: their members.
- **`chat.members`** — who is online in a room joined on this connection, each user once, up to
  200 listed (`truncated: true` when cut), `count` = the total. A DM room lists only the caller: a
  DM never reveals whether the other player is online.
- **`chat.message`** — to every member connection of the room, the sender's included (also the
  sender's other devices). A DM's message goes to every open connection of both users, joined or
  not. The `nonce`: in public and group rooms every member's push carries it (use a random value,
  never anything secret); in DMs and in the history only the sender sees it.
- **`chat.deleted`** — a message was deleted (moderation or its sender); remove it from the screen.
- **`chat.presence`** — `joined` when a user's first connection joins the room, `left` when their
  last one leaves or disconnects; `count` is the room's online users after the change. Your own
  connections get it too. Best effort: rooms with more than 100 online users get none, and each room
  pushes at most 10 per second; `chat.members` always answers the full list.

---

## 8. Storage flows

Objects live at `(owner, collection, key)` and hold one JSON value (usually an object): save slots,
settings, inventories. Players only ever see their own objects.

**Versions.** Every write bumps the object's `version` (1 for a new object).

| You want | Send |
|---|---|
| last write wins | `PUT` `{"value":…}` |
| create only if it does not exist | `PUT` `{"value":…, "if_version":0}` |
| overwrite only the version you loaded | `PUT` `{"value":…, "if_version":<version you read>}` |
| delete only the version you loaded | `DELETE …?if_version=<version>` |

On a mismatch the answer is 409 `version_conflict` with `{"current_version":N}` (absent when the
object does not exist) and nothing changes. Of several simultaneous conditional writes exactly one
wins.

**Save-game flow (two devices safe):**

1. `GET /v1/storage/saves/slot-1` → `{"value":{…},"version":3,…}` (404: no save yet; treat as
   version 0).
2. The player plays; the client keeps `version = 3`.
3. `PUT /v1/storage/saves/slot-1` `{"value":{…new…},"if_version":3}` → `{"version":4,…}`; keep 4.
4. On 409: another device saved meanwhile. `GET` again, let the player choose (or merge), then `PUT`
   with the new `if_version`.

**Listing.** `GET /v1/storage/saves` returns `StorageObjectInfo` items (key, version, size, time,
no values) ordered by key: use it for a "load game" screen, then `GET` the chosen object or
`POST /v1/storage/_batch/get` several at once.

**Batches.** `POST /v1/storage/_batch/put` writes up to 16 objects in one transaction: either every
write happens or none (the error's `details.index` names the failing item).

**Server-locked objects.** Objects with `"write":"server"` and everything in server-owned
collections (default: `server` and `server.*`) are written only by server code and admins. A player
can read them; writing, creating or deleting them answers 403 `forbidden`.

**Binary data** (images, compressed saves): encode it as a string yourself (for example base64,
+33 %, which counts against the 256 KiB value limit).

---

## 9. Chat flows

**Public room, live:**

1. `GET /v1/chat/rooms` (HTTP) to show the public rooms (`key`, `name`, `member_count`).
2. Open the WebSocket and authenticate (`auth` → `auth.ok`).
3. `chat.join` `{"room":"world"}` → `RoomInfo` (keep its `id`).
4. `chat.history` `{"room":12,"limit":50}` → the latest messages (newest first: reverse for display).
5. `chat.send` `{"room":12,"text":"hello","nonce":"k3j9x"}` → `{"message_id":981,"sent_at":…}`; then
   your own `chat.message` push with `id: 981` arrives.
6. Show every `chat.message` push for the room; remove messages on `chat.deleted`; update the
   online list on `chat.presence` (or ask `chat.members`).
7. After a reconnect: `chat.join` again, then load history newer than the last message you have
   (page with `chat.history` until you reach a known `id`).

**Direct messages:**

1. `POST /v1/chat/dm` `{"user":77}` → `RoomInfo` with `kind: "dm"`, `peer: 77` (the same room every
   time for the same pair).
2. `chat.send` `{"room":<dm room id>,"text":"…"}`: no join needed; both players receive
   `chat.message` on every open connection.
3. `GET /v1/chat/dms` lists the caller's DM rooms, newest activity first; history works as for any
   room.

**Older history:** pass the previous page's `next_cursor` as `cursor` (HTTP query or WebSocket
`data`) until `next_cursor` is absent. Messages are kept 30 days by default.

**Moderation:** `DELETE /v1/chat/rooms/{room}/messages/{message}` by the sender or a moderator; the
room gets `chat.deleted`.

**Do not resend `chat.send` automatically after a reconnect:** it could post a message twice. If the
answer was lost, look for your `nonce` in the history first.

---

## 10. Examples: curl

```sh
API=https://your-server.example

# Server info (no auth)
curl -sS "$API/v1/info"
# {"protocol":1,"min_protocol":1,"modules":["auth","chat","storage"]}

# Register (logs in at once)
curl -sS "$API/v1/auth/register" -H 'content-type: application/json' \
  -d '{"email":"player@example.com","password":"correct horse battery","display_name":"Player One"}'
# {"account":{"id":42,...},"tokens":{"token_type":"Bearer","access_token":"nbsa_...","access_expires_at":...,"refresh_token":"nbsr_...","refresh_expires_at":...}}

# Log in
curl -sS "$API/v1/auth/login" -H 'content-type: application/json' \
  -d '{"email":"player@example.com","password":"correct horse battery"}'

ACCESS=nbsa_...    # tokens.access_token
REFRESH=nbsr_...   # tokens.refresh_token

# Who am I
curl -sS "$API/v1/account" -H "authorization: Bearer $ACCESS"

# Refresh (single use: store the new pair)
curl -sS "$API/v1/auth/refresh" -H 'content-type: application/json' \
  -d "{\"refresh_token\":\"$REFRESH\"}"

# Save an object (create only if new)
curl -sS -X PUT "$API/v1/storage/saves/slot-1" -H "authorization: Bearer $ACCESS" \
  -H 'content-type: application/json' -d '{"value":{"level":3,"gold":250},"if_version":0}'
# {"collection":"saves","key":"slot-1","version":1,"updated_at":...}

# Load it (the ETag header is the version)
curl -sS -i "$API/v1/storage/saves/slot-1" -H "authorization: Bearer $ACCESS"

# Overwrite only version 1
curl -sS -X PUT "$API/v1/storage/saves/slot-1" -H "authorization: Bearer $ACCESS" \
  -H 'content-type: application/json' -d '{"value":{"level":4,"gold":300},"if_version":1}'
# a stale version: 409 {"error":{"code":"version_conflict","message":"...","details":{"current_version":2}}}

# List a collection (no values), then delete
curl -sS "$API/v1/storage/saves?limit=20" -H "authorization: Bearer $ACCESS"
curl -sS -X DELETE "$API/v1/storage/saves/slot-1?if_version=2" -H "authorization: Bearer $ACCESS"

# Chat over HTTP: public rooms, a room's history, open a DM
curl -sS "$API/v1/chat/rooms" -H "authorization: Bearer $ACCESS"
curl -sS "$API/v1/chat/rooms/12/messages?limit=50" -H "authorization: Bearer $ACCESS"
curl -sS "$API/v1/chat/dm" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"user":77}'

# Password reset (website flow)
curl -sS "$API/v1/auth/password/forgot" -H 'content-type: application/json' -d '{"email":"player@example.com"}'
curl -sS "$API/v1/auth/password/reset" -H 'content-type: application/json' \
  -d '{"token":"<token from the mail link>","new_password":"another long passphrase"}'

# Log out (this session; with an expired access token send {"refresh_token":"..."} instead)
curl -sS "$API/v1/auth/logout" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{}'

# The API documents
curl -sS "$API/v1/openapi.json" -o openapi.json
curl -sS "$API/v1/asyncapi.json" -o asyncapi.json
```

---

## 11. Examples: browser JavaScript

Plain JavaScript with `fetch` and `WebSocket`, no library. The page must be served from the API's
origin, or from an origin listed in the server's `cors.allowed_origins`.

### HTTP helper and token handling

```js
const API = "https://your-server.example";

class ApiError extends Error {
  constructor(status, error) {
    super(error.message || `HTTP ${status}`);
    this.status = status;          // HTTP status
    this.code = error.code;        // stable error code: branch on this
    this.details = error.details;  // code-specific details, if any
  }
}

async function api(method, path, { body, token, query } = {}) {
  const url = new URL(API + path);
  for (const [name, value] of Object.entries(query || {})) {
    if (value !== undefined && value !== null) url.searchParams.set(name, value);
  }
  const headers = { "x-net-backend-protocol": "1" };
  if (token) headers["Authorization"] = `Bearer ${token}`;
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const response = await fetch(url, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  const json = text ? JSON.parse(text) : null;
  if (!response.ok) {
    throw new ApiError(response.status, (json && json.error) || { code: `http_${response.status}`, message: response.statusText });
  }
  return json;
}

// Tokens are shared by every tab of the site through localStorage, so two tabs never rotate the
// same refresh token minutes apart (a reuse after 30 s would revoke the session).
const TOKENS_KEY = "nb_tokens";
const loadTokens = () => JSON.parse(localStorage.getItem(TOKENS_KEY) || "null");
const saveTokens = (tokens) => localStorage.setItem(TOKENS_KEY, JSON.stringify(tokens));
const clearTokens = () => localStorage.removeItem(TOKENS_KEY);

let refreshing = null; // single-flight within this tab

// `staleAccessToken`: the access token the server just refused (token_expired, close 4001), if any.
function refreshTokens(staleAccessToken) {
  if (!refreshing) {
    refreshing = (async () => {
      const before = loadTokens(); // re-read: another tab may have refreshed already
      if (!before) throw new ApiError(401, { code: "unauthorized", message: "not logged in" });
      const rotatedElsewhere = staleAccessToken && before.access_token !== staleAccessToken;
      const stillFresh = !staleAccessToken && before.access_expires_at - Date.now() > 60_000;
      if (rotatedElsewhere || stillFresh) return before;
      try {
        const tokens = await api("POST", "/v1/auth/refresh", { body: { refresh_token: before.refresh_token } });
        saveTokens(tokens);
        return tokens;
      } catch (error) {
        // 401 (unauthorized, refresh_token_reused) or 403 (banned): the session is over.
        // A network error or a 5xx keeps the tokens: try again later.
        if (error.status === 401 || error.status === 403) clearTokens();
        throw error;
      } finally {
        refreshing = null;
      }
    })();
  }
  return refreshing;
}

async function accessToken() {
  let tokens = loadTokens();
  if (!tokens) throw new ApiError(401, { code: "unauthorized", message: "not logged in" });
  if (tokens.access_expires_at - Date.now() < 60_000) tokens = await refreshTokens();
  return tokens.access_token;
}

// An authenticated call: refreshes ahead of expiry, and once more on `token_expired`.
async function authed(method, path, options = {}) {
  const token = await accessToken();
  try {
    return await api(method, path, { ...options, token });
  } catch (error) {
    if (error.code !== "token_expired") throw error;
    const tokens = await refreshTokens(token);
    return api(method, path, { ...options, token: tokens.access_token });
  }
}
```

### Register, log in, refresh, log out

```js
async function register(email, password, displayName) {
  const session = await api("POST", "/v1/auth/register", {
    body: { email, password, display_name: displayName },
  });
  saveTokens(session.tokens);
  return session.account;
}

async function login(email, password) {
  try {
    const session = await api("POST", "/v1/auth/login", { body: { email, password } });
    saveTokens(session.tokens);
    return session.account;
  } catch (error) {
    if (error.code === "invalid_credentials") throw new Error("Wrong email or password.");
    if (error.code === "rate_limited") throw new Error(`Too many attempts. Try again in ${Math.ceil(error.details.retry_after_ms / 1000)} s.`);
    if (error.code === "banned") throw new Error("This account is banned.");
    if (error.code === "validation_failed") throw new Error(Object.values(error.details.fields).flat().join(" "));
    throw error;
  }
}

async function logout(everywhere = false) {
  const tokens = loadTokens();
  if (!tokens) return;
  try {
    // The refresh token works even when the access token has expired.
    await api("POST", "/v1/auth/logout", { body: { refresh_token: tokens.refresh_token, everywhere } });
  } finally {
    clearTokens();
  }
}

const me = () => authed("GET", "/v1/account");
```

### Save and load an object

```js
async function loadSave(slot) {
  try {
    const object = await authed("GET", `/v1/storage/saves/${slot}`);
    return { value: object.value, version: object.version };
  } catch (error) {
    if (error.code === "not_found") return { value: null, version: 0 };
    throw error;
  }
}

// Writes only over the version that was loaded (0 = only if it does not exist yet).
async function writeSave(slot, value, version) {
  try {
    const ack = await authed("PUT", `/v1/storage/saves/${slot}`, { body: { value, if_version: version } });
    return ack.version; // keep it for the next write
  } catch (error) {
    if (error.code === "version_conflict") {
      // Another device saved meanwhile: reload, let the player decide, write again.
      throw new Error(`The save changed elsewhere (now version ${error.details.current_version ?? "deleted"}).`);
    }
    throw error;
  }
}

// Usage
const save = await loadSave("slot-1");
const newVersion = await writeSave("slot-1", { level: 3, gold: 250 }, save.version);
```

### WebSocket: connect, authenticate, join, chat, reconnect

```js
class Realtime {
  constructor() {
    this.ws = null;
    this.ready = false;
    this.nextId = 1;
    this.pending = new Map();    // request id -> { resolve, reject }
    this.listeners = new Map();  // push type -> [callback]
    this.rooms = new Set();      // room keys / ids to join again after a reconnect
    this.attempt = 0;
    this.stopped = false;
    this.lastAuthError = null;
    this.authToken = null;       // the access token sent in the last `auth`
    this.retriedAfter4001 = false;
  }

  on(type, callback) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(callback);
  }

  emit(type, data) {
    for (const callback of this.listeners.get(type) || []) callback(data);
  }

  connect() {
    this.stopped = false;
    const ws = new WebSocket(API.replace(/^http/, "ws") + "/v1/ws");
    this.ws = ws;
    this.ready = false;
    this.lastAuthError = null;
    ws.onopen = async () => {
      try {
        // Browsers cannot set headers on a WebSocket: authenticate with the first message (within 5 s).
        this.authToken = await accessToken();
        ws.send(JSON.stringify({ type: "auth", data: { token: this.authToken, protocol: 1 } }));
      } catch {
        ws.close(); // not logged in, or the refresh failed
      }
    };
    ws.onmessage = (event) => this.onFrame(JSON.parse(event.data));
    ws.onclose = (event) => this.onClose(event);
  }

  onFrame(frame) {
    const isAnswer = typeof frame.id === "number" && ("ok" in frame || !("type" in frame));
    if (isAnswer) {
      const waiting = this.pending.get(frame.id);
      if (!waiting) return;
      this.pending.delete(frame.id);
      if (frame.ok === false) waiting.reject(frame.error);
      else waiting.resolve(frame.data ?? null);
      return;
    }
    if (frame.type === "auth.ok") {
      this.ready = true;
      this.attempt = 0;
      this.retriedAfter4001 = false;
      this.onReady();
      return;
    }
    if (frame.type === "auth.failed") {
      this.lastAuthError = frame.error; // the server closes the socket right after
      return;
    }
    this.emit(frame.type, frame.data); // a push: chat.message, chat.deleted, chat.presence, ...
  }

  request(type, data) {
    return new Promise((resolve, reject) => {
      if (!this.ready) return reject({ code: "not_connected", message: "not connected" });
      const id = this.nextId++;
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, type, data }));
    });
  }

  async onReady() {
    // Membership ends with the connection: join every room again.
    for (const room of this.rooms) {
      try {
        const info = await this.request("chat.join", { room });
        this.emit("joined", info);
      } catch (error) {
        console.warn("join failed", room, error.code);
      }
    }
    this.emit("ready");
  }

  async onClose(event) {
    this.ready = false;
    for (const waiting of this.pending.values()) waiting.reject({ code: "disconnected", message: "connection closed" });
    this.pending.clear();
    if (this.stopped) return;

    if (event.code >= 4000 && event.code <= 4099) {
      // Never come back automatically with the same credentials.
      if (event.code === 4001 && !this.retriedAfter4001) {
        this.retriedAfter4001 = true;
        try {
          await refreshTokens(this.authToken); // works when the token had only expired
          this.connect();         // one more try with the new token
          return;
        } catch {
          // the session is over: fall through
        }
      }
      this.emit("gone", { code: event.code, error: this.lastAuthError }); // show login / ban / "opened elsewhere"
      return;
    }

    // Everything else: exponential backoff with jitter, 1 s .. 30 s.
    const base = Math.min(30_000, 1000 * 2 ** this.attempt++);
    const delay = base * (0.75 + Math.random() * 0.5);
    setTimeout(() => this.connect(), delay);
  }

  close() {
    this.stopped = true;
    if (this.ws) this.ws.close(1000);
  }

  // Chat helpers
  async join(room) {
    this.rooms.add(room);
    return this.request("chat.join", { room });
  }

  async leave(roomId, roomRef) {
    this.rooms.delete(roomRef ?? roomId);
    return this.request("chat.leave", { room: roomId });
  }

  send(roomId, text) {
    const nonce = crypto.randomUUID().replaceAll("-", ""); // random, never secret
    return this.request("chat.send", { room: roomId, text, nonce });
  }

  history(roomId, cursor) {
    return this.request("chat.history", { room: roomId, cursor, limit: 50 });
  }
}

// Usage
const live = new Realtime();
const shown = new Set(); // message ids already on screen (the sender gets its own echo)

live.on("chat.message", (message) => {
  if (shown.has(message.id)) return;
  shown.add(message.id);
  console.log(`[${message.room}] ${message.sender_name ?? message.sender}: ${message.text}`);
});
live.on("chat.deleted", ({ id, room }) => console.log(`message ${id} in room ${room} was deleted`));
live.on("chat.presence", ({ room, user, event, count }) => console.log(`user ${user} ${event} room ${room} (${count ?? "?"} online)`));
live.on("gone", ({ code, error }) => console.log("disconnected for good:", code, error && error.code));

live.on("joined", async (room) => {
  // Every (re)join: load the latest history (newest first) and show what is not on screen yet.
  const page = await live.history(room.id);
  for (const message of page.items.reverse()) {
    if (!shown.has(message.id)) { shown.add(message.id); console.log(message.text); }
  }
});

live.rooms.add("world"); // join the public room "world" on every (re)connect
live.connect();

// Later, once connected (12 = the `id` of the joined room):
// const ack = await live.send(12, "hello");   // { message_id, sent_at }; the echo follows
// shown.add(ack.message_id);                   // if you already drew the message yourself
```

Errors from `live.request(...)` are the protocol's error objects (`{code, message, details?}`):
for example `rate_limited` on `chat.send` carries `details.retry_after_ms`.

---

## 12. Building a client: checklist

**Tokens**

- [ ] Store the whole `TokenPair` (both tokens and both expiry times); keep it until a new pair is
      stored.
- [ ] Refresh when less than 60 s of the access token are left (compare `access_expires_at` with the
      clock), and on any 401 `token_expired` (then retry the request once).
- [ ] Refresh single-flight: one refresh at a time per refresh token, across every tab, window or
      thread that shares it. A second use within 30 s answers the same pair; a later reuse logs the
      player out everywhere (`refresh_token_reused`).
- [ ] On 401 `unauthorized` / `refresh_token_reused` or 403 `banned` from refresh: drop the tokens
      and show the login screen. On network errors and 5xx: keep them and try again later.
- [ ] Log out with the refresh token in the body, so logout works after the access token expired.
- [ ] In a browser, tokens in `localStorage` are readable by any script on the page: keep the page
      free of untrusted scripts (or keep the tokens in memory and log in per visit).

**HTTP**

- [ ] Send `Content-Type: application/json` and a JSON body (`{}` at least) on routes with a body.
- [ ] Branch on `error.code`, never on `message`; treat unknown codes like their HTTP status.
- [ ] On 429 wait `details.retry_after_ms` (or `Retry-After`); on 503 retry with backoff.
- [ ] Use `if_version` for saves that two devices may write.
- [ ] Ignore unknown fields; treat absent and `null` the same.
- [ ] Optionally send `x-net-backend-protocol: 1` and check `GET /v1/info` at start-up
      (`min_protocol` ≤ your version ≤ `protocol`, and the `modules` you need).

**WebSocket**

- [ ] One connection per client; authenticate with the header or the first-message `auth` within
      5 s; send requests only after `auth.ok`.
- [ ] Number requests with a counter; match answers by `id`; every request gets exactly one answer.
- [ ] Classify frames: numeric `id` and (`ok` or no `type`) = answer; otherwise by `type`
      (`auth.ok`, `auth.failed`, pushes). Ignore unknown push kinds.
- [ ] Never reconnect automatically after 4000–4099 (4001: one refresh + one new connection at
      most). Reconnect with exponential backoff and jitter after everything else (1000, 1001, 1006,
      1008, 1009, 1011, 1013).
- [ ] After every (re)connect: join the chat rooms again and reload what may have been missed.
- [ ] Do not resend `chat.send` automatically; use a random `nonce` and dedupe by message id.
- [ ] Keep messages under 1 MiB and below 20 frames per second (burst 40).
- [ ] Answer pings (browsers and most libraries do it for you); expect the server to drop a
      connection that is silent for 60 s.
- [ ] At most 5 connections per user: a 6th closes the oldest with 4009 (tell the player).

**What to persist between runs**

- [ ] The `TokenPair` (secret: store it like a password).
- [ ] Optionally the account (`id`, `display_name`) for an instant start, refreshed with
      `GET /v1/account`.
- [ ] The storage versions you last loaded or wrote, when the game keeps local copies of saves.
- [ ] Nothing about WebSocket state: room membership and presence are rebuilt after every connect.
