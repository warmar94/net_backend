//! The command line ("artisan-style"), built into every server binary:
//!
//! | Command | What |
//! |---|---|
//! | `serve` (also: no command) | run the server until SIGTERM / Ctrl-C |
//! | `migrate` / `migrate up` | apply pending migrations |
//! | `migrate status` | list every migration and its state |
//! | `migrations publish <module> [--dialect <d>] [--force]` | copy a module's SQL into the app (it then owns it) |
//! | `config check [--connect]` | validate the configuration (and try the database) |
//! | `openapi export [--output <file>]` | write or print the OpenAPI document (no database needed) |
//! | `asyncapi export [--output <file>]` | write or print the AsyncAPI document of the WebSocket endpoint (no database needed) |
//!
//! The configuration comes from `NBS_CONFIG` / `config.toml` + `NBS__*` variables
//! ([`Config::load`](crate::Config::load)), loaded by the app before the builder is created.
//! Modules and the app add their own commands ([`crate::command`]); the auth module adds
//! `user:create`, `user:role`, `user:ban`, `user:unban` and `sessions:revoke`. `--help` lists them.

use std::ffi::OsString;
use std::io::Write;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::command::{validate_command_name, CommandCtx};
use crate::config::{LogConfig, LogFormat};
use crate::db::{Db, Dialect};
use crate::error::Error;
use crate::migrate::MigrationState;
use crate::NetBackendServer;

#[derive(Parser, Debug)]
#[command(about = "A game backend server built with net_backend_server", disable_version_flag = true)]
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
    let mut parser = Cli::command();
    if !commands.is_empty() {
        let mut list = String::from("App commands:\n");
        for command in &commands {
            let head = format!("{} {}", command.name(), command.usage());
            list.push_str(&format!("  {:<44} {}\n", head.trim_end(), command.about()));
        }
        parser = parser.after_help(list);
    }
    let parsed = parser.try_get_matches_from(args).and_then(|matches| Cli::from_arg_matches(&matches));
    let cli = match parsed {
        Ok(cli) => cli,
        Err(error) => {
            use clap::error::ErrorKind;
            if matches!(error.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand) {
                return out(w, error.render());
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
        Command::Openapi { action: OpenapiAction::Export { output } } => export(document(server, false).await?, output, w),
        Command::Asyncapi { action: OpenapiAction::Export { output } } => export(document(server, true).await?, output, w),
        Command::Config { action: ConfigAction::Check { connect } } => {
            let config = server.config();
            config.validate()?;
            let unknown = config.unknown_module_sections(&server.module_names());
            if !unknown.is_empty() {
                return Err(Error::Config(unknown.iter().map(|name| format!("[modules.{name}]: no module named `{name}` is registered (a typo?)")).collect()));
            }
            let dialect = config.database.dialect().map_or("unknown", |d| d.display_name());
            out(w, "Configuration OK.")?;
            out(w, format!("  bind            {}", config.server.bind))?;
            out(w, format!("  database        {dialect} (URL hidden), pool {}", config.database.max_connections))?;
            out(w, format!("  modules         {}", server.module_names().join(", ")))?;
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
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match config.format {
        LogFormat::Json => builder.json().try_init().is_ok(),
        _ => builder.try_init().is_ok(),
    }
}
