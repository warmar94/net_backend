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
│  · Bevy: bevy_net_backend   │             │  modules: storage, chat,      │
│  · Other: any HTTP/WS lib   │             │  leaderboards, notifications, │
└─────────────────────────────┘             │  friends, groups, oauth,      │
                                            │  lobbies, matchmaking, files  │
           ▲                                └───────────────────────────────┘
           │                                                   ▲
           └──── net_backend_protocol (shared message types) ──┘

Not Rust? Use the HTTP / WebSocket API directly (see API.md).
```

## Clients

The server does not care which client connects; the JSON on the wire is the contract.

| Your client is… | Use |
|---|---|
| a **Rust** app (tool, bot, CLI, other engine) | [`net_backend_client`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_client) for the connection (HTTP, WebSocket, SSH / SFTP) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) for the message types. |
| a **Bevy** game | [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) for the connection (HTTP, WebSocket, SSH / SFTP) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) for the message types. |
| **other Rust** code | [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) + any HTTP / WebSocket library (for example reqwest, ureq, tokio-tungstenite). |
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
- [Leaderboards](#leaderboards)
- [Notifications](#notifications)
- [Friends](#friends)
- [Groups](#groups)
- [Lobbies](#lobbies)
- [Matchmaking](#matchmaking)
- [Files](#files)
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
| Databases | PostgreSQL (default), MySQL / MariaDB, SQLite through one `Db` handle; statements built once with sea-query for all three; portable column types. |
| Migrations | Plain SQL per dialect, ordered, tracked and checksummed, namespaced per module, safe against concurrent runs on every backend, precise recovery messages; modules' migrations can be **published** into the app, which then owns them. |
| HTTP | Routes under `/v1`, `GET /v1/info`, `/healthz`, `/readyz`, body limits (64 KiB default, 32 MiB hard cap), a header-read timeout, request ids, request tracing, request timeouts, panic safety, optional CORS, the protocol version header. |
| Errors | Every 4xx / 5xx is the protocol's error body; internal errors are logged with the request id and never reach a client. |
| OpenAPI / AsyncAPI | The HTTP document at `/v1/openapi.json` (utoipa), an optional browser UI; the WebSocket document at `/v1/asyncapi.json` (AsyncAPI 3.0, generated from the registered kinds). |
| Operations | Optional Prometheus metrics on their own loopback listener, a command line (`serve`, `migrate`, `migrations publish`, `config check`, `openapi export`, `asyncapi export`), graceful shutdown with an enforced deadline. |
| Accounts | The `Auth` module: email + password (argon2id) and Steam logins, opaque access and rotating refresh tokens stored as hashes, sessions and revocation, email verification and password reset (log or SMTP mailer), roles, an audit log, `/v1/admin` routes, rate limits and a failed-login lockout, hooks, `user:*` commands. |
| WebSocket hub | `/v1/ws` with the protocol's envelope: auth at the handshake (Bearer / `?token=`) or by first message, request handlers by kind (typed or JSON) for the game and modules, pushes to a socket / user / room / everyone, rooms with caps, close on revocation (4001) and ban (4003), connection caps, per-socket rate limits and bounded outboxes, heartbeats, hooks, 1001 on shutdown, a pub/sub seam for several instances. |
| Typed routes | Every protocol route is mounted from its `HttpCall` (method, path, payload, answer); the same for your own routes (`.call::<C, ..>(handler)`, `call_route!`). |
| Storage | The `Storage` module (feature `storage`): per-user JSON objects with versions, conditional writes (`if_version`, `If-Match`, `ETag`), batches, a server write lock, quotas, a visibility (private / public / friends) with reads of other players' objects, hooks (incl. in the transaction), audited admin access. |
| Chat | The `Chat` module (feature `chat`): public, group and DM rooms, the answer before the echo, history pages, caps and rates, moderation hooks and deletion, presence with a cap and a rate, retention; message editing (the sender's window, `chat.moderate`), read markers with coalesced `chat.read` pushes and unread counts, throttled typing indicators, rooms created by players (owner / moderators, public or private, invitations, kicks as bans, hand-on, `chat.room` pushes, limits). |
| Leaderboards | The `Leaderboards` module (feature `leaderboards`): boards with a score mode (best / latest / sum), an order and a period (all-time / daily / weekly), score submission with hooks and server-only boards, the top, the player's rank and the ranks around it, retention of old periods. |
| Notifications | The `Notifications` module (feature `notifications`): notifications stored per player and pushed live (`notify.new`), lists, counts, read / unread, deletes over HTTP and the WebSocket, a per-player cap, a retention, hooks, a server API to send them. |
| Friends | The `Friends` module (feature `friends`): friend requests by account id, display name or friend code, accept / decline / cancel / remove, blocks, the friends' online state (WebSocket connections and heartbeats, the `friends.presence` push), limits and a request rate, notifications for requests and acceptances, hooks; with Steam login, which of a list of Steam IDs belong to accounts here (for players who linked Steam; findable by default, a per-player opt-out). |
| Groups | The `Groups` module (feature `groups`): groups (guilds, clans) with an owner, admins and members, invitations (as notifications), open groups, kicks, ownership transfer, metadata, a name search, a chat room per group (with the chat module), limits and a create rate, hooks. |
| OpenID Connect | The `OAuth` module (feature `oauth`): logins with an identity provider's ID token (Google as a preset, any OpenID Connect provider by its issuer), RS256 / ES256 signatures checked with ring against the provider's keys (fetched over HTTPS, cached, rotated), issuer / audience / expiry / nonce checks, each nonce used once, linking to existing accounts, hooks. |
| Files | The `Files` module (feature `files`): players' binary files: multipart uploads streamed to a store with a content type, a SHA-256 and exact per-player quotas, downloads (`ETag`, 304, attachment), a visibility (private / public / friends / shared with accounts), metadata, listings, hooks; the bytes on the local disk or in a store you provide (`FileStore`). |
| Lobbies | The `Lobbies` module (feature `lobbies`): lobbies with a host, join codes (also as a number below 2^40), visibility (public / private / friends-only), ready flags, metadata and a search by it, an open / in-game / closed state, kicks and a new host, the `lobby.member` / `lobby.changed` pushes, a chat room per lobby, the disconnect rule, hooks, limits and rates. |
| Matchmaking | The `Matchmaking` module (feature `matchmaking`): queues, one ticket per player with attributes, rounds by the game's rules (a hook; first come, first matched by default) with data for the players, the `match.found` / `match.expired` pushes, timeouts; tickets in memory. |
| Permissions | Named rights finer than roles (`permissions`): declared by modules and the game with default roles, granted to roles in `[permissions]`, checked with `RequirePermission<P>` or `AuthContext::has_permission`; `admin` holds every declared one. |
| Seams | Authenticators (`Authenticator`, the `AuthContext` / `RequireRole` extractors), rate limiters (with an in-memory `MemoryRateLimiter`), trusted-proxy client addresses, app commands, the WebSocket `Broadcaster`. |

## Features

| Feature | Default | What |
|---|---|---|
| `postgres` | yes | PostgreSQL 16 (sqlx, TLS with rustls + ring). |
| `mysql` | no | MySQL 8.4 and MariaDB 11.4 — sqlx, TLS with rustls + ring. |
| `sqlite` | no | SQLite, compiled in (no system library needed). |
| `steam` | no | The built-in Steam ticket check (`SteamWebApiVerifier`: hyper + rustls with ring). |
| `smtp` | no | The SMTP mailer (`SmtpMailer`: lettre with rustls + ring). |
| `storage` | no | The [storage](#storage) module (no extra dependency). |
| `chat` | no | The [chat](#chat) module (no extra dependency). |
| `leaderboards` | no | The [leaderboards](#leaderboards) module (no extra dependency). |
| `notifications` | no | The [notifications](#notifications) module (no extra dependency). |
| `friends` | no | The [friends](#friends) module (no extra dependency). |
| `groups` | no | The [groups](#groups) module (no extra dependency). |
| `oauth` | no | The [OpenID Connect](#openid-connect-logins-the-oauth-module) login module (ring for the signatures; hyper + rustls with ring for the providers' keys). |
| `lobbies` | no | The [lobbies](#lobbies) module (no extra dependency). |
| `matchmaking` | no | The [matchmaking](#matchmaking) module (no extra dependency). |
| `files` | no | The [files](#files) module (axum's multipart support: `multer`; tokio's file I/O). |

The backends are additive: any combination compiles, and the server uses the one its
`database.url` names. At least one is needed to run a server; without any, starting fails with a
clear message. TLS is rustls with ring throughout. Modules are features and off by default: a
server compiles only what it registers (`features = ["storage", "chat"]` on top of the default `postgres`). A
missing `database.url` is an error at start and in `config check`: the server never falls back to
another database.

## Install

The installer `net-backend new` asks for the database (SQLite, PostgreSQL, MySQL) and the modules
(all but `oauth` ticked; every module adds `auth`), then writes a server that registers them, with a
development `config.toml` (a section per module), a `Dockerfile`, a `compose.yaml` and (with `new`) a
client and a demo app, ready for `cargo run`. Every question is a flag:

```text
cargo install net_backend
net-backend new mygame                                          # asks in a terminal
net-backend new-server mygame --db postgres --modules chat,leaderboards,lobbies   # the server alone
```

In your own project, pick ONE of the first three lines (the database and the modules), then add
tokio and serde:

```text
cargo add net_backend_server                                                  # PostgreSQL (the default)
cargo add net_backend_server --no-default-features --features sqlite          # or SQLite (MySQL / MariaDB: --features mysql)
cargo add net_backend_server --no-default-features --features sqlite,storage,chat   # or SQLite with storage + chat
cargo add tokio --features rt-multi-thread,macros
cargo add serde --features derive                                             # for your own request types
```

The same in `Cargo.toml`:

```toml
[dependencies]
net_backend_server = { version = "0.2.0" }                                   # PostgreSQL
# net_backend_server = { version = "0.2.0", default-features = false, features = ["sqlite"] }
```

The framework re-exports `axum`, `sea_query`, `sqlx`, `utoipa`, `utoipa_axum` and the protocol
crate (`net_backend_server::protocol`); prefer reaching them through these re-exports. Some macros
refer to their crate by name, so add the crate itself when you use them, in the major version the
framework uses (`cargo tree -p net_backend_server --depth 1` shows it): `utoipa` + `utoipa-axum` for
documented routes (`#[utoipa::path]`, `routes!`), `sqlx` with `default-features = false` and the
feature `derive` for `#[derive(sqlx::FromRow)]`. The framework asks for these with caret versions,
so Cargo unifies them with yours. Because they are
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

With the `sqlite` feature, a `config.toml` in the folder `cargo run` runs in names the database
(an in-memory one here; `sqlite:game.db` is a file, created on the first start):

```toml
[database]
url = "sqlite::memory:"
migrate_on_start = true
```

```text
cargo run
```

`http://127.0.0.1:8080/v1/info` in a browser (or `curl.exe` on Windows, `curl` elsewhere) answers
`{"protocol":1,"min_protocol":1,"modules":[]}`.

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
acquire_timeout_secs = 5       # the wait for a free connection; on SQLite also for the write lock
sqlite_synchronous = "normal"  # SQLite only: "normal" or "full" (see Databases)
connect_lazy = false           # true: start even while the database is down (/readyz says 503)
migrate_on_start = false       # production runs `migrate` as a deploy step
migrations_dir = "migrations"
migrate_lock_timeout_secs = 60 # how long `migrate` waits for another process's migrations lock
statement_timeout_secs = 0     # MySQL / MariaDB / PostgreSQL: the database cancels a longer statement (0: no limit)

[http]
body_limit_bytes = 65536       # every route without its own limit
request_timeout_secs = 30      # then 503 "unavailable"
upload_idle_timeout_secs = 30  # upload routes: 503 when no data arrives for this long
upload_timeout_secs = 3600     # upload routes: the whole body (0 = no overall limit)
trust_request_id = false       # keep a client's x-request-id (behind a proxy that sets it)
max_body_bytes = 33554432      # the hard cap for every body, raw streams and raised per-route limits included
trusted_proxies = []           # e.g. ["127.0.0.1"]: take the client address from X-Forwarded-For of these proxies

[cors]
allowed_origins = []           # off; ["https://example.com"] or ["*"]
max_age_secs = 600

[log]
level = "info"                 # a tracing filter; RUST_LOG wins
format = "pretty"              # or "json"; "pretty" has colours only on a terminal

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

[permissions]                  # role -> permissions (see Permissions); optional
# moderator = ["lobbies.manage"]

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
(`config::resolve_secret`). A module setting from the environment is a TOML number or boolean when it
reads back as the same text (`42`, `true`), an array as `[1, 2]`, a string in TOML quotes (`"0042"`) as
that string, anything else as plain text; a `SecretString` takes a number or boolean as its text, so a
secret given there is always exactly the variable's text. Error messages show no configured value:
the values serde quotes (strings, numbers, booleans, enum variants) are replaced by `<hidden>`.

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
  own), routes merge in that order, `start` runs in that order; `shutdown` runs for every module
  at the same time (started in reverse order). A name is registered once; names match
  `[a-z][a-z0-9_]*` (at most 32 bytes); `app`, `core` and `nbs` are reserved.
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
  caught: a failed start stops the server from starting, a failed shutdown is logged and the other
  modules still shut down. The shutdown limit is shared: all modules together get
  `module_shutdown_timeout_secs`.
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
| `POST /v1/auth/oauth/{provider}` | an OpenID Connect ID token → account + tokens ([the `oauth` module](#openid-connect-logins-the-oauth-module); links with the Bearer token of a recent login) |
| `POST /v1/auth/refresh` | a refresh token → a new pair (rotation) |
| `POST /v1/auth/logout` | this session, or every session (`"everywhere": true`), with the access token or the refresh token |
| `GET` / `PATCH /v1/account` | the caller's account; change the display name |
| `POST /v1/account/password` | change the password (knowing the current one); the other sessions are revoked |
| `DELETE /v1/account/identities/{provider}` | unlink a login provider (`steam`, or an OpenID Connect provider's name); needs a recent login; refused if it is the only way to log in |
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
  a password change revokes the others, a reset or a ban revokes all. An account keeps at most
  `max_sessions_per_user` (100) live sessions: a login over it revokes the oldest. Revocation takes
  effect on the next request. Revocations made by other processes (the command line, another
  instance) reach `subscribe_revocations` receivers within `revocation_poll_secs` (default 5 s, at
  most 3600). Each poll reads every revocation since the last one it saw, page by page (any number
  at the same instant), and the last 30 s again for revocations whose transaction committed late.
  0 turns the poll off and is accepted only with `ws.enabled = false`: with the WebSocket hub on, a
  ban made by the command line would then never close open sockets, so the configuration is
  refused (at start and by `config check`). Expired tokens and old sessions are deleted hourly
  (`purge_interval_secs`), audit entries after `audit_retention_days` (365).
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

### OpenID Connect logins (the oauth module)

The `OAuth` module (feature `oauth`, name `oauth`, registered after `Auth`) logs players in with an
OpenID Connect provider's **ID token**: `POST /v1/auth/oauth/{provider}` `{"id_token","nonce"}` →
the same `AuthSession` as a password login. The provider account (`sub`) becomes a linked identity
(`provider` = your name for the provider); the first login creates an account without email or
password (a `BeforeRegister` hook can give it a display name). With the Bearer token of a recent
login (`link_reauth_secs`) the route links the provider account to the caller's account instead; a
provider account linked to another account answers 409 (two accounts are never merged), and an
account has at most one account per provider; `DELETE /v1/account/identities/{provider}` unlinks
(refused if it is the only way to log in). Without any provider configured every login answers 404.

```toml
[modules.oauth]
[modules.oauth.providers.google]
preset = "google"                                    # issuer https://accounts.google.com, Google's keys, RS256
client_ids = ["1234-abc.apps.googleusercontent.com"] # your OAuth client id(s): the token's audience

[modules.oauth.providers.studio]                     # any OpenID Connect provider
issuer = "https://login.example.com"                 # keys found through /.well-known/openid-configuration
client_ids = ["game-desktop"]
```

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::hooks::Decision;
use net_backend_server::oauth::events::BeforeOAuthLogin;
use net_backend_server::oauth::OAuth;
use net_backend_server::{AppError, Config, NetBackendServer};

#[tokio::main]
async fn main() -> Result<(), net_backend_server::Error> {
    NetBackendServer::new(Config::load()?)
        .module(Auth::new())
        .module(OAuth::new()) // providers from [modules.oauth]
        .before::<BeforeOAuthLogin, _, _>(|_ctx, login| async move {
            // The verified claims: e.g. only verified addresses of one domain for a staff provider.
            let staff = login.email_verified && login.email.as_deref().is_some_and(|e| e.ends_with("@studio.example"));
            if login.provider == "studio" && !staff {
                return Ok(Decision::Reject(AppError::forbidden("staff accounts only")));
            }
            Ok(Decision::Continue(login))
        })
        .run()
        .await
}
```

**What is checked**, in this order (any failure answers 401 `oauth_failed`; the reason is logged at
`info`): the compact form; `alg` is one of the provider's algorithms (RS256 and ES256 only; `none`,
HMAC and every other algorithm are refused, so is a `crit` header); the key with the token's `kid`
from the provider's JWKS, of the algorithm's key type (an RSA key of 2048 bits or more, or P-256);
the signature (ring); `iss` is one of the accepted issuers; `aud` contains one of `client_ids` (and
`azp`, when present or when there are several audiences, is one of them); `exp` is in the future,
`iat` is not in the future and not older than `max_token_age_secs` (600), `nbf` has passed (each
with `clock_skew_secs`, 60); there is a `sub`; the nonce in the token equals the one the login sends
(`require_nonce`, on by default). The nonce (or, without one, the whole token) is then stored per
provider until the token expires: the same sign-in is accepted once, on every instance. The
`BeforeOAuthLogin` hook sees the provider, `sub`, `email`, `email_verified` and `name` (none of them
is stored); the accounts module's hooks run as for any login (`BeforeLogin` with the method
`OpenId` and the identity).

**The provider's keys** come from `jwks_uri` (the Google preset sets it) or from the issuer's
discovery document (`/.well-known/openid-configuration`, whose `issuer` must match). They are kept
for the answer's `Cache-Control: max-age` (`jwks_cache_secs` without one, at most
`jwks_max_cache_secs`); a token with an unknown `kid` fetches them again (a provider's key
rotation), at most once per `jwks_refetch_secs` (60). When a fetch fails, the last keys stay usable
for `jwks_stale_secs` (a day); a server without usable keys answers 503 `unavailable`. Requests to
providers use HTTPS with rustls + ring and the webpki roots (`http://` only for loopback
addresses), `http_timeout_secs` (10) each, 256 KiB at most. Logins are limited per client address
(`login_per_minute`, 10).

**The desktop flow** (how a game gets the ID token; RFC 8252): the game listens on
`http://127.0.0.1:<free port>/callback`, opens the system browser at the provider's authorization
endpoint with `response_type=code`, `scope=openid`, a random `state`, a random nonce and a PKCE
challenge (S256), receives `code` + `state` on the loopback address, checks `state`, exchanges the
code with the PKCE verifier at the token endpoint and sends the answer's `id_token` with the nonce
to `/v1/auth/oauth/{provider}`. For Google, create an OAuth client of type "Desktop app" in the
Google Cloud console and put its client id into `client_ids`; Google's token endpoint also wants the
client secret it shows for that client (in a desktop program it is not confidential). Any other way
to get an ID token for one of `client_ids` works too (for example the device authorization flow).
`net_backend_client` runs the loopback flow with its feature `oauth` (`Client::sign_in_oauth`);
[API.md](https://github.com/warmar94/net_backend/blob/main/API.md#openid-connect-google-and-other-providers)
lists the steps for other clients.

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

### Permissions

Permissions are named rights finer than roles: `lobbies.manage`, `game.mute`. A module declares
the permissions it checks (`Module::permissions`), the game declares its own
(`NetBackendServer::permission`), each with the roles that hold it by default; the operator grants
permissions to roles in `[permissions]`; code checks the caller with `RequirePermission<P>` (an
extractor: 401 without a caller, 403 `forbidden` without the permission) or
`AuthContext::has_permission` / `require_permission`.

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::axum::routing::post;
use net_backend_server::permissions::{Permission, PermissionName, RequirePermission};
use net_backend_server::{Config, NetBackendServer};

// Held by `admin` and `moderator`, unless `[permissions]` lists `moderator` with other permissions.
const MUTE: Permission = Permission::new("game.mute", "Mute a player in the game's own chat").granted_to(&["moderator"]);

struct Mute;
impl PermissionName for Mute {
    const NAME: &'static str = MUTE.name();
}

async fn mute(RequirePermission(staff, ..): RequirePermission<Mute>) -> String {
    format!("muted by {}", staff.user_id)
}

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    NetBackendServer::new(config).module(Auth::new()).permission(MUTE).route("/v1/game/mute", post(mute)).run().await
}
```

```toml
[permissions]
moderator = ["game.mute", "lobbies.manage"]   # replaces the declared defaults of `moderator`
support = ["game.mute"]                       # grants to another role
```

- A caller holds a permission when one of its roles holds it; `admin` holds every declared
  permission. A role listed in `[permissions]` holds exactly the listed permissions; roles not
  listed keep the declared defaults.
- Names are dotted parts of `a-z 0-9 _` (at most 64 bytes); a module's start with its name and a
  dot. A name in `[permissions]` that nothing declares, `admin` listed there, a permission declared
  twice or a module permission without the module's prefix stop the build.
- A permission nobody declared is never held, not even by `admin`.
- Roles are read on every request, so a role granted with `user:role` or the admin routes works on
  the caller's next request.
- `Permissions` (a state value) lists the declared permissions (`declared`), the permissions of
  roles (`of_roles`) and the roles holding one (`roles_with`).
- Declared by the framework's modules: `lobbies.manage` (act as the host of every lobby; `admin` and
  `moderator` by default).

### Rate limits and lockout

On by default (`rate_limits`), in memory, per server process:

| Limit | Default |
|---|---|
| logins (password and Steam) per client address and route | 10 per minute (`login_per_minute`) |
| OpenID Connect logins per client address (the `oauth` module) | 10 per minute (`[modules.oauth] login_per_minute`) |
| registrations per client address | 10 per hour (`register_per_hour`) |
| refreshes per client address | 60 per minute (`refresh_per_minute`) |
| forgot / reset / verify / resend / password change per client address and route | 10 per minute (`email_routes_per_minute`) |
| mails per account | 3 per hour (`mails_per_account_per_hour`; a further reset request answers the same and sends nothing, a further resend answers 429) |
| failed logins per email address and client network | 5, then one more try every 3 minutes (`login_failures`, `login_lockout_secs` = 900) |
| live sessions per account | 100 (`max_sessions_per_user`); a login over it revokes the account's oldest sessions (reason `session_limit`, their sockets close with 4001) |
| failed logins per email address, from everywhere | 50 per hour (`account_failures_per_hour`); above it only networks that logged in to the account before (last 30 days) may try |

**Lockout policy:** failures count per (address, client network), so a stranger who guesses wrong
passwords locks only themselves out, not the owner logging in from elsewhere. Each attempt is counted
when it starts and given back when the password was right, so guesses sent in parallel cannot pass
the limit together. A looser per-address ceiling stops a distributed guesser; while it is exceeded,
networks the owner logged in from before still get through (remembered in memory for 30 days). A
password reset clears the per-address ceiling. Client networks are single IPv4 addresses and IPv6
/64 blocks (`rate_limit_ipv6_prefix`; the per-address route limits use the same keys). Both limits
count whether or not an account exists, so they do not reveal which addresses are registered.
Behind a reverse proxy set `http.trusted_proxies` (e.g. `["127.0.0.1"]` for Caddy on the same
machine); the client address is then taken from
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
  refused one, so the timing differs too. Login, password reset and the failed-login limits do not
  reveal whether an address has an account.
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
| `?token=<access token>` (or `?access_token=`; only with `ws.query_token = true`; default off) | the same; the server never logs query strings, but reverse proxies (Caddy, nginx) log URLs with them, so a token there lands in access logs. Prefer the header, or first-message `auth` for clients that cannot set headers (browsers) |
| first message `{"type":"auth",…}` within `ws.auth_timeout_secs` (5 s) | `auth.ok`, or `auth.failed` + close; no `auth` in time: close 1008 |

- A refused handshake never answers 400 (clients retry those forever): an invalid token is 401
  `unauthorized`, an expired one 401 `token_expired` (refresh, then reconnect), a banned account
  403 `banned`, a `before` hook's refusal its own status, an unsupported protocol version 403 when
  the request is not an upgrade (else: upgrade, then close 4010), a full hub or too many sockets
  waiting for `auth` 503 + `Retry-After`, too many handshakes or open sockets from one address 429
  + `Retry-After`, a temporary failure (database down) 503, a plain GET or an upgrade without a
  valid `Sec-WebSocket-Key` (16 bytes in base64) 426. The per-address handshake limit is checked
  before the authenticators look at the token.
- Every `auth` frame gets exactly one `auth.ok` / `auth.failed`, also on a socket the handshake
  already authenticated (a client may do both). A later `auth` with a fresh token of the same user
  re-authenticates; another user's token is refused (`auth.failed`, close 4001). `auth.failed` is
  always definitive; a TEMPORARY failure (the database is down, a hook timed out, 5xx / 429) closes
  with 1013 without an answer, so clients reconnect and try again.
- An open socket survives the expiry of its access token. Revoking its session closes it: logout,
  password change or reset, admin revocation, refresh-token reuse → 4001; a ban → 4003. Revocations
  made by another process (the command line, another instance) arrive through the `Auth` module's
  database poll (`revocation_poll_secs`, 5 s). A revocation is applied to the hub before the call
  that made it returns, also while a socket is still authenticating (after its token was checked):
  that socket is answered `auth.failed` (`banned` with the ban's `details.until` for a ban, else
  `unauthorized`) and closed with 4003 / 4001, never `auth.ok`. The hub remembers revocations for
  30 s (at most 4096); a token checked longer ago (a slow `BeforeWsConnect` hook) or before a
  revocation it had to drop is checked again before the socket is registered.
- `WsCtx.auth.roles` follow role changes: at once for changes made in this process (admin routes,
  server code), within `ws.roles_refresh_secs` (60 s) for changes made elsewhere (the command line,
  another instance).
- Requests sent before authenticating are answered `unauthorized`; pushes reach authenticated
  sockets only.
- **Logs:** tungstenite (the WebSocket protocol under the hub) logs every frame and message it
  receives at TRACE through the `log` crate, with the content. An `auth` message never reaches it:
  the server takes each one out of the socket's bytes before tungstenite reads them (tungstenite
  receives and logs a stand-in text), so no access token is in those records. A frame the server
  refuses (reserved bits, an unknown opcode, a frame out of sequence, an unmasked one) reaches
  tungstenite as its header with an empty payload, an oversized one as a header over the limit, so
  their content is not in those records either. Requests and chat text are; `tungstenite=debug` in
  `RUST_LOG` / `[log] level` leaves them out where `log` records reach the `tracing` output
  (`init_logging` forwards them when `tracing-subscriber`'s `tracing-log` feature is on in the
  build, as in its default features). The handshake's header and `?token=` / `?access_token=` are
  read through hyper and axum, which log neither.

### Close codes

| Code | Meaning | Client reconnects |
|---|---|---|
| 1000 | normal closure | yes |
| 1001 | the server is shutting down or redeploying | yes |
| 1008 | no `auth` in time, or still flooding after the rate limit refused `ws.frame_burst` frames in a row | yes |
| 1009 | a message over `ws.max_message_bytes` (16 KiB before authentication) | yes |
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
In memory that is at most `outbox_frames` waiting frames per socket plus as many held back, each up
to `ws.max_message_bytes`: a frame pushed to a room or to everyone is shared by all its sockets, a
push to one user is that socket's own.

### Limits, heartbeats, hooks

- **Caps:** `ws.max_connections` (10 000; 503 above), `ws.max_pending_connections` (1000 sockets
  waiting for `auth`; 503 above), `ws.max_connections_per_ip` (100 open sockets per address; 429),
  `ws.max_connections_per_user` (5; the oldest is closed with 4009, the same session's first),
  `ws.handshakes_per_ip_per_minute` (60; IPv6 by /64; 429 above). Behind a reverse proxy, list it in
  `http.trusted_proxies`: otherwise every client has the proxy's address and the per-address limits
  apply to everyone together (a restart would then 429 most reconnects).
- **Rate limit per socket:** `ws.frames_per_second` (20) with a burst of `ws.frame_burst` (40),
  counting text, binary, ping and pong frames; a request over it is answered `rate_limited`, and a
  socket that keeps flooding is closed with 1008.
- **Sizes:** `ws.max_message_bytes` (1 MiB, the protocol's `MAX_MESSAGE_BYTES`) in both directions.
  Until a socket authenticated, 16 KiB per frame or message (the only message then is `auth`; a
  bigger one closes the socket with 1009), so a socket waiting for `auth` holds a few KiB.
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

After a server restart (1001) a client whose access token expired meanwhile gets a 401
`token_expired` handshake, which `bevy_net_backend` treats as final (`Disconnected`) by default.
With `WsSettings::with_credentials_refresh(WsCredentialsRefresh::new().with_close_code(4001))` the
connection waits for new credentials instead: on `WsCredentialsRefused`, refresh the token
(`POST /v1/auth/refresh`) and set the new credentials, and the connection connects once more with
them. Refreshing shortly before expiry avoids the refusal.

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
| `GET /v1/users/{user}/storage/{collection}` / `.../{key}` | another player's objects the caller may read (public ones; `friends` ones for the owner's friends), the listing without values; 404 for an object the caller may not read |
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
- **Visibility:** a write may set `visibility`: `private` (the owner only; every new object without
  one), `public` (every logged-in player reads it: a public profile, a shared level) or `friends`
  (the owner's friends; needs the `Friends` module, else 422). A write without it keeps the stored
  one. Only the owner writes; `StorageService::get_visible` / `list_visible` give server code the
  same view.
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
- **Hooks** (`storage::events`): `BeforeStorageWrite` (validate, change the value or the
  `visibility` (both checked again), or refuse: e.g. keep content private until the game reviewed
  it), `InStorageWriteTx` (inside the write's transaction: write your own rows with THAT transaction,
  or refuse and roll everything back; it may run again when the transaction is retried after a
  deadlock, so it does nothing outside the database), `AfterStorageWrite`, `BeforeStorageDelete`,
  `AfterStorageDelete`; each says who writes (`Writer::Owner`, `Server`, `Admin`).
- **Server code:** `StorageService` (`Ext<StorageService>`, `state.get::<StorageService>()`):
  `get`, `list`, `get_many`, `put` (with the lock), `delete` for any user.
- **Table:** `storage_objects` (one row per object, the value as JSON bytes, the visibility,
  cascading with the account); publishable like every module's migrations (the visibility column
  comes with its own migration: an app that published the storage migrations before runs
  `migrations publish storage` again, then `migrate`; until then `serve` refuses to start and
  names the missing files).

## Chat

The `Chat` module (feature `chat`, name `chat`) runs rooms, direct messages, history, presence,
moderation, message editing, read markers, typing indicators and rooms created by players over the
WebSocket hub. Register it after `Auth`; it needs `ws.enabled`.

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
| `DELETE /v1/chat/rooms/{room}/messages/{message}` | its sender (`allow_self_delete`), a `moderator_roles` member or a holder of `chat.moderate` (audited as `chat.message_deleted`); in a player room also its owner and moderators |
| `chat.edit`, `PATCH /v1/chat/rooms/{room}/messages/{message}` | the sender within `edit_window_secs` (900; `allow_edit`; counted on the send rate) or a holder of `chat.moderate` (audited as `chat.message_edited`); `BeforeChatSend` runs again (with `edit`); push `chat.edited`; the history shows the latest text with `edited_at` |
| `chat.mark_read`, `PUT /v1/chat/rooms/{room}/read` | the caller's read marker (forward only; `read_rate` 30 per 60 s); DM, group and player rooms push `chat.read` (at most one per user, room and `read_push_interval_ms`, the newest marker wins) |
| `chat.receipts`, `GET /v1/chat/rooms/{room}/receipts` | the markers of a DM, group or player room (members) |
| `chat.unread`, `POST /v1/chat/unread` | unread counts of up to 100 rooms (capped at 1000 each) |
| `chat.set_typing` | never stored; push `chat.typing` at most once per user, room and `typing_interval_ms` (3000), with `typing_ttl_ms` (6000) for the clients; none in rooms over `typing_max_members` (50) online users |
| `/v1/chat/rooms` (POST), `…/mine`, `…/public`, `…/{room}` (GET / PATCH / DELETE), `…/{room}/join`, `…/leave`, `…/members`, `…/invites`, `…/members/{user}`, `…/members/{user}/role`, `…/owner` | rooms created by players (below); push `chat.room` |

- **Rooms:** public rooms from `[[modules.chat.rooms]]` (created or updated at start) or
  `ChatService::create_room`; group rooms (members only) from `ChatService::create_group` /
  `add_member` / `remove_member`; DM rooms from `POST /v1/chat/dm`. Server code also sends
  (`send_as`, e.g. system messages) and deletes (`delete_message`).
- **DMs are open:** anyone may open a DM with any account (it only has to exist). Blocking is the
  game's: refuse in `BeforeDirectOpen` (opening) and `BeforeChatSend` (sending). Opening is rate
  limited per user (`dm_open_rate` per `dm_open_window_secs`, a burst of 20, then one every 30 s),
  which also bounds probing which account ids exist.
- **Limits:** a member cap per room (`max_room_members` 200 unless the room has its own,
  `max_group_members` 500 for group rooms, the groups' and lobbies' rooms included; `room_full`
  409, exact also under simultaneous joins). The cap counts CONNECTIONS, while
  `RoomInfo.member_count` and `chat.members` count USERS: a room of 150 users with 200 sockets is
  full. A player has up to `ws.max_connections_per_user` (5) sockets: raise `max_group_members`
  with a larger `[modules.groups] max_members` or `[modules.lobbies] max_players`. 16 rooms per
  connection (`ws.max_rooms_per_connection`; `quota_exceeded`), `max_text_chars` (500) with the
  protocol's text rules (no control or invisible characters, something visible, no huge stacks of
  combining marks), a send rate per user as a token bucket (`rate_messages` per `rate_window_secs`:
  a burst of 5, then one every 2 s; `rate_limited` with `retry_after_ms`, about 2000 right after a
  burst), the history retention (`history_retention_days`, 30; a background purge that deletes in
  batches of 1000).
- **Presence:** a user's first connection in a room pushes `chat.presence` `joined` to the room, its
  last one `left` (leave or disconnect), with the online count; the joiner's own connections get it
  too. Rooms with more online users than `presence_max_members` (100) get none, and each room has a
  push rate (`presence_per_second`, 10): a big room never floods. `chat.members` is the full list.
- **Hooks** (`chat::events`): `BeforeChatJoin`, `BeforeChatSend` (filter / rewrite / refuse; a
  rewrite follows the text rules; also for edits, with `edit` set), `AfterChatSend` (in its own
  task: never delays the answer), `BeforeDirectOpen` (blocks, privacy), `AfterChatDelete`,
  `AfterChatEdit`, `AfterChatRead`, `BeforeChatTyping` (mutes), `BeforeRoomCreate`,
  `BeforeRoomUpdate` (names), `BeforeRoomInvite` (after the rights check: refuse), `AfterRoomChange`.
- **Rooms created by players** (`player_rooms`, on by default): a player creates a room and owns it
  (`max_rooms_per_player` 10, `room_create_rate` 5 per hour); public rooms are listed and open to
  anyone not banned, private ones take invited players only; at most `max_player_room_members`
  (100) members plus open invitations; `max_rooms_per_player` counts when a player creates a room
  or is handed one (a player who becomes the owner because the owner left is not refused); at most
  `invite_rate` invitations per player (20 per `invite_rate_window_secs`, 600; 429), and none to a
  player who blocked the inviter (with the `Friends` module: 403). Membership is stored
  (`chat_members.role`: `owner`, `moderator`, `member`, `invited`, `banned`); `chat.join` or `POST
  …/join` makes the caller a member (accepting an invitation). The owner renames, changes the
  visibility, names moderators, hands the room on and deletes it; moderators rename, invite, kick (a
  ban until invited again; the player's sockets leave the room on every instance) and delete
  messages in the room. When the owner leaves, the oldest moderator (else member) owns the room; the
  last member's room is deleted. Every change is a `chat.room` push to the members and invited
  players; an invitation is a `chat.invite` notification with the `Notifications` module (`notify`).
  The background task gives a room whose owner's account was deleted a new owner (or deletes it when
  nobody is left). Server code: `create_player_room`, `get_room`, `leave_player_room`,
  `delete_room`, `my_rooms`, `public_player_rooms`, `room_upkeep`; `delete_group_room` deletes a
  group room (the groups and lobbies modules call it when a group or lobby goes).
- **The permission `chat.moderate`** (`chat::MODERATE`, `admin` and `moderator` by default; grant it
  to other roles in `[permissions]`): edit and delete any message, act as the owner of every player
  room: open it, read its history and join it (private rooms too, also after a ban), rename it,
  invite, kick, set roles, hand it on, delete it (audited as `chat.room_deleted`). The roles in
  `moderator_roles` (`admin` and `moderator` by default) also delete any message, besides
  `[permissions]`: an operator who takes `chat.moderate` from a role there removes the role from
  `moderator_roles` too (or sets `moderator_roles = []`). A sender deletes its own messages
  (`allow_self_delete`) while it can still read the room.
- **Group members:** `remove_member` cuts a member off at once everywhere: every instance checks
  the `chat_members` table on each group send, join and history read, and the member's sockets
  leave the room on every instance (`Hub::remove_from_room` through the `Broadcaster`), also when
  the removal races with a join.
- **Several instances:** messages, deletions, edits, read and typing pushes, room changes and
  removals travel through the hub's `Broadcaster`; joined rooms, caps, presence, the send rate, the
  read-push coalescing and the typing throttle are per instance.
- **Tables:** `chat_rooms` (+ `is_public`, + `origin`: the module that created a group room),
  `chat_members` (+ `role`), `chat_messages` (a deletion clears the body and keeps the row;
  `edited_at` / `edited_by`), `chat_reads` (one marker per user and room); publishable. An app that
  published the chat migrations before runs `migrations publish chat` again, then `migrate`; until
  then `serve` refuses to start and names the missing files.

## Leaderboards

The `Leaderboards` module (feature `leaderboards`, name `leaderboards`) keeps scores on boards the
server configures and ranks them. Register it after `Auth`.

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::hooks::Decision;
use net_backend_server::leaderboards::events::{BeforeScoreSubmit, Submitter};
use net_backend_server::leaderboards::{BoardSpec, Leaderboards, LeaderboardsConfig};
use net_backend_server::protocol::leaderboards::{Period, ScoreMode, ScoreOrder};
use net_backend_server::{AppError, Config, NetBackendServer};

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    let mut boards = LeaderboardsConfig::default();
    boards.boards = vec![
        BoardSpec::new("highscore").with_name("High score"),
        BoardSpec::new("weekly-race").with_order(ScoreOrder::Asc).with_period(Period::Weekly),
        BoardSpec::new("kills").with_mode(ScoreMode::Sum).with_period(Period::Daily),
        // Ranked matches: only the server's own code submits (after it decided the result).
        BoardSpec::new("ranked").with_client_submit(false),
    ];
    NetBackendServer::new(config)
        .module(Auth::new())
        .module(Leaderboards::new().with_config(boards))
        // Anti-cheat is the game's: refuse what cannot be true.
        .before::<BeforeScoreSubmit, _, _>(|_ctx, submit| async move {
            if submit.board == "highscore" && submit.submitter == Submitter::Player && submit.score > 100_000 {
                return Ok(Decision::Reject(AppError::forbidden("that score is not possible")));
            }
            Ok(Decision::Continue(submit))
        })
        .run()
        .await
}
```

The same boards in `config.toml`:

```toml
[modules.leaderboards]
submit_rate = 30               # player submissions per user: a burst of 30, then one every 2 s
submit_rate_window_secs = 60
max_metadata_bytes = 1024      # the JSON a score may carry
keep_periods = 8               # finished daily / weekly periods kept; 0 = keep all

[[modules.leaderboards.boards]]
key = "weekly-race"
name = "Weekly race"
mode = "best"                  # best | latest | sum
order = "asc"                  # desc (higher is better) | asc (lower is better)
period = "weekly"              # all_time | daily | weekly
client_submit = true
```

| Route | What |
|---|---|
| `GET /v1/leaderboards` | every board with its mode, order, period and the current period's start and end |
| `GET /v1/leaderboards/{board}` | a page, best first (`cursor`, `limit` up to 100, `at`) |
| `POST /v1/leaderboards/{board}/scores` | submit `{"score":N, "metadata"?:JSON}` for the caller: the stored score, whether it changed, the rank |
| `GET /v1/leaderboards/{board}/me` | the caller's entry (rank, score, metadata) and how many players have a score |
| `GET /v1/leaderboards/{board}/around` | up to `above` / `below` (default 5, at most 50) entries around the caller, the caller included |

- **Modes:** `best` keeps the better score (by the board's `order`) and its metadata; `latest`
  replaces the score; `sum` adds to it (saturating at the `i64` range). Scores are any `i64` except
  `i64::MIN`. A `sum` board adds every accepted submission, a client's retry too: feed such boards
  from server code (`client_submit = false`), or let a `BeforeScoreSubmit` hook refuse repeats.
- **Periods:** `daily` and `weekly` boards start over at 00:00 UTC (weeks on Monday); every read
  takes `at` (a time inside the period to show) to read a finished period. A background task deletes
  periods older than `keep_periods` finished ones (8; `purge_interval_secs`, 3600).
- **Ranks** are 1-based and unique: equal scores rank by who reached the score first (a `latest`
  or `sum` that lands on the same score keeps its time), then by the lower account id. Ranks are
  computed when read: an indexed count of the rows before the player's.
- **Submitting:** one submission at a time per player (the account's lock, then one UPDATE or one
  INSERT: no gap locks on MySQL, no lost `sum`). Player submissions are rate-limited (`submit_rate`
  per `submit_rate_window_secs`; 429 + `retry_after_ms`; 0 = off); `client_submit = false` boards
  answer players 403 and take scores from server code only.
- **Hooks** (`leaderboards::events`): `BeforeScoreSubmit` (check, change `score` / `metadata`, or
  refuse; it says who submits: `Submitter::Player` or `Server`) and `AfterScoreSubmit` (the stored
  score, whether it changed, the rank).
- **Server code:** `LeaderboardService` (`Ext<LeaderboardService>`,
  `state.get::<LeaderboardService>()`): `submit` (for any player, on any board, never rate-limited),
  `top`, `rank`, `around`, `boards`, `remove` (delete a player's score in a period), `purge`.
- **Table:** `leaderboard_scores` (one row per board, period and player; cascading with the
  account); publishable. Boards removed from the settings keep their rows.

## Notifications

The `Notifications` module (feature `notifications`, name `notifications`) stores notifications per
player and pushes them live. Register it after `Auth`. Server code (and other modules) create them;
players read, mark and delete their own.

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::hooks::Decision;
use net_backend_server::notifications::events::BeforeNotify;
use net_backend_server::notifications::{NewNotification, NotificationService, Notifications};
use net_backend_server::protocol::UserId;
use net_backend_server::{AppError, AppState, Config, NetBackendServer};

// Anywhere in server code: a route, a hook, a module.
async fn reward(state: &AppState, player: UserId, gold: u64) -> Result<(), AppError> {
    let notifications = state.get::<NotificationService>().ok_or_else(|| AppError::unavailable("no notifications"))?;
    let reward = NewNotification::new("reward").with_text(format!("You won {gold} gold")).with_data(serde_json::json!({ "gold": gold }));
    notifications.send(state, player, reward).await?;
    Ok(())
}

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    NetBackendServer::new(config)
        .module(Auth::new())
        .module(Notifications::new())
        // Players may mute kinds: the game keeps that list and refuses here.
        .before::<BeforeNotify, _, _>(|_ctx, notify| async move {
            if notify.notification.kind == "promo" {
                return Ok(Decision::Reject(AppError::forbidden("muted")));
            }
            Ok(Decision::Continue(notify))
        })
        .run()
        .await
}
```

| Kind / route | What |
|---|---|
| push `notify.new` | a stored notification, to every open connection of its player (on every instance, through the `Broadcaster`) |
| `GET /v1/notifications`, `notify.list` | the caller's notifications, newest first (`cursor`, `limit`, `unread_only`) |
| `GET /v1/notifications/count`, `notify.count` | unread and total |
| `POST /v1/notifications/mark`, `notify.mark` | `{"ids":[…], "read":bool}` or `{"all":true, "read":bool}`: how many changed, the unread count |
| `DELETE /v1/notifications/{id}`, `notify.delete` | delete one of the caller's (also fine when it is not there) |

- **A notification** (`NewNotification`): a `kind` the game chooses (1-64 bytes of
  `a-z 0-9 _ . : -`, starting with a letter), an optional `text` (1000 characters, the chat text
  rules), optional `data` (JSON, `max_data_bytes`, 4 KiB) and an optional `sender` (an account;
  absent once that account is deleted). Invalid: 422 `validation_failed`; an unknown recipient or
  sender: 404.
- **Limits:** `max_per_user` (200): a new notification beyond it deletes the player's oldest (in
  the same transaction). `retention_days` (30): a background task deletes older ones
  (`purge_interval_secs`, 3600; 0 = keep them).
- **No ranged writes:** marking, the cap and the purge read the ids first and change rows by id (no
  MySQL gap locks); a reported deadlock is retried.
- **Hooks** (`notifications::events`): `BeforeNotify` (change or refuse) and `AfterNotify` (after it
  was stored and pushed).
- **Server code:** `NotificationService` (`Ext<NotificationService>`,
  `state.get::<NotificationService>()`): `send` (and `send_with` inside a request's hooks),
  `list`, `count`, `mark`, `delete`, `purge` for any player.
- **Without the WebSocket hub** (`ws.enabled = false`) the HTTP routes work and nothing is pushed.
- **Table:** `notifications` (cascading with the player's account); publishable.

## Friends

The `Friends` module (feature `friends`, name `friends`): friends by account. Register it after
`Auth`; with the `Notifications` module registered too, requests and acceptances are also
notifications.

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::friends::events::BeforeFriendRequest;
use net_backend_server::friends::{FriendService, Friends};
use net_backend_server::hooks::Decision;
use net_backend_server::notifications::Notifications;
use net_backend_server::protocol::UserId;
use net_backend_server::{AppError, AppState, Config, NetBackendServer};

// Server code reads the relations, e.g. to keep blocked players out of something.
async fn may_invite(state: &AppState, from: UserId, to: UserId) -> Result<bool, AppError> {
    let friends = state.get::<FriendService>().ok_or_else(|| AppError::unavailable("no friends module"))?;
    Ok(!friends.is_blocked(state, to, from).await?)
}

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    NetBackendServer::new(config)
        .module(Auth::new())
        .module(Notifications::new())
        .module(Friends::new())
        // The game's rule: account 1 is the game's bot.
        .before::<BeforeFriendRequest, _, _>(|_ctx, request| async move {
            if request.to == UserId(1) {
                return Ok(Decision::Reject(AppError::forbidden("this account takes no friends")));
            }
            Ok(Decision::Continue(request))
        })
        .run()
        .await
}
```

| Route / kind | What |
|---|---|
| `GET /v1/friends` | the caller's friends, newest first, with `online` and `last_seen` |
| `DELETE /v1/friends/{user}` | end a friendship (both sides) |
| `GET /v1/friends/requests` | open requests, `direction=received` (default) or `sent` |
| `POST /v1/friends/requests` | send one: `{"user":42}`, `{"name":"Ada"}` or `{"code":"K7M2Q9XD"}` |
| `DELETE /v1/friends/requests/{user}` | withdraw a sent request |
| `POST /v1/friends/requests/{user}/accept`, `…/decline` | answer a received request |
| `GET /v1/friends/blocks`, `PUT` / `DELETE /v1/friends/blocks/{user}` | the blocked players, block, unblock |
| `GET /v1/friends/code`, `POST /v1/friends/code` | the caller's friend code (made on first use), a new one |
| `POST /v1/friends/presence` | the heartbeat of a client without a WebSocket |
| `POST /v1/friends/steam` | which of these Steam IDs belong to accounts here: `{"steam_ids":["7656…"]}` → `{"players":[{"steam_id", "user", "name"?, "state"?}]}` |
| `GET` / `PUT /v1/friends/settings` | the caller's settings: `{"steam_findable":true}` |
| push `friends.presence` | `{"user", "online", "last_seen"?}` to the player's friends |

- **Requests:** by account id, by display name (exact after trimming; several accounts with that
  name: 409 `conflict`) or by friend code (8 characters of `2-9 A-Z` without `O` and `I`; typed in
  any case, with spaces or dashes). Asking a player who asked first makes them friends at once;
  asking again answers the same entry. A request to oneself: 422.
- **Blocks:** a block ends a friendship and every open request between the two. The blocked
  player's requests: 403 `forbidden`; the blocker's request to the blocked: 409 (unblock first).
- **Online state:** online while the player has a WebSocket connection, and for
  `online_window_secs` (90) after a heartbeat. Each instance moves the stored online time of its
  connected players forward every third of the window, so every instance answers the same.
  `friends.presence` goes to the friends when a player's first connection opens, when its last one
  closes, and when a heartbeat brings an offline player online (`presence = false` turns the pushes
  off); one player's pushes leave an instance in the order of its connects and disconnects (a
  socket that closes while its "online" is being sent is followed by "offline", never overtaken
  by it). With several instances, connections count over all of them: each instance keeps a row
  per connected player in `friend_presence`, so closing the last connection on one instance while
  another holds one changes nothing. Opening or closing a socket waits for none of this: a
  background task per instance stores the online state and sends the pushes for a batch of players
  at a time (a second connection of an online player costs no database work).
- **Limits:** `max_friends` (200), `max_pending` (50 sent and 50 received), `max_blocks` (500): 403
  `quota_exceeded`. `request_rate` (a burst of 10, then one every 6 s; 0 = no limit): 429.
  `update_rate` (heartbeats, friend-code resets and settings changes together: a burst of 30, then
  one every 2 s; `update_rate_window_secs`, 60; 0 = no limit): 429.
- **Steam IDs** (`POST /v1/friends/steam`; works when the `Auth` module has Steam login, i.e. a Steam
  verifier, else 404): a player who linked a Steam account sends Steam IDs, e.g. its Steam friends
  list, as decimal strings (individual SteamID64s, at most `steam_max_ids` = 500, else 422) and gets
  the accounts here that have one of them linked, in the request's order: the account id, the
  display name and the caller's relation (`friend`, `sent`, `received`). Never found: the caller,
  banned accounts, players with a block between them and the caller (either direction), players who
  turned `steam_findable` off (`PUT /v1/friends/settings`; findable by default). A hidden player and a
  Steam ID without an account look the same. A caller without Steam linked: 403. `steam_rate` (a
  burst of 3, then one every 5 minutes; 0 = no limit): 429. One audit entry per lookup
  (`friends.steam_match`, the counts only).
- **Writes** between two players take both account locks (the lower id first), so two players acting
  on each other at the same moment run one after the other and no limit is passed by racing
  requests; a reported deadlock is retried.
- **Notifications** (`notify`, default true; with the `Notifications` module): `friends.request` to
  the asked player, `friends.accepted` to the one who asked, the acting player as `sender`.
- **Hooks** (`friends::events`): `BeforeFriendRequest` (refuse), `AfterFriendChange` (requested,
  accepted, declined, cancelled, removed, blocked, unblocked) and `BeforeSteamMatch` (refuse a Steam
  ID lookup, or remove Steam IDs from it: the game's own check of who may be found).
- **Server code:** `FriendService` (`Ext<FriendService>`, `state.get::<FriendService>()`):
  `state_of`, `are_friends`, `is_blocked`, `friends_of`, `is_online`, and the players' own actions
  (`add`, `accept`, `decline`, `cancel`, `remove`, `block`, `unblock`, `list`, `code`, `reset_code`,
  `heartbeat`, `steam_match`, `settings`, `update_settings`, `set_steam_findable`), `steam_id_of`.
- **Without the WebSocket hub** (`ws.enabled = false`) the routes work, the online state comes from
  heartbeats, and nothing is pushed.
- **Tables:** `friend_links`, `friend_profiles`, `friends_settings`, `friend_presence` (cascading
  with the accounts) and an index on `auth_users (display_name)`; publishable.

## Groups

The `Groups` module (feature `groups`, name `groups`): groups (guilds, clans) of players. Register
it after `Auth`; with `Chat` registered every group gets a chat room, with `Notifications`
invitations and removals are notifications.

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::chat::Chat;
use net_backend_server::groups::events::BeforeGroupCreate;
use net_backend_server::groups::{GroupService, Groups};
use net_backend_server::hooks::Decision;
use net_backend_server::notifications::Notifications;
use net_backend_server::protocol::groups::GroupRole;
use net_backend_server::protocol::{GroupId, UserId};
use net_backend_server::{AppError, AppState, Config, NetBackendServer};

// Server code reads roles, e.g. for the game's own group actions.
async fn may_start_raid(state: &AppState, group: GroupId, player: UserId) -> Result<bool, AppError> {
    let groups = state.get::<GroupService>().ok_or_else(|| AppError::unavailable("no groups module"))?;
    Ok(matches!(groups.role_of(state, group, player).await?, Some(GroupRole::Owner | GroupRole::Admin)))
}

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    NetBackendServer::new(config)
        .module(Auth::new())
        .module(Chat::new())
        .module(Notifications::new())
        .module(Groups::new())
        // The game's name filter.
        .before::<BeforeGroupCreate, _, _>(|_ctx, create| async move {
            if create.request.name.to_lowercase().contains("admin") {
                return Ok(Decision::Reject(AppError::forbidden("this name is not allowed")));
            }
            Ok(Decision::Continue(create))
        })
        .run()
        .await
}
```

| Route | What |
|---|---|
| `GET /v1/groups`, `POST /v1/groups` | the groups by name (`query`: a name prefix, any case), create one (the caller owns it) |
| `GET /v1/groups/mine`, `GET /v1/groups/invites` | the caller's groups with its role; its invitations |
| `GET` / `PATCH` / `DELETE /v1/groups/{group}` | one group; change it (owner, admins); delete it (owner) |
| `GET /v1/groups/{group}/members` | the members in the order they joined |
| `POST /v1/groups/{group}/join`, `…/leave` | join an open group (or with an invitation); leave |
| `POST /v1/groups/{group}/invites`, `…/invites/accept`, `…/invites/decline`, `DELETE …/invites/{user}` | invite (owner, admins), accept, decline, withdraw (owner, admins) |
| `DELETE /v1/groups/{group}/members/{user}` | remove a member (owner: anyone; admins: members) |
| `PUT /v1/groups/{group}/members/{user}/role` | `{"role":"admin"\|"member"}` (owner) |
| `POST /v1/groups/{group}/transfer` | hand the group to a member (owner; the old owner becomes an admin) |

- **Names:** 3-32 characters without control or invisible characters, unique without regard to
  case (409 `conflict` when taken). A description (500 characters) and the game's metadata (JSON,
  `max_metadata_bytes`, 2 KiB) are optional; `open` lets anyone join.
- **Roles:** one owner, admins, members. The owner leaves only as the last member (the group is
  deleted) or after a transfer (409 `conflict` otherwise). A background task (every
  `upkeep_interval_secs`, 3600; 0 = never) gives a group whose owner's account was deleted the
  oldest admin, else the oldest member, as owner, and deletes a group with no member left.
- **Invitations** (`max_invites`, 50 open per group): listed by `GET /v1/groups/invites`, used by
  accepting or by a join; a `groups.invite` notification (with the `Notifications` module and
  `notify = true`; `data`: `{"group", "name"}`). A removed member gets `groups.kicked`. A player
  sends at most `invite_rate` invitations (a burst of 20, then one every 30 s; 429), and none to a
  player who blocked it (with the `Friends` module: 403).
- **The chat room** (with the `Chat` module and `chat_room = true`): a chat group room per group,
  in `GroupInfo::chat_room`; joining adds the player, leaving and removals take them out at once
  (on every instance), deleting the group deletes the room with its messages. The upkeep also
  deletes rooms of this module that no group names any more (older than an hour: a deletion that
  failed, a crash in between).
- **Limits:** `max_members` (100), `max_groups_per_user` (10): 403 `quota_exceeded`; members are
  counted under the group's lock (and the player's account lock), so racing joins never pass them.
  `create_rate` (a burst of 3, then one every 20 minutes; 0 = no limit): 429.
- **Hooks** (`groups::events`): `BeforeGroupCreate` and `BeforeGroupUpdate` (change or refuse),
  `BeforeGroupJoin` and `BeforeGroupInvite` (refuse), `AfterGroupChange` (created, updated,
  deleted, joined, left, kicked, role changed, transferred, invited, invitation withdrawn or
  declined). The update and invite hooks run after the rights check.
- **Server code:** `GroupService` (`Ext<GroupService>`, `state.get::<GroupService>()`): `role_of`,
  `members_of`, and the players' actions with the same rights checks (`create`, `get`, `search`,
  `mine`, `members`, `update`, `delete`, `join`, `leave`, `kick`, `set_role`, `transfer`, `invite`,
  `revoke_invite`, `decline_invite`, `invites`, `upkeep`).
- **Tables:** `game_groups`, `game_group_members`, `game_group_invites` (members and invitations
  cascading with the group and the accounts; `groups` is a reserved word on MySQL 8); publishable.

## Lobbies

The `Lobbies` module (feature `lobbies`, name `lobbies`): lobbies where players gather before a
match. The server only coordinates; the game's own connection between the players stays the
game's. Register it after `Auth`; with `Chat` every lobby gets a chat room, with `Friends` lobbies
may be friends-only and a host's blocks keep blocked players out.

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::chat::Chat;
use net_backend_server::hooks::Decision;
use net_backend_server::lobbies::events::BeforeLobbyCreate;
use net_backend_server::lobbies::{Lobbies, LobbyService};
use net_backend_server::protocol::LobbyId;
use net_backend_server::{AppError, AppState, Config, NetBackendServer};

// Server code reads a lobby, e.g. to start the match when everyone is ready.
async fn all_ready(state: &AppState, lobby: LobbyId) -> Result<bool, AppError> {
    let lobbies = state.get::<LobbyService>().ok_or_else(|| AppError::unavailable("no lobbies module"))?;
    let info = lobbies.lobby(state, lobby).await?.ok_or_else(|| AppError::not_found("no such lobby"))?;
    Ok(info.players.iter().all(|member| member.ready))
}

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    NetBackendServer::new(config)
        .module(Auth::new())
        .module(Chat::new())
        .module(Lobbies::new())
        // The game's rule: every lobby names its mode.
        .before::<BeforeLobbyCreate, _, _>(|_ctx, create| async move {
            if !create.request.metadata.contains_key("mode") {
                return Ok(Decision::Reject(AppError::forbidden("a lobby needs a mode")));
            }
            Ok(Decision::Continue(create))
        })
        .run()
        .await
}
```

| Route | What |
|---|---|
| `POST /v1/lobbies`, `GET /v1/lobbies/mine` | create a lobby (the caller hosts it); the caller's lobbies |
| `POST /v1/lobbies/search` | open lobbies by metadata filters, newest first (public ones, or those of the caller's friends) |
| `POST /v1/lobbies/join` | join with a join code (any visibility) |
| `GET` / `PATCH /v1/lobbies/{lobby}` | one lobby with its members; change its metadata, size, visibility or state (host) |
| `POST /v1/lobbies/{lobby}/join`, `…/leave` | join by id (public, or friends-only of a friend); leave |
| `PUT /v1/lobbies/{lobby}/ready` | `{"ready":bool}` |
| `POST /v1/lobbies/{lobby}/code`, `…/host` | a new join code; hand the lobby to another member (host) |
| `DELETE /v1/lobbies/{lobby}/members/{user}` | remove a member (host) |
| push `lobby.member`, `lobby.changed` | to every member: joined / left / kicked / ready; host / metadata / settings / state / code |

- **Visibility:** `public` (listed, joined by id or code), `private` (joined with the code only),
  `friends` (with the `Friends` module: joined with the code, or by id by a friend of the host).
- **State:** `open`, `in_game` (no one joins; back to `open` resets every ready flag), `closed`
  (the lobby is removed; its members get a last `lobby.changed`).
- **Join codes:** 8 characters of the friend-code alphabet (no `0`, `O`, `1`, `I`), unique among
  the lobbies, valid while the lobby exists, replaced by the host. A code is also a number below
  2^40 (`LobbyInfo::code_number`, `LobbyCode::to_u64` / `from_u64`) for platforms that carry a
  number. Join attempts are rate-limited per player (`join_rate`, a burst of 10, then one every 6 s),
  and codes that match no lobby have their own limit (`bad_code_rate`, 20 per hour; past it every
  code answers 429).
- **Members:** a leaving host passes the lobby to the member who joined first; the last member's
  leaving removes it. With `leave_on_disconnect` (on), a player whose last WebSocket connection on
  this instance closes leaves its lobbies after `disconnect_grace_secs` (30).
- **Limits:** `max_players` (64), `max_lobbies_per_user` (1), `max_metadata_keys` (32),
  `max_metadata_bytes` (4 KiB); a full lobby or a player in too many lobbies: 403
  `quota_exceeded`, counted under the lobby's lock (and the player's account lock), so racing
  joins never pass them. A change carries at most two metadata entries per allowed key (64; more:
  422 before anything else); the metadata is merged in memory and only changed keys are written.
  `create_rate` (a burst of 5, then one every 12 s) and `update_rate` (changes and new codes: a
  burst of 30, then one every 2 s): 429.
- **Host actions** (change, new code, hand over, kick): a player who is not a member gets 404 (a
  stranger never learns that a lobby id exists), a member who is not the host 403.
- **The chat room** (with the `Chat` module and `chat_room = true`): created with the lobby,
  joined and left with it, deleted when the lobby closes or is removed; the purge also deletes
  rooms of this module that no lobby names any more (older than an hour).
- **Hooks** (`lobbies::events`): `BeforeLobbyCreate` and `BeforeLobbyUpdate` (change or refuse;
  the update hook runs after the rights check), `BeforeLobbyJoin` (refuse), `AfterLobbyChange`
  (created, joined, left, kicked, ready, updated, host changed, code changed, closed).
- **Server code:** `LobbyService`: `lobby`, `members_of`, `add_player`, `leave_all`, `purge`, and
  the players' actions; host-only actions take a `LobbyActor` (`Player`: the host only;
  `Manager`, `Server`: any lobby).
- **Tables:** `lobbies`, `lobby_members`, `lobby_metadata` (members and metadata cascading with
  the lobby, members with the accounts); a purge every `purge_interval_secs` (300) removes lobbies
  left without members (a lobby a player joined meanwhile stays) and gives a lobby whose host's
  account was deleted the member who joined first as host; publishable.

## Matchmaking

The `Matchmaking` module (feature `matchmaking`, name `matchmaking`): queues where players wait to
be matched. The game decides who plays with whom through a hook; the server keeps the queues and
tells the players. Register it after `Auth`.

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::hooks::Decision;
use net_backend_server::matchmaking::events::{MatchmakingRound, ProposedMatch};
use net_backend_server::matchmaking::{Matchmaking, MatchmakingConfig, QueueSpec};
use net_backend_server::{Config, NetBackendServer};

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    let queues = MatchmakingConfig::default().with_queue(QueueSpec::new("duel", 2));
    NetBackendServer::new(config)
        .module(Auth::new())
        .module(Matchmaking::new().with_config(queues))
        // The game's rules: pairs from the same region, with the region for the players.
        .before::<MatchmakingRound, _, _>(|_ctx, mut round| async move {
            round.matches.clear();
            let region = |t: &net_backend_server::matchmaking::events::QueuedTicket| {
                t.attributes.as_ref().and_then(|a| a["region"].as_str().map(str::to_string)).unwrap_or_default()
            };
            let mut waiting = round.tickets.clone();
            while let Some(first) = waiting.first().cloned() {
                waiting.remove(0);
                if let Some(at) = waiting.iter().position(|t| region(t) == region(&first)) {
                    let second = waiting.remove(at);
                    let data = serde_json::json!({ "region": region(&first) });
                    round.matches.push(ProposedMatch::new(vec![first.ticket, second.ticket]).with_data(data));
                }
            }
            Ok(Decision::Continue(round))
        })
        .run()
        .await
}
```

| Route | What |
|---|---|
| `GET /v1/matchmaking/queues` | the queues with how many tickets wait |
| `POST` / `GET` / `DELETE /v1/matchmaking/ticket` | queue (`{"queue", "attributes"?}`, one ticket per player); the ticket (waiting, or matched with its match); cancel |
| push `match.found`, `match.expired` | the match (players, the rules' `data`); a ticket that ran out unmatched |

- **Queues:** `[[modules.matchmaking.queues]]` with `key`, `players` (per match of the default
  rule) and `timeout_secs` (120). `GET /v1/matchmaking/queues` lists them.
- **Rounds:** every `interval_ms` (1000), each queue with waiting tickets runs the
  `MatchmakingRound` hooks with the waiting tickets (oldest first, with their `attributes` and how
  long they waited) and the default proposal (first come, first matched, `players` per match);
  the hooks set the matches they want. A match naming a ticket that is gone or named twice is
  dropped; tickets in no match keep waiting. `interval_ms = 0`: rounds only when server code calls
  `MatchmakingService::run_round`.
- **Tickets:** `attributes` up to `max_attributes_bytes` (1 KiB); `BeforeTicketCreate` changes
  (e.g. adds the player's stored rating) or refuses them; `ticket_rate` (a burst of 10, then one
  every 6 s): 429. A matched ticket answers `GET /v1/matchmaking/ticket` for `matched_keep_secs`
  (60). With `cancel_on_disconnect` (on), a player whose last WebSocket connection on this
  instance closes leaves its queue.
- **Hooks** (`matchmaking::events`): `BeforeTicketCreate`, `MatchmakingRound` (the game's rules),
  `AfterMatchFound`.
- **Server code:** `MatchmakingService`: `create`, `cancel`, `ticket_of`, `queues`, `run_round`.
- **Storage:** none. Tickets live in the memory of the instance the player queued on (they last
  seconds to minutes, and a round needs every ticket of a queue in one place); a restart empties
  the queues.

## Files

The `Files` module (feature `files`, name `files`) keeps players' binary files: screenshots,
replays, mods, levels. Register it after `Auth` (and after `Friends` for the `friends` visibility).

```rust,no_run
use net_backend_server::auth::Auth;
use net_backend_server::files::events::BeforeFileUpload;
use net_backend_server::files::{Files, FilesConfig};
use net_backend_server::hooks::Decision;
use net_backend_server::{AppError, Config, NetBackendServer};

async fn start(config: Config) -> Result<(), net_backend_server::Error> {
    let mut files = FilesConfig::default();
    files.dir = "/var/lib/my-game/files".into();
    files.allowed_content_types = vec!["image/png".into(), "application/x-replay".into()];
    NetBackendServer::new(config)
        .module(Auth::new())
        .module(Files::new().with_config(files))
        // The game's rule, after the bytes arrived (size and SHA-256 known).
        .before::<BeforeFileUpload, _, _>(|_ctx, upload| async move {
            if upload.content_type == "application/x-replay" && upload.size < 64 {
                return Ok(Decision::Reject(AppError::bad_request("that is not a replay")));
            }
            Ok(Decision::Continue(upload))
        })
        .run()
        .await
}
```

| Route | What |
|---|---|
| `POST /v1/files` | upload: `multipart/form-data` with an optional `meta` part (JSON `FileMeta`: name, content type, visibility, share list, metadata, the expected SHA-256) first, then the `file` part → `FileInfo` |
| `GET /v1/files` | the caller's files, or `?owner=` another player's files the caller may read; newest first, pages |
| `GET /v1/files/usage` | the caller's files and bytes, and the limits |
| `GET / PATCH / DELETE /v1/files/{file}` | a file's settings (the owner also sees the share list); change name, visibility, share list, metadata; delete (the owner) |
| `GET /v1/files/{file}/content` | the bytes, streamed, as an attachment: `Content-Type` as stored, `Content-Length`, `ETag` = the SHA-256 (`If-None-Match` → 304), `X-Content-Type-Options: nosniff`, a `sandbox` Content-Security-Policy |

- **Uploads stream:** the file part goes to the store piece by piece while its size and SHA-256
  are counted; it is never held in memory whole. Over `max_file_bytes` (16 MiB) answers 413, over
  the player's remaining bytes 403 `quota_exceeded`, a different SHA-256 than the meta part's 422.
  The row is written under the account lock after a fresh count of the player's files and bytes,
  so the quotas (`max_files_per_user` 100, `max_bytes_per_user` 256 MiB) hold under concurrent
  uploads. Nothing stays of a refused or broken upload (a hook, a quota, a checksum, a part after
  the file, a body that ends early, a body that stops arriving). `max_file_bytes` must fit in
  `http.max_body_bytes` (32 MiB) minus 64 KiB. An upload is timed by its data: it fails when no data
  arrives for `http.upload_idle_timeout_secs` (30) or the body takes longer than
  `http.upload_timeout_secs` (3600; 0 = no overall limit), so slow connections can send large files.
- **Names and types:** the name defaults to the file part's file name without its folders (else
  `file`): 1-255 bytes, no slashes, no control or invisible characters; the content type defaults
  to the file part's `Content-Type` (else `application/octet-stream`), a plain `type/subtype`, and
  `allowed_content_types` (`image/png`, `image/*`; empty = any) limits it. Metadata: JSON up to
  `max_metadata_bytes` (4 KiB).
- **Visibility:** `private` (the owner), `public` (every logged-in player), `friends` (the owner's
  friends; needs the `Friends` module, else 422), `shared` (the accounts in `shared_with`: existing
  accounts, at most `max_shared_with`, 50). Players who may not read a file get 404 for it; readers
  who are not the owner get 403 for a change or delete. Leaving `shared` drops the list.
- **Stores:** the bytes live under a random key in a `FileStore` (put all or nothing, get, delete).
  `LocalFileStore` (the default, in `dir`) writes `dir/tmp/<key>.part`, flushes it to disk and
  renames it to `dir/<k0k1>/<k2k3>/<key>`; no client text ever reaches a path. Give your own store
  with `Files::new().store(..)`. A delete removes the row first, then the bytes.
- **Purge:** every `purge_interval_secs` (3600; 0 = never) the module deletes stored bytes that no
  file row names and that are older than an hour: the bytes of an account deleted straight from the
  database (its rows go with it) or of an upload the server stopped in. `FileService::purge_orphans`
  runs it from server code. It needs a store that lists its keys (`FileStore::keys`;
  `LocalFileStore` does, and also removes part files left by a crash). A store's folder belongs to
  one database.
- **Limits and hooks:** `upload_rate` uploads per player per `upload_rate_window_secs` (10 per 60 s;
  429). `BeforeFileUpload` (the bytes arrived: refuse with any error, the bytes are discarded),
  `BeforeFileUpdate` (`PATCH /v1/files/{file}`: change the name, the visibility or the share list,
  checked again, or refuse: a game that keeps uploads private until reviewed keeps changes private
  too), `AfterFileChange` (uploaded, updated, deleted). A change runs under the file row's lock.
  `FileService` (`Ext<FileService>`) gives server code `read_as`, `open`, `list`, `usage`, `update`,
  `delete`.
- **Tables:** `stored_files` (one row per file: owner, name, content type, size, SHA-256, store
  key, visibility, metadata; cascading with the account) and `stored_file_shares`.

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
  database); file databases use WAL mode (switched once at connect, so several processes can open
  a new file at the same time). SQLite allows one writer at a time: fine for small games; for write
  transactions that read first, start them with `BEGIN IMMEDIATE` (the pool's `begin_with`) to avoid
  busy errors. A write waits for the write lock as long as a query waits for a free connection:
  `database.acquire_timeout_secs` (5 s) sets both.
- SQLite's write mode, `database.sqlite_synchronous` (ignored for MySQL and PostgreSQL):
  `"normal"` (the default, SQLite's recommendation for WAL mode) or `"full"`. With `normal` a power
  loss or an operating-system crash can lose the last moments of writes before it, but never
  corrupts the file, and a crash of the server process alone loses nothing. With `full` every
  commit waits until the disk has it, which allows far fewer writes per second.

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
  about new migrations until you publish again (which adds only the new files), and `serve`
  refuses to start, naming the module, the missing files and `migrations publish <module>`.
- **Pending migrations:** without `database.migrate_on_start`, `serve` refuses to start while any
  migration (embedded or published) is not applied, naming each one and saying to run `migrate`
  (a database that cannot be read at start, e.g. with `connect_lazy`, is not checked).
- **Upgrading from 0.1.0:** with the embedded migrations, `migrate` (or `migrate_on_start`) adds
  the new tables and columns. A server that published the `storage` or `chat` migrations runs
  `migrations publish storage` and `migrations publish chat` (only the new files are added; edits
  stay), then `migrate`, before it serves again.

## The command line

Every server binary built with `.run()` has these commands, plus `--help` and `--version` (the
framework's version). With `NetBackendServer::run_main(|config| NetBackendServer::new(config)…)` as
the whole `main`, `--help` and `--version` work without a configuration file or a database; every
other command loads the configuration first.

| Command | What |
|---|---|
| `serve` (or no command) | run the server until SIGTERM / Ctrl-C |
| `migrate`, `migrate up` | apply pending migrations |
| `migrate status` | list every migration: `applied`, `pending`, `MODIFIED`, `missing` |
| `migrations publish <module> [--dialect mysql\|postgres\|sqlite] [--force]` | copy a module's SQL into the app |
| `config check [--connect]` | validate the configuration and print a summary without secrets (and try the database) |
| `openapi export [--output <file>]` | write or print the OpenAPI document (needs no database) |
| `asyncapi export [--output <file>]` | write or print the AsyncAPI document of the WebSocket endpoint (needs no database) |
| `healthcheck` | ask the running server's `/readyz` on `server.bind` (an unspecified address means loopback; `[::]` tries `::1`, then `127.0.0.1`): exit 0 on `200`, 1 otherwise, within 4 s in all; needs no database |
| `user:create <email> [--name <n>] [--password-file <f>] [--admin] [--verified]` | create an account (auth module); without a file a password is generated and printed once |
| `user:role <email or id> <role> [--revoke]` | grant or revoke a role |
| `user:ban <email or id> [--reason <text>] [--hours <n>]`, `user:unban <email or id>` | ban (revokes the sessions) or lift a ban |
| `sessions:revoke <email or id>` | log an account out everywhere |

```text
my-game-server migrate && my-game-server serve
my-game-server healthcheck    # a container health check: no shell or curl needed in the image
```

Modules and the app add their own commands: implement `command::AppCommand` and register it with
`.command(..)` (or return it from `Module::commands`). The names in `command::BUILT_IN_COMMANDS` are
refused; an app command named `healthcheck` replaces the built-in one (and hides it from `--help`).
The command line builds the server first (configuration, database, modules), so a command can use
every service; `--help` lists them.
Passwords are never taken from the command line itself (it is visible to other users of the
machine).

## HTTP basics

- Routes live under `/v1`. `GET /v1/info` answers the protocol's `ServerInfo`
  (`{"protocol":1,"min_protocol":1,"modules":[…]}`, module names sorted). `GET /healthz` (liveness)
  and `GET /readyz` (readiness: the database answers and the server is not shutting down) are
  unversioned. `/v1/ws` is the WebSocket hub (see [WebSocket](#websocket)); it never answers 400,
  which clients would retry forever.
- Every answer (except CORS preflights) carries `x-net-backend-protocol: 1`. A request naming an
  unsupported version in that header gets 400 `unsupported_protocol` with `{"supported_min":1,"supported_max":1}`.
- **Body limits:** 64 KiB by default (the protocol's `DEFAULT_BODY_LIMIT_BYTES`; axum's own
  default would be 2 MB), configurable; per route with
  `post(upload).layer(net_backend_server::http::body_limit(1024 * 1024))`. That limit applies to
  the body extractors (`ApiJson`, `Json`, `Bytes`, `String`); on top, `http.max_body_bytes`
  (32 MiB) caps every body, also raw body streams and raised per-route limits (413).
- **Behind a reverse proxy** its own request body limit must allow the largest route: with the
  files module an upload is up to `max_file_bytes` (16 MiB) plus 64 KiB; with the storage module a
  batch put is up to ~4.2 MB (4 MiB of values plus JSON). In Caddy a site-wide
  `request_body { max_size .. }` wins over a route's own, so set it for the API's site, e.g.
  `request_body { max_size 17MB }`.
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
- **Timeouts:** `http.request_timeout_secs` (default 30), then 503 `unavailable`. Upload routes
  (the files module's upload, or a route of your own with
  `.layer((body_limit(64 << 20), net_backend_server::http::upload_timeout()))`) are timed by their
  data instead while the body arrives: 503 when no data comes for `http.upload_idle_timeout_secs`
  (30) or the body takes longer than `http.upload_timeout_secs` (3600; 0 = no overall limit); after
  the body, the handler has `http.request_timeout_secs` for the rest.
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
  bounded memory, `RateRule::per_ip` / `per_user`) is ready to use; the `Auth` module installs its
  own.
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
  connections. One check runs at a time and its answer is reused for 1 s, so any number of requests
  costs at most one database check per second; that is why the shipped Caddy site passes it through
  (an outside monitor or load balancer can use it). A failure is logged as a WARN when the state
  changes, the recovery as INFO.
- Metrics (`metrics.enabled`, default off): Prometheus text at `GET /metrics` on **its own
  listener**, `metrics.bind` (default `127.0.0.1:9100`), never on the API port:
  `nbs_http_requests_total` and `nbs_http_request_duration_seconds` by method, route pattern and
  status; for the WebSocket hub `nbs_ws_connections` (gauge), `nbs_ws_frames_in_total`,
  `nbs_ws_frames_out_total`, `nbs_ws_closes_total` by `code` (the code sent, or `peer` / `dead` /
  `stuck`), `nbs_ws_dropped_frames_total` and `nbs_ws_handshakes_refused_total` by `reason`,
  `nbs_ws_slow_consumers_total`. Record your own metrics with the
  [`metrics`](https://crates.io/crates/metrics) facade. If your app installs its own recorder first,
  the framework records into it and does not serve `/metrics`.

## Graceful shutdown

On SIGTERM or Ctrl-C (or the future given to `serve_with_shutdown`): the server stops accepting
connections, `/readyz` answers 503, every WebSocket gets close 1001 (clients reconnect to the next
instance), in-flight requests and closing sockets get `server.shutdown_grace_secs` to finish,
then the modules shut down at the same time (within `server.module_shutdown_timeout_secs`
together), the shutdown hooks run and the database pool closes. At the deadline the remaining
connections are closed and their handlers are dropped (cancelled), so no handler runs on after the
modules and the pool are gone. Set systemd's `TimeoutStopSec` (Compose: `stop_grace_period`) above
the grace period plus the module shutdown time plus the shutdown hooks and 5 s for the pool: the
deployment files use 60 s for 20 s + 10 s + 5 s.

## Deployment

**Try it with Docker**, the same command in PowerShell, cmd, bash and zsh:

```text
docker run --rm -p 127.0.0.1:8080:8080 ghcr.io/warmar94/net_backend_server:0.2
```

The image (`linux/amd64`, `linux/arm64`) is the reference server with a configuration built in:
SQLite in `/data` (`-v nbs-data:/data` keeps it), migrations applied on start, accounts, storage,
chat with a `world` room, a `highscore` leaderboard, notifications, friends, groups, the OpenID
Connect module (no provider configured: its logins answer 404), lobbies, matchmaking with a `duel`
queue and players' files in `/data/files`. `http://127.0.0.1:8080/v1/info` answers right away. The
trial is plain HTTP for your own computer: never publish its port on a public machine; production
is the Compose install below.

**Production**: the repository's [`deploy/`](https://github.com/warmar94/net_backend/tree/main/deploy)
folder installs a server on one Linux machine (Ubuntu 24.04), with the install path chosen once:

| | Docker Compose | systemd |
|---|---|---|
| Install | one Compose file + `DOMAIN=…` in `.env` + `docker compose up -d` (with Docker Desktop, the same commands on macOS and Windows) | build the binary, run `install.sh` |
| The server | the prebuilt image: a distroless, non-root, read-only container with a health check | a service of a dedicated user with systemd's sandboxing (`NoNewPrivileges`, `ProtectSystem=strict`, …), `LimitNOFILE=262144` |
| Database | PostgreSQL 16 or MySQL 8.4 in a container; its secrets generated on the first start | MySQL, PostgreSQL or SQLite on the machine (`install.sh` creates the database and its account) |
| Migrations | a one-shot `migrate` service before the server starts | `ExecStartPre=… migrate` before every start |
| HTTPS + WSS | Caddy: automatic certificates, `request_body max_size 17MB` (a 16 MiB file upload), access logs without tokens, the server trusting only Caddy's `X-Forwarded-For` | the same |
| Backups | a daily systemd timer: `mysqldump --single-transaction`, `pg_dump -Fc` or SQLite's online backup, checked, kept 14 days; `net-backend-restore` puts one back (safety backup, migrate, readiness wait) | the same |

```text
curl -fsSLo compose.yaml https://raw.githubusercontent.com/warmar94/net_backend/main/deploy/docker/compose.postgres.yaml
echo DOMAIN=api.example.com > .env
docker compose up -d
```

(`curl.exe` in PowerShell and cmd.) Both paths install the **reference server**,
[`examples/server.rs`](https://github.com/warmar94/net_backend/blob/main/crates/net_backend_server/examples/server.rs):
the framework with every module (`Auth`, `Storage`, `Chat`, `Leaderboards`, `Notifications`,
`Friends`, `Groups`, `OAuth`, `Lobbies`, `Matchmaking`, `Files`), configured from its
configuration file. Its command line, which every server binary started with `.run()` has,
includes `healthcheck` (exit 0 when `/readyz` answers 200) for containers without a shell. Your own
server binary uses the same files: the
[Dockerfile](https://github.com/warmar94/net_backend/blob/main/deploy/docker/Dockerfile) builds its
image with three build arguments, and the Compose file runs it in place of the reference server.
The guide also covers the configuration, capacity numbers measured on a 2 vCPU machine, the limits
to raise together (open files, `ws.max_connections`, Caddy's ~120 KiB per proxied WebSocket), an
[SSH hardening guide](https://github.com/warmar94/net_backend/blob/main/deploy/ssh-hardening.md) and the
[`load_test`](https://github.com/warmar94/net_backend/tree/main/crates/load_test) tool for sizing a machine.

## How it works

- Logins: email + password and Steam; OpenID Connect ID tokens with the `oauth` module.
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
- Capacity depends on the machine: the
  [deployment guide](https://github.com/warmar94/net_backend/tree/main/deploy#capacity-limits-and-load-testing)
  lists the numbers measured on a 2 vCPU machine and the `load_test` tool measures yours.

## Compatibility

| net_backend_server | net_backend_protocol | axum | sqlx | sea-query | utoipa | Rust |
|---|---|---|---|---|---|---|
| 0.2.0 | 0.2 (≥ 0.2.0) | 0.8 (≥ 0.8.9) | 0.9 | 1.0 (≥ 1.0.2) | 6 | 1.95+ |
| 0.1.0 | 0.1.0 | 0.8 (≥ 0.8.9) | 0.9 | 1.0 (≥ 1.0.2) | 6 | 1.95+ |

## Testing

```text
cargo test -p net_backend_server --no-default-features --features sqlite,storage,chat,leaderboards,notifications,friends,groups,oauth,lobbies,matchmaking,files   # everything runs in process
NBS_TEST_MYSQL_URL=mysql://root:pw@127.0.0.1:3306/nbs \
NBS_TEST_POSTGRES_URL=postgres://postgres:pw@127.0.0.1:5432/nbs \
  cargo test -p net_backend_server --all-features -- --ignored   # the suites on MySQL / PostgreSQL
```

The database suite (migrations, the query layer) runs on SQLite in every test run and on MySQL 8.4,
MariaDB 11.4 and PostgreSQL 16 in CI; every module's suite (auth, storage, chat and its extras,
leaderboards, notifications, friends, groups, OpenID Connect against a local mock provider, files,
lobbies) runs the same way, the matchmaking suite in memory on SQLite, the WebSocket suite on a
loopback server with a WebSocket client. A routes test checks that every protocol route is served
and documented; a log-capture test finds no access token in any log record; a separate unpublished
crate drives the server with `bevy_net_backend` in a headless app. Steam and SMTP are tested against
fakes on 127.0.0.1. CI runs fmt, clippy, docs, the tests for each backend alone and all together,
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
and `cargo test -p net_backend_server --no-default-features --features sqlite,storage,chat` before
sending a change; changes to database code should also pass the MySQL / PostgreSQL suite.

Website: [net-backend.com](https://net-backend.com) · Contact: [info@net-backend.com](mailto:info@net-backend.com)
