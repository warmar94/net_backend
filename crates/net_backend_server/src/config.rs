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

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(SecretString)
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
    /// `[modules.<name>]`: each module's own settings, read with [`Config::module_config`]. `Debug`
    /// prints only the key names (a module section may hold secrets). A section without a
    /// registered module is refused when the server is built.
    pub modules: toml::Table,
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
            .field("modules (keys only)", &modules)
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
    /// The time limit of one module's `shutdown`, in seconds (then it is abandoned). Default 10.
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
    /// How long a query waits for a free connection, in seconds. Default 5.
    pub acquire_timeout_secs: u64,
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
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: SecretString::default(),
            url_file: None,
            max_connections: 10,
            min_connections: 0,
            acquire_timeout_secs: 5,
            connect_lazy: false,
            migrate_on_start: false,
            migrations_dir: PathBuf::from("migrations"),
            migrate_lock_timeout_secs: 60,
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
    /// The time limit of one request, in seconds (then 503 `unavailable`). Default 30.
    pub request_timeout_secs: u64,
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
            ["database", "connect_lazy"] => self.database.connect_lazy = parse(value)?,
            ["database", "migrate_on_start"] => self.database.migrate_on_start = parse(value)?,
            ["database", "migrations_dir"] => self.database.migrations_dir = PathBuf::from(value),
            ["database", "migrate_lock_timeout_secs"] => self.database.migrate_lock_timeout_secs = parse(value)?,
            ["http", "body_limit_bytes"] => self.http.body_limit_bytes = parse(value)?,
            ["http", "request_timeout_secs"] => self.http.request_timeout_secs = parse(value)?,
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
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }

    /// A module's settings (`[modules.<name>]`) decoded as `T`; `None` if the section is absent.
    /// Module config structs should use `#[serde(deny_unknown_fields)]` (typos become errors) and
    /// [`SecretString`] for secrets (with a `<name>_file` alternative, see [`resolve_secret`]).
    /// Error messages never quote the configured values.
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

/// Replace every `"…"` / `'…'` quoted part of a message (serde quotes the offending value).
fn mask_quoted(message: &str) -> String {
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
                out.push(c);
                if c == '"' || c == '\'' {
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

/// An environment value for a module setting: a TOML scalar / array if it parses as one
/// (`42`, `true`, `[1, 2]`), else the plain string.
fn env_value(value: &str) -> toml::Value {
    let doc = format!("v = {value}");
    match toml::from_str::<toml::Table>(&doc) {
        Ok(mut table) => table.remove("v").unwrap_or_else(|| toml::Value::String(value.to_string())),
        Err(_) => toml::Value::String(value.to_string()),
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
        let env = [("NBS__DATABASE__MAX_CONNECTIONS", "7"), ("NBS__HTTP__REQUEST_TIMEOUT_SECS", "12"), ("OTHER", "x")];
        let config = Config::from_sources(Some(&toml), env).unwrap();
        assert_eq!(config.server.bind.port(), 9000);
        assert_eq!(config.database.max_connections, 7);
        assert_eq!(config.http.request_timeout_secs, 12);
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
            "hook_timeout_ms",
            "`*` must be",
            "example.com",
            "log.level",
        ] {
            assert!(problems.iter().any(|p| p.contains(needle)), "missing {needle}: {problems:?}");
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
}
