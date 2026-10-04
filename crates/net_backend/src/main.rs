//! `net-backend`: the net_backend installer. `net-backend new <name>` asks a few questions (or reads
//! flags) and writes a game backend that runs at once: a server, a client and an optional demo app.
//!
//! The templates are embedded in the binary; generating a project needs no network.

mod compose;
mod generate;
mod launch;
mod modules;
mod names;
mod options;
mod prompt;
mod readme;
mod style;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use generate::{Folder, CRATES_VERSION};
use options::{shell_path, Answers, ClientKind, Options};

const USAGE: &str = "\
Usage:
  net-backend new [<name>] [options]   a new project; asks what is missing (in a terminal)
  net-backend new-server <name> [options]
                                       the same as `new <name> --client api`: the server alone
  net-backend new-client <name>        a Rust client alone (for a server that exists)
  net-backend --help | --version

Options of `new` (every question is a flag; with any flag nothing is asked):
  --client rust|bevy|protocol|api      how the game talks to the server (default rust):
                                         rust      server + protocol + net_backend_client
                                         bevy      server + protocol + bevy_net_backend
                                         protocol  server + protocol, your HTTP / WebSocket library
                                         api       the server only (any language: API.md)
  --db sqlite|postgres|mysql           the database (default sqlite)
  --modules <list>|none                the server modules, comma-separated; each one adds auth:
                                         auth, storage, chat, leaderboards, notifications,
                                         friends, groups, oauth, lobbies, matchmaking, files
                                       (default: all but oauth, which needs a provider set up)
  --demo | --no-demo                   a demo app with buttons and a log (rust, bevy; default yes)
  --existing <path>                    bevy: put the demo next to this existing game, in
                                       net_backend_demo/ (the game's files are not changed)
  --run                                build, start the server in a new terminal window, then
                                       the demo (or the client) in this one
  -y, --yes                            no questions: the defaults for everything not given
  --no-color                           plain text without colors (any command; NO_COLOR does
                                       the same)

<name> is the folder to create; a path works too, its last part is the project name (a-z, 0-9,
`-` and `_`, starting with a letter). An existing folder must be empty.";

fn help() -> String {
    format!(
        "net-backend {CRATES_VERSION}: the net_backend installer. The projects use net_backend_server,\n\
         net_backend_protocol and net_backend_client {CRATES_VERSION}.\n\n\
         {USAGE}\n\n\
         Examples (the same in PowerShell, cmd, bash and zsh):\n\
         \x20 net-backend new mygame\n\
         \x20 net-backend new mygame --client rust --db sqlite --modules auth,storage,chat --demo --run\n\
         \x20 net-backend new mygame --client bevy --db postgres --modules chat,friends,lobbies --no-demo\n\
         \x20 net-backend new mygame --client rust --db sqlite --modules storage,leaderboards,oauth\n\
         \x20 net-backend new mygame --client api --db mysql --modules none\n\n\
         More: https://net-backend.com"
    )
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    New(Answers),
    NewClient(PathBuf),
}

/// Parse the arguments (without the program name).
fn parse(args: &[String]) -> Result<Command, String> {
    let Some(first) = args.first() else {
        return Ok(Command::Help);
    };
    let rest = &args[1..];
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Command::Help);
    }
    match first.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "-V" | "--version" | "version" => Ok(Command::Version),
        "new" => Ok(Command::New(Answers::parse(rest)?)),
        "new-server" => {
            let mut answers = Answers::parse(rest)?;
            if answers.client.is_some_and(|c| c != ClientKind::Api) {
                return Err("new-server writes the server alone: leave out --client (or use `new`)".into());
            }
            if answers.target.is_none() {
                return Err("`new-server` needs the project name: net-backend new-server <name>".into());
            }
            answers.client = Some(ClientKind::Api);
            Ok(Command::New(answers))
        }
        "new-client" => match rest {
            [] => Err("`new-client` needs the project name: net-backend new-client <name>".into()),
            [arg] if arg.starts_with('-') => Err(format!("unknown option `{arg}`")),
            [path] => Ok(Command::NewClient(PathBuf::from(path))),
            [_, extra, ..] => Err(format!("unexpected argument `{extra}`")),
        },
        other => Err(format!("unknown command `{other}`")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = match std::env::args_os().skip(1).map(|a| a.into_string()).collect() {
        Ok(args) => args,
        Err(arg) => {
            eprintln!("error: `{}` is not valid UTF-8", arg.to_string_lossy());
            return ExitCode::from(2);
        }
    };
    // `--no-color` works with every command and is no answer to a question.
    let no_color = args.iter().any(|a| a == "--no-color");
    let args: Vec<String> = args.into_iter().filter(|a| a != "--no-color").collect();
    style::init(no_color);
    let command = match parse(&args) {
        Ok(command) => command,
        Err(problem) => {
            style::error(&problem, &format!("\n\n{USAGE}"));
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Help => {
            println!("{}", help());
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("net-backend {CRATES_VERSION}");
            ExitCode::SUCCESS
        }
        Command::NewClient(target) => match generate::client_only(&target).and_then(|folders| generate::write(&folders)) {
            Ok(_) if style::on() => {
                println!("\n  {}\n", style::title(true, name(&target), &format!("a Rust client, net_backend {CRATES_VERSION}")));
                println!("{}", style::heading(true, "Next steps"));
                println!("{}", style::step(true, &format!("cd {}", shell_path(&target)), ""));
                println!("{}\n", style::step(true, "cargo run", "the client, against http://127.0.0.1:8080"));
                ExitCode::SUCCESS
            }
            Ok(_) => {
                println!("Created `{}` (a Rust client, net_backend {CRATES_VERSION}).\n\nNext:\n  cd {}\n  cargo run              # the client, against http://127.0.0.1:8080", name(&target), shell_path(&target));
                ExitCode::SUCCESS
            }
            Err(error) => fail(&error.to_string()),
        },
        Command::New(answers) => new(answers),
    }
}

fn fail(problem: &str) -> ExitCode {
    style::error(problem, "");
    ExitCode::FAILURE
}

fn name(target: &Path) -> &str {
    names::name_of(target).unwrap_or_default()
}

fn new(answers: Answers) -> ExitCode {
    let answers = if !answers.any_flag() && prompt::interactive() {
        if style::on() {
            print!("{}", style::banner(true, CRATES_VERSION));
        }
        match prompt::ask(answers) {
            Ok(answers) => answers,
            Err(problem) => return fail(&problem),
        }
    } else {
        answers
    };
    let mut options = match answers.complete() {
        Ok(options) => options,
        Err(problem) => {
            style::error(&problem, &format!("\n\n{USAGE}"));
            return ExitCode::from(2);
        }
    };
    if let Err(problem) = options.resolve_existing() {
        return fail(&problem);
    }
    let folders = match generate::plan(&options) {
        Ok(folders) => folders,
        Err(error) => return fail(&error.to_string()),
    };
    if let Err(error) = generate::write(&folders) {
        return fail(&error.to_string());
    }
    if style::on() {
        print!("{}", styled_summary(true, &options, &folders));
    } else {
        println!("{}", summary(&options, &folders));
    }
    if options.run {
        if let Err(problem) = launch::run(&options) {
            return fail(&problem);
        }
    }
    ExitCode::SUCCESS
}

/// The message after a project was written: what it is, the commands to run it, and the one-line
/// command that writes the same project.
fn summary(options: &Options, folders: &[Folder]) -> String {
    let mut text = format!("\nCreated `{}` (net_backend {CRATES_VERSION}).\n", name(&options.target));
    for folder in &folders[1..] {
        text.push_str(&format!("Created the demo in {} (the game's files are not changed).\n", shell_path(&folder.dir)));
    }
    if !options.run {
        text.push_str("\nNext:\n");
        for step in next_steps(options) {
            match step {
                Next::Run(command, "") => text.push_str(&format!("  {command}\n")),
                Next::Run(command, comment) => text.push_str(&format!("  {command:<23}# {comment}\n")),
                Next::Note(note) | Next::Database(note) => text.push_str(&format!("  ({note})\n")),
            }
        }
    }
    if options.client == ClientKind::Bevy && options.existing.is_some() {
        text.push_str("\nTo use the client in the game, add to its Cargo.toml [dependencies]:\n");
        for line in readme::bevy_dependency_lines().lines() {
            text.push_str(&format!("  {line}\n"));
        }
    }
    text.push_str(&format!("\nThe same setup without questions:\n  {}\n", options.command_line()));
    if let Some(note) = options.command_line_note() {
        text.push_str(&format!("  {note}\n"));
    }
    text.push_str("\nREADME.md in the project describes the rest.");
    text
}

/// A line of "Next": a command with its comment, a note, or the database note.
enum Next {
    Run(String, &'static str),
    Note(String),
    Database(String),
}

/// The commands that run the new project.
fn next_steps(options: &Options) -> Vec<Next> {
    let mut steps = vec![Next::Run(format!("cd {}", shell_path(&options.target)), ""), Next::Run("cargo run".into(), "the server on http://127.0.0.1:8080")];
    match options.client {
        ClientKind::Api => {}
        _ if generate::demo_in_workspace(options) => {
            steps.push(Next::Run("cargo run -p demo".into(), "the demo, in a second terminal"));
            steps.push(Next::Note("the demo with Steam: cargo run -p demo --features steam; see README.md".into()));
        }
        _ => steps.push(Next::Run("cargo run -p client".into(), "the client, in a second terminal")),
    }
    if let Some(dir) = options.standalone_demo_dir() {
        steps.push(Next::Note(format!("the demo: cargo run in {}", shell_path(&dir))));
    }
    if options.database != options::Database::Sqlite {
        steps.push(Next::Database(format!("{} first: README.md shows how to create the database", options.database.name())));
    }
    steps
}

/// The `✓` lines for what was written: the parts of the project folder and the demo beside a game.
fn written_lines(on: bool, options: &Options, folders: &[Folder]) -> Vec<String> {
    let files: Vec<&str> = folders[0].files.iter().map(|(path, _)| path.as_str()).collect();
    let has = |path: &str| files.contains(&path);
    let under = |dir: &str| files.iter().any(|f| f.starts_with(dir));
    let database = options.database.name();
    let modules = match options.modules.len() {
        0 => "the core server".to_string(),
        1 => "1 module".to_string(),
        n => format!("{n} modules"),
    };
    let item = |what: &str, note: &str| (what.to_string(), note.to_string());
    let mut lines = Vec::new();
    if has("Cargo.toml") && under("server/") {
        lines.push(item("Cargo.toml", "the workspace"));
    }
    if under("server/") {
        lines.push(item("server/", &format!("the game server: {database}, {modules}")));
    } else if has("src/main.rs") {
        lines.push(item("Cargo.toml, src/", &format!("the game server: {database}, {modules}")));
    }
    if under("client/") {
        let what = match options.client {
            ClientKind::Rust => "a client on net_backend_client",
            ClientKind::Bevy => "a client on bevy_net_backend",
            _ => "a client on net_backend_protocol",
        };
        lines.push(item("client/", what));
    }
    if under("demo/") {
        let what = if options.client == ClientKind::Bevy { "the Bevy demo app" } else { "the egui demo app" };
        lines.push(item("demo/", what));
    }
    let configs: Vec<&str> = ["config.toml", "config.docker.toml"].into_iter().filter(|f| has(f)).collect();
    lines.push(item(&configs.join(", "), "the server's settings"));
    let docker: Vec<&str> = ["Dockerfile", "compose.yaml", ".dockerignore"].into_iter().filter(|f| has(f)).collect();
    let deploy = if options.database == options::Database::Sqlite { "SQLite".to_string() } else { format!("{database} + Caddy") };
    lines.push(item(&docker.join(", "), &format!("Docker deploy ({deploy})")));
    lines.push(item("README.md, .gitignore", "how to run and deploy it"));
    for folder in &folders[1..] {
        let dir = folder.dir.file_name().map_or_else(|| shell_path(&folder.dir), |n| format!("{}/", n.to_string_lossy()));
        lines.push(item(&dir, "the Bevy demo beside the game (the game's files are not changed)"));
    }
    let width = lines.iter().map(|(what, _)| what.chars().count()).max().unwrap_or(0);
    lines.into_iter().map(|(what, note)| style::check(on, &format!("{what:<width$}"), &note)).collect()
}

/// The styled version of [`summary`]: what was written, a box with the choices, the one-line
/// command and the next steps.
fn styled_summary(on: bool, options: &Options, folders: &[Folder]) -> String {
    let mut text = String::from("\n");
    for line in written_lines(on, options, folders) {
        text.push_str(&format!("{line}\n"));
    }
    let project = std::path::absolute(&options.target).unwrap_or_else(|_| options.target.clone());
    let client = options.client.label().split("  ").next().unwrap_or_default().to_string();
    let modules =
        if options.modules.is_empty() { "none (the core server)".to_string() } else { options.modules.iter().map(|m| m.name).collect::<Vec<_>>().join(", ") };
    let demo = match (options.demo, options.standalone_demo_dir()) {
        (false, _) => "no".to_string(),
        (true, Some(dir)) => format!("Bevy app in {}", dir.display()),
        (true, None) if options.client == ClientKind::Bevy => "Bevy app (demo/)".to_string(),
        (true, None) => "egui window (demo/)".to_string(),
    };
    let rows = [
        ("Project", project.display().to_string()),
        ("Client", client),
        ("Database", options.database.name().to_string()),
        ("Modules", modules),
        ("Demo", demo),
    ];
    text.push('\n');
    text.push_str(&style::summary_box(on, &style::title(on, name(&options.target), &format!("net_backend {CRATES_VERSION}")), &rows, style::box_width()));
    if options.client == ClientKind::Bevy && options.existing.is_some() {
        text.push_str(&format!("\n{}\n", style::heading(on, "To use the client in the game, add to its Cargo.toml [dependencies]")));
        for line in readme::bevy_dependency_lines().lines() {
            text.push_str(&format!("{}\n", style::note(on, &format!("  {line}"))));
        }
    }
    text.push_str(&format!("\n{}\n{}\n", style::heading(on, "The same setup without questions"), style::command(on, &options.command_line())));
    if let Some(note) = options.command_line_note() {
        text.push_str(&format!("{}\n", style::note(on, note)));
    }
    if !options.run {
        text.push_str(&format!("\n{}\n", style::heading(on, "Next steps")));
        let mut database = None;
        let steps = next_steps(options);
        let width = steps.iter().map(|s| if let Next::Run(command, _) = s { command.chars().count() } else { 0 }).max().unwrap_or(0);
        for step in steps {
            match step {
                Next::Run(command, comment) => {
                    text.push_str(&format!("{}\n", style::step(on, &if comment.is_empty() { command } else { format!("{command:<width$}") }, comment)))
                }
                Next::Note(note) => text.push_str(&format!("{}\n", style::note(on, &format!("  ({note})")))),
                Next::Database(note) => database = Some(note),
            }
        }
        if let Some(note) = database {
            text.push_str(&format!("\n{}\n", style::badged(on, style::Kind::Warn, &note)));
        }
    }
    text.push_str(&format!("\n{}\n\n", style::note(on, "README.md in the project describes the rest.")));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn commands() {
        assert_eq!(parse(&args(&[])), Ok(Command::Help));
        for help in ["--help", "-h", "help"] {
            assert_eq!(parse(&args(&[help])), Ok(Command::Help));
        }
        assert_eq!(parse(&args(&["new", "g", "--help"])), Ok(Command::Help));
        for version in ["--version", "-V", "version"] {
            assert_eq!(parse(&args(&[version])), Ok(Command::Version));
        }
        let Ok(Command::New(answers)) = parse(&args(&["new", "mygame"])) else { panic!() };
        assert_eq!(answers.target, Some(PathBuf::from("mygame")));
        let Ok(Command::New(answers)) = parse(&args(&["new-server", "s", "--db", "mysql"])) else { panic!() };
        assert_eq!((answers.client, answers.target.clone()), (Some(ClientKind::Api), Some(PathBuf::from("s"))));
        assert!(parse(&args(&["new-server"])).unwrap_err().contains("needs the project name"));
        assert!(parse(&args(&["new-server", "s", "--client", "rust"])).unwrap_err().contains("server alone"));
        assert_eq!(parse(&args(&["new-client", "c"])), Ok(Command::NewClient("c".into())));
        assert!(parse(&args(&["new-client", "c", "d"])).unwrap_err().contains("unexpected argument `d`"));
        assert!(parse(&args(&["init"])).unwrap_err().contains("unknown command `init`"));
    }

    #[test]
    fn messages() {
        assert!(help().contains(CRATES_VERSION));
        let options = Answers::parse(&args(&["my games/mygame", "-y"])).unwrap().complete().unwrap();
        let folders = generate::plan(&options).unwrap();
        let text = summary(&options, &folders);
        assert!(text.contains("cd \"my games/mygame\"") && text.contains("cargo run -p demo"));
        let defaults = "auth,storage,chat,leaderboards,notifications,friends,groups,lobbies,matchmaking,files";
        assert!(text.contains(&format!("net-backend new \"my games/mygame\" --client rust --db sqlite --modules {defaults} --demo")));
        // --help lists every module of the table, in its order.
        let listed: String = USAGE.lines().skip_while(|l| !l.contains("--modules")).skip(1).take(2).map(str::trim).collect::<Vec<_>>().join(" ");
        assert_eq!(listed, modules::names_list());
        let options = Answers::parse(&args(&["g", "--client", "bevy", "--existing", "../space"])).unwrap().complete().unwrap();
        let text = summary(&options, &generate::plan(&options).unwrap());
        assert!(text.contains("bevy_net_backend = { version = ") && text.contains("net_backend_protocol = { version = "));
    }

    /// Plain output (no terminal, `NO_COLOR`, `--no-color`) is the text of the installer before
    /// the styled output existed, byte for byte.
    #[test]
    fn plain_summary_snapshot() {
        let plan = |list: &[&str]| {
            let options = Answers::parse(&args(list)).unwrap().complete().unwrap();
            let folders = generate::plan(&options).unwrap();
            summary(&options, &folders)
        };
        let defaults = "auth,storage,chat,leaderboards,notifications,friends,groups,lobbies,matchmaking,files";
        assert_eq!(
            plan(&["my games/x8", "-y"]),
            format!(
                "\nCreated `x8` (net_backend {CRATES_VERSION}).\n\nNext:\n  cd \"my games/x8\"\n  cargo run              # the server on http://127.0.0.1:8080\n  cargo run -p demo      # the demo, in a second terminal\n  (the demo with Steam: cargo run -p demo --features steam; see README.md)\n\nThe same setup without questions:\n  net-backend new \"my games/x8\" --client rust --db sqlite --modules {defaults} --demo\n\nREADME.md in the project describes the rest."
            )
        );
        assert_eq!(
            plan(&["a2", "--client", "bevy", "--db", "postgres", "--modules", "chat,friends,lobbies", "--no-demo"]),
            format!(
                "\nCreated `a2` (net_backend {CRATES_VERSION}).\n\nNext:\n  cd a2\n  cargo run              # the server on http://127.0.0.1:8080\n  cargo run -p client    # the client, in a second terminal\n  (PostgreSQL first: README.md shows how to create the database)\n\nThe same setup without questions:\n  net-backend new a2 --client bevy --db postgres --modules auth,chat,friends,lobbies --no-demo\n\nREADME.md in the project describes the rest."
            )
        );
        assert_eq!(
            plan(&["a3", "--client", "api", "--db", "mysql", "--modules", "none"]),
            format!(
                "\nCreated `a3` (net_backend {CRATES_VERSION}).\n\nNext:\n  cd a3\n  cargo run              # the server on http://127.0.0.1:8080\n  (MySQL first: README.md shows how to create the database)\n\nThe same setup without questions:\n  net-backend new a3 --client api --db mysql --modules none\n\nREADME.md in the project describes the rest."
            )
        );
    }

    /// The styled summary says the same things, and without colour it holds no escape sequence.
    #[test]
    fn styled_summary_facts() {
        for list in [&["g", "-y"][..], &["g", "--client", "bevy", "--existing", "../space"], &["g", "--client", "api", "--db", "mysql", "--modules", "none"]] {
            let options = Answers::parse(&args(list)).unwrap().complete().unwrap();
            let folders = generate::plan(&options).unwrap();
            let plain = styled_summary(false, &options, &folders);
            assert!(!plain.contains('\x1b'), "{plain}");
            assert_eq!(style::strip_ansi(&styled_summary(true, &options, &folders)), plain);
            assert!(plain.contains(&options.command_line()) && plain.contains("Next steps") && plain.contains("cargo run"), "{plain}");
            assert!(plain.contains(options.database.name()) && plain.contains("✓ config.toml"), "{plain}");
            if options.client == ClientKind::Api {
                assert!(plain.contains("none (the core server)") && plain.contains("WARN") && !plain.contains("client/"), "{plain}");
            }
        }
    }

    #[test]
    fn no_color_flag_in_help() {
        assert!(help().contains("--no-color"));
    }
}
