//! App commands: extra command-line commands from modules and the app, run like the built-in
//! ones (`game-server user:create ada@example.com --admin`).
//!
//! A command implements [`AppCommand`] and is registered with
//! [`NetBackendServer::command`](crate::NetBackendServer::command) or returned by a module's
//! [`Module::commands`](crate::Module::commands). The command line builds the server first
//! (configuration, database, modules' setup), so a command can use the database and every
//! module service through [`CommandCtx::state`], then closes the pool.
//!
//! Names: `[a-z][a-z0-9_-]*` with optional `:` sections (`user:create`), unique, and not a
//! built-in command. Arguments arrive unparsed; [`CommandArgs::parse`] covers the usual shape
//! (positionals, `--flag`, `--name value` / `--name=value`) and refuses unknown options.
//!
//! **Defaults:** `name`, `about` and `run` are required; `usage` has a default implementation.
//!
//! ```
//! use net_backend_server::command::{AppCommand, CommandArgs, CommandCtx};
//! use net_backend_server::Error;
//! use futures_util::future::BoxFuture;
//!
//! struct Motd;
//!
//! impl AppCommand for Motd {
//!     fn name(&self) -> &'static str { "game:motd" }
//!     fn about(&self) -> &'static str { "Print the message of the day" }
//!     fn usage(&self) -> &'static str { "[--loud]" }
//!     fn run<'a>(&'a self, mut ctx: CommandCtx<'a>, args: &'a [String]) -> BoxFuture<'a, Result<(), Error>> {
//!         Box::pin(async move {
//!             let args = CommandArgs::parse(args, &[], &["--loud"])?;
//!             ctx.println(if args.flag("--loud") { "WELCOME" } else { "welcome" })
//!         })
//!     }
//! }
//! ```

use std::collections::BTreeMap;
use std::io::Write;

use futures_util::future::BoxFuture;

use crate::app::PreparedServer;
use crate::error::Error;
use crate::state::AppState;

/// The built-in command names (an app command may not use them).
pub const BUILT_IN_COMMANDS: &[&str] = &["serve", "migrate", "migrations", "config", "openapi", "help"];

/// A command-line command added by a module or the app.
pub trait AppCommand: Send + Sync + 'static {
    /// The command name (`user:create`).
    fn name(&self) -> &'static str;

    /// One line for the help text.
    fn about(&self) -> &'static str;

    /// The argument synopsis for the help text (`<email> [--admin]`).
    fn usage(&self) -> &'static str {
        ""
    }

    /// Run with the arguments after the name. Write output through [`CommandCtx::println`].
    fn run<'a>(&'a self, ctx: CommandCtx<'a>, args: &'a [String]) -> BoxFuture<'a, Result<(), Error>>;
}

/// What a command runs with: the built server and the output.
pub struct CommandCtx<'a> {
    server: &'a PreparedServer,
    out: &'a mut (dyn Write + Send),
}

impl std::fmt::Debug for CommandCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandCtx").field("server", self.server).finish_non_exhaustive()
    }
}

impl<'a> CommandCtx<'a> {
    pub(crate) fn new(server: &'a PreparedServer, out: &'a mut (dyn Write + Send)) -> Self {
        Self { server, out }
    }

    /// The app state (database, configuration, module services).
    pub fn state(&self) -> &AppState {
        self.server.state()
    }

    /// The built server (e.g. to run migrations first).
    pub fn server(&self) -> &PreparedServer {
        self.server
    }

    /// Write one line of output.
    pub fn println(&mut self, line: impl std::fmt::Display) -> Result<(), Error> {
        writeln!(self.out, "{line}").map_err(|e| Error::io("writing output", e))
    }
}

/// Parsed command arguments: positionals, flags and options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandArgs {
    positionals: Vec<String>,
    flags: Vec<String>,
    options: BTreeMap<String, String>,
}

impl CommandArgs {
    /// Parse `args`: `options` take a value (`--name value` or `--name=value`), `flags` do not;
    /// anything else starting with `--` is an error, everything else is a positional. `--` ends
    /// the options.
    pub fn parse(args: &[String], options: &[&str], flags: &[&str]) -> Result<CommandArgs, Error> {
        let mut parsed = CommandArgs::default();
        let mut iter = args.iter();
        let mut only_positionals = false;
        while let Some(arg) = iter.next() {
            if only_positionals || !arg.starts_with("--") {
                parsed.positionals.push(arg.clone());
                continue;
            }
            if arg == "--" {
                only_positionals = true;
                continue;
            }
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (arg.as_str(), None),
            };
            if options.contains(&name) {
                let value = match inline {
                    Some(value) => value,
                    None => iter.next().cloned().ok_or_else(|| Error::Cli(format!("{name} needs a value")))?,
                };
                if parsed.options.insert(name.to_string(), value).is_some() {
                    return Err(Error::Cli(format!("{name} is given twice")));
                }
            } else if flags.contains(&name) && inline.is_none() {
                parsed.flags.push(name.to_string());
            } else {
                return Err(Error::Cli(format!("unknown option {name}")));
            }
        }
        Ok(parsed)
    }

    /// The positional arguments.
    pub fn positionals(&self) -> &[String] {
        &self.positionals
    }

    /// The positional at `index`, or an error naming it.
    pub fn required(&self, index: usize, name: &str) -> Result<&str, Error> {
        self.positionals.get(index).map(String::as_str).ok_or_else(|| Error::Cli(format!("missing argument <{name}>")))
    }

    /// Whether the flag was given.
    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// The value of an option.
    pub fn option(&self, name: &str) -> Option<&str> {
        self.options.get(name).map(String::as_str)
    }
}

/// Check a command name (see the module docs).
pub fn validate_command_name(name: &str) -> Result<(), String> {
    let ok = name.split(':').all(|part| {
        part.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    });
    if !ok || name.len() > 64 {
        return Err(format!("command name `{name}` must be [a-z][a-z0-9_-]* parts joined by `:` (at most 64 bytes)"));
    }
    if BUILT_IN_COMMANDS.contains(&name) {
        return Err(format!("command name `{name}` is a built-in command"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parsing() {
        let parsed =
            CommandArgs::parse(&args(&["ada@example.com", "--admin", "--name", "Ada", "--role=mod", "--", "--x"]), &["--name", "--role"], &["--admin"]);
        let parsed = parsed.unwrap_or_default();
        assert_eq!(parsed.positionals(), ["ada@example.com", "--x"]);
        assert!(parsed.flag("--admin") && !parsed.flag("--verified"));
        assert_eq!(parsed.option("--name"), Some("Ada"));
        assert_eq!(parsed.option("--role"), Some("mod"));
        assert_eq!(parsed.required(0, "email").ok(), Some("ada@example.com"));
        assert!(parsed.required(2, "x").is_err());
        assert!(CommandArgs::parse(&args(&["--nope"]), &[], &[]).is_err());
        assert!(CommandArgs::parse(&args(&["--name"]), &["--name"], &[]).is_err());
        assert!(CommandArgs::parse(&args(&["--admin=1"]), &[], &["--admin"]).is_err());
        assert!(CommandArgs::parse(&args(&["--n", "a", "--n", "b"]), &["--n"], &[]).is_err());
    }

    #[test]
    fn names() {
        for ok in ["user:create", "sessions:revoke", "game", "a:b-c:d_1"] {
            assert!(validate_command_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "User", ":x", "x:", "x::y", "serve", "migrate", "1x", "x y"] {
            assert!(validate_command_name(bad).is_err(), "{bad}");
        }
    }
}
