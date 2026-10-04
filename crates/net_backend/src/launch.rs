//! `--run`: build the project, start the server in a new terminal window (or in this terminal when
//! no window can be opened), wait until it answers, then start the demo (or the client) here.
//!
//! The window commands are built by pure functions; only `run` starts processes.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::generate::{demo_in_workspace, is_workspace};
use crate::options::{ClientKind, Database, Options};
use crate::style::{self, Kind};

/// The address the generated `config.toml` binds.
const SERVER: ([u8; 4], u16) = ([127, 0, 0, 1], 8080);
/// How long `run` waits for the server's first answer.
const START_WAIT: Duration = Duration::from_secs(120);

/// The desktop the installer runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
}

impl Platform {
    pub fn current() -> Platform {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        }
    }
}

/// What decides whether a window can be opened.
#[derive(Clone, Debug, Default)]
pub struct Desktop {
    /// An SSH session (`SSH_CONNECTION` / `SSH_TTY`): no window on the user's screen.
    pub ssh: bool,
    /// Linux: `DISPLAY` or `WAYLAND_DISPLAY` is set.
    pub display: bool,
    /// Linux: the terminal programs found on `PATH`.
    pub terminals: Vec<String>,
}

impl Desktop {
    pub fn detect() -> Desktop {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        Desktop {
            ssh: set("SSH_CONNECTION") || set("SSH_TTY"),
            display: set("DISPLAY") || set("WAYLAND_DISPLAY"),
            terminals: LINUX_TERMINALS.iter().filter(|t| on_path(t)).map(|t| t.to_string()).collect(),
        }
    }

    /// Whether a window (a terminal, the demo) can appear.
    pub fn has_screen(&self, platform: Platform) -> bool {
        !self.ssh && (platform != Platform::Linux || self.display)
    }
}

/// The Linux terminals tried, in order.
pub const LINUX_TERMINALS: [&str; 7] = ["x-terminal-emulator", "gnome-terminal", "konsole", "xfce4-terminal", "kitty", "alacritty", "xterm"];

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| executable(&dir.join(program))))
}

/// A file that can be started (on Unix: with an execute bit).
fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// A process to start: program, arguments, working folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub dir: PathBuf,
}

impl Launch {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args).current_dir(&self.dir);
        command
    }
}

/// `'…'` for a POSIX shell.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `"…"` for AppleScript.
fn applescript_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', r"\\").replace('"', "\\\""))
}

/// The command that opens a new terminal window titled `title`, running `command` in `dir` (an
/// absolute path); `None` when no window can be opened (SSH, no display, no known terminal).
pub fn window(platform: Platform, desktop: &Desktop, dir: &Path, title: &str, command: &[&str]) -> Option<Launch> {
    if !desktop.has_screen(platform) {
        return None;
    }
    let owned = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let dir_text = dir.display().to_string();
    match platform {
        // cmd's `start` opens a new console window in cmd's working folder (the project: the path is
        // never on cmd's command line, where `&`, `^` or `%` would be read as cmd syntax); `cmd /K`
        // keeps the window open after the server stops. The title keeps only plain characters.
        Platform::Windows => {
            let title: String = title.chars().filter(|c| c.is_ascii_alphanumeric() || " _-".contains(*c)).collect();
            let mut args = owned(&["/C", "start", &title, "cmd.exe", "/K"]);
            args.extend(owned(command));
            Some(Launch { program: "cmd.exe".into(), args, dir: dir.to_path_buf() })
        }
        Platform::MacOs => {
            let line = format!("cd {} && {}", sh_quote(&dir_text), command.iter().map(|c| sh_quote(c)).collect::<Vec<_>>().join(" "));
            let script = format!("tell application \"Terminal\" to do script {}", applescript_quote(&line));
            Some(Launch {
                program: "osascript".into(),
                args: owned(&["-e", &script, "-e", "tell application \"Terminal\" to activate"]),
                dir: dir.to_path_buf(),
            })
        }
        Platform::Linux => {
            let terminal = desktop.terminals.first()?;
            let mut args: Vec<String> = match terminal.as_str() {
                "gnome-terminal" => owned(&["--title", title, "--working-directory", &dir_text, "--"]),
                "konsole" => owned(&["--workdir", &dir_text, "-e"]),
                "xfce4-terminal" => owned(&["--title", title, "--working-directory", &dir_text, "-x"]),
                "kitty" => owned(&["--title", title, "--directory", &dir_text]),
                "alacritty" => owned(&["--title", title, "--working-directory", &dir_text, "-e"]),
                "xterm" => owned(&["-T", title, "-e"]),
                // x-terminal-emulator (Debian / Ubuntu): `-e` is the common option.
                _ => owned(&["-e"]),
            };
            args.extend(owned(command));
            Some(Launch { program: terminal.clone(), args, dir: dir.to_path_buf() })
        }
    }
}

/// The server's `cargo` arguments in the project folder.
pub fn server_command(options: &Options) -> Vec<&'static str> {
    if is_workspace(options) {
        vec!["cargo", "run", "-p", "server"]
    } else {
        vec!["cargo", "run"]
    }
}

/// What runs in this terminal after the server is up: (folder, cargo arguments, what it is).
/// Without a screen the windowed demo is replaced by the client.
pub fn follow_up(options: &Options, screen: bool) -> Option<(PathBuf, Vec<&'static str>, &'static str)> {
    let project = options.target.clone();
    match options.client {
        ClientKind::Api => None,
        ClientKind::Protocol => Some((project, vec!["cargo", "run", "-p", "client"], "the client")),
        ClientKind::Rust | ClientKind::Bevy if options.demo && screen => match options.standalone_demo_dir() {
            Some(dir) => Some((dir, vec!["cargo", "run"], "the demo")),
            None => Some((project, vec!["cargo", "run", "-p", "demo"], "the demo")),
        },
        ClientKind::Rust | ClientKind::Bevy => Some((project, vec!["cargo", "run", "-p", "client"], "the client")),
    }
}

/// The builds before anything starts: (folder, cargo arguments).
pub fn builds(options: &Options) -> Vec<(PathBuf, Vec<&'static str>)> {
    let mut list = vec![(options.target.clone(), if is_workspace(options) { vec!["cargo", "build", "--workspace"] } else { vec!["cargo", "build"] })];
    if options.demo && !demo_in_workspace(options) {
        if let Some(dir) = options.standalone_demo_dir() {
            list.push((dir, vec!["cargo", "build"]));
        }
    }
    list
}

/// A plain `GET /v1/info` on loopback: whether it answered 200.
fn answers(addr: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    if stream.write_all(b"GET /v1/info HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut head = [0u8; 16];
    stream.read(&mut head).is_ok_and(|n| head[..n].starts_with(b"HTTP/1.1 200"))
}

fn cargo(dir: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new("cargo").args(&args[1..]).current_dir(dir).status().map_err(|e| format!("cannot start cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` failed in {}", args.join(" "), dir.display()))
    }
}

/// The options with absolute paths: a new terminal window does not start in this process's
/// working folder (and a terminal's own folder option would read a relative path from there).
pub fn absolute(options: &Options) -> Result<Options, String> {
    let absolute = |path: &Path| std::path::absolute(path).map_err(|e| format!("cannot resolve `{}`: {e}", path.display()));
    let mut options = options.clone();
    options.target = absolute(&options.target)?;
    if let Some(existing) = &options.existing {
        options.existing = Some(absolute(existing)?);
    }
    Ok(options)
}

/// The server's command and the window that runs it (`None`: no window here), titled
/// `<name> server`.
pub fn server_window(options: &Options, platform: Platform, desktop: &Desktop) -> (Vec<&'static str>, Option<Launch>) {
    let name = crate::names::name_of(&options.target).unwrap_or("server");
    let command = server_command(options);
    let launch = window(platform, desktop, &options.target, &format!("{name} server"), &command);
    (command, launch)
}

/// How long `run` waits for the program that opens the window (`cmd /C start`, `osascript` and
/// gnome-terminal return at once; xterm, konsole, kitty and alacritty are the window and keep running).
const LAUNCHER_WAIT: Duration = Duration::from_secs(5);

/// Starts the program that opens the window and waits up to `LAUNCHER_WAIT` for it: `Err` with
/// what it printed when it could not be started or failed.
fn open_window(launch: &Launch) -> Result<(), String> {
    let mut child = launch
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("`{}` could not be started: {e}", launch.program))?;
    // Read its messages on a thread: a terminal that keeps running keeps the pipe open.
    let (sender, messages) = std::sync::mpsc::channel();
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            let _ = sender.send(text);
        });
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let text = messages.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
                let text = text.trim();
                return Err(format!("`{}` failed ({status}){}{text}", launch.program, if text.is_empty() { "" } else { ": " }));
            }
            Ok(None) if started.elapsed() >= LAUNCHER_WAIT => return Ok(()),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(format!("`{}`: {error}", launch.program)),
        }
    }
}

/// `--run`: build, the server in a new window (or here), then the demo / client here.
pub fn run(options: &Options) -> Result<(), String> {
    let options = &absolute(options)?;
    let platform = Platform::current();
    let desktop = Desktop::detect();
    let screen = desktop.has_screen(platform);
    match (options.client, options.demo) {
        (ClientKind::Bevy, _) => style::say(Kind::Warn, "\nBuilding now. The first Bevy build takes 5 to 10 minutes; the builds after it take seconds."),
        (ClientKind::Rust, true) => style::say(Kind::Info, "\nBuilding now. The first build of the demo takes about 1 to 2 minutes."),
        _ => style::say(Kind::Info, "\nBuilding now."),
    }
    for (dir, args) in builds(options) {
        if style::on() {
            // The same build under a spinner; cargo's messages are shown when it fails.
            let mut command = Command::new("cargo");
            command.args(&args[1..]).current_dir(&dir);
            style::spin(&format!("{} in {}", args.join(" "), dir.display()), command)?;
        } else {
            cargo(&dir, &args)?;
        }
    }

    let addr = SocketAddr::from(SERVER);
    let (command, launch) = server_window(options, platform, &desktop);
    let mut here: Option<Child> = None;
    if answers(addr) {
        style::say(Kind::Info, "A server already answers on http://127.0.0.1:8080; using it.");
    } else {
        let opened = match launch {
            Some(launch) => {
                style::say(Kind::Info, &format!("Starting the server in a new terminal window ({}).", command.join(" ")));
                match open_window(&launch) {
                    Ok(()) => true,
                    Err(problem) => {
                        style::say(Kind::Warn, &format!("No terminal window opened ({problem}): the server runs in this terminal instead."));
                        false
                    }
                }
            }
            None => {
                style::say(Kind::Warn, &format!("No new window possible here: the server runs in this terminal ({}).", command.join(" ")));
                false
            }
        };
        if !opened {
            let child = Command::new("cargo").args(&command[1..]).current_dir(&options.target).spawn().map_err(|e| format!("cannot start the server: {e}"))?;
            here = Some(child);
        }
        let started = Instant::now();
        while !answers(addr) {
            if let Some(child) = here.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!("the server stopped ({status}){}", database_hint(options.database)));
                }
            }
            if started.elapsed() > START_WAIT {
                return Err(format!("the server did not answer on http://127.0.0.1:8080 within {} s{}", START_WAIT.as_secs(), database_hint(options.database)));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        style::say(Kind::Done, "The server answers on http://127.0.0.1:8080.");
    }

    if let Some((dir, args, what)) = follow_up(options, screen) {
        if options.demo && !screen {
            style::say(Kind::Warn, "No screen here for the demo window: starting the client instead.");
        }
        style::say(Kind::Info, &format!("Starting {what} ({}).", args.join(" ")));
        cargo(&dir, &args)?;
    }
    if let Some(mut child) = here {
        style::say(Kind::Info, "The server keeps running here; Ctrl-C stops it.");
        let _ = child.wait();
    }
    Ok(())
}

fn database_hint(database: Database) -> &'static str {
    match database {
        Database::Sqlite => "",
        Database::Postgres | Database::Mysql => ": does its database exist? README.md shows how to create it",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Answers;

    fn options(args: &[&str]) -> Options {
        Answers::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>()).unwrap().complete().unwrap()
    }

    fn desktop(ssh: bool, display: bool, terminals: &[&str]) -> Desktop {
        Desktop { ssh, display, terminals: terminals.iter().map(|t| t.to_string()).collect() }
    }

    const SERVER_CMD: [&str; 4] = ["cargo", "run", "-p", "server"];

    #[test]
    fn windows_command() {
        let launch = window(Platform::Windows, &desktop(false, false, &[]), Path::new(r"C:\games\my game"), "mygame server", &SERVER_CMD).unwrap();
        assert_eq!(launch.program, "cmd.exe");
        assert_eq!(launch.args, ["/C", "start", "mygame server", "cmd.exe", "/K", "cargo", "run", "-p", "server"]);
        assert_eq!(launch.dir, Path::new(r"C:\games\my game"));
        assert!(window(Platform::Windows, &desktop(true, false, &[]), Path::new("x"), "t", &SERVER_CMD).is_none(), "SSH: no window");
        // Folders with cmd's special characters: the path is only the working folder, never on the
        // command line.
        for dir in [r"C:\games\R&D\mygame", r"C:\games\100%\mygame", r"C:\a^b\%PATH%\g"] {
            let launch = window(Platform::Windows, &desktop(false, false, &[]), Path::new(dir), "g server", &SERVER_CMD).unwrap();
            assert_eq!(launch.dir, Path::new(dir));
            assert!(launch.args.iter().all(|a| !a.contains('&') && !a.contains('%') && !a.contains('^') && !a.contains('\\')), "{:?}", launch.args);
        }
        // The title keeps plain characters only.
        let launch = window(Platform::Windows, &desktop(false, false, &[]), Path::new(r"C:\g"), "a&b %x% server", &SERVER_CMD).unwrap();
        assert_eq!(launch.args[2], "ab x server");
    }

    /// `run` resolves the paths first: every folder it starts a process in is absolute.
    #[test]
    fn relative_paths_become_absolute() {
        let cwd = std::env::current_dir().unwrap();
        for target in ["g", "games/g", "./g", "../g"] {
            let o = absolute(&options(&[target])).unwrap();
            assert!(o.target.is_absolute() && o.target.ends_with("g"), "{target}: {}", o.target.display());
            for platform in [Platform::Windows, Platform::MacOs, Platform::Linux] {
                let (command, launch) = server_window(&o, platform, &desktop(false, true, &["gnome-terminal"]));
                assert_eq!(command, SERVER_CMD);
                let launch = launch.unwrap();
                assert!(launch.dir.is_absolute(), "{target} {platform:?}");
                assert_eq!(launch.dir, o.target);
            }
            for (dir, _) in builds(&o) {
                assert!(dir.is_absolute(), "{target}");
            }
            assert!(follow_up(&o, true).unwrap().0.is_absolute());
        }
        let o = absolute(&options(&["g"])).unwrap();
        assert_eq!(o.target, cwd.join("g"));
        let launch = window(Platform::MacOs, &desktop(false, false, &[]), &o.target, "g server", &["cargo", "run"]).unwrap();
        let cd = format!("cd {}", sh_quote(&cwd.join("g").display().to_string())).replace('\\', r"\\");
        assert!(launch.args[1].contains(&cd), "{}", launch.args[1]);
        let bevy = absolute(&options(&["g", "--client", "bevy", "--existing", "games/space"])).unwrap();
        assert_eq!(builds(&bevy)[1].0, cwd.join("games").join("net_backend_demo"));
        assert!(follow_up(&bevy, true).unwrap().0.is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn launcher_failure_is_reported() {
        let launch = Launch { program: "sh".into(), args: vec!["-c".into(), "echo no terminal here >&2; exit 3".into()], dir: PathBuf::from(".") };
        let problem = open_window(&launch).unwrap_err();
        assert!(problem.contains("no terminal here") && problem.contains('3'), "{problem}");
        let ok = Launch { program: "sh".into(), args: vec!["-c".into(), "exit 0".into()], dir: PathBuf::from(".") };
        assert!(open_window(&ok).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn launcher_failure_is_reported() {
        // cmd without `start`: no window, only the exit status and the message.
        let launch = Launch { program: "cmd.exe".into(), args: vec!["/C".into(), "echo no terminal here 1>&2& exit 3".into()], dir: PathBuf::from(".") };
        let problem = open_window(&launch).unwrap_err();
        assert!(problem.contains("no terminal here") && problem.contains('3'), "{problem}");
        let ok = Launch { program: "cmd.exe".into(), args: vec!["/C".into(), "exit 0".into()], dir: PathBuf::from(".") };
        assert!(open_window(&ok).is_ok());
        let missing = Launch { program: "no-such-terminal-program".into(), args: vec![], dir: PathBuf::from(".") };
        assert!(open_window(&missing).unwrap_err().contains("could not be started"));
    }

    #[test]
    fn macos_command() {
        let launch = window(Platform::MacOs, &desktop(false, false, &[]), Path::new("/Users/a/it's \"here\""), "t", &["cargo", "run"]).unwrap();
        assert_eq!(launch.program, "osascript");
        assert_eq!(launch.args[0], "-e");
        assert_eq!(launch.args[1], r#"tell application "Terminal" to do script "cd '/Users/a/it'\\''s \"here\"' && 'cargo' 'run'""#);
        assert_eq!(launch.args[3], "tell application \"Terminal\" to activate");
    }

    #[test]
    fn linux_commands() {
        let dir = Path::new("/home/a/my game");
        let first = |terminals: &[&str]| window(Platform::Linux, &desktop(false, true, terminals), dir, "g server", &SERVER_CMD);
        assert_eq!(
            first(&["gnome-terminal", "xterm"]).unwrap().args,
            ["--title", "g server", "--working-directory", "/home/a/my game", "--", "cargo", "run", "-p", "server"]
        );
        assert_eq!(first(&["konsole"]).unwrap().args, ["--workdir", "/home/a/my game", "-e", "cargo", "run", "-p", "server"]);
        let x = first(&["x-terminal-emulator"]).unwrap();
        assert_eq!(
            (x.program.as_str(), x.args.clone(), x.dir.clone()),
            ("x-terminal-emulator", vec!["-e".to_string(), "cargo".into(), "run".into(), "-p".into(), "server".into()], dir.to_path_buf())
        );
        assert_eq!(first(&["xterm"]).unwrap().args[..3], ["-T", "g server", "-e"]);
        assert!(first(&[]).is_none(), "no terminal found");
        assert!(window(Platform::Linux, &desktop(false, false, &["xterm"]), dir, "t", &SERVER_CMD).is_none(), "no DISPLAY");
        assert!(window(Platform::Linux, &desktop(true, true, &["xterm"]), dir, "t", &SERVER_CMD).is_none(), "SSH");
    }

    #[test]
    fn what_runs() {
        let rust = options(&["g"]);
        assert_eq!(server_command(&rust), SERVER_CMD);
        assert_eq!(builds(&rust), [(PathBuf::from("g"), vec!["cargo", "build", "--workspace"])]);
        assert_eq!(follow_up(&rust, true).unwrap().1, ["cargo", "run", "-p", "demo"]);
        assert_eq!(follow_up(&rust, false).unwrap().1, ["cargo", "run", "-p", "client"], "no screen: the client");
        let api = options(&["g", "--client", "api"]);
        assert_eq!(server_command(&api), ["cargo", "run"]);
        assert!(follow_up(&api, true).is_none());
        let bevy = options(&["g", "--client", "bevy", "--existing", "games/space"]);
        assert_eq!(builds(&bevy)[1], (Path::new("games").join("net_backend_demo"), vec!["cargo", "build"]));
        let (dir, args, _) = follow_up(&bevy, true).unwrap();
        assert_eq!((dir, args), (Path::new("games").join("net_backend_demo"), vec!["cargo", "run"]));
        assert_eq!(follow_up(&options(&["g", "--client", "protocol"]), true).unwrap().1, ["cargo", "run", "-p", "client"]);
    }
}
