//! The answers of `net-backend new`: from flags, from the questions, or the defaults; and the
//! one-line command that gives the same project.

use std::path::{Path, PathBuf};

use crate::modules::{self, Module};

/// How the game talks to the server (`--client`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientKind {
    /// Server + protocol + net_backend_client.
    Rust,
    /// Server + protocol + bevy_net_backend.
    Bevy,
    /// Server + protocol, the user's own HTTP / WebSocket library.
    Protocol,
    /// The server only.
    Api,
}

impl ClientKind {
    pub const ALL: [ClientKind; 4] = [ClientKind::Rust, ClientKind::Bevy, ClientKind::Protocol, ClientKind::Api];

    /// The `--client` value.
    pub fn flag(self) -> &'static str {
        match self {
            ClientKind::Rust => "rust",
            ClientKind::Bevy => "bevy",
            ClientKind::Protocol => "protocol",
            ClientKind::Api => "api",
        }
    }

    /// What the question shows.
    pub fn label(self) -> &'static str {
        match self {
            ClientKind::Rust => "Rust client      (server + protocol + net_backend_client)",
            ClientKind::Bevy => "Bevy plugin      (server + protocol + bevy_net_backend)",
            ClientKind::Protocol => "Your own client  (server + protocol, your HTTP / WebSocket library)",
            ClientKind::Api => "Pure API         (the server only; any language, API.md)",
        }
    }

    /// Whether a demo app exists for it.
    pub fn has_demo(self) -> bool {
        matches!(self, ClientKind::Rust | ClientKind::Bevy)
    }

    fn parse(value: &str) -> Result<Self, String> {
        Self::ALL.into_iter().find(|k| k.flag() == value).ok_or_else(|| format!("unknown --client `{value}` (rust, bevy, protocol or api)"))
    }
}

/// The database (`--db`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Database {
    Sqlite,
    Postgres,
    Mysql,
}

impl Database {
    pub const ALL: [Database; 3] = [Database::Sqlite, Database::Postgres, Database::Mysql];

    /// The `--db` value, which is also the cargo feature of net_backend_server.
    pub fn flag(self) -> &'static str {
        match self {
            Database::Sqlite => "sqlite",
            Database::Postgres => "postgres",
            Database::Mysql => "mysql",
        }
    }

    /// What the question shows.
    pub fn label(self) -> &'static str {
        match self {
            Database::Sqlite => "SQLite      (a file, nothing to install)",
            Database::Postgres => "PostgreSQL",
            Database::Mysql => "MySQL",
        }
    }

    /// The name in texts.
    pub fn name(self) -> &'static str {
        match self {
            Database::Sqlite => "SQLite",
            Database::Postgres => "PostgreSQL",
            Database::Mysql => "MySQL",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        Self::ALL.into_iter().find(|d| d.flag() == value).ok_or_else(|| format!("unknown --db `{value}` (sqlite, postgres or mysql)"))
    }
}

/// Everything `net-backend new` needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The folder to create (its last part is the project name).
    pub target: PathBuf,
    pub client: ClientKind,
    pub database: Database,
    /// In table order, requirements included.
    pub modules: Vec<&'static Module>,
    /// A demo app (Rust and Bevy only).
    pub demo: bool,
    /// Bevy: the existing game the demo goes next to (`--existing`).
    pub existing: Option<PathBuf>,
    /// Build, start the server in a new terminal window, then the demo or client (`--run`).
    pub run: bool,
}

impl Options {
    /// Whether the project has this module.
    pub fn has(&self, name: &str) -> bool {
        self.modules.iter().any(|m| m.name == name)
    }

    /// `auth,storage,chat` or `none`.
    pub fn modules_flag(&self) -> String {
        if self.modules.is_empty() {
            "none".to_string()
        } else {
            self.modules.iter().map(|m| m.name).collect::<Vec<_>>().join(",")
        }
    }

    /// The command that writes the same project without questions.
    pub fn command_line(&self) -> String {
        let mut line = format!(
            "net-backend new {} --client {} --db {} --modules {}",
            shell_path(&self.target),
            self.client.flag(),
            self.database.flag(),
            self.modules_flag()
        );
        if self.client.has_demo() {
            line.push_str(if self.demo { " --demo" } else { " --no-demo" });
        }
        if let Some(existing) = &self.existing {
            line.push_str(&format!(" --existing {}", shell_path(existing)));
        }
        if self.run {
            line.push_str(" --run");
        }
        line
    }

    /// A note for `command_line` when a path in it has characters that shells read as syntax even
    /// inside double quotes.
    pub fn command_line_note(&self) -> Option<&'static str> {
        let mut paths = std::iter::once(&self.target).chain(self.existing.as_ref());
        paths.any(|path| !shell_safe(path)).then_some(SHELL_NOTE)
    }

    /// Checks `--existing` (a folder with a `Cargo.toml`) and replaces it with its canonical path, so
    /// that the demo's sibling folder is right for `.`, `..` or a path through a link.
    pub fn resolve_existing(&mut self) -> Result<(), String> {
        let Some(game) = &self.existing else { return Ok(()) };
        if !game.join("Cargo.toml").is_file() {
            return Err(format!("--existing: `{}` has no Cargo.toml (the folder of an existing Bevy game)", game.display()));
        }
        let canonical = canonical(game).map_err(|e| format!("--existing: cannot resolve `{}`: {e}", game.display()))?;
        if canonical.parent().is_none() {
            return Err(format!("--existing: `{}` has no parent folder for the demo next to it", canonical.display()));
        }
        self.existing = Some(canonical);
        Ok(())
    }

    /// Where the Bevy demo goes when it is placed next to an existing game: a sibling folder.
    pub fn standalone_demo_dir(&self) -> Option<PathBuf> {
        let existing = self.existing.as_ref()?;
        match existing.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(parent) => Some(parent.join(STANDALONE_DEMO)),
            None => Some(PathBuf::from(STANDALONE_DEMO)),
        }
    }
}

/// The folder name of a demo placed next to an existing Bevy game.
pub const STANDALONE_DEMO: &str = "net_backend_demo";

/// `fs::canonicalize`, on Windows without the `\\?\` prefix when the path has a drive letter (the
/// form people type, which every program accepts).
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    if cfg!(windows) {
        if let Some(rest) = canonical.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
            let bytes = rest.as_bytes();
            if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' && rest.len() < 260 {
                return Ok(PathBuf::from(rest));
            }
        }
    }
    Ok(canonical)
}

/// A path for a command line: as it is when it has only plain characters, else double-quoted
/// (which keeps spaces, `&`, `(` and `;` literal in PowerShell, cmd, bash and zsh alike).
pub fn shell_path(path: &Path) -> String {
    let shown = path.display().to_string();
    let plain = !shown.is_empty() && shown.chars().all(|c| c.is_ascii_alphanumeric() || "._-/\\:".contains(c));
    if plain {
        shown
    } else {
        format!("\"{shown}\"")
    }
}

/// Characters shells read as syntax inside double quotes: `$` and `` ` `` (bash, zsh, PowerShell),
/// `!` (bash, zsh), `%` (cmd) and `"` itself.
const SHELL_SYNTAX: &str = "$`!%\"";
const SHELL_NOTE: &str = "(a path in it has `$`, `` ` ``, `!`, `%` or `\"`, which shells read as syntax: escape them for your shell)";

/// Whether `shell_path` gives the path literally in every shell.
pub fn shell_safe(path: &Path) -> bool {
    !path.to_string_lossy().contains(|c: char| SHELL_SYNTAX.contains(c) || c.is_control())
}

/// The answers given on the command line (`None`: not given).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Answers {
    pub target: Option<PathBuf>,
    pub client: Option<ClientKind>,
    pub database: Option<Database>,
    pub modules: Option<Vec<&'static Module>>,
    pub demo: Option<bool>,
    pub existing: Option<PathBuf>,
    pub run: Option<bool>,
    /// `--yes`: take the defaults for everything not given.
    pub yes: bool,
}

impl Answers {
    /// Whether any question was answered by a flag (then none is asked).
    pub fn any_flag(&self) -> bool {
        self.yes
            || self.client.is_some()
            || self.database.is_some()
            || self.modules.is_some()
            || self.demo.is_some()
            || self.existing.is_some()
            || self.run.is_some()
    }

    /// Parse the arguments after `new` (`new-server` passes `--client api` first).
    pub fn parse(args: &[String]) -> Result<Answers, String> {
        let mut answers = Answers::default();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
                _ => (arg.as_str(), None),
            };
            let mut value = |name: &str| -> Result<String, String> {
                match inline.clone().or_else(|| iter.next().cloned()) {
                    Some(v) if !v.is_empty() => Ok(v),
                    _ => Err(format!("{name} needs a value")),
                }
            };
            match flag {
                "--client" => answers.client = Some(ClientKind::parse(&value("--client")?)?),
                "--db" => answers.database = Some(Database::parse(&value("--db")?)?),
                "--modules" => {
                    let list = value("--modules")?;
                    let names: Vec<&str> = list.split(',').map(str::trim).filter(|n| !n.is_empty()).collect();
                    answers.modules = Some(if names == ["none"] { Vec::new() } else { modules::resolve(names)? });
                }
                "--existing" => answers.existing = Some(PathBuf::from(value("--existing")?)),
                "--demo" | "--no-demo" | "--run" | "--yes" | "-y" if inline.is_some() => {
                    return Err(format!("{flag} takes no value"));
                }
                "--demo" => answers.demo = Some(true),
                "--no-demo" => answers.demo = Some(false),
                "--run" => answers.run = Some(true),
                "--yes" | "-y" => answers.yes = true,
                other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
                path if answers.target.is_none() => answers.target = Some(PathBuf::from(path)),
                extra => return Err(format!("unexpected argument `{extra}`")),
            }
        }
        Ok(answers)
    }

    /// The options: the answers given, the defaults for the rest. Refuses combinations that do not
    /// exist (a demo for `protocol` / `api`, `--existing` without Bevy or without the demo).
    pub fn complete(self) -> Result<Options, String> {
        let target = self.target.ok_or("the project name is missing: net-backend new <name>")?;
        let client = self.client.unwrap_or(ClientKind::Rust);
        if self.demo == Some(true) && !client.has_demo() {
            return Err(format!("--demo works with --client rust or bevy, not `{}`", client.flag()));
        }
        if self.existing.is_some() {
            if client != ClientKind::Bevy {
                return Err("--existing works with --client bevy (the demo goes next to an existing Bevy game)".into());
            }
            if self.demo == Some(false) {
                return Err("--existing places the demo next to a game: it cannot go with --no-demo".into());
            }
        }
        let demo = client.has_demo() && self.demo.unwrap_or(true);
        Ok(Options {
            target,
            client,
            database: self.database.unwrap_or(Database::Sqlite),
            modules: self.modules.unwrap_or_else(modules::defaults),
            demo,
            existing: self.existing,
            run: self.run.unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Answers, String> {
        Answers::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    fn options(args: &[&str]) -> Result<Options, String> {
        parse(args)?.complete()
    }

    #[test]
    fn defaults() {
        let o = options(&["mygame"]).unwrap();
        assert_eq!((o.client, o.database, o.demo, o.run, o.existing.clone()), (ClientKind::Rust, Database::Sqlite, true, false, None));
        assert_eq!(o.modules_flag(), "auth,storage,chat,leaderboards,notifications,friends,groups,lobbies,matchmaking,files");
        assert_eq!(o.command_line(), "net-backend new mygame --client rust --db sqlite --modules auth,storage,chat,leaderboards,notifications,friends,groups,lobbies,matchmaking,files --demo");
        assert!(!parse(&["mygame"]).unwrap().any_flag());
        assert!(parse(&["mygame", "-y"]).unwrap().any_flag());
    }

    #[test]
    fn flags() {
        let o = options(&["--client=bevy", "my game/x", "--db", "postgres", "--modules", "chat", "--no-demo", "--run"]).unwrap();
        assert_eq!((o.client, o.database, o.demo, o.run), (ClientKind::Bevy, Database::Postgres, false, true));
        assert_eq!(o.modules_flag(), "auth,chat");
        assert_eq!(o.command_line(), "net-backend new \"my game/x\" --client bevy --db postgres --modules auth,chat --no-demo --run");
        let o = options(&["g", "--client", "api", "--modules", "none"]).unwrap();
        assert!(!o.demo && o.modules.is_empty());
        assert_eq!(o.command_line(), "net-backend new g --client api --db sqlite --modules none");
        let o = options(&["g", "--client", "bevy", "--existing", "../games/space"]).unwrap();
        assert!(o.demo);
        assert_eq!(o.standalone_demo_dir().unwrap(), Path::new("../games").join(STANDALONE_DEMO));
        assert!(o.command_line().ends_with("--demo --existing ../games/space"));
        assert!(o.command_line_note().is_none());
        for odd in ["a$b/g", "a`b/g", "100%/g", "it\"s/g", "wow!/g"] {
            assert!(options(&[odd]).unwrap().command_line_note().is_some(), "{odd}");
        }
        let o = options(&["g", "--client", "bevy", "--existing", "$HOME/space"]).unwrap();
        assert!(o.command_line_note().is_some());
    }

    /// `--existing` with `.` or `..` in it: the demo goes next to the game, never inside it.
    #[test]
    fn existing_is_resolved() {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let parent = canonical(crate_dir).unwrap().parent().unwrap().to_path_buf();
        for game in [crate_dir.join("."), crate_dir.join("src").join(".."), crate_dir.join("src").join(".").join("..").join(".")] {
            let mut o = options(&["g", "--client", "bevy", "--existing", game.to_str().unwrap()]).unwrap();
            o.resolve_existing().unwrap();
            assert_eq!(o.existing.as_deref(), Some(canonical(crate_dir).unwrap().as_path()), "{}", game.display());
            assert_eq!(o.standalone_demo_dir().unwrap(), parent.join(STANDALONE_DEMO), "{}", game.display());
            assert!(!o.existing.as_ref().unwrap().to_string_lossy().starts_with(r"\\?\"));
        }
        // The current folder (`.`, relative): resolved against it.
        let mut o = options(&["g", "--client", "bevy", "--existing", "."]).unwrap();
        if std::env::current_dir().unwrap().join("Cargo.toml").is_file() {
            o.resolve_existing().unwrap();
            let cwd = canonical(&std::env::current_dir().unwrap()).unwrap();
            assert_eq!(o.standalone_demo_dir().unwrap(), cwd.parent().unwrap().join(STANDALONE_DEMO));
        }
        let mut none = options(&["g", "--client", "bevy", "--existing", crate_dir.join("src").to_str().unwrap()]).unwrap();
        assert!(none.resolve_existing().unwrap_err().contains("no Cargo.toml"));
    }

    #[test]
    fn refusals() {
        assert!(options(&[]).unwrap_err().contains("project name is missing"));
        assert!(options(&["g", "--client", "go"]).unwrap_err().contains("unknown --client"));
        assert!(options(&["g", "--db", "oracle"]).unwrap_err().contains("unknown --db"));
        assert!(options(&["g", "--modules", "guilds"]).unwrap_err().contains("unknown module"));
        let o = options(&["g", "--modules", "oauth,files"]).unwrap();
        assert_eq!(o.modules_flag(), "auth,oauth,files");
        assert!(options(&["g", "--client", "api", "--demo"]).unwrap_err().contains("--demo works with"));
        assert!(options(&["g", "--existing", "x"]).unwrap_err().contains("--client bevy"));
        assert!(options(&["g", "--client", "bevy", "--existing", "x", "--no-demo"]).unwrap_err().contains("--no-demo"));
        assert!(options(&["g", "--db"]).unwrap_err().contains("needs a value"));
        assert!(options(&["g", "--run=yes"]).unwrap_err().contains("takes no value"));
        assert!(options(&["g", "h"]).unwrap_err().contains("unexpected argument `h`"));
        assert!(options(&["g", "--colour"]).unwrap_err().contains("unknown option"));
    }
}
