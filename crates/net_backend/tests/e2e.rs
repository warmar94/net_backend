//! The installer from the outside: the `net-backend` binary writes projects, the projects build
//! against this repository's crates (a `[patch.crates-io]` added for the test), the generated
//! servers run headless on loopback (SQLite) and the generated clients run against them.
//!
//! The fast tests run by default (flags, the non-interactive path, `--existing`). The slow ones are
//! ignored; run them with `cargo test -p net_backend --test e2e -- --ignored`:
//! - `generated_projects_build_and_run`: the default project (Rust client + egui demo, built, not
//!   opened), a server-only and a client-only project; servers and clients run.
//! - `every_combination_checks`: a matrix of client paths x databases x module sets (the server with
//!   no module, each module alone, all of them; every path and database with all modules),
//!   `cargo check` + rustfmt.
//! - `bevy_projects_build_and_run`: the Bevy client (headless, run) and the Bevy demo (built, not
//!   opened), in the project and next to an existing game.
//! - `published_crates_build_and_run`: a release check against crates.io (no patch).
//!
//! Scratch folders live under the target folder (`CARGO_TARGET_DIR`), never the system temp folder.
//! The generated projects build in their own target folders (the outer `cargo test` holds the lock
//! of its own), or all in `NET_BACKEND_E2E_TARGET` when it is set (one shared folder: run the test
//! binary itself then, not through `cargo test`, with `--test-threads=1`: the projects' binaries
//! share names). Bevy projects are patched to the local
//! bevy_net_backend too when it sits next to this repository. Needs crates.io (Cargo downloads).

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long a generated server may take to answer `/v1/info` after it started.
const SERVER_START: Duration = Duration::from_secs(120);
/// How long one client run may take (it listens to the chat for 10 s).
const CLIENT_RUN: Duration = Duration::from_secs(180);
/// The local crates a generated project is patched to.
const LOCAL_CRATES: [&str; 3] = ["net_backend_protocol", "net_backend_server", "net_backend_client"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").canonicalize().unwrap()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| repo_root().join("target"))
}

/// A fresh scratch folder `<base>/<test>/with space` (a path with a space); the base is
/// `NET_BACKEND_E2E_DIR` when set, else `<target>/cli-e2e`.
fn scratch(test: &str) -> PathBuf {
    let base = std::env::var_os("NET_BACKEND_E2E_DIR").map(PathBuf::from).unwrap_or_else(|| target_dir().join("cli-e2e"));
    let dir = base.join(test).join("with space");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The target folder of the generated projects of one test (`NET_BACKEND_E2E_TARGET`: one folder
/// for all).
fn build_target(test: &str) -> PathBuf {
    match std::env::var_os("NET_BACKEND_E2E_TARGET") {
        Some(shared) => PathBuf::from(shared),
        None => target_dir().join("cli-e2e").join(test).join("target"),
    }
}

/// The default modules (`-y`, no `--modules`): every module but oauth.
const DEFAULT_MODULES: [&str; 10] = ["auth", "storage", "chat", "leaderboards", "notifications", "friends", "groups", "lobbies", "matchmaking", "files"];
/// Every module, in the installer's order.
const ALL_MODULES: [&str; 11] = ["auth", "storage", "chat", "leaderboards", "notifications", "friends", "groups", "oauth", "lobbies", "matchmaking", "files"];

/// A path as a TOML literal string (single quotes: Windows backslashes stay as they are).
fn toml_path(path: &Path) -> String {
    let text = path.display().to_string();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();
    assert!(!text.contains('\''), "{text}");
    format!("'{text}'")
}

/// Run `net-backend` with these arguments in `dir`, stdin closed (a non-interactive terminal).
fn net_backend_raw(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_net-backend")).args(args).current_dir(dir).stdin(Stdio::null()).output().unwrap()
}

/// The same; panics unless it succeeds; its standard output.
fn net_backend(dir: &Path, args: &[&str]) -> String {
    let output = net_backend_raw(dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "net-backend {args:?}: {stdout}{}", String::from_utf8_lossy(&output.stderr));
    stdout
}

/// Point a generated project at this repository's crates (test-only; the generated files are
/// checked before this). `standalone`: an empty `[workspace]` keeps a single-package project out
/// of any workspace above it.
fn patch_project(project: &Path, standalone: bool) {
    let root = repo_root();
    let mut extra = String::from("\n# Added by the end-to-end test.\n");
    if standalone {
        extra.push_str("[workspace]\n\n");
    }
    extra.push_str("[patch.crates-io]\n");
    for name in LOCAL_CRATES {
        extra.push_str(&format!("{name} = {{ path = {} }}\n", toml_path(&root.join("crates").join(name))));
    }
    // A Bevy project: the local bevy_net_backend too (its own repository, next to this one).
    let bevy = root.join("..").join("bevy_net_backend");
    let manifests = ["Cargo.toml", "client/Cargo.toml", "demo/Cargo.toml"].map(|m| fs::read_to_string(project.join(m)).unwrap_or_default());
    if manifests.iter().any(|m| m.contains("bevy_net_backend =")) && bevy.join("Cargo.toml").is_file() {
        extra.push_str(&format!("bevy_net_backend = {{ path = {} }}\n", toml_path(&bevy.canonicalize().unwrap())));
    }
    let manifest = project.join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str(&extra);
    fs::write(&manifest, text).unwrap();
    // The repository's lockfile as the starting point: the same dependency versions as its own CI.
    fs::copy(root.join("Cargo.lock"), project.join("Cargo.lock")).unwrap();
}

fn cargo_output(project: &Path, target: &Path, args: &[&str]) -> Output {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(args)
        .current_dir(project)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_INCREMENTAL", "0")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn cargo(project: &Path, target: &Path, args: &[&str]) {
    let output = cargo_output(project, target, args);
    assert!(
        output.status.success(),
        "cargo {args:?} in {}:\n{}{}",
        project.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The generated Rust code is formatted as default rustfmt formats it (edition 2024).
fn rustfmt_check(project: &Path) {
    let mut files = Vec::new();
    let mut stack = vec![project.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "target") {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    // An empty configuration: rustfmt's defaults, whatever lies in the folders above.
    let config = project.join("..").join("rustfmt-defaults.toml");
    fs::write(&config, "").unwrap();
    let output = Command::new("rustfmt").args(["--check", "--edition", "2024", "--config-path"]).arg(&config).args(&files).output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "rustfmt --check in {}:\n{stdout}{stderr}", project.display());
}

/// Kills the process when dropped (a failing assertion never leaves a server running).
struct Running {
    child: Child,
    log: PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Running {
    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A plain HTTP/1.1 GET on loopback: (status, body).
fn http_get(port: u16, path: &str) -> std::io::Result<(u16, String)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes())?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    let status = answer.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = answer.split_once("\r\n\r\n").map(|(_, body)| body.to_string()).unwrap_or_default();
    Ok((status, body))
}

/// A plain HTTP/1.1 POST with a JSON body on loopback: (status, body).
fn http_post(port: u16, path: &str) -> std::io::Result<(u16, String)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let body = r#"{"id_token":"eyJhbGciOiJSUzI1NiJ9.e30.c2ln"}"#;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    let status = answer.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((status, answer.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default()))
}

fn exe(target: &Path, name: &str) -> PathBuf {
    target.join("debug").join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Start a generated server in its project folder (where its config.toml is) on `port`; wait until
/// `GET /v1/info` answers with these modules.
fn start_server(binary: &Path, project: &Path, port: u16, logs: &Path, modules: &[&str]) -> Running {
    let log = logs.join(format!("server-{port}.log"));
    let file = fs::File::create(&log).unwrap();
    let child = Command::new(binary)
        .current_dir(project)
        // Only the port differs from the generated config.toml (another server may use 8080).
        .env("NBS__SERVER__BIND", format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone().unwrap()))
        .stderr(Stdio::from(file))
        .spawn()
        .unwrap();
    let mut server = Running { child, log };
    let started = Instant::now();
    loop {
        if let Ok((200, body)) = http_get(port, "/v1/info") {
            for module in modules {
                assert!(body.contains(&format!("\"{module}\"")), "/v1/info without {module}: {body}");
            }
            assert!(body.contains("\"protocol\""), "{body}");
            return server;
        }
        if let Ok(Some(status)) = server.child.try_wait() {
            panic!("the server exited with {status}:\n{}", server.log());
        }
        assert!(started.elapsed() < SERVER_START, "no /v1/info answer within {SERVER_START:?}:\n{}", server.log());
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Run a generated client to its end (bounded); its output (stdout + stderr).
fn run_client(binary: &Path, configure: impl FnOnce(&mut Command), logs: &Path, label: &str, expected: &[&str]) -> String {
    let log = logs.join(format!("client-{label}.log"));
    let file = fs::File::create(&log).unwrap();
    let mut command = Command::new(binary);
    command
        .env_remove("NET_BACKEND_URL")
        .env_remove("NET_BACKEND_EMAIL")
        .env_remove("NET_BACKEND_PASSWORD")
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone().unwrap()))
        .stderr(Stdio::from(file));
    configure(&mut command);
    let mut client = Running { child: command.spawn().unwrap(), log };
    let started = Instant::now();
    let status = loop {
        if let Some(status) = client.child.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < CLIENT_RUN, "the client ran longer than {CLIENT_RUN:?}:\n{}", client.log());
        std::thread::sleep(Duration::from_millis(250));
    };
    let output = client.log();
    assert!(status.success(), "the client ({label}) failed with {status}:\n{output}");
    for line in expected {
        assert!(output.contains(line), "the client ({label}) printed no `{line}`:\n{output}");
    }
    output
}

/// Every file under `dir` with its content (a snapshot to compare).
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(folder) = stack.pop() {
        for entry in fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}

// ---- fast tests (default) ---------------------------------------------------------------------

#[test]
fn flags_and_the_non_interactive_path() {
    let dir = scratch("flags");
    // No flag, stdin not a terminal: no question, the defaults, and the one-line command.
    let out = net_backend(&dir, &["new", "plain"]);
    assert!(out.contains(&format!("net-backend new plain --client rust --db sqlite --modules {} --demo", DEFAULT_MODULES.join(","))), "{out}");
    for file in ["Cargo.toml", "server/src/main.rs", "client/src/main.rs", "demo/src/main.rs", "config.docker.toml"] {
        assert!(dir.join("plain").join(file).is_file(), "plain/{file}");
    }
    // Flags: exactly what they say.
    let out = net_backend(&dir, &["new", "flagged", "--client", "protocol", "--db=mysql", "--modules", "storage"]);
    assert!(out.contains("--client protocol --db mysql --modules auth,storage"), "{out}");
    let project = dir.join("flagged");
    assert!(!project.join("demo").exists() && !project.join("config.docker.toml").exists());
    assert!(fs::read_to_string(project.join("config.toml")).unwrap().contains("mysql://game:"));
    assert!(fs::read_to_string(project.join("compose.yaml")).unwrap().contains("image: mysql:8.4"));
    assert!(!fs::read_to_string(project.join("server/src/main.rs")).unwrap().contains("Chat"));
    // new-server: the server alone at the folder's root.
    net_backend(&dir, &["new-server", "solo", "--db", "postgres", "--modules", "none"]);
    assert!(dir.join("solo").join("src").join("main.rs").is_file() && !dir.join("solo").join("client").exists());
    // new-client: the Rust client alone.
    net_backend(&dir, &["new-client", "viewer"]);
    assert!(dir.join("viewer").join("src").join("main.rs").is_file());
    // Errors: no name without a terminal, a bad flag, a demo for the server-only path, a used folder.
    let missing = net_backend_raw(&dir, &["new", "--client", "rust"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("project name is missing"));
    assert_eq!(net_backend_raw(&dir, &["new", "x", "--client", "go"]).status.code(), Some(2));
    assert_eq!(net_backend_raw(&dir, &["new", "x", "--client", "api", "--demo"]).status.code(), Some(2));
    let before = snapshot(&dir.join("plain"));
    let again = net_backend_raw(&dir, &["new", "plain", "--client", "api"]);
    assert_eq!(again.status.code(), Some(1));
    assert_eq!(snapshot(&dir.join("plain")), before, "a used folder is never changed");
    // --help lists every flag.
    let help = net_backend(&dir, &["--help"]);
    for flag in
        ["--client rust|bevy|protocol|api", "--db sqlite|postgres|mysql", "--modules <list>|none", "--demo | --no-demo", "--existing <path>", "--run", "--yes"]
    {
        assert!(help.contains(flag), "--help without {flag}");
    }
    assert_eq!(net_backend(&dir, &["--version"]).trim(), format!("net-backend {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn existing_bevy_game_is_never_changed() {
    let dir = scratch("existing");
    // A scratch Bevy game.
    let game = dir.join("space game");
    fs::create_dir_all(game.join("src")).unwrap();
    fs::write(game.join("Cargo.toml"), "[package]\nname = \"space\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nbevy = \"=0.19.0\"\n").unwrap();
    fs::write(game.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    let before = snapshot(&dir);

    let out = net_backend(&dir, &["new", "mygame", "--client", "bevy", "--existing", "space game"]);
    assert!(out.contains("bevy_net_backend = { version = ") && out.contains("net_backend_protocol = { version = "), "{out}");
    // The game's folder is resolved: the command and the summary name its full path.
    let space = fs::canonicalize(&game).unwrap();
    let space = space.to_str().unwrap().strip_prefix(r"\\?\").unwrap_or(space.to_str().unwrap()).to_string();
    assert!(out.contains(&format!("--existing \"{space}\"")), "{out}");

    // Only the two new folders appeared; every file that was there is unchanged.
    let after = snapshot(&dir);
    for (path, content) in &before {
        assert_eq!(after.get(path), Some(content), "{} changed", path.display());
    }
    for path in after.keys().filter(|p| !before.contains_key(*p)) {
        assert!(path.starts_with(dir.join("mygame")) || path.starts_with(dir.join("net_backend_demo")), "unexpected new file {}", path.display());
    }
    let demo = dir.join("net_backend_demo");
    assert!(fs::read_to_string(demo.join("Cargo.toml")).unwrap().contains("\n[workspace]\n"));
    assert!(demo.join("src").join("main.rs").is_file() && !dir.join("mygame").join("demo").exists());

    // A folder that is not a Cargo project is refused, and nothing is written.
    let refused = net_backend_raw(&dir, &["new", "other", "--client", "bevy", "--existing", "nowhere"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(!dir.join("other").exists());
    // A second demo next to the same game: the folder is used, nothing is written.
    let twice = net_backend_raw(&dir, &["new", "third", "--client", "bevy", "--existing", "space game"]);
    assert_eq!(twice.status.code(), Some(1));
    assert!(!dir.join("third").exists());

    // `.` (in the game's folder) and `..` (in one of its folders): the demo still goes beside the
    // game, never inside it.
    for (game, run_in, existing) in [("dot", "dot", "."), ("dotdot", "dotdot/src", "..")] {
        let base = dir.join(format!("{game}-games"));
        let game = base.join(game);
        fs::create_dir_all(game.join("src")).unwrap();
        fs::write(game.join("Cargo.toml"), "[package]\nname = \"space\"\nversion = \"0.1.0\"\nedition = \"2024\"\n").unwrap();
        let before = snapshot(&game);
        net_backend(&base.join(run_in), &["new", if existing == "." { "../srv" } else { "../../srv" }, "--client", "bevy", "--existing", existing]);
        assert_eq!(snapshot(&game), before, "{existing}: the game changed");
        assert!(base.join("net_backend_demo").join("Cargo.toml").is_file(), "{existing}: no demo beside the game");
        assert!(base.join("srv").join("Cargo.toml").is_file(), "{existing}");
    }
}

// ---- slow tests (ignored) ---------------------------------------------------------------------

#[test]
#[ignore = "slow: builds generated projects (run with --ignored)"]
fn generated_projects_build_and_run() {
    let dir = scratch("run");
    let target = build_target("run");
    let logs = dir.join("logs");
    fs::create_dir_all(&logs).unwrap();
    net_backend(&dir, &["new", "fullgame", "-y"]);
    // The server alone with every module (oauth too: registered, no provider).
    net_backend(&dir, &["new-server", "solo-server", "--modules", &ALL_MODULES.join(",")]);
    net_backend(&dir, &["new-client", "solo-client"]);
    let (full, solo_server, solo_client) = (dir.join("fullgame"), dir.join("solo-server"), dir.join("solo-client"));
    for project in [&full, &solo_server, &solo_client] {
        rustfmt_check(project);
    }
    patch_project(&full, false);
    patch_project(&solo_server, true);
    patch_project(&solo_client, true);

    // The full project: server, client and the egui demo (built, never opened).
    cargo(&full, &target, &["build", "--workspace"]);
    cargo(&full, &target, &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]);
    assert!(exe(&target, "demo").is_file(), "the demo was not built");
    // The demo's own tests (the join codes Steam carries), then the demo with its `steam`
    // feature: built and clippy-checked, never started (it would start Steam).
    cargo(&full, &target, &["test", "-p", "demo"]);
    cargo(&full, &target, &["clippy", "-p", "demo", "--all-targets", "--features", "steam", "--", "-D", "warnings"]);
    cargo(&full, &target, &["build", "-p", "demo", "--features", "steam"]);
    // The connect-string guard's test (Steam's library is loaded, Steam is never started).
    cargo(&full, &target, &["test", "-p", "demo", "--features", "steam"]);
    cargo(&solo_server, &target, &["build"]);
    cargo(&solo_server, &target, &["clippy", "--all-targets", "--", "-D", "warnings"]);
    cargo(&solo_client, &target, &["build"]);
    cargo(&solo_client, &target, &["clippy", "--all-targets", "--", "-D", "warnings"]);

    let port = free_port();
    let server = start_server(&exe(&target, "server"), &full, port, &logs, &DEFAULT_MODULES);
    assert!(full.join("game.db").is_file(), "the SQLite file is not in the project folder");
    let url = format!("http://127.0.0.1:{port}");
    let expected = ["account ", "saved version ", "joined `world`", "[world] player ", "logged out"];
    let first = run_client(
        &exe(&target, "client"),
        |c| {
            c.arg(&url);
        },
        &logs,
        "full-1",
        &expected,
    );
    assert!(first.contains("registered player@example.com"), "{first}");
    let second = run_client(
        &exe(&target, "client"),
        |c| {
            c.arg(&url);
        },
        &logs,
        "full-2",
        &expected,
    );
    assert!(second.contains("logged in as player@example.com"), "{second}");
    drop(server);

    // The server-only project (new-server) with the client-only one (new-client), the URL from the
    // variable; then the server's own health check (what the Docker image runs).
    let port = free_port();
    let server = start_server(&exe(&target, "solo-server"), &solo_server, port, &logs, &ALL_MODULES);
    // oauth without a provider: its login route answers 404.
    let (status, _) = http_post(port, "/v1/auth/oauth/google").unwrap();
    assert_eq!(status, 404);
    let url = format!("http://127.0.0.1:{port}");
    run_client(
        &exe(&target, "solo-client"),
        |c| {
            c.env("NET_BACKEND_URL", &url);
        },
        &logs,
        "solo",
        &expected,
    );
    let health = Command::new(exe(&target, "solo-server"))
        .arg("healthcheck")
        .current_dir(&solo_server)
        .env("NBS__SERVER__BIND", format!("127.0.0.1:{port}"))
        .output()
        .unwrap();
    assert!(health.status.success(), "healthcheck: {}", String::from_utf8_lossy(&health.stderr));
    // The image's configuration parses and validates with this binary (no database connection).
    let docker_config = Command::new(exe(&target, "solo-server"))
        .args(["config", "check"])
        .current_dir(&solo_server)
        .env("NBS_CONFIG", solo_server.join("config.docker.toml"))
        .output()
        .unwrap();
    assert!(docker_config.status.success(), "config check of config.docker.toml: {}", String::from_utf8_lossy(&docker_config.stderr));
    drop(server);
}

#[test]
#[ignore = "slow: cargo check of a matrix of combinations (run with --ignored)"]
fn every_combination_checks() {
    let dir = scratch("matrix");
    let target = build_target("matrix");
    let all = ALL_MODULES.join(",");
    // The modules only change the server (the clients and demos read the modules at run time), the
    // paths only the client side, the databases the server's features. So: the server alone (`api`,
    // SQLite) with no module, with each module alone (+ auth, which every module needs: each
    // feature without the optional others, e.g. groups without chat) and with all of them; then
    // every path with every database and all modules.
    let mut combinations: Vec<(&str, &str, &str)> = vec![("api", "sqlite", "none")];
    combinations.extend(ALL_MODULES.iter().map(|m| ("api", "sqlite", *m)));
    for client in ["rust", "bevy", "protocol", "api"] {
        for db in ["sqlite", "postgres", "mysql"] {
            combinations.push((client, db, all.as_str()));
        }
    }
    for (index, (client, db, modules)) in combinations.iter().enumerate() {
        let name = format!("m{index:02}-{client}-{db}");
        let mut args = vec!["new", name.as_str(), "--client", client, "--db", db, "--modules", modules];
        if matches!(*client, "rust" | "bevy") {
            args.push("--no-demo");
        }
        net_backend(&dir, &args);
        let project = dir.join(&name);
        rustfmt_check(&project);
        patch_project(&project, *client == "api");
        cargo(&project, &target, &["check", "--workspace"]);
    }
    // 1 + 11 + 12.
    assert_eq!(combinations.len(), 24);
}

#[test]
#[ignore = "slow: builds Bevy (5-10 minutes the first time; run with --ignored)"]
fn bevy_projects_build_and_run() {
    let dir = scratch("bevy");
    let target = build_target("bevy");
    let logs = dir.join("logs");
    fs::create_dir_all(&logs).unwrap();
    // A project with the client and the demo in it.
    net_backend(&dir, &["new", "bevygame", "--client", "bevy"]);
    let project = dir.join("bevygame");
    rustfmt_check(&project);
    patch_project(&project, false);
    cargo(&project, &target, &["build", "--workspace"]);
    cargo(&project, &target, &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]);
    assert!(exe(&target, "demo").is_file(), "the Bevy demo was not built");
    // The join-code tests, and the demo with its `steam` feature (built, never started).
    cargo(&project, &target, &["test", "-p", "demo"]);
    cargo(&project, &target, &["clippy", "-p", "demo", "--all-targets", "--features", "steam", "--", "-D", "warnings"]);
    cargo(&project, &target, &["build", "-p", "demo", "--features", "steam"]);

    // The headless Bevy client against the project's server.
    let port = free_port();
    let server = start_server(&exe(&target, "server"), &project, port, &logs, &["auth", "chat"]);
    let url = format!("http://127.0.0.1:{port}");
    let out = run_client(
        &exe(&target, "client"),
        |c| {
            c.env("NET_BACKEND_URL", &url);
        },
        &logs,
        "bevy",
        &["joined `world`", "[world] player ", "done"],
    );
    assert!(out.contains("registered player@example.com"), "{out}");
    drop(server);

    // The demo next to an existing game: standalone, builds on its own.
    let game = dir.join("space");
    fs::create_dir_all(game.join("src")).unwrap();
    fs::write(game.join("Cargo.toml"), "[package]\nname = \"space\"\nversion = \"0.1.0\"\nedition = \"2024\"\n").unwrap();
    fs::write(game.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    net_backend(&dir, &["new", "nextto", "--client", "bevy", "--existing", "space"]);
    let demo = dir.join("net_backend_demo");
    rustfmt_check(&demo);
    // Already standalone (its own [workspace]): only the patch.
    patch_project(&demo, false);
    cargo(&demo, &target, &["build"]);
    assert!(exe(&target, "net_backend_demo").is_file());
}

#[test]
#[ignore = "release check against crates.io (run with --ignored published)"]
fn published_crates_build_and_run() {
    let dir = scratch("published");
    let target = build_target("published");
    let logs = dir.join("logs");
    fs::create_dir_all(&logs).unwrap();
    net_backend(&dir, &["new", "pubgame", "-y"]);
    let project = dir.join("pubgame");
    // No patch and no lockfile: the generated project exactly as a user gets it.
    cargo(&project, &target, &["build", "--workspace"]);
    let tree = cargo_output(&project, &target, &["tree", "--workspace", "--depth", "1", "--prefix", "none"]);
    let tree = String::from_utf8_lossy(&tree.stdout);
    // A crates.io dependency prints as `name vX.Y.Z`; a local (path) one with its folder.
    let ours: Vec<&str> = tree.lines().filter(|line| line.starts_with("net_backend_")).collect();
    assert!(!ours.is_empty() && ours.iter().all(|line| !line.contains('(')), "not the published crates:\n{tree}");
    let port = free_port();
    let server = start_server(&exe(&target, "server"), &project, port, &logs, &["auth", "storage", "chat"]);
    let url = format!("http://127.0.0.1:{port}");
    run_client(
        &exe(&target, "client"),
        |c| {
            c.arg(&url);
        },
        &logs,
        "published",
        &["joined `world`", "logged out"],
    );
    drop(server);

    // The Bevy path against the published bevy_net_backend and protocol: it compiles.
    net_backend(&dir, &["new", "pubbevy", "--client", "bevy"]);
    cargo(&dir.join("pubbevy"), &build_target("bevy-published"), &["check", "--workspace"]);
}
