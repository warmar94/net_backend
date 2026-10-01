# net_backend_server

<p>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

> **Status: in development.** Nothing is published yet. The core and the accounts module described
> below work and are tested; the WebSocket hub and the storage and chat modules are being built
> next (see [Roadmap](#roadmap)). APIs may still change before 0.1.0.

A Rust framework for building **game backend servers**: async (tokio + axum) and modular. It speaks
plain HTTP + WebSocket + JSON, so any client can use it. Rust clients share the message types through
[`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol); for Bevy games, the
recommended client is [`bevy_net_backend`](https://crates.io/crates/bevy_net_backend).

It is a **library you build your own server with**, not a finished server application. It gives you
solid building blocks with sensible defaults and leaves the game rules to you.

```text
Your client (Rust)                        Your server (Rust binary)
┌───────────────────────────┐             ┌───────────────────────────────┐
│ your game / app / tool    │             │ your rules / hooks / logic    │
│        │                  │             │        │                      │
│ HTTP + WebSocket client ──┼─ HTTP / WS ▶│ net_backend_server            │
│  · Bevy: bevy_net_backend │             │  core: auth, sessions, WS hub │
│    (recommended)          │             │  modules: chat, leaderboards… │
│  · other: any HTTP/WS lib │             └───────────────────────────────┘
└───────────────────────────┘                          ▲
           ▲                                           │
           └──── net_backend_protocol (shared message types) ────┘

Not Rust? Use the HTTP / WebSocket API directly (OpenAPI + WebSocket reference).
```

## Clients

The server does not care which client connects; the JSON on the wire is the contract.

| Your client is… | Use |
|---|---|
| a **Bevy** game | [`bevy_net_backend`](https://crates.io/crates/bevy_net_backend) for the connection (HTTP, WebSocket) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) for the message types. The recommended path. |
| **another Rust** app (other engine, tool, bot, CLI) | `net_backend_protocol` + the HTTP / WebSocket library you already use (for example reqwest, ureq, tokio-tungstenite). A small ready-made client, [`net_backend_client`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_client), is coming. |
| **not Rust** (C#, GDScript, JavaScript, …) | The API directly: the OpenAPI document at `/v1/openapi.json` describes every HTTP route and can generate typed clients; a WebSocket message reference comes with the WebSocket hub. |

## Contents

- [Clients](#clients)
- [What works today](#what-works-today)
- [Features](#features)
- [Install](#install)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Modules and hooks](#modules-and-hooks)
- [Accounts and authentication](#accounts-and-authentication)
- [Databases](#databases)
- [Migrations](#migrations)
- [The command line](#the-command-line)
- [HTTP basics](#http-basics)
- [Errors](#errors)
- [OpenAPI, health and metrics](#openapi-health-and-metrics)
- [Graceful shutdown](#graceful-shutdown)
- [Roadmap](#roadmap)
- [Limits](#limits)
- [Compatibility](#compatibility)
- [Testing](#testing)
- [FAQ](#faq)
- [License](#license)
- [Contributing](#contributing)

## What works today

| Part | What |
|---|---|
| App builder | `NetBackendServer::new(config)` with `.module(..)`, `.route(..)` / `.routes(..)` / `.nest(..)` / `.merge(..)`, `.state(..)`, hooks, `.run()` / `.serve(listener)`. |
| Modules | The `Module` trait: name, routes, migrations per dialect, hooks, OpenAPI parts, `start` / `shutdown`. Deterministic order (registration order). |
| Hooks | Typed `before` hooks (pass on, modify or reject) and `after` hooks per event type, start and shutdown hooks; each call has a time limit and panics are contained. |
| Configuration | A TOML file plus `NBS__SECTION__KEY` environment overrides and secrets from files; typed, validated (every problem reported at once, unknown keys and module sections refused), secrets never in `Debug`. |
| Databases | MySQL (default), PostgreSQL, SQLite through one `Db` handle; statements built once with sea-query for all three; portable column types. |
| Migrations | Plain SQL per dialect, ordered, tracked and checksummed, namespaced per module, safe against concurrent runs on every backend, precise recovery messages; modules' migrations can be **published** into the app, which then owns them. |
| HTTP | Routes under `/v1`, `GET /v1/info`, `/healthz`, `/readyz`, body limits (64 KiB default, 32 MiB hard cap), a header-read timeout, request ids, request tracing, request timeouts, panic safety, optional CORS, the protocol version header. |
| Errors | Every 4xx / 5xx is the protocol's error body; internal errors are logged with the request id and never reach a client. |
| OpenAPI | The document at `/v1/openapi.json` (utoipa), an optional browser UI. |
| Operations | Optional Prometheus metrics on their own loopback listener, a command line (`serve`, `migrate`, `migrations publish`, `config check`, `openapi export`), graceful shutdown with an enforced deadline. |
| Accounts | The `Auth` module: email + password (argon2id) and Steam logins, opaque access and rotating refresh tokens stored as hashes, sessions and revocation, email verification and password reset (log or SMTP mailer), roles, an audit log, `/v1/admin` routes, rate limits and a failed-login lockout, hooks, `user:*` commands. |
| Seams | Authenticators (`Authenticator`, the `AuthContext` / `RequireRole` extractors), rate limiters (with an in-memory `MemoryRateLimiter`), trusted-proxy client addresses, app commands, the reserved `/v1/ws` path. |

## Features

| Feature | Default | What |
|---|---|---|
| `mysql` | yes | MySQL 8 and MariaDB 11.4 (the CI database job runs MySQL 8.4 and MariaDB 11.4) — sqlx, TLS with rustls + ring. |
| `postgres` | no | PostgreSQL 16 (sqlx, TLS with rustls + ring). |
| `sqlite` | no | SQLite, compiled in (no system library needed). |
| `steam` | no | The built-in Steam ticket check (`SteamWebApiVerifier`: hyper + rustls with ring). |
| `smtp` | no | The SMTP mailer (`SmtpMailer`: lettre with rustls + ring). |

The backends are additive: any combination compiles, and the server uses the one its
`database.url` names. At least one is needed to run a server; without any, starting fails with a
clear message. No OpenSSL, no aws-lc.

## Install

Not on crates.io yet. Once published:

```toml
[dependencies]
net_backend_server = { version = "0.1.0" }                                   # MySQL
# net_backend_server = { version = "0.1.0", default-features = false, features = ["sqlite"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
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
NBS__DATABASE__URL=sqlite::memory: cargo run       # with the `sqlite` feature
curl http://127.0.0.1:8080/v1/info                 # {"protocol":1,"min_protocol":1,"modules":[]}
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
provider. Only `name` is required; methods added to the trait later always come with a default.

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
- **Hooks:** `before` hooks run in order — the game's hooks (registered on the builder) first,
  then each module's, modules in registration order — and may pass the event on, modify it or
  reject it (the first rejection answers the client); `after` hooks run after the work and only
  log their errors. A module runs its hook points with `state.hooks().run_before(&ctx, event)`.
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
  protocol (a game needs a clear sign-up answer), bounded by the registration rate limit; an
  enumeration-safe registration mode is planned for 0.2. The password is hashed either way, but a
  successful registration does more database work than a refused one, so the timing differs too.
  Login, password reset and the failed-login limits do not reveal whether an address has an
  account.
- The rate limits, the failed-login counters, the known networks and the used Steam tickets live
  in memory (bounded): they reset when the server restarts and are not shared between several
  server processes. Under heavy key churn the oldest entries are dropped first.
- Bearer tokens only (no cookies), so CSRF does not apply. Tokens never appear in logs (the
  request log has the path without the query string), in `Debug` output or in error answers.
- The email address is not proven until it is verified; `login_requires_verified_email` enforces
  it. Email addresses are compared in NFC and lower-cased (internationalised domains are not
  converted to punycode, so `bücher.de` and `xn--bcher-kva.de` count as different addresses).
- Every setting is in `[modules.auth]` (see `AuthConfig`); secrets also come as `*_file`.

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
- **Portable column types** (`db::schema`): `BIGINT` ids, `BIGINT` unix-millisecond timestamps,
  bytes for JSON and binary payloads (`LONGBLOB` / `BYTEA` / `BLOB`), `VARCHAR(n)` strings. On
  MySQL tables use `utf8mb4` with the binary collation `utf8mb4_bin`, so comparisons and unique
  indexes are case-sensitive as on PostgreSQL and SQLite (`Sword` ≠ `sword`); the one remaining
  difference is that MySQL ignores trailing spaces in comparisons — normalise values in code (trim,
  lower-case emails). No native timestamp, JSON or enum types in framework tables.
- `SUM()` over a `BIGINT` is `DECIMAL` / `NUMERIC` on MySQL and PostgreSQL: cast it
  (`CAST(SUM(x) AS SIGNED)` / `::BIGINT`) before decoding into `i64`.
- **TLS to a remote database:** sqlx's default is "prefer TLS, do not verify". Use
  `ssl-mode=VERIFY_IDENTITY` (MySQL) / `sslmode=verify-full` (PostgreSQL) in the URL for a
  database on another machine; the system's root certificates are not used (webpki roots).
- **One database, plain sqlx:** an app with one backend can use its pool directly (`db.mysql()`,
  `db.postgres()`, `db.sqlite()`), including sqlx's compile-checked `query!` macros. The framework
  itself cannot use them (three dialects in one crate); every framework query runs in CI against
  all three databases instead.
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
  `-- nbs:statement-begin` / `-- nbs:statement-end` lines mark one such block. `DELIMITER` is a
  client command and is not supported.
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
  unversioned. `/v1/ws` is reserved for the WebSocket hub and answers 403 until it exists (never
  400, which clients would retry forever).
- Every answer (except CORS preflights) carries `x-net-backend-protocol: 1`. A request naming an unsupported version in
  that header gets 400 `unsupported_protocol` with `{"supported_min":1,"supported_max":1}`.
- **Body limits:** 64 KiB by default (the protocol's `DEFAULT_BODY_LIMIT_BYTES`; axum's own
  default would be 2 MB), configurable; per route with
  `post(upload).layer(net_backend_server::http::body_limit(1024 * 1024))`. That limit applies to
  the body extractors (`ApiJson`, `Json`, `Bytes`, `String`); on top, `http.max_body_bytes`
  (32 MiB) caps every body, also raw body streams and raised per-route limits (413).
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
- **CORS** is off unless `cors.allowed_origins` is set. **Compression** is not built in (put it
  in the reverse proxy if wanted).
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
- `GET /readyz` checks the database with a 1 s limit and fails at once when the database refuses
  connections.
- Metrics (`metrics.enabled`, default off): Prometheus text at `GET /metrics` on **its own
  listener**, `metrics.bind` (default `127.0.0.1:9100`), never on the API port:
  `nbs_http_requests_total` and `nbs_http_request_duration_seconds` by method, route pattern and
  status. Record your own metrics with the [`metrics`](https://crates.io/crates/metrics) facade.
  If your app installs its own recorder first, the framework records into it and does not serve
  `/metrics`.

## Graceful shutdown

On SIGTERM or Ctrl-C (or the future given to `serve_with_shutdown`): the server stops accepting
connections, `/readyz` answers 503, in-flight requests get `server.shutdown_grace_secs` to finish,
then modules shut down in reverse order, the shutdown hooks run and the database pool closes.
At the deadline the remaining connections are closed and their handlers are dropped (cancelled),
so no handler runs on after the modules and the pool are gone. Set systemd's `TimeoutStopSec`
above the grace period plus the module shutdown time.

## Roadmap

| Step | What |
|---|---|
| core | the builder, modules, databases, migrations, HTTP, OpenAPI, command line |
| accounts (this) | the `Auth` module, rate limits, trusted proxies, app commands |
| next | the WebSocket hub (the client's envelope, rooms, bounded outboxes, close codes; tokens checked at the handshake, sockets closed on revocation) |
| then | the storage (saves) and chat modules |
| then | deployment (Docker Compose or systemd, Caddy, backups) and a measured load test |

Later versions: OAuth providers, notifications, friends, leaderboards, groups, lobbies, an
optional SeaORM layer, multi-instance pub/sub, payment connectors.

## Limits

- No WebSocket hub yet (see the roadmap). No OAuth providers yet (Steam and email only).
- Rate limits and the failed-login counters are in memory, per process.
- Revocations from other processes reach this process's subscribers by a database poll
  (`revocation_poll_secs`, 5 s), not instantly.
- No compile-checked SQL inside the framework (three dialects); the CI matrix runs every query on
  all three databases.
- One process; multi-instance deployments arrive with the pub/sub seam.
- No response compression and no TLS in the process: terminate TLS in the reverse proxy.
- Capacity numbers are not published until they are measured.

## Compatibility

| net_backend_server | net_backend_protocol | axum | sqlx | sea-query | utoipa | Rust |
|---|---|---|---|---|---|---|
| 0.1.0 (in development) | 0.1 | 0.8 (≥ 0.8.9) | 0.9 | 1.0 (≥ 1.0.2) | 6 | 1.95+ |

## Testing

```text
cargo test -p net_backend_server --no-default-features --features sqlite   # everything runs in process
NBS_TEST_MYSQL_URL=mysql://root:pw@127.0.0.1:3306/nbs \
NBS_TEST_POSTGRES_URL=postgres://postgres:pw@127.0.0.1:5432/nbs \
  cargo test -p net_backend_server --all-features -- --ignored               # the database and auth suites on MySQL / PostgreSQL
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
CI runs fmt, clippy, docs, the tests for each backend alone and all together,
the external-database job, the dependency rules and the package contents.

## FAQ

**Why not `sqlx::AnyPool`?** It supports few types, and its `?` placeholders break on PostgreSQL.
One enum over the three real pools plus sea-query keeps every type and every dialect correct.

**Can I use SeaORM / Diesel?** Your own code can use anything on top of the same database. An
optional SeaORM layer reusing the framework's pool is planned; Diesel is not supported.

**Why plain SQL migrations?** So you can read, review and change them, per database, and own a
module's tables once you publish them.

**Is it Bevy-specific?** No. The server never links Bevy; any HTTP / WebSocket client works.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

This crate lives in the [`net_backend`](https://github.com/warmar94/net_backend) repository
together with the protocol and the client. Issues and pull requests are welcome. Please run
`cargo fmt --all`, `cargo clippy -p net_backend_server --all-targets --all-features -- -D warnings`
and `cargo test -p net_backend_server --no-default-features --features sqlite` before sending a
change; changes to database code should also pass the MySQL / PostgreSQL suite.
