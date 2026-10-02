//! End to end: `net-backend new`, `new-server` and `new-client` write projects under the target
//! folder (in a path with a space), the projects build against this repository's crates (a
//! `[patch.crates-io]` added for the test), the generated servers run headless on loopback (SQLite),
//! `GET /v1/info` answers, and the generated clients log in, save and chat against them.
//!
//! Slow (it builds the server and the client from scratch the first time), so it is ignored by
//! default: `cargo test -p net_backend --test e2e -- --ignored generated_projects`. Needs crates.io
//! (Cargo downloads).
//!
//! `published_crates_build_and_run` (also ignored; a release check, run by hand) builds and runs a
//! generated project WITHOUT the patch, against the crates on crates.io: it catches a template that
//! uses an API the published release lacks. `cargo test -p net_backend --test e2e -- --ignored published`.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long a generated server may take to answer `/v1/info` after it started.
const SERVER_START: Duration = Duration::from_secs(120);
/// How long one client run may take (it listens to the chat for 10 s).
const CLIENT_RUN: Duration = Duration::from_secs(120);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").canonicalize().unwrap()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| repo_root().join("target"))
}

/// A path as a TOML literal string (single quotes: Windows backslashes stay as they are).
fn toml_path(path: &Path) -> String {
    let text = path.display().to_string();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();
    assert!(!text.contains('\''), "{text}");
    format!("'{text}'")
}

/// Run `net-backend` with these arguments in `dir`; panics unless it succeeds.
fn net_backend(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_net-backend")).args(args).current_dir(dir).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "net-backend {args:?}: {stdout}{}", String::from_utf8_lossy(&output.stderr));
    stdout
}

/// Point the project at this repository's crates and keep it out of the repository's workspace
/// (the project lies under the repository's target folder). Test-only changes; the generated files
/// are checked before this.
fn patch_project(project: &Path, crates: &[&str], standalone: bool) {
    let root = repo_root();
    let mut extra = String::from("\n# Added by the end-to-end test.\n");
    if standalone {
        extra.push_str("[workspace]\n\n");
    }
    extra.push_str("[patch.crates-io]\n");
    for name in crates {
        extra.push_str(&format!("{name} = {{ path = {} }}\n", toml_path(&root.join("crates").join(name))));
    }
    let manifest = project.join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str(&extra);
    fs::write(&manifest, text).unwrap();
    // The repository's lockfile as the starting point: the same dependency versions as its own CI.
    fs::copy(root.join("Cargo.lock"), project.join("Cargo.lock")).unwrap();
}

fn cargo(project: &Path, build_target: &Path, args: &[&str]) {
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(args)
        .current_dir(project)
        // Its own target folder: the outer `cargo test` holds the lock of the shared one.
        .env("CARGO_TARGET_DIR", build_target)
        .status()
        .unwrap();
    assert!(status.success(), "cargo {args:?} in {}", project.display());
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

/// Start a generated server in its project folder (where its config.toml is) on `port`; wait until
/// `GET /v1/info` answers and check the modules.
fn start_server(binary: &Path, project: &Path, port: u16, logs: &Path) -> Running {
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
            for module in ["\"auth\"", "\"storage\"", "\"chat\""] {
                assert!(body.contains(module), "/v1/info without {module}: {body}");
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

/// Run a generated client to its end (bounded); its output.
fn run_client(binary: &Path, configure: impl FnOnce(&mut Command), logs: &Path, label: &str) -> String {
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
    for expected in ["account ", "saved version ", "read back {", "joined `world`", "[world] player ", "logged out"] {
        assert!(output.contains(expected), "the client ({label}) printed no `{expected}`:\n{output}");
    }
    output
}

#[test]
#[ignore = "slow: builds generated projects (run with --ignored)"]
fn generated_projects_build_and_run() {
    let work = target_dir().join("cli-e2e");
    let projects = work.join("with space");
    let build_target = work.join("target");
    let logs = work.join("logs");
    let _ = fs::remove_dir_all(&projects);
    let _ = fs::remove_dir_all(&logs);
    fs::create_dir_all(&projects).unwrap();
    fs::create_dir_all(&logs).unwrap();

    // Generate: a relative path with a space (the full project), names given in the folder itself.
    let message = net_backend(&work, &["new", "with space/fullgame"]);
    assert!(message.contains("cd \"with space"), "{message}");
    net_backend(&projects, &["new-server", "solo-server"]);
    net_backend(&projects, &["new-client", "solo-client"]);
    let full = projects.join("fullgame");
    let solo_server = projects.join("solo-server");
    let solo_client = projects.join("solo-client");
    for file in
        ["Cargo.toml", "README.md", "config.toml", "config.docker.toml", "Dockerfile", "compose.yaml", ".gitignore", "server/src/main.rs", "client/src/main.rs"]
    {
        assert!(full.join(file).is_file(), "fullgame/{file}");
    }
    // A second `new` into an existing project is refused and changes nothing.
    let before = fs::read_to_string(full.join("Cargo.toml")).unwrap();
    let refused = Command::new(env!("CARGO_BIN_EXE_net-backend")).args(["new", "fullgame"]).current_dir(&projects).output().unwrap();
    assert!(!refused.status.success());
    assert_eq!(fs::read_to_string(full.join("Cargo.toml")).unwrap(), before);

    patch_project(&full, &["net_backend_protocol", "net_backend_server", "net_backend_client"], false);
    patch_project(&solo_server, &["net_backend_protocol", "net_backend_server"], true);
    patch_project(&solo_client, &["net_backend_protocol", "net_backend_client"], true);

    // Build (the full project: both members; plain `cargo build` there is the server alone) and lint.
    cargo(&full, &build_target, &["build", "--workspace"]);
    cargo(&full, &build_target, &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]);
    cargo(&solo_server, &build_target, &["build"]);
    cargo(&solo_server, &build_target, &["clippy", "--all-targets", "--", "-D", "warnings"]);
    cargo(&solo_client, &build_target, &["build"]);
    cargo(&solo_client, &build_target, &["clippy", "--all-targets", "--", "-D", "warnings"]);

    let bin = |name: &str| build_target.join("debug").join(format!("{name}{}", std::env::consts::EXE_SUFFIX));

    // The full project: its server, its client twice (registers, then logs in), the URL as an argument.
    let port = free_port();
    let server = start_server(&bin("server"), &full, port, &logs);
    assert!(full.join("game.db").is_file(), "the SQLite file is not in the project folder");
    let url = format!("http://127.0.0.1:{port}");
    let first = run_client(
        &bin("client"),
        |c| {
            c.arg(&url);
        },
        &logs,
        "full-1",
    );
    assert!(first.contains("registered player@example.com"), "{first}");
    assert!(first.contains("[world] player 1: hello from the fullgame client"), "{first}");
    let second = run_client(
        &bin("client"),
        |c| {
            c.arg(&url);
        },
        &logs,
        "full-2",
    );
    assert!(second.contains("logged in as player@example.com"), "{second}");
    drop(server);

    // The server-only project with the client-only one, the URL from NET_BACKEND_URL.
    let port = free_port();
    let server = start_server(&bin("solo-server"), &solo_server, port, &logs);
    let url = format!("http://127.0.0.1:{port}");
    run_client(
        &bin("solo-client"),
        |c| {
            c.env("NET_BACKEND_URL", &url);
        },
        &logs,
        "solo",
    );
    // Another account through the variables.
    let other = run_client(
        &bin("solo-client"),
        |c| {
            c.arg(&url).env("NET_BACKEND_EMAIL", "other@example.com").env("NET_BACKEND_PASSWORD", "another password 99");
        },
        &logs,
        "solo-other",
    );
    assert!(other.contains("registered other@example.com"), "{other}");
    // A server on another machine without the variables: refused before any connection (the
    // development password is never sent elsewhere).
    let remote =
        Command::new(bin("solo-client")).arg("https://api.example.com").env_remove("NET_BACKEND_EMAIL").env_remove("NET_BACKEND_PASSWORD").output().unwrap();
    assert!(!remote.status.success());
    assert!(String::from_utf8_lossy(&remote.stderr).contains("set NET_BACKEND_EMAIL and NET_BACKEND_PASSWORD"));
    // The image's configuration parses and validates with this binary (no database connection).
    let docker_config = Command::new(bin("solo-server"))
        .args(["config", "check"])
        .current_dir(&solo_server)
        .env("NBS_CONFIG", solo_server.join("config.docker.toml"))
        .output()
        .unwrap();
    assert!(
        docker_config.status.success(),
        "config check of config.docker.toml: {}{}",
        String::from_utf8_lossy(&docker_config.stdout),
        String::from_utf8_lossy(&docker_config.stderr)
    );
    // The server's own health check command (what the Docker image runs).
    let health =
        Command::new(bin("solo-server")).arg("healthcheck").current_dir(&solo_server).env("NBS__SERVER__BIND", format!("127.0.0.1:{port}")).output().unwrap();
    assert!(health.status.success(), "healthcheck: {}", String::from_utf8_lossy(&health.stderr));
    drop(server);
}

#[test]
#[ignore = "release check against crates.io (run with --ignored published)"]
fn published_crates_build_and_run() {
    let work = target_dir().join("cli-e2e-published");
    let projects = work.join("with space");
    let build_target = work.join("target");
    let logs = work.join("logs");
    let _ = fs::remove_dir_all(&projects);
    let _ = fs::remove_dir_all(&logs);
    fs::create_dir_all(&projects).unwrap();
    fs::create_dir_all(&logs).unwrap();

    net_backend(&projects, &["new", "pubgame"]);
    let project = projects.join("pubgame");
    // No patch and no lockfile: the generated project exactly as a user gets it (its own [workspace]
    // keeps it out of the repository's).
    cargo(&project, &build_target, &["build", "--workspace"]);
    let tree = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["tree", "--workspace", "--depth", "1", "--prefix", "none"])
        .current_dir(&project)
        .env("CARGO_TARGET_DIR", &build_target)
        .output()
        .unwrap();
    let tree = String::from_utf8_lossy(&tree.stdout);
    // A crates.io dependency prints as `name vX.Y.Z`; a local (path) one with its folder in parentheses.
    let ours: Vec<&str> = tree.lines().filter(|line| line.starts_with("net_backend_")).collect();
    assert!(
        !ours.is_empty() && ours.iter().all(|line| !line.contains('(')),
        "not the published crates:
{tree}"
    );

    let bin = |name: &str| build_target.join("debug").join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let port = free_port();
    let server = start_server(&bin("server"), &project, port, &logs);
    let url = format!("http://127.0.0.1:{port}");
    run_client(
        &bin("client"),
        |c| {
            c.arg(&url);
        },
        &logs,
        "published",
    );
    drop(server);
}
