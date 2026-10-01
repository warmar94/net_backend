# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a key dependency bump).

## [0.1.0] - Unreleased

In development. This entry grows until the first release.

### Added

- The WebSocket hub at `/v1/ws` (module `ws`, settings `[ws]` / `WsConfig`, on by default): the
  protocol's envelope (requests, answers, pushes; malformed frames with an id answered
  `bad_request`, `unknown_type` for unregistered kinds); authentication at the handshake through
  the app's authenticators (`Authorization: Bearer`, or `?token=`) or by first-message `auth` with
  exactly one `auth.ok` / `auth.failed` per `auth` and a deadline (close 1008); refused handshakes
  never answer 400 (401 / 401 `token_expired` / 403 `banned`, upgrade + close 4010 for an
  unsupported protocol version, 503 / 429 with `Retry-After`); open sockets survive token expiry and
  close on revocation (4001) or ban (4003), also when another process revoked; request handlers by
  kind for the game (`NetBackendServer::ws`, `ws_call`, `ws_handler`) and modules
  (`Module::ws_handlers`), typed through the protocol's `WsCall` or on JSON, with `WsCtx`
  (`AuthContext`, connection) and `KindDoc` documentation; pushes to a connection / user / room /
  everyone (`Hub`, `AppState::ws`, encoded once), rooms with member and per-connection caps; global
  and per-user connection caps (4009 replaced), per-address handshake and per-socket frame rate
  limits (`rate_limited`, close 1008), bounded outboxes (close 1013) and a write deadline,
  heartbeats with dead-peer detection, a message size limit (close 1009), tuned socket buffers
  (8 KiB read buffer, no write buffering; ~15 KiB heap per idle socket measured), hooks
  (`ws::events`: `BeforeWsConnect`, `AfterWsConnect`, `AfterWsDisconnect`, `BeforeWsFrame`), close
  1001 for every socket on shutdown within the grace period, metrics (`nbs_ws_*`), the `Broadcaster`
  seam (`LocalBroadcaster`) for several instances later.
- WebSocket hub hardening (review and live-test round): request handlers run while the socket keeps writing
  pushes (only a handler's pushes to its own socket wait for its answer); pushes over `ws.max_message_bytes` are
  refused (`PushError::TooLarge`); a revocation or ban landing while a socket authenticates is applied; a temporary
  failure during first-message `auth` (5xx / 429) closes with 1013 without `auth.failed` (clients retry); open
  sockets get role changes (`AuthService::subscribe_role_changes`, `roles_of_users`, `ws.roles_refresh_secs`);
  `Target::Connection` pushes never leave the process and `Target` / `Delivery` / `ConnectionId` are serde types;
  `ws.max_connections_per_ip` (100, 429) and `ws.max_pending_connections` (1000, 503); the per-user cap closes the
  oldest socket of the same session first; an `auth` over the rate limit is still answered; unique AsyncAPI keys and
  the handshake statuses in the AsyncAPI description; `BeforeWsConnect::origin`; unsendable close codes become
  1011; `WsCtx::for_tests`; `HubStats::pending`; `ConnectionInfo::roles`; the registry is a `RwLock`;
  `request_timeout_secs < idle_timeout_secs - ping_interval_secs` is validated.
- The AsyncAPI 3.0 document of the WebSocket endpoint at `/v1/asyncapi.json` (`ASYNCAPI_PATH`,
  `PreparedServer::asyncapi_json`, command `asyncapi export`), generated from the registered kinds.
- The accounts module `Auth` (name `auth`, settings `[modules.auth]` / `AuthConfig`): the protocol's
  `/v1/auth/*` and `/v1/account*` routes (register, login, Steam login, refresh, logout, email
  verification and resend, password forgot / reset, account read / update, password change) and
  `/v1/admin/*` (list / get users, ban / unban, revoke sessions, grant / revoke roles, audit log).
  Passwords: argon2id (19 MiB, t=2, p=1 by default) on the blocking pool behind a concurrency
  limit, dummy hashing for unknown accounts, rehash on login. Tokens: opaque `nbsa_` / `nbsr_`
  tokens stored as SHA-256; 1 h / 30 days; refresh rotation with the 30-second grace window (the
  same pair, derived with HMAC-SHA-256 under a stored server key) and family revocation on later
  reuse; `token_expired` vs `unauthorized` vs `banned`. Sessions per login, revocation with
  `AuthService::subscribe_revocations` (close codes 4001 / 4003) and `authenticate_token` for the
  WebSocket hub. Case-insensitive unique emails on every database (normalised column). 24
  migrations per dialect (one statement each), publishable. Steam: the `SteamVerifier` trait,
  `FakeSteamVerifier`, `SteamWebApiVerifier` (feature `steam`). Mail: `mail::{Mailer, Mail,
  LogMailer, MemoryMailer}`, `SmtpMailer` (feature `smtp`), a bounded background queue. Roles
  (`RequireRole`, `RequireAdmin`, `AuthContext::require_role`), the audit log (`auth::audit`),
  rate limits and a failed-login lockout, hooks (`auth::events`), commands `user:create`,
  `user:role`, `user:ban`, `user:unban`, `sessions:revoke`, a periodic purge of expired rows.
- `rate_limit::{KeyedBuckets, MemoryRateLimiter, RateRule, RateScope}`: in-memory token buckets
  with bounded memory.
- `http.trusted_proxies` and the `ClientIp` extractor (the client address from `X-Forwarded-For`
  of trusted proxies only); rate limiters key on it.
- App commands: `command::{AppCommand, CommandCtx, CommandArgs}`, `NetBackendServer::command`,
  `Module::commands`; `--help` lists them.
- `Module::setup` (with `module::Setup`): modules register state values, authenticators and rate
  limiters at build time.
- Features `steam` and `smtp`.

- Security hardening of the accounts module (review round): strict `local@domain` emails (no
  display-name / angle-bracket forms) with Unicode NFC, mail sent to the bare validated address;
  NFC passwords; Steam linking needs a recent login (`link_reauth_secs`, 403
  `reauthentication_required`), one Steam account per account, the ban check and `BeforeLogin` hook
  before the link, an owner notification, unlink routes (`DELETE /v1/account/identities/{provider}`,
  `DELETE /v1/admin/users/{user}/identities/{provider}`), a password reset unlinks providers; the
  configured `steam_identity` (now required with any verifier) goes to Steam, accepted tickets are
  refused for 10 minutes, only Steam's documented success shape counts; a stale Bearer on
  `/v1/auth/steam` is refused (`MaybeAuth` extractor); the lockout counts per (address, client
  network) plus an account-wide ceiling that known networks bypass; IPv6 clients count by /64
  (`rate_limit_ipv6_prefix`); a random per-rotation nonce in the refresh derivation, forgotten after
  the grace window; failed logins do the same awaited work for known and unknown addresses (one
  query, background audit); banned accounts get 403 `banned` from tokens and refresh;
  revocations of other processes reach `subscribe_revocations` by a database poll
  (`revocation_poll_secs`; `AuthService::revocations_since`); indexes on the token tables'
  `session_id` and on session revocation / expiry (29 migrations); `audit_retention_days`; logout
  "everywhere" needs a current refresh token; a password change voids pending reset mails; last-admin
  and admin-ban guards; a warning when a local proxy's `X-Forwarded-For` is not trusted; the dummy
  hash is built at start; `AuthContext::session_started_at`, `rate_limit::{ip_key, RecentSet}`.

### Changed

- `ws.query_token` defaults to `false` (reverse proxies log URLs with their query). `Hub::publish` returns
  `Result<(), PushError>`.
- `/v1/ws` is the WebSocket hub (it answered 403 while reserved; with `ws.enabled = false` it
  still does). `AppState` holds the hub.
- Several authenticators and rate limiters may be installed (asked in order). An authenticator's
  error no longer answers the request at once: the request continues anonymously, and handlers
  that need a user (`AuthContext`) answer that error; `Option<AuthContext>` sees `None`.
- `AuthContext` has `session_id` and `roles`.

- The app builder `NetBackendServer`: `new(config)`, `module`, `route` / `routes` (documented) /
  `nest` / `merge`, `state`, `before` / `after` / `on_start` / `on_shutdown` hooks, `clock`, `db`,
  `authenticator`, `rate_limiter`, `publish_migrations`, `build` → `PreparedServer` (`router`,
  `migrate`, `migration_status`, `serve`, `serve_with_shutdown`), `run` / `run_with_args` /
  `run_with_output` (the command line).
- The `Module` trait (name, migrations per dialect, routes, OpenAPI parts, hooks, `start`,
  `shutdown`; later methods always get defaults), module name rules, deterministic registration
  order.
- Hooks: typed `before` (continue, modify, reject) and `after` hooks per `Event` type, start and
  shutdown hooks, a time limit per call (503 `hook_timeout`), contained panics.
- `AppState` (configuration, database, clock, hooks, the app's own state values, the shutdown
  signal), `Clock` with `SystemClock` and `ManualClock`, the extractors `ApiJson`, `Ext`,
  `RequestId`, `AuthContext`.
- `Config`: TOML file + `NBS__SECTION__KEY` environment overrides + `database.url_file`, strict
  keys, validation that reports every problem, `SecretString` (redacted `Debug`), per-module
  sections.
- Errors: `AppError` (the protocol's error body and status table; internal causes logged with the
  request id, never sent) and the framework `Error`.
- Databases: features `mysql` (default), `postgres`, `sqlite`, additive; the `Db` / `DbTx` enums
  over the sqlx pools with sea-query statements (`execute`, `fetch_all`, `fetch_optional`,
  `fetch_one`, `insert_id`, `execute_script`, transactions with the same methods), `check_ready`,
  `Dialect`, SQLite files switched to WAL once at connect (safe for parallel starts on a new file),
  portable column helpers (`db::schema`; MySQL tables `utf8mb4_bin`, case-sensitive),
  `DbError` with unique / foreign-key checks and `describe()`; unsigned values above `i64::MAX` are
  refused. TLS through rustls with ring.
- Migrations: plain SQL per dialect, ordered by module then version, the `nbs_migrations`
  tracking table with line-ending-independent SHA-256 checksums, safe concurrent runs (MySQL
  `GET_LOCK` per database and PostgreSQL advisory lock, both with `database.migrate_lock_timeout_secs`;
  SQLite `BEGIN IMMEDIATE`; a re-check of the tracking row everywhere), MySQL migrations run statement
  by statement with a precise recovery message on failure (every statement that ran counts once a
  DDL statement committed), `-- nbs:no-transaction`, `-- nbs:single-statement` and
  `-- nbs:statement-begin` / `-- nbs:statement-end` for MySQL `BEGIN … END` bodies, a read-only
  status (applied, pending, modified, missing), publishing a module's SQL into the app (the app copy
  then wins; an existing file, also a non-UTF-8 one, is never overwritten without `force`).
- HTTP: `GET /healthz`, `GET /readyz`, `GET /v1/info` (the protocol's `ServerInfo`), the protocol
  version header on every answer and the version check (400 `unsupported_protocol` on plain
  routes), `/v1/ws` reserved (403, never 400), 64 KiB default body limit, per-route limits and a
  32 MiB hard cap, a header-read timeout (slow clients, idle keep-alive), request ids, request
  tracing without query strings, request timeouts (503), panic safety (500 without details),
  error-body normalising with the protocol's statuses, optional CORS.
- OpenAPI at `/v1/openapi.json` (utoipa 6), an optional `/v1/docs` page (a pinned third-party
  script with Subresource Integrity and a Content-Security-Policy); optional Prometheus metrics at
  `/metrics` on their own listener (`metrics.bind`, loopback by default).
- The command line: `serve`, `migrate [up|status]`, `migrations publish <module> [--dialect]
  [--force]`, `config check [--connect]`, `openapi export [--output]`.
- Graceful shutdown on SIGTERM / Ctrl-C with an enforced grace deadline (remaining connections and
  their handlers are dropped), module start / shutdown with time limits and contained panics,
  module shutdown in reverse order, shutdown hooks, pool closing.
- The authentication seam (`Authenticator`) and the rate-limit seam (`RateLimiter`, asked before
  and after authentication).
- Configuration: module sections print only their key names in `Debug`, errors never quote
  configured values, unknown `[modules.*]` sections are refused, `config::resolve_secret` for
  `<name>` / `<name>_file` secrets.
- Dependencies use caret minimums (no exact pins), so apps can use newer compatible axum, sqlx and
  utoipa releases.
- The example `minimal` (a headless game server with one custom route).
