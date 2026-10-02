//! The command line ("artisan-style"), built into every server binary:
//!
//! | Command | What |
//! |---|---|
//! | `serve` (also: no command) | run the server until SIGTERM / Ctrl-C |
//! | `migrate` / `migrate up` | apply pending migrations |
//! | `migrate status` | list every migration and its state |
//! | `migrations publish <module> [--dialect <d>] [--force]` | copy a module's SQL into the app (it then owns it) |
//! | `config check [--connect]` | validate the configuration, every module's settings included (and try the database) |
//! | `openapi export [--output <file>]` | write or print the OpenAPI document (no database needed) |
//! | `asyncapi export [--output <file>]` | write or print the AsyncAPI document of the WebSocket endpoint (no database needed) |
//! | `healthcheck` | ask the running server's `/readyz` on `server.bind`: exit 0 on `200`, 1 otherwise, within 4 s (no database needed; an app command of that name replaces it) |
//!
//! The configuration comes from `NBS_CONFIG` / `config.toml` + `NBS__*` variables
//! ([`Config::load`](crate::Config::load)), loaded by the app before the builder is created.
//! Modules and the app add their own commands ([`crate::command`]); the auth module adds
//! `user:create`, `user:role`, `user:ban`, `user:unban` and `sessions:revoke`. `--help` lists them.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::command::{validate_command_name, CommandCtx, BUILT_IN_COMMANDS};
use crate::config::{LogConfig, LogFormat};
use crate::db::{Db, Dialect};
use crate::error::Error;
use crate::migrate::MigrationState;
use crate::NetBackendServer;

#[derive(Parser, Debug)]
#[command(about = "A game backend server built with net_backend_server", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the server (the default).
    Serve,
    /// Apply or inspect database migrations.
    Migrate {
        #[command(subcommand)]
        action: Option<MigrateAction>,
    },
    /// Manage module migrations.
    Migrations {
        #[command(subcommand)]
        action: MigrationsAction,
    },
    /// Check the configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// The OpenAPI document.
    Openapi {
        #[command(subcommand)]
        action: OpenapiAction,
    },
    /// The AsyncAPI document of the WebSocket endpoint.
    Asyncapi {
        #[command(subcommand)]
        action: OpenapiAction,
    },
    /// Check that the running server is ready: `GET /readyz` on `server.bind` (an unspecified
    /// address means loopback); exit 0 on `200`, 1 otherwise. Needs no database.
    Healthcheck,
    /// An app command (from a module or the app; listed below).
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Subcommand, Debug)]
enum OpenapiAction {
    /// Write the document (JSON) to a file, or print it. Needs no database.
    Export {
        /// The file to write; default: print it.
        #[arg(long)]
        output: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum MigrateAction {
    /// Apply every pending migration (the default).
    Up,
    /// List every migration and its state.
    Status,
}

#[derive(Subcommand, Debug)]
enum MigrationsAction {
    /// Copy a module's migrations into the app's migrations directory; the app then owns them.
    Publish {
        /// The module name.
        module: String,
        /// Only this dialect (mysql, postgres, sqlite); default: all.
        #[arg(long)]
        dialect: Option<String>,
        /// Overwrite files that exist with other content (the app's edits are lost).
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Validate the configuration and print a summary (secrets hidden).
    Check {
        /// Also connect to the database.
        #[arg(long)]
        connect: bool,
    },
}

fn out(w: &mut (dyn Write + Send), line: impl std::fmt::Display) -> Result<(), Error> {
    writeln!(w, "{line}").map_err(|e| Error::io("writing output", e))
}

/// Whether `args` only ask for the help or the version (`--help` / `-h` / `help` / `--version` /
/// `-V`, also after a built-in command's name): those need no configuration.
pub(crate) fn informational(args: &[OsString]) -> bool {
    let flag = |a: &OsString| a == "--help" || a == "-h" || a == "--version" || a == "-V";
    match args.get(1) {
        Some(first) if flag(first) || first == "help" => true,
        Some(first) => {
            let built_in = first.to_str().is_some_and(|name| BUILT_IN_COMMANDS.contains(&name) || name == HEALTHCHECK);
            built_in && args[2..].iter().any(flag)
        }
        None => false,
    }
}

/// Parse `args` and run the command, writing human output to `w`.
pub(crate) async fn run<I, T>(server: NetBackendServer, args: I, w: &mut (dyn Write + Send)) -> Result<(), Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let commands = server.app_commands();
    let mut names = std::collections::HashSet::new();
    for command in &commands {
        validate_command_name(command.name()).map_err(Error::Cli)?;
        if !names.insert(command.name()) {
            return Err(Error::Cli(format!("command `{}` is registered twice", command.name())));
        }
    }
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    // An app command named `healthcheck` (written before the framework had one) replaces the
    // built-in: it runs, and the built-in is hidden from `--help`.
    let own_healthcheck = commands.iter().any(|c| c.name() == HEALTHCHECK);
    let mut parser = Cli::command();
    if own_healthcheck {
        parser = parser.mut_subcommand(HEALTHCHECK, |c| c.hide(true));
    }
    if !commands.is_empty() {
        let mut list = String::from("App commands:\n");
        for command in &commands {
            let head = format!("{} {}", command.name(), command.usage());
            list.push_str(&format!("  {:<44} {}\n", head.trim_end(), command.about()));
        }
        parser = parser.after_help(list);
    }
    let parsed = if own_healthcheck && args.get(1).is_some_and(|a| a == HEALTHCHECK) {
        Ok(Cli { command: Some(Command::External(args[1..].to_vec())) })
    } else {
        parser.try_get_matches_from(args).and_then(|matches| Cli::from_arg_matches(&matches))
    };
    let cli = match parsed {
        Ok(cli) => cli,
        Err(error) => {
            use clap::error::ErrorKind;
            if matches!(error.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand | ErrorKind::DisplayVersion) {
                return out(w, error.render().to_string().trim_end());
            }
            return Err(Error::Cli(error.render().to_string()));
        }
    };
    match cli.command.unwrap_or(Command::Serve) {
        Command::External(raw) => {
            let mut raw = raw.into_iter().map(|a| a.to_string_lossy().into_owned());
            let name = raw.next().unwrap_or_default();
            let args: Vec<String> = raw.collect();
            let Some(command) = commands.iter().find(|c| c.name() == name) else {
                return Err(Error::Cli(format!("unknown command `{name}` (see --help)")));
            };
            let prepared = server.build().await?;
            let result = command.run(CommandCtx::new(&prepared, w), &args).await;
            prepared.state().db().close().await;
            result
        }
        Command::Serve => {
            let bind = server.config().server.bind;
            let listener = tokio::net::TcpListener::bind(bind).await.map_err(|e| Error::io(format!("binding {bind}"), e))?;
            server.serve(listener).await
        }
        Command::Migrate { action } => {
            let prepared = server.build().await?;
            let result = match action.unwrap_or(MigrateAction::Up) {
                MigrateAction::Up => {
                    let report = prepared.migrate().await;
                    match report {
                        Ok(report) => {
                            for warning in &report.warnings {
                                out(w, format!("warning: {warning}"))?;
                            }
                            if report.applied.is_empty() {
                                out(w, "Nothing to migrate.")?;
                            }
                            for (module, version, name) in &report.applied {
                                out(w, format!("Migrated  {module}/{version}_{name}"))?;
                            }
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                MigrateAction::Status => match prepared.migration_status().await {
                    Ok(list) => {
                        if list.is_empty() {
                            out(w, "No migrations.")?;
                        }
                        for s in &list {
                            out(w, format!("{:<9} {:<16} {:>14}  {}", s.state.to_string(), s.module, s.version, s.name))?;
                        }
                        if list.iter().any(|s| s.state == MigrationState::Modified) {
                            out(w, "warning: MODIFIED migrations were changed after they were applied")?;
                        }
                        Ok(())
                    }
                    Err(error) => Err(error),
                },
            };
            prepared.state().db().close().await;
            result
        }
        Command::Migrations { action: MigrationsAction::Publish { module, dialect, force } } => {
            let dialects: Vec<Dialect> = match dialect {
                None => Vec::new(),
                Some(name) => vec![Dialect::from_name(&name).ok_or_else(|| Error::Cli(format!("unknown dialect `{name}` (mysql, postgres, sqlite)")))?],
            };
            let report = server.publish_migrations(&module, &dialects, force)?;
            for path in &report.written {
                out(w, format!("Published {}", path.display()))?;
            }
            for path in &report.unchanged {
                out(w, format!("Unchanged {}", path.display()))?;
            }
            for path in &report.skipped {
                out(w, format!("Skipped   {} (edited by the app; --force overwrites)", path.display()))?;
            }
            out(w, format!("The app now owns the `{module}` migrations; edit them before the first `migrate`."))
        }
        Command::Healthcheck => {
            let bind = server.config().server.bind;
            // Blocking socket calls with their own timeouts, off the runtime's worker threads.
            match tokio::task::spawn_blocking(move || ready(bind)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(problem)) => Err(Error::Cli(format!("healthcheck: {problem}"))),
                Err(error) => Err(Error::Cli(format!("healthcheck: {error}"))),
            }
        }
        Command::Openapi { action: OpenapiAction::Export { output } } => export(document(server, false).await?, output, w),
        Command::Asyncapi { action: OpenapiAction::Export { output } } => export(document(server, true).await?, output, w),
        Command::Config { action: ConfigAction::Check { connect } } => {
            let config = server.config();
            config.validate()?;
            let unknown = config.unknown_module_sections(&server.module_names());
            if !unknown.is_empty() {
                return Err(Error::Config(unknown.iter().map(|name| format!("[modules.{name}]: no module named `{name}` is registered (a typo?)")).collect()));
            }
            // Every module reads and checks its own `[modules.<name>]` section while the server is
            // built: build it once with a lazy pool (no database contact), so a typo in a module's
            // settings fails here and not at the next start.
            let config = config.clone();
            let modules = server.module_names().join(", ");
            let mut probe = server;
            probe.config.database.connect_lazy = true;
            let prepared = probe.build().await?;
            prepared.state().db().close().await;
            let dialect = config.database.dialect().map_or("unknown", |d| d.display_name());
            out(w, "Configuration OK.")?;
            out(w, format!("  bind            {}", config.server.bind))?;
            out(w, format!("  database        {dialect} (URL hidden), pool {}", config.database.max_connections))?;
            out(w, format!("  modules         {modules}"))?;
            out(w, format!("  body limit      {} bytes, timeout {} s", config.http.body_limit_bytes, config.http.request_timeout_secs))?;
            out(w, format!("  openapi         {}, ui {}", config.openapi.enabled, config.openapi.ui))?;
            out(w, format!("  metrics         {}", config.metrics.enabled))?;
            out(
                w,
                format!(
                    "  cors            {}",
                    if config.cors.allowed_origins.is_empty() { "off".to_string() } else { config.cors.allowed_origins.join(", ") }
                ),
            )?;
            if connect {
                let db = Db::connect(&config.database).await?;
                let ping = db.ping().await;
                db.close().await;
                ping?;
                out(w, "  database        reachable")?;
            }
            Ok(())
        }
    }
}

/// The built-in health check's name (an app command of that name replaces it).
const HEALTHCHECK: &str = "healthcheck";

/// The longest a whole `healthcheck` may take (connect, request and answer together): below the
/// 5 s `HEALTHCHECK --timeout` of the Docker files, so the check reports its own reason.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(4);

/// `GET /readyz` on the listening address (an unspecified address means loopback: `[::]` tries
/// `::1`, then `127.0.0.1`, for a machine without IPv6); `Ok` on a `200` answer. Everything
/// together within [`HEALTHCHECK_TIMEOUT`].
fn ready(bind: SocketAddr) -> Result<(), String> {
    let deadline = Instant::now() + HEALTHCHECK_TIMEOUT;
    let left = || deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let port = bind.port();
    let candidates = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)],
        IpAddr::V6(ip) if ip.is_unspecified() => {
            vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port), SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)]
        }
        ip => vec![SocketAddr::new(ip, port)],
    };
    let mut problem = String::new();
    let mut connected = None;
    for addr in candidates {
        match TcpStream::connect_timeout(&addr, left()) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(e) => problem = format!("cannot connect to {addr}: {e}"),
        }
    }
    let mut stream = connected.ok_or(problem)?;
    stream.set_write_timeout(Some(left())).map_err(|e| e.to_string())?;
    stream.write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").map_err(|e| format!("cannot send the request: {e}"))?;
    // The status line is all that matters: read until its end (at most 4 KiB).
    let mut answer = Vec::with_capacity(256);
    let mut chunk = [0u8; 512];
    while !answer.contains(&b'\n') && answer.len() < 4096 {
        if Instant::now() >= deadline {
            return Err(format!("no answer within {} s", HEALTHCHECK_TIMEOUT.as_secs()));
        }
        stream.set_read_timeout(Some(left())).map_err(|e| e.to_string())?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => answer.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(format!("cannot read the answer: {e}")),
        }
    }
    let status_line = answer.split(|&b| b == b'\n').next().map(String::from_utf8_lossy).unwrap_or_default();
    let status = status_line.split_whitespace().nth(1).unwrap_or("");
    if status == "200" {
        Ok(())
    } else {
        Err(format!("/readyz answered `{}`", status_line.trim()))
    }
}

/// Write a document to `output`, or print it.
fn export(json: String, output: Option<std::path::PathBuf>, w: &mut (dyn Write + Send)) -> Result<(), Error> {
    match output {
        Some(path) => {
            std::fs::write(&path, json.as_bytes()).map_err(|e| Error::io(format!("writing {}", path.display()), e))?;
            out(w, format!("Wrote {}", path.display()))
        }
        None => out(w, json),
    }
}

/// The OpenAPI (or AsyncAPI) document without touching the database (the pool is created lazily
/// and never used).
async fn document(mut server: NetBackendServer, asyncapi: bool) -> Result<String, Error> {
    server.config.database.connect_lazy = true;
    let prepared = server.build().await?;
    let json = if asyncapi { prepared.asyncapi_json().to_string() } else { prepared.openapi_json().to_string() };
    prepared.state().db().close().await;
    Ok(json)
}

/// Install a `tracing` subscriber from `[log]` (`RUST_LOG` wins). Returns false if one was
/// already installed (the app's own stays).
pub fn init_logging(config: &LogConfig) -> bool {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(&config.level)).unwrap_or_else(|_| EnvFilter::new("info"));
    // Colours only on a terminal: `docker logs`, journald and files get plain text.
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()));
    match config.format {
        LogFormat::Json => builder.json().try_init().is_ok(),
        _ => builder.try_init().is_ok(),
    }
}
