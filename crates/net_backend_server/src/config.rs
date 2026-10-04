//! Configuration: a TOML file plus environment overrides, typed and validated at startup.
//!
//! Sources, later ones win:
//! 1. the defaults ([`Config::default`]),
//! 2. a TOML file: the path in `NBS_CONFIG`, else `./config.toml` if it exists (optional),
//! 3. environment variables `NBS__<SECTION>__<KEY>` (e.g. `NBS__DATABASE__URL`,
//!    `NBS__SERVER__BIND`); module settings as `NBS__MODULES__<MODULE>__<KEY>`,
//! 4. secrets from files: `database.url_file` (e.g. a systemd credential or a Docker secret).
//!
//! Unknown keys are errors (a typo must not silently fall back to a default). [`Config::validate`]
//! collects every problem at once. Secrets ([`SecretString`]) never show in `Debug`.
//!
//! ```toml
//! [server]
//! bind = "127.0.0.1:8080"
//! shutdown_grace_secs = 20
//!
//! [database]
//! url = "mysql://game:secret@127.0.0.1:3306/game"
//! max_connections = 10
//!
//! [modules.chat]          # read by the module itself
//! max_message_chars = 500
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

use crate::db::Dialect;
use crate::error::Error;

/// The environment variable naming the configuration file.
pub const CONFIG_PATH_ENV: &str = "NBS_CONFIG";
/// The configuration file used when [`CONFIG_PATH_ENV`] is not set (only if it exists).
pub const DEFAULT_CONFIG_FILE: &str = "config.toml";
/// The prefix of environment overrides: `NBS__SECTION__KEY`.
pub const ENV_PREFIX: &str = "NBS__";

/// A secret string (a database URL with a password, a key): `Debug` prints `<redacted>`, there is
/// no `Display` and no `Serialize`, so it cannot end up in logs by accident.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a secret.
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret itself; never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() { "<empty>" } else { "<redacted>" })
    }
}

/// A secret is text: a number or `true` / `false` (an environment value like
/// `NBS__MODULES__AUTH__SMTP_PASSWORD=12345678` reads as a TOML number) is taken as its text. Any
/// other type is refused with a message that never shows the value.
impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SecretVisitor;

        impl<'de> serde::de::Visitor<'de> for SecretVisitor {
            type Value = SecretString;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<SecretString, E> {
                Ok(SecretString(value.to_string()))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<SecretString, E> {
                Ok(SecretString(value))
            }
            // The TOML text of the number (`env_value` keeps a number only when this gives its
            // text back unchanged).
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<SecretString, E> {
                Ok(SecretString(toml::Value::Integer(value).to_string()))
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<SecretString, E> {
                Ok(SecretString(value.to_string()))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<SecretString, E> {
                Ok(SecretString(toml::Value::Float(value).to_string()))
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<SecretString, E> {
                Ok(SecretString(value.to_string()))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, _: A) -> Result<SecretString, A::Error> {
                Err(serde::de::Error::custom("a secret must be a string, not a list"))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, _: A) -> Result<SecretString, A::Error> {
                Err(serde::de::Error::custom("a secret must be a string, not a table"))
            }
        }

        deserializer.deserialize_any(SecretVisitor)
    }
}

/// The whole configuration. Build it with [`Config::load`] (file + environment) or start from
/// [`Config::default`] and change fields.
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct Config {
    /// `[server]`: listening, shutdown, hooks.
    pub server: ServerConfig,
    /// `[database]`: the connection pool and migrations.
    pub database: DatabaseConfig,
    /// `[http]`: body limit, timeout, request ids.
    pub http: HttpConfig,
    /// `[cors]`: cross-origin requests (off by default).
    pub cors: CorsConfig,
    /// `[log]`: level and format (used by [`run`](crate::NetBackendServer::run)).
    pub log: LogConfig,
    /// `[metrics]`: the Prometheus endpoint (off by default).
    pub metrics: MetricsConfig,
    /// `[openapi]`: the API description and its optional browser UI.
    pub openapi: OpenApiConfig,
    /// `[ws]`: the WebSocket hub at `/v1/ws` (on by default).
    pub ws: WsConfig,
    /// `[modules.<name>]`: each module's own settings, read with [`Config::module_config`]. `Debug`
    /// prints only the key names (a module section may hold secrets). A section without a
    /// registered module is refused when the server is built.
    pub modules: toml::Table,
    /// `[permissions]`: role → the permissions it holds, replacing the declared defaults for that
    /// role (see [`crate::permissions`]); checked against the declared permissions at build.
    pub permissions: BTreeMap<String, Vec<String>>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let modules: BTreeMap<&str, Vec<&str>> = self
            .modules
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_table().map(|t| t.keys().map(String::as_str).collect()).unwrap_or_default()))
            .collect();
        f.debug_struct("Config")
            .field("server", &self.server)
            .field("database", &self.database)
            .field("http", &self.http)
            .field("cors", &self.cors)
            .field("log", &self.log)
            .field("metrics", &self.metrics)
            .field("openapi", &self.openapi)
            .field("ws", &self.ws)
            .field("modules (keys only)", &modules)
            .field("permissions", &self.permissions)
            .finish()
    }
}

/// `[server]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct ServerConfig {
    /// The address to listen on. Default `127.0.0.1:8080` (behind a reverse proxy such as Caddy).
    pub bind: SocketAddr,
    /// How long in-flight requests may finish after a shutdown signal, in seconds. Default 20.
    pub shutdown_grace_secs: u64,
    /// The time limit of one hook call, in milliseconds. Default 2000.
    pub hook_timeout_ms: u64,
    /// The time a client has to send a request's headers, in seconds; also closes idle keep-alive
    /// connections (against slow-header attacks). Default 15.
    pub header_read_timeout_secs: u64,
    /// The time limit of one module's `start`, in seconds (then the start fails). Default 30.
    pub module_start_timeout_secs: u64,
    /// The time limit of the modules' `shutdown`, in seconds: they shut down at the same time, so
    /// this is the time all of them get together (a module still running then is abandoned).
    /// Default 10.
    pub module_shutdown_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            shutdown_grace_secs: 20,
            hook_timeout_ms: 2000,
            header_read_timeout_secs: 15,
            module_start_timeout_secs: 30,
            module_shutdown_timeout_secs: 10,
        }
    }
}

/// `[database]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct DatabaseConfig {
    /// The connection URL: `mysql://…` (or `mariadb://…`), `postgres://…` (or `postgresql://…`),
    /// `sqlite:path/to/file.db` / `sqlite::memory:`. A secret (it may hold a password).
    pub url: SecretString,
    /// A file holding the URL instead (trailing whitespace is trimmed). Not together with `url`.
    pub url_file: Option<PathBuf>,
    /// The most pooled connections. Default 10 (an in-memory SQLite database always uses 1).
    pub max_connections: u32,
    /// Connections kept open while idle. Default 0.
    pub min_connections: u32,
    /// How long a query waits for a free connection, in seconds. Default 5. On SQLite also the
    /// busy timeout: how long a statement waits for another connection's write lock.
    pub acquire_timeout_secs: u64,
    /// SQLite only: when a commit waits for the disk (`PRAGMA synchronous`). Default
    /// [`SqliteSynchronous::Normal`]. Ignored for MySQL and PostgreSQL.
    pub sqlite_synchronous: SqliteSynchronous,
    /// Open connections only when first needed (the server starts even while the database is
    /// down; `/readyz` reports it). Default false: connect at startup and fail fast.
    pub connect_lazy: bool,
    /// Apply pending migrations when the server starts. Default false: production runs
    /// `migrate` as a deploy step (e.g. systemd `ExecStartPre`).
    pub migrate_on_start: bool,
    /// The app's migrations directory (its own and published module migrations). Default `migrations`.
    pub migrations_dir: PathBuf,
    /// How long `migrate` waits for another process's migrations lock, in seconds. Default 60.
    pub migrate_lock_timeout_secs: u64,
    /// MySQL / MariaDB / PostgreSQL: the longest a statement may run on the server's connections, in
    /// seconds; the database cancels it after that (a request that timed out does not leave its
    /// query running). PostgreSQL `statement_timeout` (every statement); MySQL `max_execution_time`
    /// (read-only `SELECT`s); MariaDB `max_statement_time` (every statement). Migrations run without
    /// it (on their own short-lived connections). Default 0: no limit. Ignored for SQLite.
    pub statement_timeout_secs: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: SecretString::default(),
            url_file: None,
            max_connections: 10,
            min_connections: 0,
            acquire_timeout_secs: 5,
            sqlite_synchronous: SqliteSynchronous::Normal,
            connect_lazy: false,
            migrate_on_start: false,
            migrations_dir: PathBuf::from("migrations"),
            migrate_lock_timeout_secs: 60,
            statement_timeout_secs: 0,
        }
    }
}

/// `database.sqlite_synchronous`: SQLite's `PRAGMA synchronous` for every pooled connection.
///
/// The server runs SQLite file databases in WAL mode. With `normal` a power loss or an operating
/// system crash can lose the last commits before it; the file is never corrupted, and a crash of
/// the server process alone loses nothing. With `full` every commit waits until the disk has it,
/// which allows far fewer writes per second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SqliteSynchronous {
    /// `PRAGMA synchronous = NORMAL` (SQLite's recommendation for WAL mode). The default.
    #[default]
    Normal,
    /// `PRAGMA synchronous = FULL`: every commit waits for the disk.
    Full,
}

impl std::str::FromStr for SqliteSynchronous {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" => Ok(Self::Normal),
            "full" => Ok(Self::Full),
            _ => Err("expected `normal` or `full`".into()),
        }
    }
}

impl DatabaseConfig {
    /// The dialect of the configured URL (`None` if empty or not recognised).
    pub fn dialect(&self) -> Option<Dialect> {
        Dialect::from_url(self.url.expose())
    }
}

/// `[http]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct HttpConfig {
    /// The request body limit of every route without its own, in bytes. Default 64 KiB (the
    /// protocol's `DEFAULT_BODY_LIMIT_BYTES`; axum's own default would be 2 MB).
    pub body_limit_bytes: usize,
    /// The time limit of one request, in seconds (then 503 `unavailable`). Default 30. Routes with
    /// [`upload_timeout`](crate::http::upload_timeout) (the files module's upload) use the upload
    /// limits below while their body arrives, and this limit for the work after it.
    pub request_timeout_secs: u64,
    /// An upload fails when no data arrives for this long, in seconds (503 `unavailable`). Default
    /// 30; 1 to 3600.
    pub upload_idle_timeout_secs: u64,
    /// The time limit of a whole upload body, in seconds (503 `unavailable`). Default 3600 (an
    /// hour); 0 = no overall limit (only the idle limit); at most 86400.
    pub upload_timeout_secs: u64,
    /// Keep a client's `x-request-id` (when short and plain) instead of always making a new one.
    /// Default false; enable it behind a proxy that sets the header.
    pub trust_request_id: bool,
    /// The hard cap of every request body, in bytes, also for handlers reading the raw body stream
    /// and for routes with a raised per-route limit. Default 32 MiB (the client's upload limit).
    pub max_body_bytes: usize,
    /// Reverse proxies whose `X-Forwarded-For` is believed (addresses or blocks: `127.0.0.1`,
    /// `10.0.0.0/8`, `::1`). The client address ([`ClientIp`](crate::http::ClientIp)) is then the
    /// right-most forwarded address that is not one of them. Default none: the connection's peer.
    /// List only proxies that append the address they see (Caddy and nginx do).
    pub trusted_proxies: Vec<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            body_limit_bytes: net_backend_protocol::routes::DEFAULT_BODY_LIMIT_BYTES,
            request_timeout_secs: 30,
            upload_idle_timeout_secs: 30,
            upload_timeout_secs: 3600,
            trust_request_id: false,
            max_body_bytes: 32 * 1024 * 1024,
            trusted_proxies: Vec::new(),
        }
    }
}

/// `[cors]`: off while `allowed_origins` is empty (game clients do not need CORS; browsers do).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct CorsConfig {
    /// Allowed origins (`https://example.com`), or `["*"]` for any. Default none (CORS off).
    pub allowed_origins: Vec<String>,
    /// How long browsers may cache a preflight answer, in seconds. Default 600.
    pub max_age_secs: u64,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self { allowed_origins: Vec::new(), max_age_secs: 600 }
    }
}

/// `[log]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct LogConfig {
    /// A `tracing` filter (`info`, `debug`, `info,sqlx=warn`). `RUST_LOG` wins when set. Default `info`.
    pub level: String,
    /// `pretty` (development) or `json` (production). Default `pretty`.
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { level: "info".into(), format: LogFormat::Pretty }
    }
}

/// The log output format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Pretty,
    /// One JSON object per line.
    Json,
}

/// `[metrics]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct MetricsConfig {
    /// Serve Prometheus metrics at `GET /metrics` on their own listener ([`bind`](Self::bind)).
    /// Default false.
    pub enabled: bool,
    /// The metrics listener's address, separate from the API. Default `127.0.0.1:9100` (loopback:
    /// only the monitoring agent on the same machine can scrape it).
    pub bind: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: false, bind: SocketAddr::from(([127, 0, 0, 1], 9100)) }
    }
}

/// `[openapi]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct OpenApiConfig {
    /// Serve the OpenAPI document at `/v1/openapi.json`. Default true.
    pub enabled: bool,
    /// Serve a browser UI at `/v1/docs`: an HTML page that loads a third-party viewer script
    /// ([`ui_script_url`](Self::ui_script_url), e.g. Scalar from a CDN) in the visitor's browser,
    /// on the API's own origin. Default false. Needs `ui_script_url` and `ui_script_integrity`.
    pub ui: bool,
    /// The viewer script, pinned to an exact version
    /// (`https://cdn.jsdelivr.net/npm/@scalar/api-reference@<version>`). Required with `ui`.
    pub ui_script_url: Option<String>,
    /// The script's Subresource Integrity hash (`sha384-…`), checked by the browser. Required with `ui`.
    pub ui_script_integrity: Option<String>,
    /// The API title in the document. Default `Game backend API`.
    pub title: String,
    /// The API version in the document. Default `1`.
    pub version: String,
}

impl Default for OpenApiConfig {
    fn default() -> Self {
        Self { enabled: true, ui: false, ui_script_url: None, ui_script_integrity: None, title: "Game backend API".into(), version: "1".into() }
    }
}

/// `[ws]`: the WebSocket hub at `/v1/ws` (see [`crate::ws`]).
///
/// The socket buffers are tuned for many idle connections: an 8 KiB read buffer, no write
/// buffering (each frame is written at once) and the protocol's 1 MiB message limit. Defaults
/// follow the protocol (`AUTH_TIMEOUT_SECS`, `MAX_MESSAGE_BYTES`, the chat room caps).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct WsConfig {
    /// Serve the hub at `/v1/ws`. Default true; when false the path answers 403 (never 400).
    pub enabled: bool,
    /// The most open sockets (authenticated or not); a handshake over it gets 503 + `Retry-After`.
    /// Default 10 000. Keep it below the process's open-file limit.
    pub max_connections: usize,
    /// The most sockets per user; a newer one closes the oldest with 4009 (replaced), the oldest of
    /// its own session first. Default 5.
    pub max_connections_per_user: usize,
    /// The most open sockets per client address (IPv6 by /64), authenticated or not; 429 above.
    /// Default 100 (players behind one NAT or at a LAN party share an address).
    pub max_connections_per_ip: usize,
    /// The most sockets still waiting for their first-message `auth` (anonymous for up to
    /// `auth_timeout_secs`); 503 above, so they never crowd out authenticated players. Default 1000.
    pub max_pending_connections: usize,
    /// How often the hub re-reads the roles of every connected user, in seconds (role changes made by
    /// another process, e.g. the command line, reach open sockets within this). Changes made in this
    /// process apply at once. Default 60, 0 = off.
    pub roles_refresh_secs: u64,
    /// WebSocket handshakes per client address (IPv6 by /64) per minute; 429 above. Default 60, 0 = off.
    pub handshakes_per_ip_per_minute: u32,
    /// Seconds an unauthenticated socket has to send `auth` (then close 1008). Default 5 (the
    /// protocol's `AUTH_TIMEOUT_SECS`).
    pub auth_timeout_secs: u64,
    /// Seconds between the server's pings. Default 20 (the client pings every 15 s itself).
    pub ping_interval_secs: u64,
    /// Seconds without any frame from the client (data, ping or pong) before the socket counts
    /// as dead and is dropped. Default 60 (must exceed `ping_interval_secs`).
    pub idle_timeout_secs: u64,
    /// The time limit of one request handler, in seconds (then 503 `unavailable`). Default 10.
    pub request_timeout_secs: u64,
    /// How long one frame may take to write before the peer counts as stuck (the socket is
    /// dropped), in seconds. Default 10.
    pub write_timeout_secs: u64,
    /// Frames waiting to be sent per socket (pushes): the largest burst the game may push to one
    /// socket at once. While a request handler runs, as many again are held back (they follow its
    /// answer). When both are full the socket is closed with 1013 (the client reconnects and
    /// resyncs). Raise it for broadcast-heavy games. Default 256. Memory: per socket up to this many
    /// waiting frames plus as many held back, each up to `max_message_bytes` (a frame pushed to a
    /// room or to everyone is shared by its sockets).
    pub outbox_frames: usize,
    /// Incoming frames per second per socket (sustained); over it a request is answered
    /// `rate_limited`, and a socket that keeps flooding is closed with 1008. Default 20.
    pub frames_per_second: u32,
    /// Incoming frames a socket may send at once (the bucket size). Default 40.
    pub frame_burst: u32,
    /// The largest message in either direction, in bytes (a bigger incoming one closes the socket
    /// with 1009). Default 1 MiB (the protocol's `MAX_MESSAGE_BYTES` and the client's default).
    /// Until a socket authenticated the limit is 16 KiB (or this, if smaller): the only message
    /// then is `auth`.
    pub max_message_bytes: usize,
    /// The socket's read buffer, in bytes. Default 8 KiB (tungstenite's own default, 128 KiB per
    /// socket, costs ~120 KiB more per idle connection).
    pub read_buffer_bytes: usize,
    /// The most rooms one socket may be in at once (`Hub::join`). Default 16 (the protocol's
    /// `DEFAULT_MAX_JOINED_ROOMS`).
    pub max_rooms_per_connection: usize,
    /// The most sockets in one room unless the room is joined with its own cap. Default 200 (the
    /// protocol's `DEFAULT_MAX_ROOM_MEMBERS`).
    pub max_room_members: usize,
    /// Accept the access token as `?token=` (or `?access_token=`) on the handshake. Default false:
    /// reverse proxies (Caddy, nginx) log URLs with their query, so a token there ends up in access
    /// logs. Clients that cannot set headers (browsers) use first-message `auth` instead.
    pub query_token: bool,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 10_000,
            max_connections_per_user: 5,
            max_connections_per_ip: 100,
            max_pending_connections: 1000,
            roles_refresh_secs: 60,
            handshakes_per_ip_per_minute: 60,
            auth_timeout_secs: net_backend_protocol::envelope::AUTH_TIMEOUT_SECS,
            ping_interval_secs: 20,
            idle_timeout_secs: 60,
            request_timeout_secs: 10,
            write_timeout_secs: 10,
            outbox_frames: 256,
            frames_per_second: 20,
            frame_burst: 40,
            max_message_bytes: net_backend_protocol::envelope::MAX_MESSAGE_BYTES,
            read_buffer_bytes: 8 * 1024,
            max_rooms_per_connection: net_backend_protocol::chat::DEFAULT_MAX_JOINED_ROOMS as usize,
            max_room_members: net_backend_protocol::chat::DEFAULT_MAX_ROOM_MEMBERS as usize,
            query_token: false,
        }
    }
}

impl WsConfig {
    fn problems(&self, problems: &mut Vec<String>) {
        let ranges: [(&str, u64, u64, u64); 7] = [
            ("ws.auth_timeout_secs", self.auth_timeout_secs, 1, 300),
            ("ws.ping_interval_secs", self.ping_interval_secs, 1, 3600),
            ("ws.idle_timeout_secs", self.idle_timeout_secs, 2, 7200),
            ("ws.request_timeout_secs", self.request_timeout_secs, 1, 3600),
            ("ws.write_timeout_secs", self.write_timeout_secs, 1, 3600),
            ("ws.frames_per_second", u64::from(self.frames_per_second), 1, 100_000),
            ("ws.frame_burst", u64::from(self.frame_burst), 1, 100_000),
        ];
        for (name, value, min, max) in ranges {
            if !(min..=max).contains(&value) {
                problems.push(format!("{name} must be between {min} and {max}"));
            }
        }
        if self.idle_timeout_secs <= self.ping_interval_secs {
            problems.push("ws.idle_timeout_secs must be greater than ws.ping_interval_secs".into());
        } else if self.request_timeout_secs >= self.idle_timeout_secs - self.ping_interval_secs {
            // A socket reads nothing while its handler runs; it must not look dead afterwards.
            problems.push("ws.request_timeout_secs must be less than ws.idle_timeout_secs - ws.ping_interval_secs".into());
        }
        if self.roles_refresh_secs > 86_400 {
            problems.push("ws.roles_refresh_secs must be at most 86400".into());
        }
        let sizes: [(&str, usize, usize, usize); 9] = [
            ("ws.max_connections", self.max_connections, 1, 10_000_000),
            ("ws.max_connections_per_user", self.max_connections_per_user, 1, 10_000),
            ("ws.max_connections_per_ip", self.max_connections_per_ip, 1, 10_000_000),
            ("ws.max_pending_connections", self.max_pending_connections, 1, 10_000_000),
            ("ws.outbox_frames", self.outbox_frames, 4, 1 << 20),
            ("ws.max_message_bytes", self.max_message_bytes, 1024, 64 * 1024 * 1024),
            ("ws.read_buffer_bytes", self.read_buffer_bytes, 1024, 1024 * 1024),
            ("ws.max_rooms_per_connection", self.max_rooms_per_connection, 1, 100_000),
            ("ws.max_room_members", self.max_room_members, 1, 10_000_000),
        ];
        for (name, value, min, max) in sizes {
            if !(min..=max).contains(&value) {
                problems.push(format!("{name} must be between {min} and {max}"));
            }
        }
    }
}

impl Config {
    /// Load from the file named by `NBS_CONFIG` (or `./config.toml` if it exists) plus the
    /// `NBS__*` environment overrides, then [`validate`](Config::validate).
    pub fn load() -> Result<Config, Error> {
        let path = match std::env::var_os(CONFIG_PATH_ENV) {
            Some(path) => Some(PathBuf::from(path)),
            None => Some(PathBuf::from(DEFAULT_CONFIG_FILE)).filter(|p| p.is_file()),
        };
        let text = match &path {
            Some(path) => Some(read_file(path)?),
            None => None,
        };
        Self::from_sources(text.as_deref(), std::env::vars())
    }

    /// Load from this file plus the `NBS__*` environment overrides, then validate.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Config, Error> {
        let text = read_file(path.as_ref())?;
        Self::from_sources(Some(&text), std::env::vars())
    }

    /// Parse TOML text (no environment), then validate.
    pub fn from_toml_str(text: &str) -> Result<Config, Error> {
        Self::from_sources(Some(text), std::iter::empty::<(String, String)>())
    }

    /// The general form: optional TOML text plus `(name, value)` environment pairs (only the
    /// `NBS__*` ones are used), then secrets from files, then validation.
    pub fn from_sources<I, K, V>(toml_text: Option<&str>, env: I) -> Result<Config, Error>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut config = match toml_text {
            // toml's messages quote the offending line; a secret on a broken line would be logged.
            Some(text) => toml::from_str::<Config>(text).map_err(|e| Error::Config(vec![redact_toml_error(&e)]))?,
            None => Config::default(),
        };
        let mut problems = Vec::new();
        // Sorted, so the error list is stable.
        let env: BTreeMap<String, String> =
            env.into_iter().filter_map(|(k, v)| k.as_ref().strip_prefix(ENV_PREFIX).map(|k| (k.to_string(), v.as_ref().to_string()))).collect();
        for (key, value) in &env {
            if let Err(problem) = config.apply_env(key, value) {
                problems.push(format!("{ENV_PREFIX}{key}: {problem}"));
            }
        }
        if !problems.is_empty() {
            return Err(Error::Config(problems));
        }
        config.resolve_secret_files()?;
        config.validate()?;
        Ok(config)
    }

    fn resolve_secret_files(&mut self) -> Result<(), Error> {
        if let Some(path) = &self.database.url_file {
            if !self.database.url.is_empty() {
                return Err(Error::Config(vec!["database.url and database.url_file are both set; use one".into()]));
            }
            let text = read_file(path)?;
            self.database.url = SecretString::new(text.trim_end());
            self.database.url_file = None;
        }
        Ok(())
    }

    /// One `NBS__…` override (the key without the prefix).
    fn apply_env(&mut self, key: &str, value: &str) -> Result<(), String> {
        let lower = key.to_ascii_lowercase();
        let path: Vec<&str> = lower.split("__").collect();
        match path.as_slice() {
            ["server", "bind"] => self.server.bind = parse(value)?,
            ["server", "shutdown_grace_secs"] => self.server.shutdown_grace_secs = parse(value)?,
            ["server", "hook_timeout_ms"] => self.server.hook_timeout_ms = parse(value)?,
            ["server", "header_read_timeout_secs"] => self.server.header_read_timeout_secs = parse(value)?,
            ["server", "module_start_timeout_secs"] => self.server.module_start_timeout_secs = parse(value)?,
            ["server", "module_shutdown_timeout_secs"] => self.server.module_shutdown_timeout_secs = parse(value)?,
            ["database", "url"] => self.database.url = SecretString::new(value),
            ["database", "url_file"] => self.database.url_file = Some(PathBuf::from(value)),
            ["database", "max_connections"] => self.database.max_connections = parse(value)?,
            ["database", "min_connections"] => self.database.min_connections = parse(value)?,
            ["database", "acquire_timeout_secs"] => self.database.acquire_timeout_secs = parse(value)?,
            ["database", "sqlite_synchronous"] => self.database.sqlite_synchronous = value.parse()?,
            ["database", "connect_lazy"] => self.database.connect_lazy = parse(value)?,
            ["database", "migrate_on_start"] => self.database.migrate_on_start = parse(value)?,
            ["database", "migrations_dir"] => self.database.migrations_dir = PathBuf::from(value),
            ["database", "migrate_lock_timeout_secs"] => self.database.migrate_lock_timeout_secs = parse(value)?,
            ["database", "statement_timeout_secs"] => self.database.statement_timeout_secs = parse(value)?,
            ["http", "body_limit_bytes"] => self.http.body_limit_bytes = parse(value)?,
            ["http", "request_timeout_secs"] => self.http.request_timeout_secs = parse(value)?,
            ["http", "upload_idle_timeout_secs"] => self.http.upload_idle_timeout_secs = parse(value)?,
            ["http", "upload_timeout_secs"] => self.http.upload_timeout_secs = parse(value)?,
            ["http", "trust_request_id"] => self.http.trust_request_id = parse(value)?,
            ["http", "max_body_bytes"] => self.http.max_body_bytes = parse(value)?,
            ["http", "trusted_proxies"] => self.http.trusted_proxies = value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
            ["cors", "allowed_origins"] => self.cors.allowed_origins = value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
            ["cors", "max_age_secs"] => self.cors.max_age_secs = parse(value)?,
            ["log", "level"] => self.log.level = value.to_string(),
            ["log", "format"] => {
                self.log.format = match value.to_ascii_lowercase().as_str() {
                    "pretty" => LogFormat::Pretty,
                    "json" => LogFormat::Json,
                    _ => return Err("expected `pretty` or `json`".into()),
                }
            }
            ["metrics", "enabled"] => self.metrics.enabled = parse(value)?,
            ["metrics", "bind"] => self.metrics.bind = parse(value)?,
            ["openapi", "enabled"] => self.openapi.enabled = parse(value)?,
            ["openapi", "ui"] => self.openapi.ui = parse(value)?,
            ["openapi", "ui_script_url"] => self.openapi.ui_script_url = Some(value.to_string()),
            ["openapi", "ui_script_integrity"] => self.openapi.ui_script_integrity = Some(value.to_string()),
            ["openapi", "title"] => self.openapi.title = value.to_string(),
            ["openapi", "version"] => self.openapi.version = value.to_string(),
            ["ws", "enabled"] => self.ws.enabled = parse(value)?,
            ["ws", "max_connections"] => self.ws.max_connections = parse(value)?,
            ["ws", "max_connections_per_user"] => self.ws.max_connections_per_user = parse(value)?,
            ["ws", "max_connections_per_ip"] => self.ws.max_connections_per_ip = parse(value)?,
            ["ws", "max_pending_connections"] => self.ws.max_pending_connections = parse(value)?,
            ["ws", "roles_refresh_secs"] => self.ws.roles_refresh_secs = parse(value)?,
            ["ws", "handshakes_per_ip_per_minute"] => self.ws.handshakes_per_ip_per_minute = parse(value)?,
            ["ws", "auth_timeout_secs"] => self.ws.auth_timeout_secs = parse(value)?,
            ["ws", "ping_interval_secs"] => self.ws.ping_interval_secs = parse(value)?,
            ["ws", "idle_timeout_secs"] => self.ws.idle_timeout_secs = parse(value)?,
            ["ws", "request_timeout_secs"] => self.ws.request_timeout_secs = parse(value)?,
            ["ws", "write_timeout_secs"] => self.ws.write_timeout_secs = parse(value)?,
            ["ws", "outbox_frames"] => self.ws.outbox_frames = parse(value)?,
            ["ws", "frames_per_second"] => self.ws.frames_per_second = parse(value)?,
            ["ws", "frame_burst"] => self.ws.frame_burst = parse(value)?,
            ["ws", "max_message_bytes"] => self.ws.max_message_bytes = parse(value)?,
            ["ws", "read_buffer_bytes"] => self.ws.read_buffer_bytes = parse(value)?,
            ["ws", "max_rooms_per_connection"] => self.ws.max_rooms_per_connection = parse(value)?,
            ["ws", "max_room_members"] => self.ws.max_room_members = parse(value)?,
            ["ws", "query_token"] => self.ws.query_token = parse(value)?,
            ["modules", module, rest @ ..] if !module.is_empty() && !rest.is_empty() && rest.iter().all(|s| !s.is_empty()) => {
                let mut table = match self.modules.remove(*module) {
                    Some(toml::Value::Table(table)) => table,
                    None => toml::Table::new(),
                    Some(_) => return Err(format!("modules.{module} is not a table")),
                };
                insert_module_value(&mut table, rest, env_value(value))?;
                self.modules.insert((*module).to_string(), toml::Value::Table(table));
            }
            _ => return Err("unknown setting".into()),
        }
        Ok(())
    }

    /// Check every setting; all problems are reported together.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        let db = &self.database;
        if Dialect::enabled().is_empty() {
            problems.push("no database backend is compiled in: enable one of the features `mysql`, `postgres`, `sqlite`".into());
        }
        if db.url.is_empty() {
            problems.push("database.url (or database.url_file) is required".into());
        } else {
            match db.dialect() {
                None => problems.push("database.url: not a mysql://, mariadb://, postgres://, postgresql:// or sqlite: URL".into()),
                Some(dialect) if !dialect.is_enabled() => problems.push(format!(
                    "database.url is a {} URL, but the `{}` feature of net_backend_server is not enabled",
                    dialect.display_name(),
                    dialect.name()
                )),
                Some(_) => {}
            }
        }
        if !(1..=10_000).contains(&db.max_connections) {
            problems.push("database.max_connections must be between 1 and 10000".into());
        }
        if db.min_connections > db.max_connections {
            problems.push("database.min_connections must not exceed database.max_connections".into());
        }
        if !(1..=600).contains(&db.acquire_timeout_secs) {
            problems.push("database.acquire_timeout_secs must be between 1 and 600".into());
        }
        if db.migrations_dir.as_os_str().is_empty() {
            problems.push("database.migrations_dir must not be empty".into());
        }
        if self.server.shutdown_grace_secs > 3600 {
            problems.push("server.shutdown_grace_secs must be at most 3600".into());
        }
        if !(1..=600_000).contains(&self.server.hook_timeout_ms) {
            problems.push("server.hook_timeout_ms must be between 1 and 600000".into());
        }
        if !(1..=3600).contains(&self.server.header_read_timeout_secs) {
            problems.push("server.header_read_timeout_secs must be between 1 and 3600".into());
        }
        if !(1..=3600).contains(&self.server.module_start_timeout_secs) || !(1..=3600).contains(&self.server.module_shutdown_timeout_secs) {
            problems.push("server.module_start_timeout_secs and server.module_shutdown_timeout_secs must be between 1 and 3600".into());
        }
        if !(1..=3600).contains(&self.database.migrate_lock_timeout_secs) {
            problems.push("database.migrate_lock_timeout_secs must be between 1 and 3600".into());
        }
        if self.database.statement_timeout_secs > 86_400 {
            problems.push("database.statement_timeout_secs must be at most 86400 (0 = no limit)".into());
        }
        if !(1024..=64 * 1024 * 1024).contains(&self.http.body_limit_bytes) {
            problems.push("http.body_limit_bytes must be between 1024 and 67108864 (raise it per route for uploads)".into());
        }
        if !(1024..=1024 * 1024 * 1024).contains(&self.http.max_body_bytes) || self.http.max_body_bytes < self.http.body_limit_bytes {
            problems.push("http.max_body_bytes must be between 1024 and 1073741824 and not below http.body_limit_bytes".into());
        }
        for proxy in &self.http.trusted_proxies {
            if crate::http::client_ip::IpNet::parse(proxy).is_none() {
                problems.push(format!("http.trusted_proxies: `{proxy}` is not an address or address block like 10.0.0.0/8"));
            }
        }
        if self.metrics.enabled && self.metrics.bind == self.server.bind {
            problems.push("metrics.bind must differ from server.bind (metrics have their own listener)".into());
        }
        if self.openapi.ui {
            let url_ok =
                self.openapi.ui_script_url.as_deref().is_some_and(|u| u.starts_with("https://") && u.bytes().all(|b| b.is_ascii_graphic()) && !u.contains('"'));
            let sri_ok = self.openapi.ui_script_integrity.as_deref().is_some_and(|i| {
                (i.starts_with("sha256-") || i.starts_with("sha384-") || i.starts_with("sha512-"))
                    && i.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'/' | b'='))
            });
            if !url_ok || !sri_ok {
                problems.push("openapi.ui needs openapi.ui_script_url (an https:// URL pinned to an exact version) and openapi.ui_script_integrity (sha256-/sha384-/sha512-…)".into());
            }
        }
        if !(1..=3600).contains(&self.http.request_timeout_secs) {
            problems.push("http.request_timeout_secs must be between 1 and 3600".into());
        }
        if !(1..=3600).contains(&self.http.upload_idle_timeout_secs) {
            problems.push("http.upload_idle_timeout_secs must be between 1 and 3600".into());
        }
        if self.http.upload_timeout_secs > 86_400 {
            problems.push("http.upload_timeout_secs must be at most 86400 (0 = no overall limit)".into());
        }
        let origins = &self.cors.allowed_origins;
        if origins.iter().any(|o| o == "*") && origins.len() > 1 {
            problems.push("cors.allowed_origins: `*` must be the only entry".into());
        }
        for origin in origins.iter().filter(|o| *o != "*") {
            let plain = origin.bytes().all(|b| b.is_ascii_graphic()) && !origin.ends_with('/');
            if !(origin.starts_with("https://") || origin.starts_with("http://")) || !plain {
                problems.push(format!("cors.allowed_origins: `{origin}` is not an origin like https://example.com"));
            }
        }
        if tracing_subscriber::EnvFilter::try_new(&self.log.level).is_err() {
            problems.push(format!("log.level: `{}` is not a valid filter", self.log.level));
        }
        if self.openapi.title.trim().is_empty() || self.openapi.version.trim().is_empty() {
            problems.push("openapi.title and openapi.version must not be empty".into());
        }
        self.ws.problems(&mut problems);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }

    /// A module's settings (`[modules.<name>]`) decoded as `T`; `None` if the section is absent.
    /// Module config structs should use `#[serde(deny_unknown_fields)]` (typos become errors) and
    /// [`SecretString`] for secrets (with a `<name>_file` alternative, see [`resolve_secret`]).
    /// The values serde quotes in its error messages (strings, numbers, booleans, enum variants)
    /// are replaced by `<hidden>`; a module's own `Deserialize` code should not put values in its
    /// messages either.
    pub fn module_config<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, Error> {
        match self.modules.get(name) {
            None => Ok(None),
            Some(value) => value.clone().try_into().map(Some).map_err(|e| Error::Config(vec![format!("modules.{name}: {}", mask_quoted(e.message()))])),
        }
    }

    /// The `[modules.*]` sections that no registered module reads (most likely typos).
    pub fn unknown_module_sections(&self, registered: &[&str]) -> Vec<String> {
        self.modules.keys().filter(|name| !registered.contains(&name.as_str())).cloned().collect()
    }
}

/// A secret given either inline or as a file (`<name>` / `<name>_file`, e.g. `smtp_password` /
/// `smtp_password_file`): the file wins only when the inline value is absent; both set is an
/// error. File contents are trimmed at the end (a trailing newline is not part of the secret).
pub fn resolve_secret(name: &str, value: Option<SecretString>, file: Option<&Path>) -> Result<Option<SecretString>, Error> {
    match (value, file) {
        (Some(_), Some(_)) => Err(Error::Config(vec![format!("{name} and {name}_file are both set; use one")])),
        (Some(value), None) => Ok(Some(value)),
        (None, Some(path)) => Ok(Some(SecretString::new(read_file(path)?.trim_end()))),
        (None, None) => Ok(None),
    }
}

/// Replace every value serde quotes in a message: `"…"` / `'…'` (strings), and `` `…` `` after
/// `integer`, `floating point`, `boolean`, `character` or `variant` (numbers, booleans, enum
/// names). Backtick-quoted field names and expected values stay.
fn mask_quoted(message: &str) -> String {
    const VALUE_KINDS: [&str; 5] = ["integer ", "floating point ", "boolean ", "character ", "variant "];
    let mut out = String::with_capacity(message.len());
    let mut quote: Option<char> = None;
    for c in message.chars() {
        match quote {
            Some(q) if c == q => {
                out.push_str("<hidden>");
                out.push(c);
                quote = None;
            }
            Some(_) => {}
            None => {
                let value = c == '"' || c == '\'' || (c == '`' && VALUE_KINDS.iter().any(|kind| out.ends_with(kind)));
                out.push(c);
                if value {
                    quote = Some(c);
                }
            }
        }
    }
    if quote.is_some() {
        out.push_str("<hidden>");
    }
    out
}

fn read_file(path: &Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|e| Error::io(format!("reading {}", path.display()), e))
}

fn parse<T: std::str::FromStr>(value: &str) -> Result<T, String>
where
    T::Err: fmt::Display,
{
    value.trim().parse::<T>().map_err(|e| format!("invalid value: {e}"))
}

/// The message of a TOML error without the quoted source line.
fn redact_toml_error(error: &toml::de::Error) -> String {
    match error.span() {
        Some(span) => format!("config file: {} (at byte {})", error.message(), span.start),
        None => format!("config file: {}", error.message()),
    }
}

/// An environment value for a module setting: a TOML number or boolean when it reads back as the
/// same text (`42`, `1.5`, `true`), an array or inline table (`[1, 2]`), a string in TOML quotes
/// (`"0042"`) as that string; anything else is the plain string (`1e5`, `0042`, `+5`, dates,
/// passwords). A [`SecretString`] takes a number or boolean as its text, so a secret given here is
/// always exactly the variable's text.
fn env_value(value: &str) -> toml::Value {
    let plain = || toml::Value::String(value.to_string());
    let doc = format!("v = {value}");
    let Some(parsed) = toml::from_str::<toml::Table>(&doc).ok().and_then(|mut table| table.remove("v")) else { return plain() };
    match parsed {
        toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_) if parsed.to_string() == value => parsed,
        toml::Value::String(_) | toml::Value::Array(_) | toml::Value::Table(_) => parsed,
        _ => plain(),
    }
}

fn insert_module_value(table: &mut toml::Table, path: &[&str], value: toml::Value) -> Result<(), String> {
    match path {
        [] => Err("empty key".into()),
        [last] => {
            table.insert((*last).to_string(), value);
            Ok(())
        }
        [first, rest @ ..] => {
            let entry = table.entry((*first).to_string()).or_insert_with(|| toml::Value::Table(toml::Table::new()));
            match entry {
                toml::Value::Table(inner) => insert_module_value(inner, rest, value),
                _ => Err(format!("`{first}` is not a table")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url_for_enabled() -> &'static str {
        match Dialect::enabled().first() {
            Some(Dialect::MySql) => "mysql://u:p@127.0.0.1/db",
            Some(Dialect::Postgres) => "postgres://u:p@127.0.0.1/db",
            _ => "sqlite::memory:",
        }
    }

    fn problems(result: Result<Config, Error>) -> Vec<String> {
        match result {
            Err(Error::Config(problems)) => problems,
            Err(other) => vec![format!("other error: {other}")],
            Ok(_) => Vec::new(),
        }
    }

    #[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
    #[test]
    fn defaults_need_only_a_url() {
        let env = [("NBS__DATABASE__URL", url_for_enabled())];
        let config = Config::from_sources(None, env);
        assert!(config.is_ok(), "{:?}", problems(config));
        let problems = problems(Config::from_sources(None, std::iter::empty::<(&str, &str)>()));
        assert!(problems.iter().any(|p| p.contains("database.url")), "{problems:?}");
    }

    #[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
    #[test]
    fn file_then_env_override() {
        let toml = format!("[server]\nbind = \"0.0.0.0:9000\"\n[database]\nurl = \"{}\"\nmax_connections = 4\n", url_for_enabled());
        let env = [("NBS__DATABASE__MAX_CONNECTIONS", "7"), ("NBS__HTTP__REQUEST_TIMEOUT_SECS", "12"), ("NBS__HTTP__UPLOAD_TIMEOUT_SECS", "0"), ("OTHER", "x")];
        let config = Config::from_sources(Some(&toml), env).unwrap();
        assert_eq!(config.server.bind.port(), 9000);
        assert_eq!(config.database.max_connections, 7);
        assert_eq!(config.http.request_timeout_secs, 12);
        assert_eq!((config.http.upload_idle_timeout_secs, config.http.upload_timeout_secs), (30, 0));
        assert_eq!(config.http.body_limit_bytes, 64 * 1024);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_reported_together() {
        let toml = format!("[database]\nurl = \"{}\"\n", url_for_enabled());
        let env = [("NBS__SERVER__BINDD", "x"), ("NBS__SERVER__BIND", "not an address"), ("NBS__DATABASE__CONNECT_LAZY", "maybe")];
        let problems = problems(Config::from_sources(Some(&toml), env));
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].starts_with("NBS__DATABASE__CONNECT_LAZY"));
        assert!(problems.iter().any(|p| p.contains("unknown setting")));
        let problems = problems_of_toml("[server]\nbindd = \"1.2.3.4:5\"\n");
        assert!(problems[0].contains("unknown field"), "{problems:?}");
    }

    fn problems_of_toml(toml: &str) -> Vec<String> {
        problems(Config::from_toml_str(toml))
    }

    #[test]
    fn validation_collects_every_problem() {
        let mut config = Config::default();
        config.database.url = SecretString::new("redis://x");
        config.database.max_connections = 0;
        config.database.min_connections = 5;
        config.http.body_limit_bytes = 10;
        config.http.request_timeout_secs = 0;
        config.http.upload_idle_timeout_secs = 0;
        config.http.upload_timeout_secs = 86_401;
        config.server.hook_timeout_ms = 0;
        config.cors.allowed_origins = vec!["*".into(), "example.com".into()];
        config.log.level = "info,[".into();
        let problems = problems(config.validate().map(|_| Config::default()));
        for needle in [
            "not a mysql://",
            "max_connections",
            "min_connections",
            "body_limit_bytes",
            "request_timeout_secs",
            "upload_idle_timeout_secs",
            "upload_timeout_secs must be at most",
            "hook_timeout_ms",
            "`*` must be",
            "example.com",
            "log.level",
        ] {
            assert!(problems.iter().any(|p| p.contains(needle)), "missing {needle}: {problems:?}");
        }
    }

    #[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
    #[test]
    fn sqlite_synchronous_default_full_and_invalid() {
        let url = url_for_enabled();
        // The default is NORMAL.
        let config = Config::from_sources(Some(&format!("[database]\nurl = \"{url}\"\n")), std::iter::empty::<(&str, &str)>()).unwrap();
        assert_eq!(config.database.sqlite_synchronous, SqliteSynchronous::Normal);
        assert_eq!(DatabaseConfig::default().sqlite_synchronous, SqliteSynchronous::Normal);
        // `full` from the file, `normal` from the environment (which wins), any case there.
        let toml = format!("[database]\nurl = \"{url}\"\nsqlite_synchronous = \"full\"\n");
        let config = Config::from_sources(Some(&toml), std::iter::empty::<(&str, &str)>()).unwrap();
        assert_eq!(config.database.sqlite_synchronous, SqliteSynchronous::Full);
        let config = Config::from_sources(Some(&toml), [("NBS__DATABASE__SQLITE_SYNCHRONOUS", "Normal")]).unwrap();
        assert_eq!(config.database.sqlite_synchronous, SqliteSynchronous::Normal);
        let config = Config::from_sources(None, [("NBS__DATABASE__URL", url), ("NBS__DATABASE__SQLITE_SYNCHRONOUS", "FULL")]).unwrap();
        assert_eq!(config.database.sqlite_synchronous, SqliteSynchronous::Full);
        // Anything else is refused, in the file and in the environment.
        for bad in ["\"off\"", "\"extra\"", "\"\"", "\"Full\"", "2", "true"] {
            let found = problems_of_toml(&format!("[database]\nurl = \"{url}\"\nsqlite_synchronous = {bad}\n"));
            assert!(!found.is_empty(), "{bad} was accepted");
        }
        for bad in ["off", "", "2"] {
            let found = problems(Config::from_sources(None, [("NBS__DATABASE__URL", url), ("NBS__DATABASE__SQLITE_SYNCHRONOUS", bad)]));
            assert!(found.iter().any(|p| p.starts_with("NBS__DATABASE__SQLITE_SYNCHRONOUS") && p.contains("`normal` or `full`")), "{bad:?}: {found:?}");
        }
    }

    #[test]
    fn disabled_backend_is_named() {
        for dialect in Dialect::ALL.iter().copied().filter(|d| !d.is_enabled()) {
            let url = match dialect {
                Dialect::MySql => "mysql://u:p@h/db",
                Dialect::Postgres => "postgres://u:p@h/db",
                Dialect::Sqlite => "sqlite::memory:",
            };
            let problems = problems(Config::from_sources(None, [("NBS__DATABASE__URL", url)]));
            assert!(problems.iter().any(|p| p.contains(&format!("`{}` feature", dialect.name()))), "{problems:?}");
        }
    }

    #[cfg(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")))]
    #[test]
    fn no_backend_is_a_clear_error() {
        let problems = problems(Config::from_sources(None, [("NBS__DATABASE__URL", "sqlite::memory:")]));
        assert!(problems.iter().any(|p| p.contains("no database backend is compiled in")), "{problems:?}");
    }

    #[test]
    fn module_secrets_never_show() {
        #[derive(Deserialize, Debug)]
        #[allow(dead_code)]
        struct Mail {
            port: u32,
        }
        let mut config = Config::default();
        let mut mail = toml::Table::new();
        mail.insert("smtp_password".into(), toml::Value::String("hunter2-secret".into()));
        mail.insert("port".into(), toml::Value::String("hunter3-secret".into()));
        config.modules.insert("mail".into(), toml::Value::Table(mail));
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter") && debug.contains("smtp_password"), "{debug}");
        let error = config.module_config::<Mail>("mail").err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("modules.mail") && !error.contains("hunter"), "{error}");
        assert_eq!(mask_quoted(r#"invalid type: string "x", expected u32 'y"#), r#"invalid type: string "<hidden>", expected u32 '<hidden>"#);
    }

    #[test]
    fn secret_or_file() {
        let dir = crate::test_support::temp_dir("config-secret-file");
        let path = dir.join("pw");
        std::fs::write(&path, "s3cret\n").unwrap();
        assert_eq!(resolve_secret("x", None, Some(&path)).unwrap().map(|s| s.expose().to_string()).as_deref(), Some("s3cret"));
        assert!(resolve_secret("x", Some(SecretString::new("a")), Some(&path)).is_err());
        assert!(resolve_secret("x", None, None).unwrap().is_none());
    }

    #[test]
    fn ui_needs_a_pinned_script() {
        let mut config = Config::default();
        config.openapi.ui = true;
        let found = problems(config.validate().map(|_| Config::default()));
        assert!(found.iter().any(|p| p.contains("ui_script_integrity")), "{found:?}");
        config.openapi.ui_script_url = Some("https://cdn.example/viewer@1.2.3".into());
        config.openapi.ui_script_integrity = Some("sha384-abc+/=".into());
        let problems = problems(config.validate().map(|_| Config::default()));
        assert!(!problems.iter().any(|p| p.contains("ui_script")), "{problems:?}");
    }

    #[test]
    fn secrets_are_redacted() {
        let secret = "mysql://game:hunter2-very-secret@db/game";
        let config = Config::from_sources(None, [("NBS__DATABASE__URL", secret)]);
        let text = match &config {
            Ok(config) => format!("{config:?}"),
            Err(error) => format!("{error:?} {error}"),
        };
        assert!(!text.contains("hunter2"), "{text}");
        let mut config = Config::default();
        config.database.url = SecretString::new(secret);
        assert!(!format!("{config:?}").contains("hunter2"));
        assert!(format!("{config:?}").contains("<redacted>"));
        // A broken TOML line holding a secret is not quoted back.
        let problems = problems_of_toml("[database]\nurl = \"mysql://u:hunter2@h/db\" garbage\n");
        assert!(!problems.join(" ").contains("hunter2"), "{problems:?}");
    }

    #[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
    #[test]
    fn url_file_is_read_and_trimmed() {
        let dir = crate::test_support::temp_dir("config-url-file");
        let path = dir.join("db_url");
        std::fs::write(&path, format!("{}\n", url_for_enabled())).unwrap();
        let env = [("NBS__DATABASE__URL_FILE", path.to_string_lossy().to_string())];
        let config = Config::from_sources(None, env).unwrap();
        assert_eq!(config.database.url.expose(), url_for_enabled());
        let both = [("NBS__DATABASE__URL_FILE", path.to_string_lossy().to_string()), ("NBS__DATABASE__URL", url_for_enabled().to_string())];
        assert!(problems(Config::from_sources(None, both))[0].contains("both set"));
    }

    #[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
    #[test]
    fn module_sections() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct Chat {
            max_chars: u32,
            name: String,
            nested: Option<BTreeMap<String, bool>>,
        }
        let toml = format!("[database]\nurl = \"{}\"\n[modules.chat]\nmax_chars = 10\nname = \"x\"\n", url_for_enabled());
        let env = [("NBS__MODULES__CHAT__MAX_CHARS", "500"), ("NBS__MODULES__CHAT__NESTED__ON", "true")];
        let config = Config::from_sources(Some(&toml), env).unwrap();
        let chat: Option<Chat> = config.module_config("chat").unwrap();
        let chat = chat.unwrap();
        assert_eq!(chat.max_chars, 500);
        assert_eq!(chat.name, "x");
        assert_eq!(chat.nested.and_then(|n| n.get("on").copied()), Some(true));
        assert!(config.module_config::<Chat>("storage").unwrap().is_none());
        assert!(config.module_config::<u32>("chat").is_err());
        assert_eq!(config.unknown_module_sections(&["chat"]), Vec::<String>::new());
        assert_eq!(config.unknown_module_sections(&["chta"]), ["chat"]);
        assert_eq!(env_value("mysql://x"), toml::Value::String("mysql://x".into()));
    }

    /// NIT9: `database.statement_timeout_secs` from the environment, at most a day.
    #[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
    #[test]
    fn statement_timeout_setting() {
        let url = url_for_enabled();
        assert_eq!(Config::from_sources(None, [("NBS__DATABASE__URL", url)]).expect("config").database.statement_timeout_secs, 0);
        let config = Config::from_sources(None, [("NBS__DATABASE__URL", url), ("NBS__DATABASE__STATEMENT_TIMEOUT_SECS", "30")]).expect("config");
        assert_eq!(config.database.statement_timeout_secs, 30);
        let problems = problems(Config::from_sources(None, [("NBS__DATABASE__URL", url), ("NBS__DATABASE__STATEMENT_TIMEOUT_SECS", "86401")]));
        assert!(problems.iter().any(|p| p.contains("statement_timeout_secs")), "{problems:?}");
    }

    /// SF2: a secret given by environment variable is its exact text, whatever it looks like, and a
    /// wrong-type value never appears in an error message.
    #[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
    #[test]
    fn secrets_from_the_environment_are_text_and_never_echoed() {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Mail {
            password: SecretString,
            port: Option<u16>,
            ratio: Option<f64>,
            on: Option<bool>,
            name: Option<String>,
        }
        let url = url_for_enabled();
        for secret in
            ["12345678", "-17", "1.5", "true", "1e5", "0042", "+5", "1979-05-27", "1979-05-27T07:32:00Z", "inf", "nan", "1_000", "s3cr3t", "\"quoted\""]
        {
            let env = [("NBS__DATABASE__URL", url), ("NBS__MODULES__MAIL__PASSWORD", secret), ("NBS__MODULES__MAIL__PORT", "587")];
            let config = Config::from_sources(None, env).expect("config");
            let mail = config.module_config::<Mail>("mail").map_err(|e| e.to_string()).expect("decodes").expect("section");
            let expected = if secret == "\"quoted\"" { "quoted" } else { secret };
            assert_eq!(mail.password.expose(), expected, "{secret}");
            assert_eq!(mail.port, Some(587));
        }
        // Numbers, booleans and quoted strings for other settings still work.
        let env = [
            ("NBS__DATABASE__URL", url),
            ("NBS__MODULES__MAIL__PASSWORD", "x"),
            ("NBS__MODULES__MAIL__RATIO", "0.25"),
            ("NBS__MODULES__MAIL__ON", "false"),
            ("NBS__MODULES__MAIL__NAME", "\"2048\""),
        ];
        let mail = Config::from_sources(None, env).expect("config").module_config::<Mail>("mail").ok().flatten().expect("decodes");
        assert_eq!((mail.ratio, mail.on, mail.name.as_deref()), (Some(0.25), Some(false), Some("2048")));
        // Wrong types: the value is never in the message.
        for (key, value) in [
            ("NBS__MODULES__MAIL__PORT", "98765432"),
            ("NBS__MODULES__MAIL__PORT", "hunter22"),
            ("NBS__MODULES__MAIL__NAME", "98765432"),
            ("NBS__MODULES__MAIL__ON", "98765432"),
            ("NBS__MODULES__MAIL__PASSWORD", "[98765432]"),
        ] {
            let env = [("NBS__DATABASE__URL", url), ("NBS__MODULES__MAIL__PASSWORD", "x"), (key, value)];
            let error = Config::from_sources(None, env).expect("config").module_config::<Mail>("mail").err().map(|e| e.to_string()).expect("an error");
            assert!(!error.contains("98765432") && !error.contains("hunter22"), "{key}={value}: {error}");
            assert!(error.contains("modules.mail"), "{error}");
        }
        assert_eq!(mask_quoted("invalid type: integer `12345678`, expected a string"), "invalid type: integer `<hidden>`, expected a string");
        assert_eq!(mask_quoted("invalid type: floating point `1.5`, expected u16"), "invalid type: floating point `<hidden>`, expected u16");
        assert_eq!(mask_quoted("unknown field `pasword`, expected one of `password`, `port`"), "unknown field `pasword`, expected one of `password`, `port`");
        assert_eq!(mask_quoted("unknown variant `hunter2`, expected `log` or `smtp`"), "unknown variant `<hidden>`, expected `log` or `smtp`");
    }
}
