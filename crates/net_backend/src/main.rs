//! `net-backend`: creates net_backend game backend projects that run at once.
//!
//! ```text
//! net-backend new <name>          a workspace: server/ + client/
//! net-backend new-server <name>   a server project
//! net-backend new-client <name>   a client project
//! ```
//!
//! The templates are embedded in the binary; generating a project needs no network.

mod generate;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use generate::{Kind, CRATES_VERSION};

const USAGE: &str = "\
Usage:
  net-backend new <name>          a workspace: server/ (accounts, saves, chat on SQLite) + client/
  net-backend new-server <name>   a server project
  net-backend new-client <name>   a client project
  net-backend --help              this text
  net-backend --version           the version

<name> is the folder to create; a path works too, its last part is the project name (a-z, 0-9,
`-` and `_`, starting with a letter). An existing folder must be empty.";

fn help() -> String {
    format!(
        "net-backend {CRATES_VERSION}: creates net_backend game backend projects that run at once.\n\
         The projects use net_backend_server / net_backend_client {CRATES_VERSION}.\n\n\
         {USAGE}\n\n\
         Then, in every shell (PowerShell, cmd, bash, zsh):\n\
         \x20 net-backend new mygame\n\
         \x20 cd mygame\n\
         \x20 cargo run              # the server on http://127.0.0.1:8080\n\
         \x20 cargo run -p client    # the client, in a second terminal\n\n\
         More: https://net-backend.com"
    )
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    New(Kind, PathBuf),
}

/// Parse the arguments (without the program name).
fn parse(args: &[OsString]) -> Result<Command, String> {
    let text = |arg: &OsString| arg.to_str().map(str::to_owned);
    let Some(first) = args.first() else {
        return Ok(Command::Help);
    };
    let kind = match text(first).as_deref() {
        Some("-h" | "--help" | "help") => return Ok(Command::Help),
        Some("-V" | "--version" | "version") => return Ok(Command::Version),
        Some("new") => Kind::Full,
        Some("new-server") => Kind::Server,
        Some("new-client") => Kind::Client,
        Some(other) => return Err(format!("unknown command `{other}`")),
        None => return Err(format!("unknown command `{}`", first.to_string_lossy())),
    };
    let command = text(first).unwrap_or_default();
    match &args[1..] {
        [] => Err(format!("`{command}` needs the project name: net-backend {command} <name>")),
        [arg] if matches!(text(arg).as_deref(), Some("-h" | "--help")) => Ok(Command::Help),
        [arg] if arg.to_string_lossy().starts_with('-') => Err(format!("unknown option `{}`", arg.to_string_lossy())),
        [path] => Ok(Command::New(kind, PathBuf::from(path))),
        [_, extra, ..] => Err(format!("unexpected argument `{}`", extra.to_string_lossy())),
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse(&args) {
        Ok(Command::Help) => {
            println!("{}", help());
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("net-backend {CRATES_VERSION}");
            ExitCode::SUCCESS
        }
        Ok(Command::New(kind, target)) => match generate::generate(kind, &target) {
            Ok(_) => {
                println!("{}", next_steps(kind, &target));
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        },
        Err(problem) => {
            eprintln!("error: {problem}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// The message after a project was written: what it is and the commands to run it.
fn next_steps(kind: Kind, target: &Path) -> String {
    let shown = target.display().to_string();
    // A path of plain characters needs no quotes; anything else is double-quoted, which keeps spaces,
    // `&`, `(` and `;` literal in PowerShell, cmd, bash and zsh alike.
    let plain = shown.chars().all(|c| c.is_ascii_alphanumeric() || "._-/\\:".contains(c));
    let cd = if plain { format!("cd {shown}") } else { format!("cd \"{shown}\"") };
    let name = generate::name_of(target).unwrap_or_default();
    let mut text = format!("Created `{name}` ({}, net_backend {CRATES_VERSION}).\n\nNext:\n  {cd}\n", kind.describe());
    match kind {
        Kind::Full => {
            text.push_str("  cargo run              # the server on http://127.0.0.1:8080\n");
            text.push_str("  cargo run -p client    # the client, in a second terminal\n");
        }
        Kind::Server => text.push_str("  cargo run              # the server on http://127.0.0.1:8080\n"),
        Kind::Client => text.push_str("  cargo run              # the client, against http://127.0.0.1:8080\n"),
    }
    text.push_str("\nREADME.md in the project describes the rest.");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn commands() {
        assert_eq!(parse(&args(&[])), Ok(Command::Help));
        for help in ["--help", "-h", "help"] {
            assert_eq!(parse(&args(&[help])), Ok(Command::Help));
        }
        for version in ["--version", "-V", "version"] {
            assert_eq!(parse(&args(&[version])), Ok(Command::Version));
        }
        assert_eq!(parse(&args(&["new", "mygame"])), Ok(Command::New(Kind::Full, "mygame".into())));
        assert_eq!(parse(&args(&["new-server", "a b/s"])), Ok(Command::New(Kind::Server, "a b/s".into())));
        assert_eq!(parse(&args(&["new-client", "c"])), Ok(Command::New(Kind::Client, "c".into())));
        assert_eq!(parse(&args(&["new", "--help"])), Ok(Command::Help));
        assert!(parse(&args(&["new"])).unwrap_err().contains("needs the project name"));
        assert!(parse(&args(&["new", "a", "b"])).unwrap_err().contains("unexpected argument `b`"));
        assert!(parse(&args(&["new", "--db"])).unwrap_err().contains("unknown option"));
        assert!(parse(&args(&["init"])).unwrap_err().contains("unknown command `init`"));
    }

    #[test]
    fn messages() {
        assert!(help().contains(CRATES_VERSION));
        let full = next_steps(Kind::Full, Path::new("mygame"));
        assert!(full.contains("cd mygame\n") && full.contains("cargo run -p client"));
        let spaced = next_steps(Kind::Server, Path::new("my games/mygame"));
        assert!(spaced.contains("cd \"my games/mygame\"") && !spaced.contains("-p client"));
        assert!(next_steps(Kind::Client, Path::new("a&b/mygame")).contains("cd \"a&b/mygame\""));
        assert!(next_steps(Kind::Client, Path::new(r"C:\games\my-game_2")).contains(r"cd C:\games\my-game_2"));
    }
}
