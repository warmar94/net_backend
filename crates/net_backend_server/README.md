# net_backend_server

<p>
  <a href="https://crates.io/crates/net_backend_server"><img alt="crates.io" src="https://img.shields.io/crates/v/net_backend_server.svg"></a>
  <a href="https://docs.rs/net_backend_server"><img alt="docs.rs" src="https://img.shields.io/docsrs/net_backend_server"></a>
  <a href="https://net-backend.com"><img alt="Website: net-backend.com" src="https://img.shields.io/badge/website-net--backend.com-informational"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

A Rust framework for building **game backend servers**: async (tokio + axum) and modular. It speaks
plain HTTP + WebSocket + JSON, so any client can use it. Rust clients share the message types through
[`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol); the ready-made clients are
[`net_backend_client`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_client) for Rust and
[`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) for Bevy games.

It is a **library you build your own server with**, not a finished server application. It gives you
solid building blocks with sensible defaults and leaves the game rules to you.

```text
Your client (Rust)                          Your server (Rust binary)
┌─────────────────────────────┐             ┌───────────────────────────────┐
│ your game / app / tool      │             │ your rules / hooks / logic    │
│        │                    │             │        │                      │
│ HTTP + WebSocket client ────┼─ HTTP / WS ▶│ net_backend_server            │
│  · Rust: net_backend_client │             │  core: auth, sessions, WS hub │
│  · Bevy: bevy_net_backend   │             │  modules: storage, chat       │
│  · Other: any HTTP/WS lib   │             └───────────────────────────────┘
└─────────────────────────────┘                                ▲
           ▲                                                   │
           └──── net_backend_protocol (shared message types) ──┘

Not Rust? Use the HTTP / WebSocket API directly (see API.md).
```

## Clients

The server does not care which client connects; the JSON on the wire is the contract.

| Your client is… | Use |
|---|---|
| a **Rust** app (tool, bot, CLI, other engine) | [`net_backend_client`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_client) for the connection (HTTP, WebSocket, SSH / SFTP) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) for the message types. |
| a **Bevy** game | [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) for the connection (HTTP, WebSocket, SSH / SFTP) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) for the message types. |
| **other** | [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) + any HTTP / WebSocket library (for example reqwest, ureq, tokio-tungstenite). |
| **not Rust** (C#, GDScript, JavaScript, …) | The API directly: [API.md](https://github.com/warmar94/net_backend/blob/main/API.md) is the complete HTTP + WebSocket reference with examples; the OpenAPI document at `/v1/openapi.json` describes every HTTP route and can generate typed clients; the AsyncAPI document at `/v1/asyncapi.json` and the [WebSocket](#websocket) section describe every WebSocket frame. |

## Contents

- [Clients](#clients)
- [What it includes](#what-it-includes)
- [Features](#features)
- [Install](#install)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Modules and hooks](#modules-and-hooks)
- [Accounts and authentication](#accounts-and-authentication)
- [WebSocket](#websocket)
- [Typed routes (HttpCall)](#typed-routes-httpcall)
- [Storage](#storage)
- [Chat](#chat)
- [Databases](#databases)
- [Migrations](#migrations)
- [The command line](#the-command-line)
- [HTTP basics](#http-basics)
- [Errors](#errors)
- [OpenAPI, health and metrics](#openapi-health-and-metrics)
- [Graceful shutdown](#graceful-shutdown)
- [Deployment](#deployment)
- [How it works](#how-it-works)
- [Compatibility](#compatibility)
- [Testing](#testing)
- [FAQ](#faq)
- [License](#license)
- [Contributing](#contributing)

## What it includes

| Part | What |
|---|---|
| App builder | `NetBackendServer::new(config)` with `.module(..)`, `.route(..)` / `.routes(..)` / `.nest(..)` / `.merge(..)`, `.state(..)`, hooks, `.run()` / `.serve(listener)`. |
| Modules | The `Module` trait: name, dependencies (`depends_on`), routes, migrations per dialect, hooks, OpenAPI parts, `start` / `shutdown`. Deterministic order (registration order). |
| Hooks | Typed `before` hooks (pass on, modify or reject), `in_tx` hooks (inside a module's transaction) and `after` hooks per event type, start and shutdown hooks; each call has a time limit and panics are contained. |
| Configuration | A TOML file plus `NBS__SECTION__KEY` environment overrides and secrets from files; typed, validated (every problem reported at once, unknown keys and module sections refused), secrets never in `Debug`. |
| Databases | MySQL (default), PostgreSQL, SQLite through one `Db` handle; statements built once with sea-query for all three; portable column types. |
| Migrations | Plain SQL per dialect, ordered, tracked and checksummed, namespaced per module, safe against concurrent runs on every backend, precise recovery messages; modules' migrations can be **published** into the app, which then owns them. |
| HTTP | Routes under `/v1`, `GET /v1/info`, `/healthz`, `/readyz`, body limits (64 KiB default, 32 MiB hard cap), a header-read timeout, request ids, request tracing, request timeouts, panic safety, optional CORS, the protocol version header. |
| Errors | Every 4xx / 5xx is the protocol's error body; internal errors are logged with the request id and never reach a client. |
| OpenAPI / AsyncAPI | The HTTP document at `/v1/openapi.json` (utoipa), an optional browser UI; the WebSocket document at `/v1/asyncapi.json` (AsyncAPI 3.0, generated from the registered kinds). |
| Operations | Optional Prometheus metrics on their own loopback listener, a command line (`serve`, `migrate`, `migrations publish`, `config check`, `openapi export`, `asyncapi export`), graceful shutdown with an enforced deadline. |
| Accounts | The `Auth` module: email + password (argon2id) and Steam logins, opaque access and rotating refresh tokens stored as hashes, sessions and revocation, email verification and password reset (log or SMTP mailer), roles, an audit log, `/v1/admin` routes, rate limits and a failed-login lockout, hooks, `user:*` commands. |
| WebSocket hub | `/v1/ws` with the protocol's envelope: auth at the handshake (Bearer / `?token=`) or by first message, request handlers by kind (typed or JSON) for the game and modules, pushes to a socket / user / room / everyone, rooms with caps, close on revocation (4001) and ban (4003), connection caps, per-socket rate limits and bounded outboxes, heartbeats, hooks, 1001 on shutdown, a pub/sub seam for several instances. |
| Typed routes | Every protocol route is mounted from its `HttpCall` (method, path, payload, answer); the same for your own routes (`.call::<C, ..>(handler)`, `call_route!`). |
| Storage | The `Storage` module (feature `storage`): per-user JSON objects with versions, conditional writes (`if_version`, `If-Match`, `ETag`), batches, a server write lock, quotas, hooks (incl. in the transaction), audited admin access. |
| Chat | The `Chat` module (feature `chat`): public, group and DM rooms, the answer before the echo, history pages, caps and rates, moderation hooks and deletion, presence with a cap and a rate, retention. |
| Seams | Authenticators (`Authenticator`, the `AuthContext` / `RequireRole` extractors), rate limiters (with an in-memory `MemoryRateLimiter`), trusted-proxy client addresses, app commands, the WebSocket `Broadcaster`. |

## Features

| Feature | Default | What |
|---|---|---|
| `mysql` | yes | MySQL 8 and MariaDB 11.4 (the CI database job runs MySQL 8.4 and MariaDB 11.4) — sqlx, TLS with rustls + ring. |
| `postgres` | no | PostgreSQL 16 (sqlx, TLS with rustls + ring). |
| `sqlite` | no | SQLite, compiled in (no system library needed). |
| `steam` | no | The built-in Steam ticket check (`SteamWebApiVerifier`: hyper + rustls with ring). |
| `smtp` | no | The SMTP mailer (`SmtpMailer`: lettre with rustls + ring). |
| `storage` | no | The [storage](#storage) module (no extra dependency). |
| `chat` | no | The [chat](#chat) module (no extra dependency). |

The backends are additive: any combination compiles, and the server uses the one its
`database.url` names. At least one is needed to run a server; without any, starting fails with a
clear message. TLS is rustls with ring throughout. Modules are features and off by default: a server compiles
only what it registers (`features = ["mysql", "storage", "chat"]`).

## Install

Pick ONE of the first two lines (the database), then add tokio and serde:

```text
cargo add net_backend_server                                                  # MySQL (the default)
cargo add net_backend_server --no-default-features --features sqlite          # or SQLite (postgres: --features postgres)
cargo add tokio --features rt-multi-thread,macros
cargo add serde --features derive                                             # for your own request types
```

The same in `Cargo.toml`:

```toml
[dependencies]
net_backend_server = { version = "0.1.0" }                                   # MySQL
# net_backend_server = { version = "0.1.0", default-features = false, features = ["sqlite"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
serde = { version = "1", features = ["derive"] }
```

The framework re-exports `axum`, `sea_query`, `sqlx`, `utoipa`, `utoipa_axum` and the protocol
crate (`net_backend_server::protocol`); prefer reaching them through these re-exports. Some macros
refer to their crate by name, so add the crate itself when you use them: `utoipa = "6"` +
`utoipa-axum = "0.3"` for documented routes (`#[utoipa::path]`, `routes!`), `sqlx = { version =
"0.9", default-features = false, features = ["derive"] }` for `#[derive(sqlx::FromRow)]`. The
framework asks for these with caret versions, so Cargo unifies them with yours. Because they are
part of its API, a breaking (minor) release of any of them means a minor release of this crate.

## Quick start

```rust,no_run
use net_backend_server::axum::{routing::post, Json};
use net_backend_server::{ApiJson, AppError, Config, NetBackendServer};
use serde::Deserialize;

#[derive(Deserialize)]
struct Craft {
    item: String,
}

async fn craft(ApiJson(body): ApiJson<Craft>) -> Result<Json<String>, AppError> {
    if body.item.is_empty() {
        return Err(AppError::bad_request("item is empty"));
    }
    Ok(Json(format!("crafted {}", body.item)))
}

#[tokio::main]
async fn main() -> Result<(), net_backend_server::Error> {
    let config = Config::load()?;                    // NBS_CONFIG / config.toml + NBS__* variables
    NetBackendServer::new(config)
        .route("/v1/game/craft", post(craft))      // plain axum handlers
        .run()                                     // the command line; `serve` by default
        .await
}
```

```text
NBS__DATABASE__URL=sqlite::memory: cargo run       # with the `sqlite` feature (Linux / macOS shells)
curl http://127.0.0.1:8080/v1/info                 # {"protocol":1,"min_protocol":1,"modules":[]}
```

On Windows (PowerShell):

```text
$env:NBS__DATABASE__URL = "sqlite::memory:"
cargo run
curl.exe http://127.0.0.1:8080/v1/info
```

A complete headless example: [`examples/minimal.rs`](examples/minimal.rs).

## Configuration

Sources, later ones win: defaults → a TOML file (the path in `NBS_CONFIG`, else `./config.toml`
if it exists) → environment variables `NBS__<SECTION>__<KEY>` → secrets read from files
(`database.url_file`). Unknown keys are errors, so a typo never silently falls back to a default;
a `[modules.<name>]` section without a registered module is refused too. `Config::validate`
reports every problem at once; secrets print as `<redacted>`, and module sections print only
their key names (they may hold secrets).

```toml
[server]
bind = "127.0.0.1:8080"        # behind a reverse proxy such as Caddy
shutdown_grace_secs = 20       # in-flight requests may finish this long after SIGTERM
hook_timeout_ms = 2000         # the time limit of one hook call
header_read_timeout_secs = 15  # a client must send its request headers within this (also idle keep-alive)
module_start_timeout_secs = 30
module_shutdown_timeout_secs = 10

[database]
url = "mysql://game:secret@127.0.0.1:3306/game"   # or url_file = "/run/credentials/game/db_url"
max_connections = 10
min_connections = 0
acquire_timeout_secs = 5
connect_lazy = false           # true: start even while the database is down (/readyz says 503)
migrate_on_start = false       # production runs `migrate` as a deploy step
migrations_dir = "migrations"
migrate_lock_timeout_secs = 60 # how long `migrate` waits for another process's migrations lock

[http]
body_limit_bytes = 65536       # every route without its own limit
request_timeout_secs = 30      # then 503 "unavailable"
trust_request_id = false       # keep a client's x-request-id (behind a proxy that sets it)
max_body_bytes = 33554432      # the hard cap for every body, raw streams and raised per-route limits included
trusted_proxies = []           # e.g. ["127.0.0.1"]: take the client address from X-Forwarded-For of these proxies

[cors]
allowed_origins = []           # off; ["https://example.com"] or ["*"]
max_age_secs = 600

[log]
level = "info"                 # a tracing filter; RUST_LOG wins
format = "pretty"              # or "json"

[metrics]
enabled = false                # Prometheus text at GET /metrics on its own listener
bind = "127.0.0.1:9100"        # loopback: only a monitoring agent on the same machine can scrape it

[openapi]
enabled = true                 # /v1/openapi.json
ui = false                     # /v1/docs; needs the two pinned script settings below
# ui_script_url = "https://cdn.jsdelivr.net/npm/@scalar/api-reference@<exact version>"
# ui_script_integrity = "sha384-…"
title = "Game backend API"
version = "1"

[ws]                           # the WebSocket hub at /v1/ws (see WebSocket)
enabled = true                 # false: /v1/ws answers 403
max_connections = 10000        # 503 above
max_connections_per_user = 5   # a newer socket closes the oldest with 4009 (its own session's first)
max_connections_per_ip = 100   # open sockets per client address (IPv6 by /64); 429 above
max_pending_connections = 1000 # sockets still waiting for `auth`; 503 above
roles_refresh_secs = 60        # role changes made by another process reach open sockets within this
handshakes_per_ip_per_minute = 60
auth_timeout_secs = 5          # unauthenticated sockets: send `auth` within this, else close 1008
ping_interval_secs = 20
idle_timeout_secs = 60         # nothing from the client for this long: dropped
request_timeout_secs = 10      # one handler call
write_timeout_secs = 10        # a frame not taken within this: the socket is dropped
outbox_frames = 256            # pushes waiting per socket; full: close 1013
frames_per_second = 20         # per socket, burst below; over it: `rate_limited`, flooding: close 1008
frame_burst = 40
max_message_bytes = 1048576    # both directions; bigger incoming: close 1009
read_buffer_bytes = 8192
max_rooms_per_connection = 16
max_room_members = 200         # unless a room is joined with its own cap
query_token = false            # accept ?token= on the handshake (proxies log URLs: off by default)

[modules.auth]                 # each module reads its own section (here: the accounts module)
app_name = "My Game"           # in mail subjects
verify_url = "https://example.com/verify?token={token}"
reset_url = "https://example.com/reset?token={token}"
# mailer = "smtp"              # feature `smtp`; smtp_host, smtp_username, smtp_password_file, mail_from
# steam_app_id = 480           # feature `steam`; steam_web_api_key_file, steam_identity
```

Every key has an environment form: `NBS__DATABASE__URL`, `NBS__SERVER__BIND`,
`NBS__CORS__ALLOWED_ORIGINS=https://a.example,https://b.example`,
`NBS__HTTP__TRUSTED_PROXIES=127.0.0.1,::1`, `NBS__MODULES__AUTH__APP_NAME=Space`. A module reads its
section with `config.module_config::<T>("auth")`; module config structs should use
`#[serde(deny_unknown_fields)]` and `SecretString` for secrets, with a `<name>_file` alternative
(`config::resolve_secret`). Error messages never quote configured values.

## Modules and hooks

A module bundles routes, migrations, hooks and OpenAPI parts under a name, like a service
provider. Only `name` is required; every other method has a default.

```rust,no_run
use net_backend_server::axum::routing::get;
use net_backend_server::hooks::{Decision, Event, Hooks};
use net_backend_server::utoipa_axum::router::OpenApiRouter;
use net_backend_server::{AppError, AppState, Dialect, Migration, Module, NetBackendServer, Config};

struct Scores;

/// An event other code can hook into.
struct BeforeSubmit {
    points: i64,
}
impl Event for BeforeSubmit {
    const NAME: &'static str = "scores.before_submit";
}

impl Module for Scores {
    fn name(&self) -> &'static str {
        "scores"
    }
    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        let id = match dialect {
            Dialect::MySql => "id BIGINT AUTO_INCREMENT PRIMARY KEY",
            Dialect::Postgres => "id BIGSERIAL PRIMARY KEY",
            _ => "id INTEGER PRIMARY KEY AUTOINCREMENT",
        };
        vec![Migration::new(202610010001, "create_scores", format!("CREATE TABLE scores ({id}, points BIGINT NOT NULL)"))]
    }
    fn routes(&self) -> OpenApiRouter<AppState> {
        OpenApiRouter::new().route("/v1/scores/top", get(|| async { "[]" }))
    }
    fn register_hooks(&self, hooks: &mut Hooks) {
        hooks.before::<BeforeSubmit, _, _>(|_ctx, event| async move {
            if event.points < 0 { Ok(Decision::Reject(AppError::bad_request("negative"))) } else { Ok(Decision::Continue(event)) }
        });
    }
}

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    NetBackendServer::new(config)
        .module(Scores)
        .before::<BeforeSubmit, _, _>(|_ctx, mut event| async move {
            event.points = event.points.min(1_000_000); // the game's own rule
            Ok(Decision::Continue(event))
        })
        .run()
        .await
}
```

- **Order is deterministic:** registration order. Migrations run in that order (then the app's
  own), routes merge in that order, `start` runs in that order and `shutdown` in reverse. A name
  is registered once; names match `[a-z][a-z0-9_]*` (at most 32 bytes); `app`, `core` and `nbs`
  are reserved.
- **Dependencies:** a module names the modules it needs (`depends_on`, e.g. `["auth"]` for tables
  with a foreign key to the accounts); each must be registered before it, or the build fails.
- **Hooks:** `before` hooks run in order — the game's hooks (registered on the builder) first,
  then each module's, modules in registration order — and may pass the event on, modify it or
  reject it (the first rejection answers the client); `in_tx` hooks (`.in_tx::<E, _>(..)`) run
  inside a module's transaction after its own write and may write with it or refuse (everything
  rolls back); `after` hooks run after the work and only log their errors. A module runs its hook
  points with `state.hooks().run_before(&ctx, event)` / `run_in_tx(&mut tx, &ctx, &event)`.
  Each call has a time limit (`server.hook_timeout_ms`; a `before` hook that runs out answers 503
  `hook_timeout`); a panic in a hook is caught (500 `internal`), the server keeps running.
- **Lifecycle:** `on_start` (an error aborts the start and shuts down what already started) and
  `on_shutdown`. A module's `start` and `shutdown` are bounded too
  (`server.module_start_timeout_secs` / `module_shutdown_timeout_secs`) and a panic in them is
  caught: a failed start stops the server from starting, a failed shutdown is logged and the next
  module still shuts down.
- **State:** `.state(MyGameState { .. })` registers a value; handlers take `Ext<MyGameState>`,
  hooks and modules use `state.get::<MyGameState>()`. Handlers can also take `State<AppState>`,
  `State<Db>` or `State<Arc<Config>>`.

## Accounts and authentication

The built-in `Auth` module (name `auth`) gives a server accounts, logins, tokens, sessions, email
verification, password reset, Steam logins, roles, an audit log and admin routes, exactly as
[`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) defines them. It is a
module like any other: register it, and keep it out to bring your own authentication.

```rust,no_run
use net_backend_server::auth::events::BeforeRegister;
use net_backend_server::auth::Auth;
use net_backend_server::hooks::Decision;
use net_backend_server::{AppError, AuthContext, Config, NetBackendServer};
use net_backend_server::axum::routing::get;

async fn whoami(ctx: AuthContext) -> String {
    format!("user {} (roles {:?})", ctx.user_id, ctx.roles)
}

#[tokio::main]
async fn main() -> Result<(), net_backend_server::Error> {
    NetBackendServer::new(Config::load()?)
        .module(Auth::new())                                  // settings: [modules.auth]
        .before::<BeforeRegister, _, _>(|_ctx, event| async move {
            if event.display_name.as_deref() == Some("admin") {
                return Ok(Decision::Reject(AppError::forbidden("this name is reserved")));
            }
            Ok(Decision::Continue(event))
        })
        .route("/v1/game/whoami", get(whoami))                // 401 without a valid token
        .run()
        .await
}
```

```text
my-game-server migrate                                        # creates the auth_* tables (29 migrations)
my-game-server user:create admin@example.com --admin          # prints a generated password once
```

### Accounts

| Route | What |
|---|---|
| `POST /v1/auth/register` | email + password (+ display name) → the account and a token pair; sends a verification mail |
| `POST /v1/auth/login` | email + password → account + tokens |
| `POST /v1/auth/steam` | a Steam Web API ticket → account + tokens (the first login creates the account; with the Bearer token of a recent login it links Steam to that account) |
| `POST /v1/auth/refresh` | a refresh token → a new pair (rotation) |
| `POST /v1/auth/logout` | this session, or every session (`"everywhere": true`), with the access token or the refresh token |
| `GET` / `PATCH /v1/account` | the caller's account; change the display name |
| `POST /v1/account/password` | change the password (knowing the current one); the other sessions are revoked |
| `DELETE /v1/account/identities/{provider}` | unlink a login provider (`steam`); needs a recent login; refused if it is the only way to log in |
| `POST /v1/auth/email/verify`, `/v1/auth/email/resend` | confirm the address with the mailed token; send it again |
| `POST /v1/auth/password/forgot`, `/v1/auth/password/reset` | ask for a reset mail (always the same answer); set a new password with its token (every session is revoked) |

Email addresses must be a plain `local@domain` (the protocol's `is_valid_email`: no display name,
no angle brackets, comments, quoted local parts or address literals), and mail goes to exactly that
address. They are unique **case-insensitively** on every database: the module stores the address
as entered (trimmed, Unicode NFC) and a lower-cased copy with a unique index (it does not rely on
the database's collation). Passwords are compared in NFC too, so the same text typed on different
keyboards matches. Requests are checked with the protocol's `validate()` rules (passwords 10
characters to 128 bytes; display names up to 32 characters without control or invisible
characters).

### Passwords and tokens

- **Passwords:** argon2id, by default 19 MiB memory, 2 iterations, 1 lane (OWASP's recommendation;
  `argon2_memory_kib`, `argon2_iterations`, `argon2_parallelism`), about 10 ms per hash on a
  desktop CPU. Hashing runs on tokio's blocking pool, at most `hash_concurrency` at once (default:
  the number of cores), so a flood of logins can neither stall the server nor use unbounded
  memory; a request that waits longer than `hash_queue_timeout_secs` gets 503. A failed login
  does the same work for a known and an unknown address (one query, one hash, a dummy hash
  without an account; the audit row of a known address is written in the background). Hashes with
  old parameters are upgraded at the next login (until then such an account's hash costs what its
  old parameters cost).
- **Tokens** are opaque: `nbsa_` / `nbsr_` plus 64 hex characters (32 random bytes from the
  operating system). The database stores only their SHA-256, so a database copy holds no usable
  token. Access tokens live 1 hour, refresh tokens 30 days (`access_token_ttl_secs`,
  `refresh_token_ttl_secs`).
- **Answers:** an expired access token is 401 `token_expired` (refresh and retry); an unknown,
  malformed or revoked one is 401 `unauthorized`; a banned account's token or refresh is 403
  `banned` (with `until`; a WebSocket closes with 4003). A route that does not need a user ignores
  a stale token (`Option<AuthContext>`), so a client that always sends its last token can still log
  in, refresh or log out; Steam login refuses one (a stale Bearer there would otherwise create a
  second account).
- **Rotation:** every refresh token works once and is replaced. Presenting it again within 30
  seconds (an HTTP retry, two parts of a game refreshing at once) answers the **same** new pair;
  later, the whole session is revoked (`refresh_token_reused`: the token was probably stolen). The
  replacement pair is derived from the old token with HMAC-SHA-256 under a key the server keeps
  in its database and a random nonce of that one rotation, so a retry gets the same tokens without
  any token being stored in plain text. The nonce is forgotten once the 30 s have passed: then
  even the key plus an old token cannot compute the current tokens.
- **Sessions:** one per login, each its own token family. Logout revokes one session or all;
  a password change revokes the others, a reset or a ban revokes all. Revocation takes effect on
  the next request. Revocations made by other processes (the command line, another instance)
  reach `subscribe_revocations` receivers within `revocation_poll_secs` (5 s). Expired tokens and
  old sessions are deleted hourly (`purge_interval_secs`), audit entries after
  `audit_retention_days` (365).
- **Your handlers** take `AuthContext` (user id, session id, roles, session start),
  `Option<AuthContext>` (bad credentials = anonymous) or `MaybeAuth` (bad credentials answer their
  error); `RequireRole<R>` / `RequireAdmin` also check a role (403 without it).

### Steam

The game gets a ticket with `GetAuthTicketForWebApi(identity)` and posts it hex-encoded with the
identity string. The server asks Steam's `ISteamUserAuth/AuthenticateUserTicket` (publisher Web API
key, app id, ticket, identity) and links the account to the SteamID64. With the `steam` feature
and `steam_app_id`, `steam_web_api_key` (or `steam_web_api_key_file`) and `steam_identity`, the
built-in client is used (HTTPS with rustls + ring). Publisher bans are refused by default, VAC bans
and borrowed copies (Family Sharing) allowed (`steam_reject_publisher_banned`,
`steam_reject_vac_banned`, `steam_allow_family_sharing`); a `BeforeLogin` hook sees all of it.
Without a verifier the route answers 404. Bring your own with `Auth::new().steam_verifier(..)`
(the `SteamVerifier` trait); `FakeSteamVerifier` serves tests. `steam_identity` is required with
any verifier: the server checks every ticket against the CONFIGURED identity (a ticket the player
made for another service cannot be replayed here), and an accepted ticket is refused when it comes
again within 10 minutes. Only Steam's success shape counts (`result: "OK"` and a SteamID).

Linking Steam to an email account (posting a ticket with a Bearer token) needs a login younger than
`link_reauth_secs` (10 minutes; else 403 `reauthentication_required`), allows one Steam account per
account, runs the ban check and `BeforeLogin` hook first, is audited and mails the owner
(`notify_on_link`). `DELETE /v1/account/identities/steam` unlinks (recent login too; refused if it is
the only way to log in), admins use `DELETE /v1/admin/users/{user}/identities/steam`, and a password
reset unlinks every provider (`unlink_identities_on_reset`).

### Mail

Verification and reset mails go through a `Mailer`: the log mailer by default (it logs recipient
and subject; the body with its one-time link only with `log_mailer_show_links = true`, for
development), SMTP with the `smtp` feature (lettre, rustls + ring; `mailer = "smtp"`, `smtp_host`,
`smtp_port`, `smtp_tls = "starttls" | "tls" | "none"` (none only towards this machine),
`smtp_username`, `smtp_password` / `smtp_password_file`, `mail_from`), or your own (an HTTP mail
API) with `Auth::new().mailer(..)`. Requests never wait for a mail: a bounded queue sends them in the
background (`mail_queue`, `mail_concurrency`). `verify_url` / `reset_url` put the token into a link
(`https://example.com/verify?token={token}`); without them the mail contains the token itself.
Mail tokens are single-use, expire (24 h / 1 h) and are stored as SHA-256.

### Roles, admin routes and the audit log

Roles are strings (`admin`, `moderator`, your own); a normal player has none. Grant them with
`user:role`, `PUT /v1/admin/users/{user}/roles/{role}` or `AuthService::set_user_role`. The admin
routes need the `admin` role: list and inspect accounts, ban (with an end time and a reason;
sessions are revoked) and unban, revoke sessions, unlink login providers, grant and revoke roles,
read the audit log. An admin cannot be banned over HTTP (remove the role first; the command line
may), and the last admin cannot lose the role. They stay out of the public OpenAPI document unless
`admin_in_openapi = true`.

The audit log (`auth_audit_log`) records who did what, when and from where: registrations,
logins (and failed logins of existing accounts), Steam links, logouts, refresh-token reuse,
password changes and resets, verifications, every admin and command-line action. Admin actions
are written in the same transaction as the change. Entries never hold passwords, tokens or hashes.
Your code adds its own with `net_backend_server::auth::audit::record`.

### Rate limits and lockout

On by default (`rate_limits`), in memory, per server process:

| Limit | Default |
|---|---|
| logins (password and Steam) per client address and route | 10 per minute (`login_per_minute`) |
| registrations per client address | 10 per hour (`register_per_hour`) |
| refreshes per client address | 60 per minute (`refresh_per_minute`) |
| forgot / reset / verify / resend / password change per client address and route | 10 per minute (`email_routes_per_minute`) |
| mails per account | 3 per hour (`mails_per_account_per_hour`; a further reset request answers the same and sends nothing, a further resend answers 429) |
| failed logins per email address and client network | 5, then one more try every 3 minutes (`login_failures`, `login_lockout_secs` = 900) |
| failed logins per email address, from everywhere | 50 per hour (`account_failures_per_hour`); above it only networks that logged in to the account before (last 30 days) may try |

**Lockout policy:** failures count per (address, client network), so a stranger who guesses wrong
passwords locks only themselves out, not the owner logging in from elsewhere. A looser per-address
ceiling stops a distributed guesser; while it is exceeded, networks the owner logged in from before
still get through (remembered in memory for 30 days). A password reset clears the per-address
ceiling. Client networks are single IPv4 addresses and IPv6 /64 blocks (`rate_limit_ipv6_prefix`;
the per-address route limits use the same keys). Both limits count whether or not an account
exists, so they do not reveal which addresses are registered. Behind a reverse proxy set `http.trusted_proxies` (e.g.
`["127.0.0.1"]` for Caddy on the same machine); the client address is then taken from
`X-Forwarded-For`. `MemoryRateLimiter` with `RateRule`s is available for your own routes.

### Hooks

`BeforeRegister` (change the display name or refuse), `AfterRegister`, `BeforeLogin` (refuse, e.g.
maintenance; sees Steam's ban flags), `AfterLogin`, `BeforeAccountUpdate`, `AfterEmailVerified`,
`AfterPasswordChanged`, `AfterSessionsRevoked` (in `net_backend_server::auth::events`). Code that
holds connections open subscribes to revocations with `AuthService::subscribe_revocations` (close
code 4003 for a ban, else 4001).

### Security notes

- Registration answers 409 `email_taken` for a known address: a deliberate trade-off of the
  protocol (a game needs a clear sign-up answer), bounded by the registration rate limit. The
  password is hashed either way, but a successful registration does more database work than a
  refused one, so the timing differs too.
  Login, password reset and the failed-login limits do not reveal whether an address has an
  account.
- The rate limits, the failed-login counters, the known networks and the used Steam tickets live
  in memory (bounded), per server process: they reset when the server restarts. Under heavy key
  churn the oldest entries are dropped first.
- Bearer tokens only (no cookies), so CSRF does not apply. Tokens never appear in logs (the
  request log has the path without the query string), in `Debug` output or in error answers.
- The email address is not proven until it is verified; `login_requires_verified_email` enforces
  it. Email addresses are compared in NFC and lower-cased, with internationalised domains as
  written (`bücher.de` and `xn--bcher-kva.de` count as different addresses).
- Every setting is in `[modules.auth]` (see `AuthConfig`); secrets also come as `*_file`.

## WebSocket

The hub at `/v1/ws` (on by default, `[ws]`) carries requests, answers and server pushes as JSON
objects in text frames, exactly the envelope of
[`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol)
(which is what `bevy_net_backend`'s `JsonEnvelope` speaks). Clients in other languages can follow
this section or the AsyncAPI 3.0 document the server generates at **`GET /v1/asyncapi.json`**
(every frame type, the kinds your server registered, close codes; `asyncapi export` writes it
without a database).

### Frames

```json
{"id":7,"type":"game.shout","data":{"text":"hello"}}
{"id":7,"ok":true,"data":{"listeners":12}}
{"id":7,"ok":false,"error":{"code":"room_full","message":"the room is full"}}
{"type":"game.heard","data":{"from":42,"text":"hello"}}
```

| Direction | Frame | Rules |
|---|---|---|
| client → server | request `{"id","type","data"}` | `id` is an unsigned 64-bit integer, echoed in the answer; `data` may be left out (`null`) |
| server → client | answer `{"id","ok":true,"data"}` / `{"id","ok":false,"error"}` | exactly one per request; `error` is the protocol's `{"code","message","details"?}` |
| server → client | push `{"type","data"}` | never an `id` or `ok` field |
| client → server | `{"type":"auth","data":{"token":"…","protocol":1}}` | first-message authentication (no `id`) |
| server → client | `{"type":"auth.ok","data":{"user_id":42,"protocol":1}}` / `{"type":"auth.failed","error":{…}}` | exactly one per `auth`; `auth.failed` is followed by a close |

A malformed frame that has an unsigned-integer `id` is answered `bad_request`; one without an id
cannot be answered and is dropped (counted in the metrics). Binary frames are not part of the
protocol and are dropped. Error codes on requests: `bad_request` (malformed frame or `data`),
`unauthorized` (not authenticated yet), `unknown_type`, `rate_limited` (`details.retry_after_ms`),
`payload_too_large` (an answer over the message limit), `unavailable` (the handler took longer
than `ws.request_timeout_secs`), `internal` (never with details), plus whatever a handler answers.

### Authentication

| How | What happens |
|---|---|
| `Authorization: Bearer <access token>` on the handshake | checked by the app's authenticators (the `Auth` module's, or your own) before the upgrade |
| `?token=<access token>` (only with `ws.query_token = true`; default off) | the same; the server never logs query strings, but reverse proxies (Caddy, nginx) log URLs with them, so a token there lands in access logs. Prefer the header, or first-message `auth` for clients that cannot set headers (browsers) |
| first message `{"type":"auth",…}` within `ws.auth_timeout_secs` (5 s) | `auth.ok`, or `auth.failed` + close; no `auth` in time: close 1008 |

- A refused handshake never answers 400 (clients retry those forever): an invalid token is 401
  `unauthorized`, an expired one 401 `token_expired` (refresh, then reconnect), a banned account
  403 `banned`, a `before` hook's refusal its own status, an unsupported protocol version 403 when
  the request is not an upgrade (else: upgrade, then close 4010), a full hub or too many sockets
  waiting for `auth` 503 + `Retry-After`, too many handshakes or open sockets from one address 429
  + `Retry-After`, a temporary failure (database down) 503, a plain GET 426.
- Every `auth` frame gets exactly one `auth.ok` / `auth.failed`, also on a socket the handshake
  already authenticated (a client may do both). A later `auth` with a fresh token of the same user
  re-authenticates; another user's token is refused (`auth.failed`, close 4001). `auth.failed` is
  always definitive; a TEMPORARY failure (the database is down, a hook timed out, 5xx / 429) closes
  with 1013 without an answer, so clients reconnect and try again.
- An open socket survives the expiry of its access token. Revoking its session closes it: logout,
  password change or reset, admin revocation, refresh-token reuse → 4001; a ban → 4003. Revocations
  made by another process (the command line, another instance) arrive through the `Auth` module's
  database poll (`revocation_poll_secs`, 5 s). A revocation that lands while a socket is still
  authenticating (after its token was checked) is applied too.
- `WsCtx.auth.roles` follow role changes: at once for changes made in this process (admin routes,
  server code), within `ws.roles_refresh_secs` (60 s) for changes made elsewhere (the command line,
  another instance).
- Requests sent before authenticating are answered `unauthorized`; pushes reach authenticated
  sockets only.

### Close codes

| Code | Meaning | Client reconnects |
|---|---|---|
| 1000 | normal closure | yes |
| 1001 | the server is shutting down or redeploying | yes |
| 1008 | no `auth` in time, or still flooding after the rate limit refused `ws.frame_burst` frames in a row | yes |
| 1009 | a message over `ws.max_message_bytes` | yes |
| 1011 | an unexpected server error | yes |
| 1013 | this socket could not keep up with its pushes (outbox full); reconnect and resync | yes |
| 4001 | authentication refused or revoked: log in again | no |
| 4003 | the account is banned | no |
| 4009 | replaced: the user opened more than `ws.max_connections_per_user` sockets (the oldest of the same session goes first, then the oldest overall) | no |
| 4010 | the client's protocol version is not supported | no |

`net_backend_client` and `bevy_net_backend` never reconnect after 4000–4099; the server uses that
range only for "do not come back".

### Handlers

Register a handler per request kind: typed through the protocol's `WsCall` (the request type and
its answer type), or untyped on `serde_json::Value`. A handler gets a `WsCtx` (the app state, its
`connection`, the caller's `AuthContext`, the request `id` and `kind`) and answers
`Result<Response, AppError>`. Requests of one socket are handled one after another, in order; the
answer is written before any push the handler queued (a chat echo arrives after its
acknowledgement).

```rust,no_run
use net_backend_server::protocol::{ServerPush, WsCall};
use net_backend_server::ws::{WsCtx, WsHandlers};
use net_backend_server::{AppError, Config, Module, NetBackendServer};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Shout {
    text: String,
}

#[derive(Serialize, Deserialize)]
struct Shouted {
    listeners: usize,
}

impl WsCall for Shout {
    type Response = Shouted;
    const KIND: &'static str = "game.shout";
}

#[derive(Serialize, Deserialize)]
struct Heard {
    from: i64,
    text: String,
}

impl ServerPush for Heard {
    const KIND: &'static str = "game.heard";
}

async fn shout(ctx: WsCtx, shout: Shout) -> Result<Shouted, AppError> {
    let hub = ctx.hub();
    hub.join(ctx.connection, "lobby")?; // room_full / quota_exceeded become error answers
    hub.push_room("lobby", &Heard { from: ctx.auth.user_id.get(), text: shout.text })?;
    Ok(Shouted { listeners: hub.room_size("lobby") })
}

/// A module registers its kinds the same way.
struct Lobby;

impl Module for Lobby {
    fn name(&self) -> &'static str {
        "lobby"
    }
    fn ws_handlers(&self, ws: &mut WsHandlers) {
        ws.raw("lobby.ping", |_ctx, data| async move { Ok(data) }).summary("Echo the data back");
        ws.push::<Heard>().summary("Someone in the room shouted");
    }
}

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    NetBackendServer::new(config)
        .module(Lobby)
        .ws(|ws| {
            ws.call::<Shout, _, _>(shout).summary("Shout into the lobby");
        })
        .run()
        .await
}
```

- `NetBackendServer::ws(|ws| …)` (or the shortcuts `.ws_call::<C, _, _>(handler)` and
  `.ws_handler(kind, handler)`) for the game; `Module::ws_handlers` for modules. A kind is 1–64
  bytes of `[a-z0-9_.:-]` starting with a letter, registered once (a duplicate stops the build);
  `auth`, `auth.ok` and `auth.failed` are reserved.
- The typed form decodes `data` as the request type (`bad_request` if it does not fit) and encodes
  the answer. `KindDoc` (returned by every registration) documents a kind for the AsyncAPI
  document: `.summary(..)`, `.description(..)`, `.data_schema(json)`, `.answer_schema(json)`, or
  `.schemas::<Req, Res>()` from types deriving `utoipa::ToSchema`. `ws.push::<P>()` /
  `ws.push_kind(kind)` documents a push (sending one needs no registration).
- A handler that panics answers `internal` (the socket stays open); one that runs longer than
  `ws.request_timeout_secs` (10 s) answers `unavailable`.

### Pushes and rooms

The hub is `state.ws()` (`AppState::ws`), `State<Hub>` in HTTP handlers and `ctx.hub()` in
WebSocket handlers, so an HTTP route can push too.

| Call | What |
|---|---|
| `push_user(user, &push)`, `push_room(room, &push)`, `push_all(&push)`, `push_connection(id, &push)` | a typed `ServerPush`, encoded once for every receiver |
| `push(target, &push)`, `push_raw(target, kind, &data)`, `publish(Delivery)` | any `Target` (`Connection`, `User`, `Room`, `All`); untyped kinds; a pre-encoded frame. A frame over `ws.max_message_bytes` is refused with `PushError::TooLarge` (logged, counted): clients would close for it |
| `join(id, room)`, `join_with_cap(id, room, cap)`, `leave(id, room)` | membership ends with the socket; caps `ws.max_room_members` (200) and `ws.max_rooms_per_connection` (16) |
| `room_members(room)`, `room_size(room)`, `rooms_of(id)`, `connections_of(user)`, `is_online(user)`, `connection(id)`, `stats()` | lookups (this instance) |
| `close(id, code, reason)`, `close_user(user, code, reason)` | close sockets yourself (e.g. 4001 after your own revocation) |

Each socket has a bounded outbox (`ws.outbox_frames`, 256): **the largest burst you may push to one
socket at once**. A push to a socket whose outbox is full closes it with 1013 (the client reconnects
and resyncs) instead of buffering without limit or silently dropping messages, and a socket that
does not take a frame within `ws.write_timeout_secs` (10 s) is dropped: one slow client never holds
the others back. Raise `outbox_frames` for broadcast-heavy games (a 3000-message burst to a room
needs it above 3000). Pushes keep flowing while a request handler of the socket runs; only the
pushes that handler makes to its own socket wait for its answer (up to `outbox_frames` of them).

### Limits, heartbeats, hooks

- **Caps:** `ws.max_connections` (10 000; 503 above), `ws.max_pending_connections` (1000 sockets
  waiting for `auth`; 503 above), `ws.max_connections_per_ip` (100 open sockets per address; 429),
  `ws.max_connections_per_user` (5; the oldest is closed with 4009, the same session's first),
  `ws.handshakes_per_ip_per_minute` (60; IPv6 by /64; 429 above). Behind a reverse proxy, list it in
  `http.trusted_proxies`: otherwise every client has the proxy's address and the per-address limits
  apply to everyone together (a restart would then 429 most reconnects).
- **Rate limit per socket:** `ws.frames_per_second` (20) with a burst of `ws.frame_burst` (40),
  counting text, binary and ping frames; a request over it is answered `rate_limited`, and a socket
  that keeps flooding is closed with 1008.
- **Sizes:** `ws.max_message_bytes` (1 MiB, the protocol's `MAX_MESSAGE_BYTES`) in both directions.
- **Heartbeats:** the server answers pings and pings every `ws.ping_interval_secs` (20 s); a socket
  that sent nothing (not even a pong) for `ws.idle_timeout_secs` (60 s) is dropped. This fits
  `bevy_net_backend`'s heartbeat (a ping every 15 s, dead after 45 s of silence).
- **Memory:** an 8 KiB read buffer per socket (`ws.read_buffer_bytes`), no write buffering;
  measured about 15 KiB of server heap per idle authenticated socket (tungstenite's defaults would
  add ~120 KiB). Behind Caddy, the proxy needs far more per socket (~120 KiB measured): it, not the
  hub, bounds the socket count on a small box.
- **Hooks** (`ws::events`): `BeforeWsConnect` (refuse a player: 403 at the handshake, else
  `auth.failed` + 4001; a 5xx / 429 refusal is temporary; it carries the handshake's `Origin`: an app
  that authenticates with cookies must check it), `AfterWsConnect` (in its own task),
  `AfterWsDisconnect` (with the rooms it left; not for sockets dropped when the shutdown grace ran
  out), `BeforeWsFrame` (change a request's `data` or refuse it; its `kind` is read-only).
- **Presence** (who is in a room) comes from the [chat](#chat) module, built on these hooks.
- **Several instances:** pushes to users, rooms and everyone go through a `Broadcaster`
  (`.broadcaster(..)`); the default `LocalBroadcaster` delivers in this process. A pub/sub
  implementation publishes each `Delivery` (serde-serializable) AS A WHOLE to every instance, which
  hands it to its own sockets with the `LocalDelivery` it got in `start`. A delivery may carry a
  `Control` instead of a frame (`Hub::remove_from_room(user, room)`: that user's sockets leave the
  room on every instance); an implementation must forward its `control` field unchanged.
  `push_connection` never leaves the process (connection ids are per instance), and rooms,
  `is_online`, `close_user` and the caps are per instance.
- **Shutdown:** every socket gets close 1001 when the shutdown starts; the hub waits for them
  within `server.shutdown_grace_secs`, then drops the rest.

### With bevy_net_backend

```text
// The access token on every handshake (Bearer), the default JsonEnvelope, ws:// only on loopback.
credentials.set(BearerToken::new(session.tokens.access_token.expose()));
ws.connect("main", WsSettings::new("wss://game.example.com/v1/ws"));
// First-message auth instead (or as well): return `WsAuth::new(token).to_message()` from your
// `Credentials::ws_auth_message`, and `.with_auth_ack(Duration::from_secs(5))` to hold requests
// until `auth.ok`.
```

`bevy_net_backend` 0.1.0 treats a 401 handshake as final. After a server restart (1001) a client
whose access token expired meanwhile gets 401 `token_expired` and goes `Disconnected`: watch
`WsStateChanged` for that error, refresh the token (`POST /v1/auth/refresh`), set the new
credentials and `connect` again. Refreshing shortly before expiry avoids it.

## Typed routes (HttpCall)

Every HTTP route of the protocol has an [`HttpCall`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol#typed-http-calls-httpcall)
type naming its method, path, payload and answer. The server mounts its handlers FROM those types,
so a path, a method or an answer type cannot drift from what the clients use, and a test checks that
every route of `routes::ALL` is served and documented. Your own routes can do the same with your
own `HttpCall` types (a `Route::new(..)` of your own):

```rust,no_run
use net_backend_server::http::call::{Call, CallResult, Reply};
use net_backend_server::protocol::routes::{HttpMethod, Route};
use net_backend_server::protocol::{ApiError, HttpCall, NoPayload, PathParams, PayloadKind};
use net_backend_server::{AuthContext, Config, NetBackendServer};
use serde::{Deserialize, Serialize};

/// `GET /v1/game/inventory/{slot}` → `Item`.
struct GetSlot { slot: i64 }
#[derive(Serialize, Deserialize)]
struct Item { name: String }

impl HttpCall for GetSlot {
    type Payload = NoPayload;
    type Response = Item;
    const ROUTE: Route = Route::new(HttpMethod::Get, "/v1/game/inventory/{slot}", true);
    const PAYLOAD: PayloadKind = PayloadKind::Empty;
    fn payload(&self) -> &NoPayload { &net_backend_server::protocol::http_call::NO_PAYLOAD }
    fn path_params(&self) -> PathParams { PathParams::new().with("slot", self.slot) }
    fn from_parts(params: &PathParams, _: NoPayload) -> Result<Self, ApiError> { Ok(GetSlot { slot: params.id("slot")? }) }
}

// The handler takes `Call<C>` last and answers `CallResult<C>` (`Reply::new(..)`, plus headers).
async fn get_slot(_who: AuthContext, Call(call): Call<GetSlot>) -> CallResult<GetSlot> {
    Ok(Reply::new(Item { name: format!("slot {}", call.slot) }))
}

let server = NetBackendServer::new(Config::default()).call::<GetSlot, _, _, _>(get_slot);
// Documented: `#[utoipa::path(..)]` on the handler and `.routes(net_backend_server::call_route!(GetSlot, get_slot))`.
# let _ = server;
```

`Call<C>` decodes the path parameters, the JSON body or query string, and checks the parameters'
shape (an id that is not a number, a name that is not a storage name: 400 `bad_request`).
**`ROUTE.auth` is enforced by the mount:** a route marked "token required" answers 401
(`unauthorized` / `token_expired`, or 403 `banned`) to a request without a valid token before the
handler runs, also when the handler does not take an `AuthContext` (as `get_slot` above would not
need to). The flag in the protocol and the OpenAPI document is a guarantee, not a convention.

## Storage

The `Storage` module (feature `storage`, name `storage`) keeps per-user JSON objects: save slots,
settings, inventory snapshots. Register it after `Auth`.

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::hooks::Decision;
use net_backend_server::storage::events::BeforeStorageWrite;
use net_backend_server::storage::{Storage, StorageConfig};
use net_backend_server::{AppError, Config, NetBackendServer};

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    let mut storage = StorageConfig::default();
    storage.max_objects_per_user = 200;
    NetBackendServer::new(config)
        .module(Auth::new())
        .module(Storage::new().with_config(storage))
        // The game decides what a valid save is.
        .before::<BeforeStorageWrite, _, _>(|_ctx, write| async move {
            if write.collection == "saves" && write.value.get("level").is_none() {
                return Ok(Decision::Reject(AppError::bad_request("a save needs a level")));
            }
            Ok(Decision::Continue(write))
        })
        .run()
        .await
}
```

| Route | What |
|---|---|
| `GET /v1/storage/{collection}` | a page of the caller's objects, **without values** (key, version, size, write access, time), ordered by key |
| `GET / PUT / DELETE /v1/storage/{collection}/{key}` | one object; a GET or PUT answer carries the object's version as an `ETag` (`"3"`; a DELETE answer has none) |
| `POST /v1/storage/_batch/get`, `/_batch/put` | up to 16 objects and 4 MiB of values; a batch put is one transaction (all or nothing) |
| `GET / PUT / DELETE /v1/admin/users/{user}/storage/...` | any user's objects, role `admin`, every access in the audit log (`admin.storage_*`) |

- **Versions:** every write bumps the version (1 for a new object). Without a condition the last
  write wins; `if_version: N` in the body (or `If-Match: "N"`) writes only over version N, and
  `if_version: 0` (or `If-None-Match: *`) only if the object is new. Otherwise: 409
  `version_conflict` with `{"current_version":N}` (+ `"index"` of the failing item in a batch) and
  nothing changes. A header and a body that disagree are a 400. Deleting an absent object is fine
  unless a version is named. Simultaneous conditional writes are safe: exactly one wins.
- **Concurrency:** every write and delete of a player's objects takes that account's lock first,
  then reads the object, then writes exactly the row that exists (or inserts a new one). One
  player's writes run one at a time (exact conditions and quotas); different players' writes never
  wait for each other, and never deadlock on MySQL (no gap locks: nothing updates or deletes a row
  that is not there). A transaction the database still aborts as a deadlock is retried.
- **Write lock:** objects written by server code or an admin with `write: "server"` are read-only
  for their owner (403); the server keeps writing them (`StorageService::put`).
- **Server-owned collections:** `server_collections` (default `["server"]`; an entry `x` covers
  `x` and `x.*`) are written only by server code and admins: the owner reads objects there but can
  never create, change or delete one (403), so a player cannot pre-create a key meant for the
  server (`wallet/gold`: add `"wallet"`). New objects there are server-locked.
- **Limits:** `max_object_bytes` (256 KiB of JSON per value; at most 4 MiB), `max_objects_per_user`
  (1000) and `max_bytes_per_user` (4 MiB of values; a write that does not grow an object always
  passes) — both 403 `quota_exceeded`, exact under concurrent writes, binding the owner's writes
  only (server code and admins may always write; their objects still count). Owner writes (PUT,
  DELETE, a batch put counts once) per user: `write_rate` per `write_rate_window_secs` (60 per
  60 s: a burst of 60, then one a second; 429 `rate_limited` + `retry_after_ms`; 0 = off). Names
  1-128 bytes of `[A-Za-z0-9_.-]` starting with a letter or digit; the PUT body limit follows
  `max_object_bytes`. A failing batch item is named by `"index"` in the error's details (a
  conflict, a lock, a quota, a hook's refusal).
- **Hooks** (`storage::events`): `BeforeStorageWrite` (validate, change the value or refuse),
  `InStorageWriteTx` (inside the write's transaction: write your own rows with THAT transaction,
  or refuse and roll everything back; it may run again when the transaction is retried after a
  deadlock, so it does nothing outside the database), `AfterStorageWrite`, `BeforeStorageDelete`,
  `AfterStorageDelete`; each says who writes (`Writer::Owner`, `Server`, `Admin`).
- **Server code:** `StorageService` (`Ext<StorageService>`, `state.get::<StorageService>()`):
  `get`, `list`, `get_many`, `put` (with the lock), `delete` for any user.
- **Table:** `storage_objects` (one row per object, the value as JSON bytes, cascading with the
  account); publishable like every module's migrations.

## Chat

The `Chat` module (feature `chat`, name `chat`) runs rooms, direct messages, history, presence and
moderation over the WebSocket hub. Register it after `Auth`; it needs `ws.enabled`.

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::chat::events::BeforeChatSend;
use net_backend_server::chat::{Chat, ChatConfig, RoomSpec};
use net_backend_server::hooks::Decision;
use net_backend_server::{AppError, Config, NetBackendServer};

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    let mut chat = ChatConfig::default();
    chat.rooms = vec![RoomSpec::new("world").with_name("World").with_max_members(500), RoomSpec::new("trade")];
    NetBackendServer::new(config)
        .module(Auth::new())
        .module(Chat::new().with_config(chat))
        // Moderation is the game's: filter, rewrite or refuse.
        .before::<BeforeChatSend, _, _>(|_ctx, mut message| async move {
            if message.text.contains("buy gold") {
                return Ok(Decision::Reject(AppError::forbidden("no advertising")));
            }
            message.text = message.text.replace("darn", "d**n");
            Ok(Decision::Continue(message))
        })
        .run()
        .await
}
```

| Kind / route | What |
|---|---|
| `chat.join` / `chat.leave` | by id or a public room's key; membership lasts as long as the connection (rejoin after a reconnect) |
| `chat.send` | to a room joined on this connection, or a DM room (no join); answered with the stored id BEFORE the sender's own `chat.message` echo, which carries the sender's `nonce` (in public and group rooms every member's push carries it: use a random value, never a secret; in DMs and in the history only the sender sees it) |
| `chat.history`, `GET /v1/chat/rooms/{room}/messages` | cursor pages, newest first (public rooms: anyone; group and DM rooms: their members) |
| `chat.members` | who is online in a joined room (each user once; this instance); a DM room lists only the caller: a DM never reveals whether the peer is online |
| pushes `chat.message`, `chat.deleted`, `chat.presence` | to the room's members; a DM's to every connection of both users |
| `GET /v1/chat/rooms` | the public rooms, with online counts and caps |
| `POST /v1/chat/dm`, `GET /v1/chat/dms` | open (or find) a DM room with another user (`dm_open_rate`: 20 per 600 s per user, then 429); the caller's DM rooms, newest activity first |
| `DELETE /v1/chat/rooms/{room}/messages/{message}` | its sender (`allow_self_delete`) or a `moderator_roles` member (audited as `chat.message_deleted`) |

- **Rooms:** public rooms from `[[modules.chat.rooms]]` (created or updated at start) or
  `ChatService::create_room`; group rooms (members only) from `ChatService::create_group` /
  `add_member` / `remove_member`; DM rooms from `POST /v1/chat/dm`. Server code also sends
  (`send_as`, e.g. system messages) and deletes (`delete_message`).
- **DMs are open:** anyone may open a DM with any account (it only has to exist). Blocking is the
  game's: refuse in `BeforeDirectOpen` (opening) and `BeforeChatSend` (sending). Opening is rate
  limited per user (`dm_open_rate` per `dm_open_window_secs`, a burst of 20, then one every 30 s),
  which also bounds probing which account ids exist.
- **Limits:** a member cap per room (`max_room_members` 200 unless the room has its own,
  `max_group_members` for groups; `room_full` 409, exact also under simultaneous joins). The cap
  counts CONNECTIONS, while `RoomInfo.member_count` and `chat.members` count USERS: a room of 150
  users with 200 sockets is full. 16 rooms per connection (`ws.max_rooms_per_connection`;
  `quota_exceeded`), `max_text_chars` (500) with the protocol's text rules (no control or invisible
  characters, something visible, no huge stacks of combining marks), a send rate per user as a
  token bucket (`rate_messages` per `rate_window_secs`: a burst of 5, then one every 2 s;
  `rate_limited` with `retry_after_ms`, about 2000 right after a burst), the history retention
  (`history_retention_days`, 30; a background purge that deletes in batches of 1000).
- **Presence:** a user's first connection in a room pushes `chat.presence` `joined` to the room, its
  last one `left` (leave or disconnect), with the online count; the joiner's own connections get it
  too. Rooms with more online users than `presence_max_members` (100) get none, and each room has a
  push rate (`presence_per_second`, 10): a big room never floods. `chat.members` is the full list.
- **Hooks** (`chat::events`): `BeforeChatJoin`, `BeforeChatSend` (filter / rewrite / refuse; a
  rewrite follows the text rules), `AfterChatSend` (in its own task: never delays the answer),
  `BeforeDirectOpen` (blocks, privacy), `AfterChatDelete`.
- **Group members:** `remove_member` cuts a member off at once everywhere: every instance checks
  the `chat_members` table on each group send, join and history read, and the member's sockets
  leave the room on every instance (`Hub::remove_from_room` through the `Broadcaster`), also when
  the removal races with a join.
- **Several instances:** messages, deletions and group removals travel through the hub's
  `Broadcaster`; joined rooms, caps, presence and the send rate are per instance.
- **Tables:** `chat_rooms`, `chat_members`, `chat_messages` (a deletion clears the body and keeps
  the row); publishable.

## Databases

One `Db` handle over the pool of the configured backend. Statements are built with
[sea-query](https://crates.io/crates/sea-query) once and rendered for MySQL, PostgreSQL or
SQLite; rows decode into `#[derive(sqlx::FromRow)]` structs on every backend.

```rust,no_run
use net_backend_server::sea_query::{Expr, ExprTrait, Query};
use net_backend_server::{Db, DbError};

#[derive(sqlx::FromRow)]
struct Score {
    user_id: i64,
    points: i64,
}

async fn top(db: &Db) -> Result<Vec<Score>, DbError> {
    let query = Query::select()
        .columns(["user_id", "points"])
        .from("scores")
        .and_where(Expr::col("points").gt(0))
        .order_by("points", net_backend_server::sea_query::Order::Desc)
        .limit(10)
        .to_owned();
    db.fetch_all(&query).await
}
```

- `execute`, `fetch_all`, `fetch_optional`, `insert_id` (the new `BIGINT` id on every backend),
  `execute_script` (raw SQL, several statements, no bound values), `begin` → `DbTx` (commit,
  rollback, the same methods plus `insert_id` / `fetch_one`), `fetch_one`, `ping`, `close`;
  `DbError::is_unique_violation()` / `is_foreign_key_violation()` on every backend. Unsigned values
  above `i64::MAX` are refused with an error (every portable integer column is a signed `BIGINT`).
- **Deadlocks:** `DbError::is_retryable()` is true when the database aborted a transaction to
  resolve a conflict (a MySQL deadlock 1213, PostgreSQL `40P01` / `40001`, a busy SQLite);
  `db::Retry` runs such a transaction again from the start (bounded, with a short jittered pause)
  and `DbTx::finish(result)` commits or rolls back. The framework's own write transactions use
  both; use them for yours whenever the transaction can run twice (nothing outside the database
  inside it).
- **Portable column types** (`db::schema`): `BIGINT` ids, `BIGINT` unix-millisecond timestamps,
  bytes for JSON and binary payloads (`LONGBLOB` / `BYTEA` / `BLOB`), `VARCHAR(n)` strings. On
  MySQL tables use `utf8mb4` with the binary collation `utf8mb4_bin`, so comparisons and unique
  indexes are case-sensitive as on PostgreSQL and SQLite (`Sword` ≠ `sword`); the one remaining
  difference is that MySQL ignores trailing spaces in comparisons — normalise values in code (trim,
  lower-case emails). Framework tables use only these portable types.
- `SUM()` over a `BIGINT` is `DECIMAL` / `NUMERIC` on MySQL and PostgreSQL: cast it
  (`CAST(SUM(x) AS SIGNED)` / `::BIGINT`) before decoding into `i64`.
- **TLS to a remote database:** sqlx's default is "prefer TLS, do not verify". Use
  `ssl-mode=VERIFY_IDENTITY` (MySQL) / `sslmode=verify-full` (PostgreSQL) in the URL for a
  database on another machine; certificates are checked against the webpki roots.
- **One database, plain sqlx:** an app with one backend can use its pool directly (`db.mysql()`,
  `db.postgres()`, `db.sqlite()`), including sqlx's compile-checked `query!` macros. The framework's
  own queries cover three dialects in one crate and run in CI against all three databases.
- SQLite: `sqlite::memory:` keeps exactly one connection (each connection would be its own empty
  database); file databases use WAL mode (switched once at connect, so several processes can open a new file
  at the same time) and a 5 s busy timeout. SQLite allows one writer at a
  time: fine for small games; for write transactions that read first, start them with
  `BEGIN IMMEDIATE` (the pool's `begin_with`) to avoid busy errors.

## Migrations

Plain SQL per dialect, in files you can read and change.

- **Sources:** each module embeds its SQL per dialect; the app's own migrations live in
  `migrations/app/<dialect>/<version>_<name>.sql`.
- **Order:** modules in registration order, then `app`; by version inside each (positive
  integers, by convention `YYYYMMDDnnnn`). App migrations may depend on module tables, never the
  reverse.
- **Tracking:** the table `nbs_migrations (module, version, name, checksum, applied_at)`. The
  checksum is the SHA-256 of the SQL with line endings normalised (a CRLF checkout is not an
  edit). Changing an applied migration is an error: add a new one instead.
- **Transactions:** each migration runs with its tracking row in one transaction. PostgreSQL and
  SQLite roll a failed migration back completely. **MySQL commits DDL at once**, so on MySQL the
  statements of a migration run one by one and a failure says exactly which statements already
  took effect, that the migration is not recorded, and how to recover (undo them by hand, or delete
  them from the file and fix the failing one; then `migrate` again). Prefer one DDL statement per
  MySQL migration. Once a DDL statement ran, MySQL has committed and every later statement
  autocommits, so the message then lists every statement before the failure. Directives in the
  leading comment lines: `-- nbs:no-transaction` runs a migration without a transaction (e.g.
  PostgreSQL's `CREATE INDEX CONCURRENTLY`); `-- nbs:single-statement` sends a MySQL file as one
  statement (a trigger, procedure or event with a `BEGIN … END` body); inside a file,
  `-- nbs:statement-begin` / `-- nbs:statement-end` lines mark one such block (they take the place
  of the client command `DELIMITER`).
- **Concurrent runs** (two servers starting with `migrate_on_start`, a deploy racing a start):
  MySQL takes `GET_LOCK` with a name per database, PostgreSQL an advisory lock (per database);
  both wait at most `database.migrate_lock_timeout_secs` (60 s), then fail with a message naming
  the lock. SQLite takes the write lock per migration (`BEGIN IMMEDIATE`, waiting up to the same
  time); on every backend each migration re-checks its tracking row inside its transaction, so a
  migration applied meanwhile by the other process is skipped. `migrate status` only reads.
- **Publishing (the app owns a module's tables):** `migrations publish <module>` copies the
  module's SQL into `migrations/<module>/<dialect>/`. From then on, when that directory exists,
  its files are used instead of the embedded ones: edit them **before** the first `migrate`.
  Existing files are never overwritten without `--force`; after a module upgrade, `migrate` warns
  about new migrations until you publish again (which adds only the new files).

## The command line

Every server binary built with `.run()` has these commands:

| Command | What |
|---|---|
| `serve` (or no command) | run the server until SIGTERM / Ctrl-C |
| `migrate`, `migrate up` | apply pending migrations |
| `migrate status` | list every migration: `applied`, `pending`, `MODIFIED`, `missing` |
| `migrations publish <module> [--dialect mysql\|postgres\|sqlite] [--force]` | copy a module's SQL into the app |
| `config check [--connect]` | validate the configuration and print a summary without secrets (and try the database) |
| `openapi export [--output <file>]` | write or print the OpenAPI document (needs no database) |
| `asyncapi export [--output <file>]` | write or print the AsyncAPI document of the WebSocket endpoint (needs no database) |
| `user:create <email> [--name <n>] [--password-file <f>] [--admin] [--verified]` | create an account (auth module); without a file a password is generated and printed once |
| `user:role <email or id> <role> [--revoke]` | grant or revoke a role |
| `user:ban <email or id> [--reason <text>] [--hours <n>]`, `user:unban <email or id>` | ban (revokes the sessions) or lift a ban |
| `sessions:revoke <email or id>` | log an account out everywhere |

```text
my-game-server migrate && my-game-server serve
```

Modules and the app add their own commands: implement `command::AppCommand` and register it with
`.command(..)` (or return it from `Module::commands`). The command line builds the server first
(configuration, database, modules), so a command can use every service; `--help` lists them.
Passwords are never taken from the command line itself (it is visible to other users of the
machine).

## HTTP basics

- Routes live under `/v1`. `GET /v1/info` answers the protocol's `ServerInfo`
  (`{"protocol":1,"min_protocol":1,"modules":[…]}`, module names sorted). `GET /healthz` (liveness)
  and `GET /readyz` (readiness: the database answers and the server is not shutting down) are
  unversioned. `/v1/ws` is the WebSocket hub (see [WebSocket](#websocket)); it never answers 400,
  which clients would retry forever.
- Every answer (except CORS preflights) carries `x-net-backend-protocol: 1`. A request naming an unsupported version in
  that header gets 400 `unsupported_protocol` with `{"supported_min":1,"supported_max":1}`.
- **Body limits:** 64 KiB by default (the protocol's `DEFAULT_BODY_LIMIT_BYTES`; axum's own
  default would be 2 MB), configurable; per route with
  `post(upload).layer(net_backend_server::http::body_limit(1024 * 1024))`. That limit applies to
  the body extractors (`ApiJson`, `Json`, `Bytes`, `String`); on top, `http.max_body_bytes`
  (32 MiB) caps every body, also raw body streams and raised per-route limits (413).
- **Behind a reverse proxy** its own request body limit must allow the largest route: with the
  storage module a batch put is up to ~4.2 MB (4 MiB of values plus JSON). In Caddy a site-wide
  `request_body { max_size .. }` wins over a route's own, so set it for the API's site, e.g.
  `request_body { max_size 5MB }`.
- **Slow clients:** a client must send a request's headers within
  `server.header_read_timeout_secs` (15 s), else the connection is closed; idle keep-alive
  connections are closed after the same time.
- `ApiJson<T>` is `Json<T>` with protocol error bodies: 400 `bad_request` for malformed JSON or
  wrong fields, 413 `payload_too_large`, 415 `unsupported_media_type` without
  `content-type: application/json`. A wrong method is 405 `method_not_allowed`.
- **Request ids:** every request gets one (in the request's log span and the `x-request-id`
  header); a client's id is kept only with `http.trust_request_id`.
- **Tracing:** one span per request with method, path (never the query string: it may carry
  credentials) and request id; the answer logged at `info`.
- **Timeouts:** `http.request_timeout_secs` (default 30), then 503 `unavailable`.
- **Panics** in handlers answer 500 `internal` without details; the server keeps running.
- **CORS** is off unless `cors.allowed_origins` is set. **Compression**, when wanted, is done by
  the reverse proxy.
- **Authentication:** authenticators (the app's, then the modules', e.g. the `Auth` module's
  Bearer check) decide who is calling; handlers take `AuthContext` (401 without one, or the
  authenticator's own error such as `token_expired`), `Option<AuthContext>` (a refused credential
  counts as anonymous; this differs from a plain "401 at once" rule) or `MaybeAuth` (a refused
  credential answers its error, no credential is `None`).
- **Rate limits:** every installed `RateLimiter` is asked twice: before authentication (client
  address and route pattern, so token guessing is throttled too) and after it (with the user); a
  refusal is 429 `rate_limited` plus `Retry-After`. `MemoryRateLimiter` (token buckets with
  bounded memory, `RateRule::per_ip` / `per_user`) is ready to use; the `Auth` module installs its own.
- **Client address:** `ClientIp` is the connection's peer, or behind the proxies listed in
  `http.trusted_proxies` the right-most `X-Forwarded-For` address that is not one of them.

## Errors

Every 4xx / 5xx answer is the protocol's error body:

```json
{"error":{"code":"validation_failed","message":"the request is invalid","details":{"fields":{"sides":["must be between 2 and 1000"]}}}}
```

Handlers return `AppError` (`bad_request`, `validation`, `unauthorized`, `forbidden`,
`not_found`, `conflict`, `payload_too_large`, `rate_limited`, `unavailable`, `internal`, or any
code with `AppError::new` / `with_status`); the status follows the protocol's code table. `?`
converts database and framework errors into 500 `internal`: the cause is logged with the request
id, the client sees only `{"error":{"code":"internal","message":"internal server error"}}`.
Framework rejections (unknown route, wrong method, a plain-text 4xx, axum's 422 for a wrong JSON
shape, which becomes 400) are rewritten into the error body with the protocol's status for the
code; a game's own JSON 4xx passes only if it already is a protocol error body. A 5xx that did not
come from an `AppError` has its body replaced, so no SQL, driver message or stack trace ever
reaches a client.

## OpenAPI, health and metrics

- `GET /v1/openapi.json` (`openapi.enabled`, default on): the core routes, every module's
  documented routes and yours. Document a handler with `#[utoipa::path(..)]` and register it with
  `.routes(utoipa_axum::routes!(handler))`; plain `.route(..)` handlers work but stay out of the
  document. The document lists every route, so decide before exposing admin routes whether it
  should be public (`openapi.enabled = false` hides it; `openapi export` still writes it).
  `openapi.ui = true` serves `/v1/docs`: a page that runs a **third-party script** (e.g. the
  Scalar viewer from a CDN) on the API's own origin. It is off by default and needs the script
  pinned to an exact version (`openapi.ui_script_url`) plus its Subresource Integrity hash
  (`openapi.ui_script_integrity`); the page sends a Content-Security-Policy that allows no other
  script.
- `GET /v1/asyncapi.json` (with `openapi.enabled` and `ws.enabled`): the AsyncAPI 3.0 document of
  the WebSocket endpoint, generated when the server is built: the envelope, `auth` / `auth.ok` /
  `auth.failed`, the error answer, every registered request kind (with its schemas when
  documented) and documented push, the close codes (also as `x-close-codes`).
- `GET /readyz` checks the database with a 1 s limit and fails at once when the database refuses
  connections.
- Metrics (`metrics.enabled`, default off): Prometheus text at `GET /metrics` on **its own
  listener**, `metrics.bind` (default `127.0.0.1:9100`), never on the API port:
  `nbs_http_requests_total` and `nbs_http_request_duration_seconds` by method, route pattern and
  status; for the WebSocket hub `nbs_ws_connections` (gauge), `nbs_ws_frames_in_total`,
  `nbs_ws_frames_out_total`, `nbs_ws_closes_total` by `code` (the code sent, or `peer` / `dead` /
  `stuck`), `nbs_ws_dropped_frames_total` and `nbs_ws_handshakes_refused_total` by `reason`,
  `nbs_ws_slow_consumers_total`. Record your own metrics with the [`metrics`](https://crates.io/crates/metrics) facade.
  If your app installs its own recorder first, the framework records into it and does not serve
  `/metrics`.

## Graceful shutdown

On SIGTERM or Ctrl-C (or the future given to `serve_with_shutdown`): the server stops accepting
connections, `/readyz` answers 503, every WebSocket gets close 1001 (clients reconnect to the next
instance), in-flight requests and closing sockets get `server.shutdown_grace_secs` to finish,
then modules shut down in reverse order, the shutdown hooks run and the database pool closes.
At the deadline the remaining connections are closed and their handlers are dropped (cancelled),
so no handler runs on after the modules and the pool are gone. Set systemd's `TimeoutStopSec`
above the grace period plus the module shutdown time.

## Deployment

The repository's [`deploy/`](https://github.com/warmar94/net_backend/tree/main/deploy) folder installs a server on one
Linux machine (Ubuntu 24.04), with the install path chosen once:

| | Docker Compose | systemd |
|---|---|---|
| The server | a distroless, non-root, read-only container with a health check | a service of a dedicated user with systemd's sandboxing (`NoNewPrivileges`, `ProtectSystem=strict`, …), `LimitNOFILE=262144` |
| Database | MySQL 8.4 or PostgreSQL 16 in a container | MySQL, PostgreSQL or SQLite on the machine (`install.sh` creates the database and its account) |
| Migrations | a one-shot `migrate` service before the server starts | `ExecStartPre=… migrate` before every start |
| HTTPS + WSS | Caddy: automatic certificates, `request_body max_size 5MB` (the 4 MiB batch put), access logs without tokens, the server trusting only Caddy's `X-Forwarded-For` | the same |
| Backups | a daily systemd timer: `mysqldump --single-transaction`, `pg_dump -Fc` or SQLite's online backup, checked, kept 14 days; `net-backend-restore` puts one back (safety backup, migrate, readiness wait) | the same |

Both install the **reference server**,
[`examples/server.rs`](https://github.com/warmar94/net_backend/blob/main/crates/net_backend_server/examples/server.rs):
the framework with `Auth`, `Storage` and `Chat`, configured from `config.toml`, plus a `healthcheck`
command (exit 0 when `/readyz` answers 200) for containers without a shell. Your own server binary has the
same command line and drops into the same files. The guide also covers the configuration, capacity numbers
measured on a 2 vCPU machine, the limits to raise together (open files, `ws.max_connections`, Caddy's
~120 KiB per proxied WebSocket), an
[SSH hardening guide](https://github.com/warmar94/net_backend/blob/main/deploy/ssh-hardening.md) and the
[`load_test`](https://github.com/warmar94/net_backend/tree/main/crates/load_test) tool for sizing a machine.

```text
cargo build --release --locked -p net_backend_server --example server --no-default-features --features mysql,storage,chat,smtp
```

## How it works

- Logins: email + password and Steam.
- Each server process holds its own WebSocket rooms, users and connections; pushes travel through the
  `Broadcaster` (the built-in one delivers in this process). Sockets waiting for their
  first-message `auth` count against `ws.max_connections` for up to `ws.auth_timeout_secs`.
- Rate limits, the failed-login counters, chat presence and the chat send rate are in memory, per
  process.
- Storage values are JSON (binary data as a string in JSON); every object belongs to one user and
  is read through that user's routes, server code or the admin routes.
- Chat: presence is per room and per instance; server code creates public rooms and groups,
  players open DM rooms.
- Revocations from other processes reach this process's subscribers by a database poll
  (`revocation_poll_secs`, 5 s).
- The framework's queries run in CI on all three databases.
- TLS is terminated by the reverse proxy (Caddy), which also compresses answers when configured to.
- Capacity depends on the machine: the [deployment guide](https://github.com/warmar94/net_backend/tree/main/deploy#capacity-limits-and-load-testing) lists the numbers measured on a 2 vCPU machine and the `load_test` tool measures yours.

## Compatibility

| net_backend_server | net_backend_protocol | axum | sqlx | sea-query | utoipa | Rust |
|---|---|---|---|---|---|---|
| 0.1.0 | 0.1.0 | 0.8 (≥ 0.8.9) | 0.9 | 1.0 (≥ 1.0.2) | 6 | 1.95+ |

## Testing

```text
cargo test -p net_backend_server --no-default-features --features sqlite,storage,chat   # everything runs in process
NBS_TEST_MYSQL_URL=mysql://root:pw@127.0.0.1:3306/nbs \
NBS_TEST_POSTGRES_URL=postgres://postgres:pw@127.0.0.1:5432/nbs \
  cargo test -p net_backend_server --all-features -- --ignored               # the database, auth, storage and chat suites on MySQL / PostgreSQL
# incl. the concurrency stress tests: mysql_first_saves_stress, postgres_first_saves_stress (40 new
# players save at once), mysql_dm_stress, postgres_dm_stress (200 DM sends into one room at once)
```

The database suite (migrations: order, idempotence, checksums, failing multi-statement
migrations and their recovery, publishing and overrides, concurrent runs and lock timeouts; the
query layer: inserts, ids, updates, case-sensitive unique violations, transactions) runs on SQLite
in every test run and on MySQL 8.4, MariaDB 11.4 and PostgreSQL 16 in CI (each test creates and
drops its own database). The HTTP tests run the assembled router in process; the serving tests use
a loopback port with bounded waits (shutdown deadline, slow headers, module start / shutdown
limits). The auth suite covers every account route (happy and failure paths), expiry, rotation
with the grace window and family revocation, logout, password change and reset, verification,
Steam (a fake verifier), roles, admin routes, the audit log, rate limits and lockout, hooks, the
same answer for known and unknown addresses, redaction (no secret in logs, `Debug` or errors) and
hashing on the blocking pool; it runs on SQLite here and on MySQL / MariaDB / PostgreSQL in CI. The
real Steam and SMTP clients are tested against fakes on 127.0.0.1, never a real service.
The storage suite covers every route, versions and conditions (body and headers), isolation
between users, batches, limits and the quota, the server lock, the hooks (before, in the
transaction, after), audited admin access and the races (one winner among simultaneous
conditional writes, the quota under concurrent creates, first saves of many new players at once),
the byte quota, the write rate and the server-owned collections; the chat suite runs a loopback
server with a WebSocket client: rooms, the answer before the echo, history, caps (also simultaneous
joins at the cap), the send rate, the text rules, hooks, DMs to every connection of both users (and
many concurrent DM sends into one room), the DM-open rate, DM privacy (no peer presence), groups
(a removal racing with a join; two instances sharing a database and a broadcaster), deletion and
moderation, presence (transitions, disconnects, the cap and the rate) and retention.
Both run on SQLite here and on MySQL / MariaDB / PostgreSQL in CI. A routes test checks that every
protocol route is served with its method and documented, and that a game's own call marked
`auth` answers 401 without a token although its handler never asks for the caller.
The WebSocket suite runs a loopback server with a WebSocket client: both ways to authenticate, the
auth deadline, every refusal, version mismatches, token expiry, logout / ban / another process's
ban closing sockets, malformed frames, handler panics and timeouts, rooms and their caps,
connection caps, slow consumers, heartbeats and dead peers, the rate and size limits, hooks and
shutdown. A separate unpublished crate in the repository drives the server with the published
`bevy_net_backend` client in a headless app (Bearer and first-message auth with the
acknowledgement, requests, pushes, reconnecting after 1001, staying away after 4003 / 4010 / 401,
heartbeats both ways; storage over HTTP with the protocol's `HttpCall` types; chat with the
protocol's own types as the client's typed requests and pushes). `tests/ws_memory.rs` measures the heap per idle socket (ignored; run it in
release with `--ignored --nocapture`).
CI runs fmt, clippy, docs, the tests for each backend alone and all together,
the external-database job, the end-to-end client job, the dependency rules and the package contents.

## FAQ

**Why not `sqlx::AnyPool`?** It supports few types, and its `?` placeholders break on PostgreSQL.
One enum over the three real pools plus sea-query keeps every type and every dialect correct.

**Can I use SeaORM / Diesel?** Your own code can use any database library on top of the same
database.

**Why plain SQL migrations?** So you can read, review and change them, per database, and own a
module's tables once you publish them.

**Is it Bevy-specific?** No. The server never links Bevy; any HTTP / WebSocket client works.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

This crate lives in the [`net_backend`](https://github.com/warmar94/net_backend) repository
together with the protocol and the client. Issues and pull requests are welcome. Please run
`cargo fmt --all`, `cargo clippy -p net_backend_server --all-targets --all-features -- -D warnings`
and `cargo test -p net_backend_server --no-default-features --features sqlite,storage,chat` before sending a
change; changes to database code should also pass the MySQL / PostgreSQL suite.

Website: [net-backend.com](https://net-backend.com) · Contact: [info@net-backend.com](mailto:info@net-backend.com)
