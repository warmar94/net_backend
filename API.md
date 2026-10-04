# net_backend API reference

The complete HTTP + WebSocket + JSON API of a [`net_backend_server`](crates/net_backend_server/README.md)
server, for clients that talk to it directly: web pages, JavaScript / TypeScript, C#, GDScript,
Python, anything with an HTTP and a WebSocket library.

Rust clients have ready-made libraries that already speak everything below:

| Your client is… | Use |
|---|---|
| a **Rust** app | [`net_backend_client`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_client) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) |
| a **Bevy** game | [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) |
| other **Rust** code | [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) + any HTTP / WebSocket library |
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
reference server (the one the repository's deployment files install) registers every module below; a
game's own server registers the modules it uses:

| Module | What it adds |
|---|---|
| core | `GET /v1/info`, `/healthz`, `/readyz`, the WebSocket endpoint `/v1/ws`, the API documents |
| `auth` | accounts, logins (email + password, Steam), tokens, sessions, email verification, password reset, roles, admin routes |
| `storage` | per-player JSON objects (save slots, settings) with versions; other players read the public ones (and the owner's friends the `friends` ones) |
| `chat` | public rooms, direct messages, history, presence, moderation, message editing, read markers and unread counts, typing indicators, rooms created by players (owner / moderators, public or private, invitations, kicks) |
| `leaderboards` | boards (best / latest / sum scores; all-time, daily or weekly), score submission, the top, the player's rank and the ranks around it |
| `notifications` | notifications stored per player and pushed live (`notify.new`), read / unread, deletes, a retention |
| `friends` | friends by account: requests by account id, display name or friend code, accept / decline / cancel / remove, blocks, the friends' online state (`friends.presence`); with Steam login, which of a list of Steam IDs belong to accounts here (for players who linked Steam), with a per-player opt-out |
| `groups` | groups (guilds, clans): create, invitations, open groups, join / leave / kick, roles (owner, admin, member), ownership transfer, metadata, a name search, a group chat room |
| `files` | players' binary files (screenshots, replays, mods, levels): multipart uploads with a content type, a SHA-256 and quotas, downloads, a visibility (private, public, friends, shared with chosen accounts), metadata |
| `oauth` | logins with an OpenID Connect provider's ID token (Google, or any OpenID Connect provider the operator configures), linked to the accounts |
| `lobbies` | lobbies: create, join by id or join code, leave, kick, a new host, ready flags, metadata, visibility (public / private / friends-only), state (open / in game / closed), a search by metadata, live pushes to the members (`lobby.member`, `lobby.changed`), a lobby chat room |
| `matchmaking` | queues and tickets; matches made by the server's rules (the game's, or first come, first matched), told to the players (`match.found`, `match.expired`) |

`GET /v1/info` lists the modules a server runs. A game's own server may add its own routes and
WebSocket kinds on top; they follow the same conventions and appear in the server's API documents.

### Base URL

Throughout this document the server is `https://your-server.example`. Every API path starts with
`/v1/` (two health routes do not). WebSocket: `wss://your-server.example/v1/ws`.

Production servers run behind a reverse proxy that terminates TLS: use `https://` and `wss://`.

### Versioning

- **Paths:** `/v1` is stable: within `/v1` routes, fields and WebSocket kinds keep their names
  and stay.
- **Protocol version:** an integer, `1`. A client may name the version it speaks in the
  `x-net-backend-protocol` header (HTTP requests and the WebSocket handshake) or in the `protocol`
  field of the WebSocket `auth` message. Without it the server assumes `1`. Every HTTP answer
  (except CORS preflights) carries the server's own version in the same header, and `GET /v1/info`
  lists the accepted range:

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
| chat room name (player rooms) | 1–64 characters, not blank; no control or invisible characters |
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
| `quota_exceeded` | 403 | a per-user quota is used up (stored objects / bytes, rooms per WebSocket, friends, open friend requests, blocks, groups per player, a full group, a group's open invitations, a full lobby, lobbies per player, chat rooms a player owns, a full player chat room) | free something up |
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
| `oauth_failed` | 401 | an OpenID Connect ID token was refused (signature, issuer, audience, expiry, nonce, or used before) | sign in at the provider again |
| `room_full` | 409 | the chat room is at its member cap | try later |
| `not_a_member` | 403 | not a member of the chat room (join it first; group / DM / player rooms: members only) | |
| `hook_timeout` | 503 | a server rule did not answer in time | retry later |
| `unavailable` | 503 | overloaded, shutting down, or the request took longer than the server's limit | retry later with backoff |
| `internal` | 500 | unexpected server error (never with details) | retry later; report with `x-request-id` |

Servers and their games may add their own codes: treat an unknown code like its HTTP status.

Notes:

- An unknown route answers 404 `{"error":{"code":"not_found","message":"no such route or object"}}`.
- A request that takes longer than the server's request timeout (default 30 s) answers 503
  `unavailable`. A file upload is timed by its data instead: 503 when no data arrives for 30 s or
  the upload takes longer than an hour (the server's defaults).
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
| OpenID Connect logins (`/v1/auth/oauth/{provider}`), per client address | 10 per minute |
| file uploads, per user | 10 per 60 s (a burst of 10, then one every 6 s) |
| refreshes, per client address | 60 per minute |
| forgot / reset / verify / resend / password change, per client address and route | 10 per minute |
| mails per account (verification, reset) | 3 per hour (a further reset request answers the same and sends nothing; a further resend answers 429) |
| failed logins per email address and client network | 5, then one more try every 3 minutes |
| failed logins per email address, from everywhere | 50 per hour; above it only networks that logged in to the account before may try |
| storage writes (`PUT`, `DELETE`, a batch counts once), per user | 60 per 60 s (a burst of 60, then one per second) |
| opening a direct-message room, per user | 20 per 600 s (a burst of 20, then one every 30 s) |
| leaderboard score submissions, per user | 30 per 60 s (a burst of 30, then one every 2 s) |
| friend requests, per user | 10 per 60 s (a burst of 10, then one every 6 s) |
| friends heartbeats, friend-code resets and friends settings changes (`POST /v1/friends/presence`, `POST /v1/friends/code`, `PUT /v1/friends/settings`; one bucket), per user | 30 per 60 s (a burst of 30, then one every 2 s) |
| Steam ID lookups (`POST /v1/friends/steam`), per user | 3 per 15 minutes (a burst of 3, then one every 5 minutes) |
| new groups, per user | 3 per hour (a burst of 3, then one every 20 minutes) |
| group invitations (`POST /v1/groups/{group}/invites`), per user | 20 per 600 s (a burst of 20, then one every 30 s) |
| new lobbies, per user | 5 per 60 s (a burst of 5, then one every 12 s) |
| lobby join attempts (by id or code), per user | 10 per 60 s (a burst of 10, then one every 6 s) |
| lobby join codes that match no lobby, per user | 20 per hour (then every code attempt answers 429 until the hour frees one) |
| lobby changes (`PATCH /v1/lobbies/{lobby}`, `POST /v1/lobbies/{lobby}/code`), per user | 30 per 60 s (a burst of 30, then one every 2 s) |
| matchmaking tickets, per user | 10 per 60 s (a burst of 10, then one every 6 s) |
| chat rooms created by a player, per user | 5 per hour (a burst of 5, then one every 12 minutes) |
| chat room invitations (`POST /v1/chat/rooms/{room}/invites`), per user | 20 per 600 s (a burst of 20, then one every 30 s) |
| chat read markers (`chat.mark_read`, `PUT …/read`), per user | 30 per 60 s (a burst of 30, then one every 2 s) |

Client networks are single IPv4 addresses and IPv6 /64 blocks. The failed-login limits count
whether or not the address has an account.

### WebSocket limits

| What | Limit |
|---|---|
| frames per connection | 20 per second, burst 40 (text, binary, ping and pong frames count); over it requests are answered `rate_limited`; a client that keeps flooding is closed with 1008 |
| chat messages per user | a burst of 5, then one every 2 s (`rate_limited` with `retry_after_ms`); a sender's message edits count too |
| chat typing pushes | at most one per user, room and 3 s; none in rooms with more than 50 online users |
| chat read pushes (`chat.read`) | at most one per user, room and 2 s (the newest marker wins) |
| message size | 1 MiB (1048576 bytes) in both directions, 16 KiB before the connection authenticated (only `auth` is sent then); bigger: close 1009 |
| connections per user | 5; a 6th closes the oldest with 4009 (the same session's first) |
| connections per client address | 100 (429 at the handshake) |
| handshakes per client address | 60 per minute (429 at the handshake) |
| rooms per connection | 16 (`quota_exceeded`) |
| connections per public room | 200 (`room_full`) |
| connections per group room (a group's, a lobby's) | 500 (`room_full`; a player with several devices counts once per connection) |
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
| leaderboard score metadata | 1 KiB of JSON (422 `validation_failed` above) |
| notifications per player | 200 (a newer one removes the oldest); kept 30 days |
| friends per player | 200 (403 `quota_exceeded`) |
| open friend requests per player | 50 sent and 50 received (403 `quota_exceeded`) |
| blocked players per player | 500 (403 `quota_exceeded`) |
| Steam IDs per lookup (`POST /v1/friends/steam`) | 500 (422 `validation_failed` above) |
| members per group | 100 (403 `quota_exceeded`) |
| groups per player | 10 (403 `quota_exceeded`) |
| open invitations per group | 50 (403 `quota_exceeded`) |
| group metadata | 2 KiB of JSON (422 `validation_failed` above) |
| players per lobby | 64 (the most a lobby may set; a full lobby: 403 `quota_exceeded`) |
| lobbies per player at a time | 1 (403 `quota_exceeded`) |
| lobby metadata | 32 keys, 4 KiB of keys and values (422 `validation_failed` above); one change sets or removes at most 64 keys |
| matchmaking ticket attributes | 1 KiB of JSON (422 `validation_failed` above) |
| one file | 16 MiB (413 `payload_too_large` above) |
| files per player | 100 (403 `quota_exceeded`) |
| file bytes per player | 256 MiB (403 `quota_exceeded`) |
| file metadata | 4 KiB of JSON (422 `validation_failed` above) |
| accounts a file is shared with | 50 (422 `validation_failed` above) |
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
  the other sessions; a password reset or a ban revokes all. An account keeps at most 100 live
  sessions (the operator's `max_sessions_per_user`): a login over it revokes the oldest. Revocation
  takes effect on the next request; open WebSockets of a revoked session are closed (4001, or 4003
  for a ban).
- **Expired or revoked tokens:** an expired access token answers 401 `token_expired` (refresh and
  retry); an unknown, malformed or revoked one 401 `unauthorized`; a banned account's token or
  refresh 403 `banned`. Routes that do not need a caller (register, login, refresh, logout,
  forgot / reset, verify) ignore a stale `Authorization` header, so a client that always sends its
  last token still works there. `POST /v1/auth/steam` and `POST /v1/auth/oauth/{provider}` are the
  exceptions: there a Bearer token that is invalid or expired is refused.

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
password. With the `friends` module, a player who linked Steam finds which of its Steam friends have
an account here ([`POST /v1/friends/steam`](#post-v1friendssteam)).

### OpenID Connect (Google and other providers)

A server with the `oauth` module and a configured provider accepts that provider's **ID token**:
`POST /v1/auth/oauth/{provider}` `{"id_token":"…","nonce":"…"}` → `AuthSession`. `{provider}` is the
server's name for it (for example `google`). The first login creates an account (its `email` is
`null`; `identities` holds `{"provider":"google","subject":"<the token's sub>"}`); with the Bearer
token of a login younger than 10 minutes the request links the provider account to that account
instead (otherwise 403 `reauthentication_required`). A provider account that is linked to another
account answers 409 `conflict`: accounts are never merged. Unlinking is
`DELETE /v1/account/identities/{provider}`.

The server checks the token's signature with the provider's published keys (RS256 or ES256 only),
the issuer, that the audience is one of the game's client ids, the expiry, the issue time and the
nonce; each nonce (or token) is accepted once. A refused token answers 401 `oauth_failed`; an
unknown provider 404; 503 `unavailable` when the provider's keys cannot be fetched.

How a desktop game gets the ID token (the authorization code flow with PKCE and a loopback redirect,
RFC 8252):

1. Make a random PKCE verifier, `state` and nonce. Listen on `http://127.0.0.1:<free port>/callback`.
2. Open the system browser at the provider's authorization endpoint (Google:
   `https://accounts.google.com/o/oauth2/v2/auth`) with `response_type=code`, `client_id`,
   `redirect_uri` (the loopback address), `scope=openid`, `state`, `nonce`,
   `code_challenge` (base64url SHA-256 of the verifier) and `code_challenge_method=S256`.
3. The browser comes back to the loopback address with `code` and `state`; check `state`.
4. Exchange the code at the token endpoint (Google: `https://oauth2.googleapis.com/token`):
   `grant_type=authorization_code`, `code`, `redirect_uri`, `client_id`, `code_verifier` (and the
   `client_secret` Google gives "Desktop app" clients). The answer's `id_token` is the ID token.
5. `POST /v1/auth/oauth/google` `{"id_token":"…","nonce":"<the nonce from step 1>"}`.

Any other way to get an ID token for the game's client id works too (for example the device
authorization flow on consoles and TVs). `net_backend_client` runs steps 1–5 with its feature
`oauth`.

### Bans

A banned account gets 403 `banned` on login, refresh and every authenticated request, with
`details.until` (unix ms) for a timed ban. Its sessions are revoked and its WebSockets closed with
4003. Show the ban and do not retry automatically.

### Roles

Roles are strings in `account.roles` (`admin`, `moderator`, a game's own); a normal player has
none. The admin routes need `admin`; deleting other players' chat messages needs `admin` or
`moderator` (server setting) or the permission `chat.moderate`; editing them needs `chat.moderate`.

A server may also check **permissions** that roles hold (named rights such as `lobbies.manage`;
`admin` holds every one; the server's operator grants them to other roles). With `lobbies.manage`
(`admin` and `moderator` by default), a player acts as the host of every lobby; with `chat.moderate`
(`admin` and `moderator` by default), a player edits and deletes any chat message and acts as the
owner of every player chat room (it opens, reads and joins private ones too). A missing permission
answers 403 `forbidden`.

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
LinkedIdentity     {"provider":"steam" or an OpenID Connect provider's name, "subject":string}
TokenPair          {"token_type":"Bearer", "access_token":string, "access_expires_at":ms,
                    "refresh_token":string, "refresh_expires_at":ms}
AuthSession        {"account":Account, "tokens":TokenPair}
StorageObject      {"collection":string, "key":string, "owner":int, "value":JSON, "version":int,
                    "write":"owner"|"server", "visibility":"private"|"public"|"friends", "updated_at":ms}
StorageObjectInfo  {"collection":string, "key":string, "version":int, "write":"owner"|"server",
                    "visibility":"private"|"public"|"friends", "size_bytes":int, "updated_at":ms}
FileInfo           {"id":int, "owner":int, "name":string, "content_type":string, "size":int,
                    "sha256":string, "visibility":"private"|"public"|"friends"|"shared",
                    "shared_with"?:[int], "metadata"?:JSON, "created_at":ms, "updated_at":ms}
ObjectAck          {"collection":string, "key":string, "version":int, "updated_at":ms}
RoomInfo           {"id":int, "kind":"room"|"dm"|"group"|"player", "key"?:string, "name"?:string,
                    "member_count"?:int, "max_members"?:int, "peer"?:int,
                    "visibility"?:"public"|"private", "owner"?:int,
                    "role"?:"owner"|"moderator"|"member"|"invited"|"banned"}
ChatMessage        {"id":int, "room":int, "sender":int, "sender_name"?:string|null, "text":string,
                    "sent_at":ms, "nonce"?:string, "edited_at"?:ms}
Page<T>            {"items":[T], "next_cursor"?:string}
```

- `RoomInfo.kind`: `room` (public), `dm` (direct messages, with `peer` = the other user), `group`
  (members only), `player` (created by a player; with `visibility`, `owner` and the caller's
  `role`). `member_count` counts users online in the room now; `max_members` is the cap in
  connections.
- `ChatMessage.edited_at`: present when the text was edited; `text` is the latest text.
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

#### `POST /v1/auth/oauth/{provider}`

- Auth: no; optional Bearer of a recent login to **link** the provider account to that account.
- Body: `{"id_token":string, "nonce":string}` (the provider's ID token as a compact JWT, at most
  16 KiB; the nonce of the sign-in, 1–256 printable ASCII characters; servers configured without
  required nonces accept a body without `nonce`).
- 200: `AuthSession`.
- Errors: 401 `oauth_failed` (the token was refused), or the Bearer token sent is `token_expired` /
  `unauthorized`; 403 `banned`, `reauthentication_required` (linking needs a login younger than 10
  minutes), or refused by a server rule; 404 no such provider on this server; 409 `conflict` (the
  provider account is linked to another account, or this account already has one of this
  provider); 422 `validation_failed` (not a compact JWT, a bad nonce); 429 `rate_limited`; 503
  `unavailable` (the provider's keys cannot be fetched).

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

Unlink a login provider (`steam`, or an OpenID Connect provider's name such as `google`) from the
caller's account.

- Auth: yes, with a login younger than 10 minutes.
- 200: `{}`.
- Errors: 403 `reauthentication_required` (log in again first); 404 `not_found` (no such linked
  provider); 409 `conflict` (it is the account's only way to log in).

### Storage

The `/v1/storage` routes address the caller's own objects; `/v1/users/{user}/storage` reads another
player's objects the caller may read. See [Storage flows](#8-storage-flows).

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
- Body: `{"value":JSON, "if_version"?:int, "visibility"?:"private"|"public"|"friends"}`.
  `if_version: N` writes only over version N; `if_version: 0` writes only if the object does not
  exist. `visibility` sets who may read it (absent: unchanged; a new object is `private`; `friends`
  needs the friends module).
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
- Body: `{"objects":[{"collection":string, "key":string, "value":JSON, "if_version"?:int, "visibility"?:string}]}`
  (1–16 distinct objects, at most 4 MiB of values together).
- 200: `{"objects":[ObjectAck]}`, in request order.
- Errors: 403 `forbidden` / `quota_exceeded` / a server rule's refusal (details: `index` of the
  failing item); 409 `version_conflict` (details: `index` and `current_version` of the first
  failing item); 422 `validation_failed`; 429 `rate_limited` (a batch counts as one write).

#### `GET /v1/users/{user}/storage/{collection}`

A page of another player's objects in a collection that the caller may read (public ones; `friends`
ones for the owner's friends; every one for the owner), **without values**, ordered by key.

- Auth: yes.
- Query: `cursor`?, `limit`? (1–100, default 50).
- 200: `Page<StorageObjectInfo>` (an unknown player: an empty page).
- Errors: 400 `bad_request` (an invalid name or cursor).

#### `GET /v1/users/{user}/storage/{collection}/{key}`

- Auth: yes.
- 200: `StorageObject`; header `ETag: "<version>"`.
- Errors: 404 `not_found` (no such object, or the caller may not read it).

### Files

Players' binary files. Only the owner changes or deletes a file; who else reads it follows its
`visibility`: `private` (nobody), `public` (every logged-in player), `friends` (the owner's friends;
servers with the friends module), `shared` (the accounts in `shared_with`). A player who may not
read a file gets 404 for it.

#### `POST /v1/files`

Upload a file: a `multipart/form-data` body with up to two parts, in this order:

| Part | Content |
|---|---|
| `meta` (optional) | `application/json`: `{"name"?:string, "content_type"?:string, "visibility"?:string, "shared_with"?:[int], "metadata"?:JSON, "sha256"?:string}` |
| `file` | the bytes; its `filename` and `Content-Type` are the defaults for `name` and `content_type` |

- Auth: yes.
- Names: 1–255 bytes, no `/` or `\`, no control or invisible characters (default: the file part's
  file name without its folders, else `file`). Content types: a plain `type/subtype` (default
  `application/octet-stream`). `sha256`: the lower-case hex SHA-256 the bytes must have.
- 200: `FileInfo` (with the server's `sha256` of the bytes).
- Errors: 400 `bad_request` (not multipart, an unknown or repeated part, a part after `file`, a body
  that ended early); 403 `quota_exceeded` (too many files or bytes) or a server rule's refusal; 413
  `payload_too_large` (the file is larger than the server's limit); 422 `validation_failed` (no
  `file` part, a bad name, content type, visibility, share list or metadata, an account in
  `shared_with` that does not exist, a `sha256` that does not match); 429 `rate_limited`.
- Nothing is kept of a refused upload. An upload is timed by its data, not by the request time
  limit: it fails with 503 `unavailable` when no data arrives for 30 s or the upload takes longer
  than an hour (the server's defaults), so a slow connection can send a large file.

#### `GET /v1/files`

- Auth: yes.
- Query: `owner`? (another player: the files the caller may read; default: the caller's own),
  `cursor`?, `limit`? (1–100, default 50).
- 200: `Page<FileInfo>`, newest first.
- Errors: 400 `bad_request` (an invalid cursor).

#### `GET /v1/files/usage`

- Auth: yes.
- 200: `{"files":int, "bytes":int, "max_files":int, "max_bytes":int, "max_file_bytes":int}`.

#### `GET /v1/files/{file}`

- Auth: yes.
- 200: `FileInfo` (`shared_with` for the owner of a `shared` file).
- Errors: 404 `not_found`.

#### `GET /v1/files/{file}/content`

The bytes, as an attachment.

- Auth: yes.
- Headers: `If-None-Match`? (the `ETag` from before: 304 when unchanged).
- 200: the bytes; headers `Content-Type` (as stored), `Content-Length`, `ETag: "<sha256>"`,
  `Content-Disposition: attachment`, `X-Content-Type-Options: nosniff`.
- Errors: 404 `not_found`.

#### `PATCH /v1/files/{file}`

- Auth: yes (the owner).
- Body: `{"name"?:string, "visibility"?:string, "shared_with"?:[int], "metadata"?:JSON}` (absent
  fields stay; `"metadata":null` clears it; `shared_with` replaces the list and goes with
  `shared`; leaving `shared` drops the list). A server rule may refuse the change or keep a
  setting (e.g. a file stays private until the game reviewed it): the answer shows the result.
- 200: `FileInfo`.
- Errors: 403 `forbidden` (a reader who is not the owner, or a server rule); 404 `not_found`; 422
  `validation_failed`.

#### `DELETE /v1/files/{file}`

- Auth: yes (the owner).
- 200: `{}` (the file and its bytes are gone).
- Errors: 403 `forbidden` (a reader who is not the owner); 404 `not_found`.

### Chat

Joining, sending and live messages use the [WebSocket](#chat-kinds); these HTTP routes list rooms,
read history, open direct messages, edit and delete messages, keep read markers and unread counts,
and manage rooms created by players. See [Chat flows](#9-chat-flows).

#### `GET /v1/chat/rooms`

The public rooms, with online counts and caps.

- Auth: yes.
- Query: `cursor`?, `limit`?.
- 200: `Page<RoomInfo>`.

#### `GET /v1/chat/rooms/{room}/messages`

A page of a room's history, **newest first**.

- Auth: yes. Public rooms: any player; group, player and DM rooms: their members (player rooms
  also players with `chat.moderate`).
- Query: `cursor`? (older messages), `limit`?.
- 200: `Page<ChatMessage>`.
- Errors: 403 `not_a_member`; 404 `not_found`.

#### `DELETE /v1/chat/rooms/{room}/messages/{message}`

Delete a message: its sender, or a moderator (roles `admin` / `moderator` by default). The room
gets a `chat.deleted` push and the message leaves the history.

- Auth: yes.
- 200: `{}`.
- Errors: 403 `forbidden` (neither its sender nor a moderator), `not_a_member` (its sender left the
  group or player room, or was removed from it); 404 `not_found` (no such message, or deleted
  already).

In a player room, its owner and moderators may delete any message of the room too.

#### `PATCH /v1/chat/rooms/{room}/messages/{message}`

Edit a message: its sender within 900 s of sending (server setting; the edit counts on the send
rate), or a player with the permission `chat.moderate` (any message; recorded in the audit log). The
text follows the rules of a new message and passes the server's message rules again. The room gets a
`chat.edited` push.

- Auth: yes.
- Body: `{"text":string}`.
- 200: `ChatMessage` with `edited_at`.
- Errors: 403 `forbidden` (not its sender, the edit window is over, editing turned off, or a server
  rule), `not_a_member`; 404 `not_found` (no such message, or deleted); 422 `validation_failed`; 429
  `rate_limited`.

#### `PUT /v1/chat/rooms/{room}/read`

Store the caller's read marker: "read up to this message". It only moves forward (an older message
leaves it where it is). In DM, group and player rooms the room gets a `chat.read` push (at most one
per user, room and 2 s; the newest marker wins); public rooms keep the marker for unread counts only.

- Auth: yes (a player who may read the room).
- Body: `{"message":int}` (a message of this room).
- 200: `{}`.
- Errors: 403 `not_a_member`; 404 `not_found` (no such room, or the message is not in it); 429
  `rate_limited`.

#### `GET /v1/chat/rooms/{room}/receipts`

The read markers of a DM, group or player room, the newest 200 first.

- Auth: yes (members).
- 200: `{"room":int, "receipts":[{"room":int, "user":int, "message":int, "read_at":ms}]}`.
- Errors: 400 `bad_request` (a public room); 403 `not_a_member`; 404 `not_found`.

#### `POST /v1/chat/unread`

The caller's unread counts of up to 100 rooms. A message counts when it is newer than the caller's
read marker, not the caller's own, not deleted and inside the history retention; a count stops at
1000 ("1000 or more"). Rooms the caller cannot read, or that do not exist, are left out.

- Auth: yes.
- Body: `{"rooms":[int]}` (1–100).
- 200: `{"rooms":[{"room":int, "unread":int, "last_read"?:int}]}` (in the order asked).
- Errors: 422 `validation_failed`.

#### Rooms created by players

A player creates a room and owns it. **Public** rooms are listed and any player who is not banned
may join; **private** rooms take invited players only. Membership is stored: it outlives
connections. The owner renames the room, changes its visibility, names moderators, hands the room
on and deletes it; moderators rename it, invite players, kick members and invited players, and
delete messages in it. A kicked player is **banned** from the room until invited again. When the
owner leaves, the oldest moderator (else the oldest member) owns the room; when the last member
leaves, the room is deleted. Every change is a `chat.room` push to the members and invited players
([Chat kinds](#chat-kinds)); with the `notifications` module an invited player also gets a
`chat.invite` notification (`data`: `{"room":int, "name":string}`). Players with the permission
`chat.moderate` act as the owner of every player room: they open, read and join private rooms too
(also after a ban). Limits (server settings): 10 rooms owned per player (counted when a player
creates a room or is handed one; a player who becomes the owner because the owner left is not
refused), 100 members plus open invitations per room, 5 new rooms per player and hour, 20
invitations per player and 10 minutes.

To chat in a player room, join it with `chat.join` on the WebSocket (that also makes the caller a
member of a public room, or accepts an invitation).

#### `POST /v1/chat/rooms`

Create a player room.

- Auth: yes.
- Body: `{"name":string, "visibility"?:"public"|"private"}` (default `private`).
- 200: `RoomInfo` (`kind: "player"`, `owner`, `role: "owner"`).
- Errors: 403 `quota_exceeded` (the caller owns 10 rooms), `forbidden` (player rooms turned off, or
  a server rule); 422 `validation_failed`; 429 `rate_limited`.

#### `GET /v1/chat/rooms/mine`

The caller's player rooms and invitations, oldest membership first, each with the caller's `role`.

- Auth: yes.
- Query: `cursor`?, `limit`?.
- 200: `Page<RoomInfo>`.

#### `GET /v1/chat/rooms/public`

The public player rooms, oldest first (the server's own rooms: `GET /v1/chat/rooms`).

- Auth: yes.
- Query: `cursor`?, `limit`?.
- 200: `Page<RoomInfo>`.

#### `GET /v1/chat/rooms/{room}`

One room as the caller sees it: public rooms and public player rooms for any player; DM, group and
private player rooms for their members (and invited players; private player rooms also for players
with `chat.moderate`).

- Auth: yes.
- 200: `RoomInfo`.
- Errors: 403 `not_a_member`; 404 `not_found`.

#### `PATCH /v1/chat/rooms/{room}`

Rename a player room (owner, moderators) or change its visibility (owner); absent fields stay.

- Auth: yes.
- Body: `{"name"?:string, "visibility"?:"public"|"private"}` (at least one).
- 200: `RoomInfo`.
- Errors: 400 `bad_request` (not a player room); 403 `not_a_member` / `forbidden`; 404 `not_found`;
  422 `validation_failed`.

#### `DELETE /v1/chat/rooms/{room}`

Delete a player room (its owner): its messages, members and read markers go; every member and invited
player gets `chat.room` with `change: "deleted"`.

- Auth: yes.
- 200: `{}`.
- Errors: 400 `bad_request` (not a player room); 403 `not_a_member` / `forbidden` (not the owner); 404
  `not_found`.

#### `POST /v1/chat/rooms/{room}/join`

Become a member of a player room: a public room, or accept an invitation (players with
`chat.moderate`: any player room).

- Auth: yes.
- 200: `RoomInfo` (with `role`).
- Errors: 400 `bad_request` (not a player room); 403 `not_a_member` (private, not invited),
  `forbidden` (banned, or a server rule), `quota_exceeded` (the room is full); 404 `not_found`.

#### `POST /v1/chat/rooms/{room}/leave`

Stop being a member of a player room, or decline an invitation (a ban stays).

- Auth: yes.
- 200: `{}` (also when the caller was no member).
- Errors: 400 `bad_request` (not a player room); 404 `not_found`.

#### `GET /v1/chat/rooms/{room}/members`

The rows of a player room, oldest first: members and invited players; the owner and moderators also
see bans.

- Auth: yes (members and invited players).
- Query: `cursor`?, `limit`?.
- 200: `Page<{"user":int, "name"?:string, "role":"owner"|"moderator"|"member"|"invited"|"banned", "since":ms}>`.
- Errors: 400 `bad_request` (not a player room); 403 `not_a_member`; 404 `not_found`.

#### `POST /v1/chat/rooms/{room}/invites`

Invite a player (owner, moderators). An invitation lifts a ban; inviting a member or an invited
player changes nothing. A player who blocked the caller (friends module) is not invited.

- Auth: yes.
- Body: `{"user":int}`.
- 200: `{}`.
- Errors: 400 `bad_request` (not a player room, or yourself); 403 `not_a_member` / `forbidden` (not
  the owner or a moderator, the player blocked the caller, or a server rule), `quota_exceeded` (the
  room is full); 404 `not_found` (no such room or account); 429 `rate_limited` (20 invitations per
  10 minutes).

#### `DELETE /v1/chat/rooms/{room}/members/{user}`

Kick a player (owner: anyone; moderators: members and invited players): banned until invited
again; its connections leave the room at once. Withdraws an invitation the same way.

- Auth: yes.
- 200: `{}` (also when the player was banned already).
- Errors: 400 `bad_request` (not a player room, or yourself); 403 `not_a_member` / `forbidden`; 404
  `not_found` (the player is not in the room).

#### `PUT /v1/chat/rooms/{room}/members/{user}/role`

Make a member a moderator or a member again (the owner).

- Auth: yes.
- Body: `{"role":"moderator"|"member"}`.
- 200: `{}`.
- Errors: 400 `bad_request` (the owner's own role: hand the room on instead); 403 `not_a_member` /
  `forbidden`; 404 `not_found` (no such member); 422 `validation_failed` (another role).

#### `POST /v1/chat/rooms/{room}/owner`

Hand the room to a member (the owner); the old owner becomes a moderator.

- Auth: yes.
- Body: `{"user":int}`.
- 200: `{}`.
- Errors: 400 `bad_request` (yourself); 403 `not_a_member` / `forbidden`, `quota_exceeded` (the new
  owner owns 10 rooms); 404 `not_found` (no such account, or not a member).

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

### Leaderboards

The server configures its boards; players submit scores and read them. A board has a key
(`highscore`, 1–64 characters of `a-z 0-9 _ - .`), a **mode** (what a new score does to the
player's stored one: `best` keeps the better, `latest` replaces it, `sum` adds to it), an
**order** (`desc`: higher is better; `asc`: lower is better, e.g. race times) and a **period**
(`all_time`, or `daily` / `weekly`: a fresh board every day at 00:00 UTC / every Monday at 00:00
UTC). Finished periods stay readable for a while (the server keeps 8 by default): every read takes
`at`, any time inside the period to show.

**Ranks** start at 1 and are unique: equal scores rank by who reached the score first, then by the
lower account id. A score is any 64-bit integer except the smallest; a `sum` stops at the largest
and the smallest-but-one.

```text
BoardInfo        {"key":string, "name"?:string, "mode":"best"|"latest"|"sum", "order":"desc"|"asc",
                  "period":"all_time"|"daily"|"weekly", "period_start"?:ms, "period_end"?:ms, "client_submit":bool}
LeaderboardEntry {"rank":int, "user":int, "name"?:string, "score":int, "metadata"?:JSON, "achieved_at":ms}
LeaderboardPage  {"board":string, "period_start"?:ms, "period_end"?:ms, "items":[LeaderboardEntry], "next_cursor"?:string}
```

`period_start` / `period_end` are absent on all-time boards.

#### `GET /v1/leaderboards`

- Auth: yes.
- 200: `{"boards":[BoardInfo]}`, ordered by key, with the current period of each.

#### `GET /v1/leaderboards/{board}`

A page of the board, **best first**.

- Auth: yes.
- Query: `cursor`?, `limit`? (1–100, default 50), `at`? (ms; default now).
- 200: `LeaderboardPage`.
- Errors: 400 `bad_request` (an invalid key or cursor); 404 `not_found` (no such board).

#### `POST /v1/leaderboards/{board}/scores`

Submit a score for the caller.

- Auth: yes.
- Body: `{"score":int, "metadata"?:JSON}` (metadata: at most 1 KiB of JSON, stored with the score it
  belongs to: on a `best` board only when the score improves).
- 200: `{"board":string, "period_start"?:ms, "score":int, "submitted":int, "changed":bool, "rank":int}`:
  `score` is the stored score now, `submitted` what was submitted (after the server's rules),
  `changed` whether the stored score changed (`false`: a better score was kept).
- A `sum` board adds every submission it accepts: a request sent again (a retry after a timeout)
  adds again. Such boards are usually written by the server (`client_submit: false`).
- Errors: 403 `forbidden` (the board takes scores from the server only: `client_submit: false`; or
  a server rule's refusal); 404 `not_found`; 422 `validation_failed`; 429 `rate_limited`
  (details `retry_after_ms`).

#### `GET /v1/leaderboards/{board}/me`

- Auth: yes.
- Query: `at`?.
- 200: `{"board":string, "period_start"?:ms, "period_end"?:ms, "entry"?:LeaderboardEntry, "total":int}`
  (`entry` absent when the caller has no score in the period; `total`: players with a score).
- Errors: 404 `not_found`.

#### `GET /v1/leaderboards/{board}/around`

The entries around the caller, the caller included, best first.

- Auth: yes.
- Query: `above`?, `below`? (0–50 each, default 5), `at`?.
- 200: `LeaderboardPage` (no `next_cursor`; empty `items` when the caller has no score in the period).
- Errors: 404 `not_found`.

### Notifications

Messages the server stores for one player (a reward, an invitation, a system notice): pushed live
as [`notify.new`](#notification-kinds) to the player's open connections and kept until the player
deletes them or they pass the server's retention (30 days by default). A player keeps at most 200
(a newer one removes the oldest). Only the server creates them; players read, mark and delete their
own. Every route has a WebSocket twin (`notify.*`) with the same answer.

```text
Notification {"id":int, "kind":string, "text"?:string, "data"?:JSON, "sender"?:int, "created_at":ms, "read":bool}
```

`kind` is the game's own (`reward`, `invite.match`); `sender` is the account that caused it (absent
for system notices, and once that account is deleted).

#### `GET /v1/notifications`

The caller's notifications, **newest first**.

- Auth: yes.
- Query: `cursor`?, `limit`? (1–100, default 50), `unread_only`? (`true`: only the unread ones).
- 200: `Page<Notification>`.
- Errors: 400 `bad_request` (an invalid cursor).

#### `GET /v1/notifications/count`

- Auth: yes.
- 200: `{"unread":int, "total":int}`.

#### `POST /v1/notifications/mark`

Mark notifications read or unread. Ids of other players, or already in that state, are skipped.

- Auth: yes.
- Body: `{"ids":[int], "read":bool}` (1–100 distinct ids) or `{"all":true, "read":bool}`.
- 200: `{"changed":int, "unread":int}` (how many changed; the caller's unread count now).
- Errors: 422 `validation_failed`.

#### `DELETE /v1/notifications/{id}`

- Auth: yes.
- 200: `{}` (also when it does not exist, or is not the caller's: nothing is deleted then).
- Errors: 400 `bad_request` (the id is not a number).

### Friends

Friends by account. A player adds another by **account id**, by **display name** or by **friend
code**; the other accepts or declines, the sender may cancel; either ends the friendship. When two
players ask each other, they are friends at once. A player **blocks** anyone: a block ends a
friendship and every open request between the two, and the blocked player's requests answer 403.

Every player has a **friend code** (made on first use): 8 characters of `2-9` and `A-Z` without `O`
and `I` (`K7M2Q9XD`). Players type it in any case, with spaces or dashes. A new code replaces the
old one. **Names** are display names, matched exactly (after trimming); display names are not
unique, so a name several players have answers 409 and the player uses the friend code instead.

**Online state** (friends only): a player is online while it has an open WebSocket connection, and
for 90 s after its last heartbeat (`POST /v1/friends/presence`: clients without a WebSocket send one
every minute or so). Friends get [`friends.presence`](#friend-kinds) when a player comes online or
goes offline. With the `notifications` module on the server, a request also sends the other player a
`friends.request` notification and an acceptance sends `friends.accepted` (the notification's
`sender` is the acting player).

**Steam IDs** (servers with Steam login): a player who linked a Steam account sends a list of Steam
IDs, e.g. its Steam friends list, and gets back the ones that belong to accounts here
([`POST /v1/friends/steam`](#post-v1friendssteam)). Found are accounts with that Steam account
linked; never the caller, banned accounts, players with a block between them and the caller (either
direction), or players who turned **`steam_findable`** off ([`PUT /v1/friends/settings`](#put-v1friendssettings)).
Steam-linked players are findable until they turn it off. A hidden player and a Steam ID without an
account look the same: absent from the answer.

```text
FriendEntry {"user":int, "name"?:string, "state":"friend"|"sent"|"received"|"blocked", "since":ms,
             "online"?:bool, "last_seen"?:ms}
```

`since` is when the state began (the request, the friendship, the block); `online` and `last_seen`
are present for friends only.

#### `GET /v1/friends`

The caller's friends, newest first, with their online state.

- Auth: yes.
- Query: `cursor`?, `limit`? (1–100, default 50).
- 200: `Page<FriendEntry>`.
- Errors: 400 `bad_request` (an invalid cursor).

#### `DELETE /v1/friends/{user}`

End a friendship (both sides).

- Auth: yes.
- 200: `{}` (also when there was none).
- Errors: 404 `not_found` (no such account); 422 `validation_failed` (the caller's own id).

#### `GET /v1/friends/requests`

The caller's open requests, newest first.

- Auth: yes.
- Query: `direction`? (`received`, the default, or `sent`), `cursor`?, `limit`?.
- 200: `Page<FriendEntry>` (state `received` or `sent`).
- Errors: 400 `bad_request` (an invalid cursor or direction).

#### `POST /v1/friends/requests`

Send a friend request.

- Auth: yes.
- Body: exactly one of `{"user":int}`, `{"name":string}`, `{"code":string}`.
- 200: `FriendEntry`: state `sent`, or `friend` when that player had already asked the caller (a
  request sent before answers its entry again).
- Errors: 403 `forbidden` (that player blocked the caller), 403 `quota_exceeded` (the caller has 50
  open requests, or that player has 50 received ones); 404 `not_found` (no such account, name or
  code); 409 `conflict` (several players have that name; or the caller blocked that player: unblock
  first); 422 `validation_failed` (not exactly one of the three, or the caller's own account); 429
  `rate_limited`.

#### `DELETE /v1/friends/requests/{user}`

Withdraw a request the caller sent.

- Auth: yes.
- 200: `{}` (also when there was none).
- Errors: 404 `not_found` (no such account).

#### `POST /v1/friends/requests/{user}/accept`

Accept a request the caller received.

- Auth: yes.
- 200: `FriendEntry` (state `friend`; also when they were friends already).
- Errors: 403 `quota_exceeded` (one of the two has 200 friends); 404 `not_found` (no request from
  this player).

#### `POST /v1/friends/requests/{user}/decline`

- Auth: yes.
- 200: `{}` (also when there was no request).
- Errors: 404 `not_found` (no such account).

#### `GET /v1/friends/blocks`

The players the caller blocked, newest first.

- Auth: yes.
- Query: `cursor`?, `limit`?.
- 200: `Page<FriendEntry>` (state `blocked`).

#### `PUT /v1/friends/blocks/{user}`

Block a player.

- Auth: yes.
- 200: `{}` (also when it was blocked already).
- Errors: 403 `quota_exceeded` (500 blocked players); 404 `not_found`; 422 `validation_failed` (the
  caller's own id).

#### `DELETE /v1/friends/blocks/{user}`

Lift a block.

- Auth: yes.
- 200: `{}` (also when there was none).
- Errors: 404 `not_found`.

#### `GET /v1/friends/code`

- Auth: yes.
- 200: `{"code":string}`.

#### `POST /v1/friends/code`

A new friend code; the old one stops working.

- Auth: yes.
- 200: `{"code":string}`.
- Errors: 429 `rate_limited` (heartbeats, code resets and settings changes: 30 per 60 s together).

#### `POST /v1/friends/presence`

The heartbeat of a client without a WebSocket connection: the caller counts as online for 90 s.

- Auth: yes.
- 200: `{}`.
- Errors: 429 `rate_limited` (heartbeats, code resets and settings changes: 30 per 60 s together).

#### `POST /v1/friends/steam`

Which of these Steam IDs belong to accounts here. Steam IDs are SteamID64s of individual Steam
accounts as **decimal strings** (a SteamID64 is larger than the integers a JSON number carries
exactly).

- Auth: yes; the caller must have a Steam account linked.
- Body: `{"steam_ids":[string]}` (at most 500; an empty list answers an empty list).
- 200: `{"players":[SteamPlayer]}`, in the order of the request, each account once:

  ```text
  SteamPlayer {"steam_id":string, "user":int, "name"?:string, "state"?:"friend"|"sent"|"received"}
  ```

  `steam_id` is the Steam ID as sent; `state` is the caller's relation to that player (absent: none).
- Errors: 403 `forbidden` (the caller has no Steam account linked, or the server's own rule refused
  it); 404 `not_found` (the server has no Steam login); 422 `validation_failed` (an entry is not the
  SteamID64 of an individual Steam account in decimal digits, `details` name the entry as
  `steam_ids[3]`; or more than 500); 429 `rate_limited` (3 lookups, then one every 5 minutes).

#### `GET /v1/friends/settings`

The caller's friends settings.

- Auth: yes.
- 200: `{"steam_findable":bool}` (`true` until the player turns it off).

#### `PUT /v1/friends/settings`

Change the caller's friends settings; fields left out keep their value.

- Auth: yes.
- Body: `{"steam_findable"?:bool}`.
- 200: `{"steam_findable":bool}` (after the change).
- Errors: 429 `rate_limited` (heartbeats, code resets and settings changes: 30 per 60 s together).

### Groups

Groups (guilds, clans). A player creates a group and **owns** it; others join by **invitation**
(the owner and admins invite; the invited player accepts or declines) or directly when the group is
**open**. Roles: one **owner** (every right; hands the group to a member with a transfer and becomes
an admin), **admins** (change the group, invite, withdraw invitations, remove members) and
**members**. The owner leaves only as the last member (the group is then deleted) or after a
transfer.

Group names are 3–32 characters (no control or invisible characters), unique on the server without
regard to case. With the `notifications` module on the server, an invitation reaches the player as
a `groups.invite` notification and a removal as `groups.kicked` (`data`: `{"group":int,
"name":string}`, `sender`: the acting player). With the `chat` module, every group has a chat room
for its members (`chat_room`: join it with `chat.join`); joining the group adds the player, leaving
or a removal takes them out at once, and deleting the group deletes the room with its messages.

```text
GroupInfo   {"id":int, "name":string, "description"?:string, "open":bool, "metadata"?:JSON, "owner"?:int,
             "members":int, "max_members":int, "chat_room"?:int, "created_at":ms, "role"?:"owner"|"admin"|"member"}
GroupMember {"user":int, "name"?:string, "role":"owner"|"admin"|"member", "joined_at":ms}
GroupInvite {"group":GroupInfo, "inviter"?:int, "created_at":ms}
```

`role` is the caller's, present when the caller is a member; `owner` is absent when the owner's
account was deleted.

#### `GET /v1/groups`

The groups by name.

- Auth: yes.
- Query: `query`? (the start of the name, any case, at most 32 characters), `cursor`?, `limit`?
  (1–100, default 50).
- 200: `Page<GroupInfo>`.
- Errors: 422 `validation_failed` (the query is too long).

#### `POST /v1/groups`

Create a group; the caller is its owner.

- Auth: yes.
- Body: `{"name":string, "description"?:string, "open"?:bool, "metadata"?:JSON}` (description: at
  most 500 characters; metadata: at most 2 KiB of JSON; `open` false by default).
- 200: `GroupInfo` (`role`: `owner`).
- Errors: 403 `quota_exceeded` (the caller is in 10 groups), 403 `forbidden` (a server rule); 409
  `conflict` (the name is taken); 422 `validation_failed`; 429 `rate_limited`.

#### `GET /v1/groups/mine`

- Auth: yes.
- 200: `{"groups":[GroupInfo]}`: the caller's groups with its role, oldest membership first.

#### `GET /v1/groups/invites`

The caller's invitations, newest first.

- Auth: yes.
- Query: `cursor`?, `limit`?.
- 200: `Page<GroupInvite>`.

#### `GET /v1/groups/{group}`

- Auth: yes.
- 200: `GroupInfo`.
- Errors: 404 `not_found`.

#### `PATCH /v1/groups/{group}`

Change a group (the owner or an admin). Absent fields stay; `"description":""` removes the
description, `"metadata":null` the metadata.

- Auth: yes.
- Body: `{"name"?:string, "description"?:string, "open"?:bool, "metadata"?:JSON}`.
- 200: `GroupInfo`.
- Errors: 403 `not_a_member` / `forbidden` (not the owner or an admin, or a server rule); 404
  `not_found`; 409 `conflict` (the name is taken); 422 `validation_failed`.

#### `DELETE /v1/groups/{group}`

Delete a group (the owner): its members, invitations and chat room go.

- Auth: yes.
- 200: `{}`.
- Errors: 403 `not_a_member` / `forbidden`; 404 `not_found`.

#### `GET /v1/groups/{group}/members`

The members in the order they joined.

- Auth: yes.
- Query: `cursor`?, `limit`?.
- 200: `Page<GroupMember>`.
- Errors: 404 `not_found`.

#### `POST /v1/groups/{group}/join`

Join an open group, or one that invited the caller (the invitation is used).

- Auth: yes.
- 200: `GroupInfo` (also when the caller was a member already).
- Errors: 403 `forbidden` (invitation only, or a server rule), 403 `quota_exceeded` (the group has
  100 members, or the caller is in 10 groups); 404 `not_found`.

#### `POST /v1/groups/{group}/leave`

- Auth: yes.
- 200: `{}` (also when the caller was no member). The owner as the last member: the group is deleted.
- Errors: 404 `not_found`; 409 `conflict` (the owner of a group with other members: transfer first).

#### `POST /v1/groups/{group}/invites`

Invite a player (the owner or an admin). A player who blocked the caller (friends module) is not
invited.

- Auth: yes.
- Body: `{"user":int}`.
- 200: `{}` (also when the player was invited already: no second notification).
- Errors: 403 `not_a_member` / `forbidden` (not the owner or an admin, the player blocked the
  caller, or a server rule), 403 `quota_exceeded` (50 open invitations); 404 `not_found` (no such
  group or account); 409 `conflict` (a member already); 422 `validation_failed` (the caller's own
  id); 429 `rate_limited` (20 invitations per 10 minutes).

#### `POST /v1/groups/{group}/invites/accept`

Accept an invitation: join the group.

- Auth: yes.
- 200: `GroupInfo`.
- Errors: 403 `quota_exceeded` / `forbidden` (as for join); 404 `not_found` (no such group, or no
  invitation).

#### `POST /v1/groups/{group}/invites/decline`

- Auth: yes.
- 200: `{}` (also when there was no invitation).

#### `DELETE /v1/groups/{group}/invites/{user}`

Withdraw an invitation (the owner or an admin).

- Auth: yes.
- 200: `{}` (also when there was none).
- Errors: 403 `not_a_member` / `forbidden`; 404 `not_found`.

#### `DELETE /v1/groups/{group}/members/{user}`

Remove a member: the owner removes anyone, admins remove members.

- Auth: yes.
- 200: `{}` (also when the player was no member).
- Errors: 403 `not_a_member` / `forbidden`; 404 `not_found`; 422 `validation_failed` (the caller's
  own id: leave instead).

#### `PUT /v1/groups/{group}/members/{user}/role`

Give a member the role `admin` or `member` (the owner).

- Auth: yes.
- Body: `{"role":"admin"|"member"}`.
- 200: `{}`.
- Errors: 403 `not_a_member` / `forbidden`; 404 `not_found` (no such group or member); 422
  `validation_failed` (another role, or the owner's own id).

#### `POST /v1/groups/{group}/transfer`

Hand the group to a member (the owner); the old owner becomes an admin.

- Auth: yes.
- Body: `{"user":int}`.
- 200: `{}`.
- Errors: 403 `not_a_member` / `forbidden`; 404 `not_found` (no such group or member); 422
  `validation_failed` (the owner's own id).

### Lobbies

Lobbies bring players together before a match; the server only coordinates (the game's own
connection between the players stays the game's). A player creates a lobby and **hosts** it; other
players join; every member has a **ready** flag; the host sets the lobby's **metadata** (text keys
and values the game chooses, e.g. `"mode":"ranked"`), its size, its visibility and its state, kicks
members and hands the lobby to another member. A leaving host passes the lobby to the member who
joined first; the last member's leaving removes it. A player is in one lobby at a time (the
server's default). With `leave_on_disconnect` (on by default), a player whose last WebSocket
connection closes leaves its lobbies after 30 seconds unless it reconnects.

- **Visibility:** `public` lobbies are listed by the search and joined by id or code; `private`
  ones only with the join code; `friends` ones (servers with the `friends` module) with the code,
  or by id by a friend of the host, and listed in the friends' search. With the `friends` module, a
  player the host blocked cannot join.
- **State:** `open` (players join), `in_game` (no one joins; the members stay; back to `open`
  resets every ready flag), `closed` (the lobby is removed; its members get a last
  `lobby.changed` with state `closed`).
- **Join codes:** every lobby has one, shown to its members: 8 characters of
  `23456789ABCDEFGHJKLMNPQRSTUVWXYZ` (no `0`, `O`, `1`, `I`), unique among the server's lobbies,
  valid as long as the lobby exists; the host replaces it (the old one stops working). Case,
  spaces and dashes do not matter when typed (`k7m2-q9xd`). A code is also a number below 2^40
  (`code_number`): each character's place in the alphabet above is 5 bits, the first character the
  highest (`22222223` = 1, `K7M2Q9XD` = 590122524587); it fits a 64-bit integer wherever a
  platform carries a number instead of a text.
- **Metadata:** keys of 1–64 bytes of ASCII letters, digits, `_ . : -`; values of at most 256
  characters (no control or invisible characters); at most 32 keys and 4 KiB per lobby.
- **Pushes** (WebSocket, to every member): `lobby.member` and `lobby.changed`
  ([Lobby kinds](#lobby-kinds)). With the `chat` module, every lobby has a chat room for its
  members (`chat_room`), joined and left with the lobby and deleted with it.
- **Host actions** (change, new code, hand over, remove a member): a player who is not a member of
  the lobby gets 404 `not_found`, a member who is not the host 403 `forbidden`.

```text
LobbyInfo   {"id":int, "visibility":"public"|"private"|"friends", "state":"open"|"in_game"|"closed",
             "host"?:int, "max_players":int, "members":int, "metadata"?:{string:string}, "created_at":ms,
             "code"?:string, "code_number"?:int, "chat_room"?:int, "players"?:[LobbyMember]}
LobbyMember {"user":int, "name"?:string, "ready":bool, "joined_at":ms}
```

`code`, `code_number` and `chat_room` are shown to members only; `players` (in the order they
joined) is filled in the answers about one lobby and absent in search results.

#### `POST /v1/lobbies`

Create a lobby; the caller hosts it.

- Auth: yes.
- Body: `{"max_players":int, "visibility"?:"public"|"private"|"friends", "metadata"?:{string:string}}`
  (`max_players`: 1 to the server's limit, 64 by default; `public` by default).
- 200: `LobbyInfo` (with its code and members).
- Errors: 403 `quota_exceeded` (the caller is in a lobby already), 403 `forbidden` (a server rule);
  422 `validation_failed` (the size, the visibility, `friends` without the friends module, the
  metadata); 429 `rate_limited`.

#### `GET /v1/lobbies/mine`

- Auth: yes.
- 200: `{"lobbies":[LobbyInfo]}`: the caller's lobbies with their members, oldest membership first.

#### `POST /v1/lobbies/search`

Open lobbies, newest first. Every filter must match exactly; full lobbies are left out unless
`include_full`.

- Auth: yes.
- Body: `{"filters"?:[{"key":string, "value":string}], "friends"?:bool, "include_full"?:bool,
  "cursor"?:string, "limit"?:int}` (at most 8 filters; `friends: true`: the lobbies the caller's
  friends host, public and friends-only, instead of every public lobby).
- 200: `Page<LobbyInfo>` (without codes and member lists).
- Errors: 400 `bad_request` (an invalid cursor); 422 `validation_failed` (the filters, `friends`
  without the friends module).

#### `POST /v1/lobbies/join`

Join with a join code (any visibility).

- Auth: yes.
- Body: `{"code":string}`.
- 200: `LobbyInfo` (also when the caller was a member already).
- Errors: 403 `quota_exceeded` (the lobby is full, or the caller is in a lobby already), 403
  `forbidden` (the host blocked the caller, a server rule); 404 `not_found` (no lobby has this
  code); 409 `conflict` (the lobby is not open); 422 `validation_failed` (not a code); 429
  `rate_limited` (too many join attempts, or 20 codes per hour that matched no lobby: then every
  code answers 429 until the hour frees one).

#### `GET /v1/lobbies/{lobby}`

- Auth: yes.
- 200: `LobbyInfo` with its members.
- Errors: 404 `not_found` (no such lobby, or one the caller may not see: a private one it is not
  in, a friends-only one of a stranger).

#### `PATCH /v1/lobbies/{lobby}`

Change a lobby (the host). Absent fields stay; in `metadata` a text sets the key and `null` removes
it (other keys stay).

- Auth: yes.
- Body: `{"visibility"?:string, "max_players"?:int, "state"?:"open"|"in_game"|"closed",
  "metadata"?:{string:string|null}}`.
- 200: `LobbyInfo` as it is now (after `closed`: as it was, with state `closed`).
- Errors: 403 `forbidden` (not the host, a server rule); 404 `not_found` (also for a caller who is
  not a member); 422 `validation_failed` (a size below the member count or above the server's
  limit, the metadata limits, more than 64 metadata changes); 429 `rate_limited` (lobby changes:
  30 per 60 s).

#### `POST /v1/lobbies/{lobby}/join`

Join by id: a public lobby, or a friends-only one of a friend.

- Auth: yes.
- 200: `LobbyInfo` (also when the caller was a member already).
- Errors: as `POST /v1/lobbies/join`; 404 `not_found` also for lobbies joined with their code only.

#### `POST /v1/lobbies/{lobby}/leave`

- Auth: yes.
- 200: `{}` (also when the caller was no member).
- Errors: 404 `not_found` (no such lobby).

#### `PUT /v1/lobbies/{lobby}/ready`

- Auth: yes.
- Body: `{"ready":bool}`.
- 200: `{}`.
- Errors: 403 `not_a_member`; 404 `not_found`.

#### `POST /v1/lobbies/{lobby}/code`

A new join code (the host); the old one stops working.

- Auth: yes.
- 200: `LobbyInfo` with the new code.
- Errors: 403 `forbidden` (not the host); 404 `not_found` (also for a caller who is not a member);
  429 `rate_limited` (lobby changes: 30 per 60 s).

#### `POST /v1/lobbies/{lobby}/host`

Hand the lobby to another member (the host).

- Auth: yes.
- Body: `{"user":int}`.
- 200: `{}`.
- Errors: 403 `forbidden` (not the host); 404 `not_found` (no such lobby or member; also for a
  caller who is not a member).

#### `DELETE /v1/lobbies/{lobby}/members/{user}`

Remove a member (the host).

- Auth: yes.
- 200: `{}` (also when the player was no member).
- Errors: 403 `forbidden` (not the host); 404 `not_found` (also for a caller who is not a member);
  422 `validation_failed` (the caller's own id: leave instead).

### Matchmaking

Queues where players wait to be matched; the game's rules (on the server) decide who plays with
whom, the server tells the players. A player has one **ticket** at a time, in one of the server's
queues, with `attributes` the game's rules read (a rating, a region). A round runs every second:
the server's default rule matches the oldest tickets in groups of the queue's `players`; a game's
server may use its own rules and attach `data` to a match (a lobby, a server address, the teams).
Every matched player gets `match.found`; a ticket nobody matched runs out after its queue's timeout
(120 s by default) with `match.expired`. A client without a WebSocket reads its ticket instead: a
matched ticket answers its match for 60 seconds. A player whose last WebSocket connection closes
leaves its queue. Tickets live in the server's memory: a restart empties the queues.

```text
MatchTicket {"id":int, "queue":string, "status":"waiting"|"matched", "created_at":ms, "expires_at":ms, "found"?:MatchFound}
MatchFound  {"ticket":int, "queue":string, "players":[int], "data"?:JSON}
```

#### `GET /v1/matchmaking/queues`

- Auth: yes.
- 200: `{"queues":[{"key":string, "players":int, "waiting":int}]}` (`players`: per match of the
  default rule; `waiting`: tickets waiting now).

#### `POST /v1/matchmaking/ticket`

- Auth: yes.
- Body: `{"queue":string, "attributes"?:JSON}` (attributes: at most 1 KiB of JSON).
- 200: `MatchTicket` (`waiting`; a matched ticket of the caller is replaced).
- Errors: 403 `forbidden` (a server rule); 404 `not_found` (no such queue); 409 `conflict` (a
  ticket of the caller waits already); 422 `validation_failed`; 429 `rate_limited`; 503
  `unavailable` (the server holds too many tickets).

#### `GET /v1/matchmaking/ticket`

- Auth: yes.
- 200: `MatchTicket`.
- Errors: 404 `not_found` (no ticket: none made, cancelled, run out, or matched more than 60 s ago).

#### `DELETE /v1/matchmaking/ticket`

- Auth: yes.
- 200: `{}` (also when there was none).

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
| PUT | `/v1/admin/users/{user}/storage/{collection}/{key}` | `{"value":JSON, "if_version"?:int, "write"?:"owner"\|"server", "visibility"?:"private"\|"public"\|"friends"}` (`write` / `visibility` absent: unchanged; new objects `owner`, `private`) | `ObjectAck` | 404 (no such account); 409 `version_conflict`; 422 |
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
| 426 | a plain GET without an upgrade, or an upgrade without a valid `Sec-WebSocket-Key` | |
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
automatically. Clients may send their own pings (pings and pongs count against the frame rate).

### Close codes

| Code | Meaning | Reconnect? |
|---|---|---|
| 1000 | normal closure | yes |
| 1001 | the server is shutting down or redeploying | yes (soon) |
| 1006 | (set by the client library) the connection dropped without a close frame: network loss, or a refused handshake in a browser | yes, with backoff |
| 1008 | no `auth` in time, or still flooding after the rate limit refused requests | yes, with backoff (fix the cause) |
| 1009 | a message over 1 MiB (16 KiB before authentication) | yes |
| 1011 | an unexpected server error | yes, with backoff |
| 1013 | overloaded, or this socket could not keep up with its pushes | yes, later; then resync |
| 4001 | authentication refused or revoked (logout, password change, admin, refresh-token reuse, a login over the account's session limit) | **no** (see below) |
| 4003 | the account is banned | **no** |
| 4009 | replaced: the user opened more connections than allowed (5); the oldest goes first | **no** |
| 4010 | the client's protocol version is not supported | **no** |

**Never reconnect automatically after 4000–4099.** The server uses that range only for "do not come
back with these credentials". For 4001: refresh the tokens once; if the refresh succeeds, connect
once more with the new access token; if the refresh is refused (or the new connection gets 4001
again), show the login screen; a refresh that fails for a passing reason (network, 5xx) is tried
again with the backoff below. For 4009: tell the player another window or device took over;
reconnect only on a user action.

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
| `chat.edit` | request | `{"room":int, "message":int, "text":string}` → `ChatMessage` (with `edited_at`) |
| `chat.edited` | push | `{"id":int, "room":int, "text":string, "edited_at":ms, "edited_by"?:int}` |
| `chat.mark_read` | request | `{"room":int, "message":int}` → `{}` |
| `chat.read` | push | `{"room":int, "user":int, "message":int, "read_at":ms}` |
| `chat.receipts` | request | `{"room":int}` → `{"room":int, "receipts":[{"room":int, "user":int, "message":int, "read_at":ms}]}` |
| `chat.unread` | request | `{"rooms":[int]}` → `{"rooms":[{"room":int, "unread":int, "last_read"?:int}]}` |
| `chat.set_typing` | request | `{"room":int, "typing":bool}` → `{}` |
| `chat.typing` | push | `{"room":int, "user":int, "typing":bool, "expires_in_ms":int}` |
| `chat.room` | push | `{"room":int, "change":string, "user"?:int, "by"?:int, "role"?:string, "info"?:RoomInfo}` |

A game's own server may register more kinds; its `/v1/asyncapi.json` lists them all. An unknown
request kind is answered `unknown_type`; ignore push kinds you do not know.

<a id="chat-kinds"></a>
#### Chat kinds in detail

- **`chat.join`** — by room id (`{"room":12}`) or a public room's key (`{"room":"world"}`). Joining
  a room already joined is not an error. Membership lasts as long as the connection. Group rooms
  need membership; a player room makes the caller a member first (a public room, or accepting an
  invitation; that membership is stored); DM rooms need no join (joining answers their info).
  Errors: `not_found`, `not_a_member`, `forbidden` (banned from a player room), `room_full` (the
  room's cap counts connections), `quota_exceeded` (16 rooms on this connection, or a full player
  room), `validation_failed` (an invalid key).
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
- **`chat.edit`** / **`chat.edited`** — as `PATCH /v1/chat/rooms/{room}/messages/{message}`. Replace
  the message's text on `chat.edited` and mark it edited.
- **`chat.mark_read`** / **`chat.read`** — as `PUT /v1/chat/rooms/{room}/read`. `chat.read` reaches
  the room's joined connections (a DM: every connection of both users), the reader's own too; show
  "seen" up to `message`.
- **`chat.receipts`**, **`chat.unread`** — as the HTTP routes.
- **`chat.set_typing`** / **`chat.typing`** — nothing is stored. The room must be joined on this
  connection (a DM room: no join). Send `typing: true` about every 3 s while the player types and
  `typing: false` when they stop; the server pushes at most one `chat.typing` per user, room and 3 s,
  a stop only after a pushed start, and none in rooms with more than 50 online users. Show the
  indicator for `expires_in_ms` (6000) unless a new push or that user's `chat.message` comes first.
  Your own connections get it too. Errors: `not_a_member`, or a server rule's refusal.
- **`chat.room`** — a player room changed, to its members and invited players: `updated` (`info`:
  the room), `deleted`, `invited` (also to the invited player), `joined`, `left`, `kicked` (also to
  the kicked player), `role` (`role`: the new role), `owner` (`user`: the new owner). `user` is the
  player the change is about, `by` who made it.

<a id="notification-kinds"></a>
#### Notification kinds (servers with the `notifications` module)

| Kind | Direction | `data` → answer `data` |
|---|---|---|
| `notify.new` | push | `Notification` (see [Notifications](#notifications)), to every open connection of its player |
| `notify.list` | request | `{"cursor"?:string, "limit"?:int, "unread_only"?:bool}` → `Page<Notification>` (newest first) |
| `notify.count` | request | `{}` → `{"unread":int, "total":int}` |
| `notify.mark` | request | `{"ids":[int], "read":bool}` or `{"all":true, "read":bool}` → `{"changed":int, "unread":int}` |
| `notify.delete` | request | `{"id":int}` → `{}` |

Each request answers exactly like its HTTP route. A client that was offline reads what it missed
with `notify.list` (`unread_only: true`); while connected, each new notification arrives as
`notify.new`.

<a id="friend-kinds"></a>
#### Friend kinds (servers with the `friends` module)

| Kind | Direction | `data` |
|---|---|---|
| `friends.presence` | push | `{"user":int, "online":bool, "last_seen"?:ms}`, to every open connection of each of that player's friends |

It is sent when a player's first connection on a server instance opens (`online: true`), when its
last one there closes (`online: false`, with `last_seen`), and when a heartbeat brings an offline
player online. A player whose heartbeats stop goes offline without a push: `GET /v1/friends`
always answers the current state.

<a id="lobby-kinds"></a>
#### Lobby kinds (servers with the `lobbies` module)

| Kind | Direction | `data` |
|---|---|---|
| `lobby.member` | push | `{"lobby":int, "change":"joined"\|"left"\|"kicked"\|"ready", "member":LobbyMember}`, to every member (a leaving or kicked member gets it too) |
| `lobby.changed` | push | `{"changes":["host"\|"metadata"\|"settings"\|"state"\|"code"], "lobby":LobbyInfo}` (without `players`), to every member |

`settings` is the visibility or the size. A lobby that closes sends a last `lobby.changed` with
state `closed`. `GET /v1/lobbies/{lobby}` always answers the current state.

<a id="match-kinds"></a>
#### Matchmaking kinds (servers with the `matchmaking` module)

| Kind | Direction | `data` |
|---|---|---|
| `match.found` | push | `MatchFound` (the receiving player's `ticket`), to every open connection of each matched player |
| `match.expired` | push | `{"ticket":int, "queue":string}`: the ticket ran out unmatched |

---

## 8. Storage flows

Objects live at `(owner, collection, key)` and hold one JSON value (usually an object): save slots,
settings, inventories. Players write only their own objects; other players read an object only when
its `visibility` is `public` (or `friends` and they are the owner's friends), through
`/v1/users/{owner}/storage/...` (a public profile, a shared level).

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

**Binary data** (images, replays): a server with the `files` module stores files
(`POST /v1/files`, up to 16 MiB each by default). Without it, encode bytes as a string yourself (for
example base64, +33 %, which counts against the 256 KiB value limit).

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
room gets `chat.deleted`. Editing: `chat.edit` (or `PATCH` on the same path) by the sender within
the edit window, or a moderator; the room gets `chat.edited`.

**Unread counts and "seen":**

1. On opening a room, take the newest message's `id` and send `chat.mark_read`
   `{"room":12,"message":<id>}` (once per batch of messages, not for every message).
2. For badges, `chat.unread` `{"rooms":[12,13,…]}` with the rooms on screen; after a reconnect ask
   again.
3. In DM, group and player rooms show the other members' markers from `chat.read` pushes;
   `chat.receipts` `{"room":12}` gives the current ones (for example after a reconnect).

**Typing:** while the player types, `chat.set_typing` `{"room":12,"typing":true}` about every 3 s;
`{"typing":false}` when they stop or clear the input. Show others' `chat.typing` until
`expires_in_ms` passes or their message arrives.

**A player's own room:**

1. `POST /v1/chat/rooms` `{"name":"Night Owls","visibility":"private"}` → `RoomInfo` (`role: "owner"`).
2. `POST /v1/chat/rooms/{room}/invites` `{"user":77}`: the player gets `chat.room` `invited` (and a
   `chat.invite` notification with the `notifications` module); `GET /v1/chat/rooms/mine` lists it
   with `role: "invited"`.
3. The invited player joins with `chat.join` `{"room":<id>}` (or `POST /v1/chat/rooms/{room}/join`)
   and chats as in any room; `POST …/leave` declines or leaves.
4. The owner: `PUT …/members/{user}/role` `{"role":"moderator"}`, `DELETE …/members/{user}` (a kick:
   banned until invited again), `PATCH …` to rename or open it, `POST …/owner` to hand it on,
   `DELETE …` to delete it.

**Do not resend `chat.send` automatically after a reconnect:** it could post a message twice. If the
answer was lost, look for your `nonce` in the history first.

---

## 10. Examples: curl

```sh
API=https://your-server.example

# Server info (no auth)
curl -sS "$API/v1/info"
# {"protocol":1,"min_protocol":1,"modules":["auth","chat","files","friends","groups","leaderboards","lobbies","matchmaking","notifications","oauth","storage"]}

# Register (logs in at once)
curl -sS "$API/v1/auth/register" -H 'content-type: application/json' \
  -d '{"email":"player@example.com","password":"correct horse battery","display_name":"Player One"}'
# {"account":{"id":42,...},"tokens":{"token_type":"Bearer","access_token":"nbsa_...","access_expires_at":...,"refresh_token":"nbsr_...","refresh_expires_at":...}}

# Log in
curl -sS "$API/v1/auth/login" -H 'content-type: application/json' \
  -d '{"email":"player@example.com","password":"correct horse battery"}'

# Log in with an OpenID Connect provider's ID token (the server's `oauth` module, provider `google`)
curl -sS "$API/v1/auth/oauth/google" -H 'content-type: application/json' \
  -d '{"id_token":"eyJhbGciOiJSUzI1NiIs...","nonce":"the-nonce-of-the-sign-in"}'

ACCESS=nbsa_...    # tokens.access_token
REFRESH=nbsr_...   # tokens.refresh_token

# Who am I
curl -sS "$API/v1/account" -H "authorization: Bearer $ACCESS"

# Refresh (single use: store the new pair)
curl -sS "$API/v1/auth/refresh" -H 'content-type: application/json' \
  -d "{\"refresh_token\":\"$REFRESH\"}"

# Upload a file (multipart: the optional meta part first, then the file), download it
curl -sS "$API/v1/files" -H "authorization: Bearer $ACCESS" \
  -F 'meta={"visibility":"public"};type=application/json' -F 'file=@screenshot.png;type=image/png'
# {"id":12,"owner":42,"name":"screenshot.png","content_type":"image/png","size":48213,"sha256":"…","visibility":"public",...}
curl -sS "$API/v1/files/12/content" -H "authorization: Bearer $ACCESS" -o screenshot.png

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
# Chat extras: edit a message, the read marker, unread counts, a player's own room
curl -sS -X PATCH "$API/v1/chat/rooms/12/messages/981" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"text":"hello again"}'
curl -sS -X PUT "$API/v1/chat/rooms/12/read" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"message":981}'
curl -sS "$API/v1/chat/unread" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"rooms":[12,13]}'
curl -sS "$API/v1/chat/rooms" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"name":"Night Owls","visibility":"public"}'
curl -sS "$API/v1/chat/rooms/40/invites" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"user":77}'

# Leaderboards (a server with the leaderboards module and a board `highscore`)
curl -sS "$API/v1/leaderboards/highscore/scores" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"score":1200}'
# {"board":"highscore","score":1200,"submitted":1200,"changed":true,"rank":3}
curl -sS "$API/v1/leaderboards/highscore?limit=10" -H "authorization: Bearer $ACCESS"
curl -sS "$API/v1/leaderboards/highscore/around?above=2&below=2" -H "authorization: Bearer $ACCESS"

# Friends: my friend code, a request by code, accept one, my friends with their online state
curl -sS "$API/v1/friends/code" -H "authorization: Bearer $ACCESS"
curl -sS "$API/v1/friends/requests" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"code":"K7M2Q9XD"}'
curl -sS -X POST "$API/v1/friends/requests/77/accept" -H "authorization: Bearer $ACCESS"
curl -sS "$API/v1/friends" -H "authorization: Bearer $ACCESS"
# Which of my Steam friends play here (a Steam-linked account), and hiding myself from that lookup
curl -sS "$API/v1/friends/steam" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"steam_ids":["76561201960265729"]}'
curl -sS -X PUT "$API/v1/friends/settings" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"steam_findable":false}'

# Groups: create one, find groups by name, invite a player, accept an invitation (as that player)
curl -sS "$API/v1/groups" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"name":"Night Owls","open":false}'
curl -sS "$API/v1/groups?query=night" -H "authorization: Bearer $ACCESS"
curl -sS "$API/v1/groups/5/invites" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"user":77}'
curl -sS -X POST "$API/v1/groups/5/invites/accept" -H "authorization: Bearer $ACCESS"

# Lobbies: create one, join another with its code, set ready, search public lobbies by metadata
curl -sS "$API/v1/lobbies" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"max_players":4,"metadata":{"mode":"ranked"}}'
curl -sS "$API/v1/lobbies/join" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"code":"K7M2-Q9XD"}'
curl -sS -X PUT "$API/v1/lobbies/7/ready" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"ready":true}'
curl -sS "$API/v1/lobbies/search" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"filters":[{"key":"mode","value":"ranked"}]}'

# Matchmaking: queue for a duel, then read the ticket (waiting, or matched with its players)
curl -sS "$API/v1/matchmaking/ticket" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"queue":"duel","attributes":{"rating":1520}}'
curl -sS "$API/v1/matchmaking/ticket" -H "authorization: Bearer $ACCESS"

# Notifications (a server with the notifications module): unread ones, mark all read
curl -sS "$API/v1/notifications?unread_only=true" -H "authorization: Bearer $ACCESS"
curl -sS "$API/v1/notifications/mark" -H "authorization: Bearer $ACCESS" -H 'content-type: application/json' -d '{"all":true,"read":true}'

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
        } catch (error) {
          if (error.status !== 401 && error.status !== 403) {
            // A network error or a 5xx: the session may be fine, try again with the backoff.
            this.retriedAfter4001 = false;
            this.reconnectLater();
            return;
          }
          // The refresh was refused: the session is over, fall through.
        }
      }
      this.emit("gone", { code: event.code, error: this.lastAuthError }); // show login / ban / "opened elsewhere"
      return;
    }

    this.reconnectLater();
  }

  // Exponential backoff with jitter, 1 s .. 30 s.
  reconnectLater() {
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
      most; a refresh that fails for a passing reason is retried with the backoff). Reconnect with exponential backoff and jitter after everything else (1000, 1001, 1006,
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
