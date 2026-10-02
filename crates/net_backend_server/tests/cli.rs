//! The command line, run in process with a temp directory under `target/` (SQLite file database).
#![cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]

mod common;

use net_backend_server::{Config, Dialect, Error, Migration, Module, NetBackendServer};

struct Scores;

impl Module for Scores {
    fn name(&self) -> &'static str {
        "scores"
    }

    fn migrations(&self, dialect: Dialect) -> Vec<Migration> {
        let id = match dialect {
            Dialect::MySql => "id BIGINT AUTO_INCREMENT PRIMARY KEY",
            Dialect::Postgres => "id BIGSERIAL PRIMARY KEY",
            _ => "id INTEGER PRIMARY KEY AUTOINCREMENT",
        };
        vec![Migration::new(202610010001, "create_scores", format!("CREATE TABLE scores ({id}, points BIGINT NOT NULL)"))]
    }
}

async fn run(config: &Config, args: &[&str]) -> (Result<(), Error>, String) {
    let mut output = Vec::new();
    let mut argv = vec!["game-server"];
    argv.extend_from_slice(args);
    let result = NetBackendServer::new(config.clone()).module(Scores).run_with_output(argv, &mut output).await;
    (result, String::from_utf8_lossy(&output).into_owned())
}

#[cfg(feature = "sqlite")]
fn sqlite_config(name: &str) -> Config {
    let dir = common::temp_dir(name);
    let mut config = Config::default();
    config.database.url = net_backend_server::SecretString::new(format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/")));
    config.database.migrations_dir = dir.join("migrations");
    config
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn migrate_up_and_status() {
    let config = sqlite_config("cli-migrate");
    let (result, out) = run(&config, &["migrate", "status"]).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(out.contains("pending") && out.contains("scores") && out.contains("create_scores"), "{out}");

    let (result, out) = run(&config, &["migrate"]).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(out.contains("Migrated  scores/202610010001_create_scores"), "{out}");
    let (result, out) = run(&config, &["migrate", "up"]).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(out.contains("Nothing to migrate."), "{out}");
    let (_, out) = run(&config, &["migrate", "status"]).await;
    assert!(out.contains("applied"), "{out}");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn migrations_publish() {
    let config = sqlite_config("cli-publish");
    let (result, out) = run(&config, &["migrations", "publish", "scores"]).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out.matches("Published").count(), 3, "{out}");
    let file = config.database.migrations_dir.join("scores").join("sqlite").join("202610010001_create_scores.sql");
    assert!(file.is_file());
    std::fs::write(&file, "CREATE TABLE scores (id INTEGER PRIMARY KEY AUTOINCREMENT, points BIGINT NOT NULL, season BIGINT NOT NULL DEFAULT 1)")
        .expect("edit");
    let (result, out) = run(&config, &["migrations", "publish", "scores", "--dialect", "sqlite"]).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(out.contains("Skipped"), "{out}");
    // The edited, app-owned copy is what migrates.
    let (result, _) = run(&config, &["migrate"]).await;
    assert!(result.is_ok(), "{result:?}");
    let db = net_backend_server::Db::connect(&config.database).await.expect("connect");
    db.execute_script("INSERT INTO scores (points, season) VALUES (5, 2)").await.expect("the edited column exists");
    db.close().await;

    let (result, _) = run(&config, &["migrations", "publish", "scores", "--dialect", "oracle"]).await;
    assert!(matches!(result, Err(Error::Cli(m)) if m.contains("unknown dialect")));
    let (result, _) = run(&config, &["migrations", "publish", "chat"]).await;
    assert!(matches!(result, Err(Error::Migration(m)) if m.contains("no module `chat`")));
}

#[tokio::test]
async fn config_check_and_help() {
    let config = common::http_config();
    let (result, out) = run(&config, &["config", "check"]).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(out.contains("Configuration OK.") && out.contains("URL hidden") && out.contains("scores"), "{out}");
    assert!(!out.contains("test:test"), "no credentials in the output: {out}");

    let (result, out) = run(&config, &["--help"]).await;
    assert!(result.is_ok(), "{result:?}");
    for command in ["serve", "migrate", "migrations", "config"] {
        assert!(out.contains(command), "{command}: {out}");
    }
    let (result, _) = run(&config, &["frobnicate"]).await;
    assert!(matches!(result, Err(Error::Cli(_))));

    let mut typo = config.clone();
    typo.modules.insert("scroes".into(), toml::Value::Table(toml::Table::new()));
    let (result, _) = run(&typo, &["config", "check"]).await;
    assert!(matches!(result, Err(Error::Config(p)) if p.iter().any(|p| p.contains("[modules.scroes]"))));

    let mut bad = config.clone();
    bad.database.max_connections = 0;
    let (result, _) = run(&bad, &["config", "check"]).await;
    assert!(matches!(result, Err(Error::Config(p)) if p.iter().any(|p| p.contains("max_connections"))));
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn config_check_connect() {
    let config = sqlite_config("cli-connect");
    let (result, out) = run(&config, &["config", "check", "--connect"]).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(out.contains("reachable"), "{out}");
}

/// `config check` builds the server (without touching the database), so every registered module
/// parses its own section: an unknown key under `[modules.auth]` fails the check, as it would fail
/// `serve` / `migrate`.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn config_check_reads_module_settings() {
    async fn check(config: &Config) -> (Result<(), Error>, String) {
        let mut output = Vec::new();
        let server = NetBackendServer::new(config.clone()).module(net_backend_server::Auth::new());
        let result = server.run_with_output(["game-server", "config", "check"], &mut output).await;
        (result, String::from_utf8_lossy(&output).into_owned())
    }
    let mut config = sqlite_config("cli-module-settings");
    let mut auth = toml::Table::new();
    auth.insert("app_name".into(), toml::Value::String("Test Game".into()));
    config.modules.insert("auth".into(), toml::Value::Table(auth.clone()));
    let (result, out) = check(&config).await;
    assert!(result.is_ok() && out.contains("Configuration OK."), "{result:?} {out}");

    auth.insert("app_nmae".into(), toml::Value::String("typo".into()));
    config.modules.insert("auth".into(), toml::Value::Table(auth));
    let (result, out) = check(&config).await;
    assert!(matches!(&result, Err(Error::Config(p)) if p.iter().any(|p| p.contains("app_nmae"))), "{result:?}");
    assert!(!out.contains("Configuration OK."), "{out}");
}

#[tokio::test]
async fn openapi_export_needs_no_database() {
    // A MySQL / PostgreSQL URL towards a closed port: export must not connect.
    let mut config = common::http_config();
    config.database.connect_lazy = false;
    let (result, out) = run(&config, &["openapi", "export"]).await;
    assert!(result.is_ok(), "{result:?}");
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("JSON");
    assert!(doc["paths"]["/v1/info"].is_object(), "{doc}");
    let file = common::temp_dir("cli-openapi").join("openapi.json");
    let (result, out) = run(&config, &["openapi", "export", "--output", &file.display().to_string()]).await;
    assert!(result.is_ok() && out.contains("Wrote"), "{result:?} {out}");
    assert!(std::fs::read_to_string(&file).expect("file").contains("\"/v1/info\""));
    // The WebSocket endpoint's AsyncAPI document, likewise without a database.
    let (result, out) = run(&config, &["asyncapi", "export"]).await;
    assert!(result.is_ok(), "{result:?}");
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("JSON");
    assert_eq!(doc["asyncapi"], "3.0.0");
    assert_eq!(doc["channels"]["ws"]["address"], "/v1/ws");
}

#[cfg(feature = "sqlite")]
async fn run_auth(config: &Config, args: &[&str]) -> (Result<(), Error>, String) {
    let mut auth = net_backend_server::auth::AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    let mut output = Vec::new();
    let mut argv = vec!["game-server"];
    argv.extend_from_slice(args);
    let server = NetBackendServer::new(config.clone()).module(net_backend_server::Auth::new().with_config(auth));
    let result = server.run_with_output(argv, &mut output).await;
    (result, String::from_utf8_lossy(&output).into_owned())
}

/// The auth module's commands against a SQLite file: create (generated password / from a file),
/// roles, ban / unban, revoke sessions; every action audited as `cli.*`.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn user_commands() {
    use net_backend_server::protocol::routes;
    let config = sqlite_config("cli-users");
    let (result, out) = run_auth(&config, &["--help"]).await;
    assert!(result.is_ok(), "{result:?}");
    for command in ["user:create", "user:role", "user:ban", "user:unban", "sessions:revoke"] {
        assert!(out.contains(command), "{command}: {out}");
    }
    assert!(run_auth(&config, &["migrate"]).await.0.is_ok());

    let (result, out) = run_auth(&config, &["user:create", "Boss@Example.com", "--admin", "--verified", "--name", "Boss"]).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(out.contains("Created user 1 (Boss@Example.com) with the admin role"), "{out}");
    let generated = out.lines().find_map(|l| l.strip_prefix("Password (shown once): ")).expect("a generated password").to_string();
    assert_eq!(generated.len(), 24);

    let dir = common::temp_dir("cli-users-secret");
    let file = dir.join("password");
    std::fs::write(&file, "a password from a file\n").expect("write");
    let (result, out) = run_auth(&config, &["user:create", "player@example.com", "--password-file", &file.display().to_string()]).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(out.contains("Created user 2") && !out.contains("shown once"), "{out}");
    let (result, _) = run_auth(&config, &["user:create", "PLAYER@example.com"]).await;
    assert!(matches!(&result, Err(Error::Cli(m)) if m.contains("email_taken")), "{result:?}");
    std::fs::write(&file, "short").expect("write");
    let (result, _) = run_auth(&config, &["user:create", "weak@example.com", "--password-file", &file.display().to_string()]).await;
    assert!(matches!(&result, Err(Error::Cli(m)) if m.contains("validation_failed") && !m.contains("short\"")), "{result:?}");
    assert!(matches!(run_auth(&config, &["user:create"]).await.0, Err(Error::Cli(m)) if m.contains("<email>")));
    assert!(matches!(run_auth(&config, &["user:create", "x@example.com", "--nope"]).await.0, Err(Error::Cli(m)) if m.contains("--nope")));

    let (result, out) = run_auth(&config, &["user:role", "player@example.com", "moderator"]).await;
    assert!(result.is_ok() && out.contains("Granted role `moderator` to user 2"), "{result:?} {out}");
    let (result, out) = run_auth(&config, &["user:ban", "2", "--reason", "spam", "--hours", "2"]).await;
    assert!(result.is_ok() && out.contains("Banned user 2"), "{result:?} {out}");
    assert!(matches!(run_auth(&config, &["user:ban", "2", "--hours", "-1"]).await.0, Err(Error::Cli(_))));
    assert!(matches!(run_auth(&config, &["user:ban", "nobody@example.com"]).await.0, Err(Error::Cli(m)) if m.contains("not_found")));

    // Check through the HTTP API: the admin logs in with the generated password and sees it all.
    let server = NetBackendServer::new(config.clone()).module(net_backend_server::Auth::new());
    let router = server.build().await.expect("build").router();
    let login = |email: &str, password: &str| common::post_json(routes::auth::LOGIN, serde_json::json!({"email": email, "password": password}).to_string());
    let (status, _, body) = common::call(&router, login("boss@example.com", &generated)).await;
    assert_eq!(status, http::StatusCode::OK, "{body}");
    assert_eq!(body["account"]["roles"], serde_json::json!(["admin"]));
    assert_eq!(body["account"]["email_verified"], true);
    assert_eq!(body["account"]["display_name"], "Boss");
    let token = body["tokens"]["access_token"].as_str().unwrap_or_default().to_string();
    let (status, _, body) = common::call(&router, login("player@example.com", "a password from a file")).await;
    assert_eq!((status, body["error"]["code"].as_str()), (http::StatusCode::FORBIDDEN, Some("banned")));

    let (result, out) = run_auth(&config, &["user:unban", "player@example.com"]).await;
    assert!(result.is_ok() && out.contains("Unbanned user 2"), "{result:?} {out}");
    let (status, _, _) = common::call(&router, login("player@example.com", "a password from a file")).await;
    assert_eq!(status, http::StatusCode::OK);
    let (result, out) = run_auth(&config, &["sessions:revoke", "player@example.com"]).await;
    assert!(result.is_ok() && out.contains("Revoked 1 session(s) of user 2"), "{result:?} {out}");
    let (result, out) = run_auth(&config, &["user:role", "2", "moderator", "--revoke"]).await;
    assert!(result.is_ok() && out.contains("Revoked role `moderator` from user 2"), "{result:?} {out}");

    let request =
        http::Request::get("/v1/admin/audit?action=cli.").header("authorization", format!("Bearer {token}")).body(axum::body::Body::empty()).expect("request");
    let (status, _, body) = common::call(&router, request).await;
    assert_eq!(status, http::StatusCode::OK, "{body}");
    let actions: Vec<&str> = body["items"].as_array().map(|a| a.iter().filter_map(|e| e["action"].as_str()).collect()).unwrap_or_default();
    assert_eq!(actions, ["cli.role_revoke", "cli.revoke_sessions", "cli.unban", "cli.ban", "cli.role_grant", "cli.user_create", "cli.user_create"], "{body}");
}

/// The app's own command, and the name rules.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn app_commands() {
    use futures_util::future::BoxFuture;
    use net_backend_server::command::{AppCommand, CommandArgs, CommandCtx};

    struct Count(&'static str);
    impl AppCommand for Count {
        fn name(&self) -> &'static str {
            self.0
        }
        fn about(&self) -> &'static str {
            "Count the applied migrations"
        }
        fn run<'a>(&'a self, mut ctx: CommandCtx<'a>, args: &'a [String]) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                let args = CommandArgs::parse(args, &["--prefix"], &[])?;
                let applied = ctx.server().migrate().await?.applied.len();
                ctx.println(format!("{}{applied}", args.option("--prefix").unwrap_or("")))
            })
        }
    }
    let config = sqlite_config("cli-app-command");
    async fn run_with(config: &Config, command: Count, args: &[&str]) -> (Result<(), Error>, String) {
        let mut output = Vec::new();
        let mut argv = vec!["game-server"];
        argv.extend_from_slice(args);
        let result = NetBackendServer::new(config.clone()).module(Scores).command(command).run_with_output(argv, &mut output).await;
        (result, String::from_utf8_lossy(&output).into_owned())
    }
    let (result, out) = run_with(&config, Count("game:count"), &["game:count", "--prefix=applied "]).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out.trim(), "applied 1");
    let (_, out) = run_with(&config, Count("game:count"), &["--help"]).await;
    assert!(out.contains("App commands:") && out.contains("game:count") && out.contains("Count the applied migrations"), "{out}");
    let (result, _) = run_with(&config, Count("migrate"), &["serve"]).await;
    assert!(matches!(result, Err(Error::Cli(m)) if m.contains("built-in")));
    let (result, _) = run_with(&config, Count("Bad Name"), &["serve"]).await;
    assert!(matches!(result, Err(Error::Cli(_))));
    // `asyncapi` is a built-in name too.
    let (result, _) = run_with(&config, Count("asyncapi"), &["serve"]).await;
    assert!(matches!(result, Err(Error::Cli(m)) if m.contains("built-in")));
    // An app command named `healthcheck` replaces the built-in one (and hides it from --help).
    let (result, out) = run_with(&config, Count("healthcheck"), &["healthcheck", "--prefix=own "]).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out.trim(), "own 0", "the app command ran (nothing left to migrate)");
    let (_, out) = run_with(&config, Count("healthcheck"), &["--help"]).await;
    assert!(out.contains("Count the applied migrations") && !out.contains("Check that the running server is ready"), "{out}");
    let (_, out) = run_with(&config, Count("game:count"), &["--help"]).await;
    assert!(out.contains("Check that the running server is ready"), "{out}");
}

/// `healthcheck` (0.1.1): exit 0 while the running server's `/readyz` answers 200 (also with an
/// unspecified bind address, which means loopback), an error otherwise; no database contact.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthcheck_asks_readyz() {
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let config = sqlite_config("cli-healthcheck");
    let prepared = NetBackendServer::new(config.clone()).module(Scores).build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(prepared.serve_with_shutdown(listener, async move {
        let _ = stopped.await;
    }));
    let mut running = config.clone();
    running.server.bind = addr;
    // Ready once the modules started (one overall deadline, not a fixed sleep).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (result, _) = run(&running, &["healthcheck"]).await;
        if result.is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "the server never became ready: {result:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut unspecified = config.clone();
    unspecified.server.bind = std::net::SocketAddr::from(([0, 0, 0, 0], addr.port()));
    let (result, _) = run(&unspecified, &["healthcheck"]).await;
    assert!(result.is_ok(), "0.0.0.0 means loopback: {result:?}");
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(30), serving).await;
    // Nobody listens there any more: refused.
    let (result, _) = run(&running, &["healthcheck"]).await;
    assert!(matches!(&result, Err(Error::Cli(m)) if m.starts_with("healthcheck:")), "{result:?}");
    // Something answers, but not 200.
    let fake = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let mut busy = config.clone();
    busy.server.bind = fake.local_addr().expect("addr");
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = fake.accept().await {
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let _ = socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        }
    });
    let (result, _) = run(&busy, &["healthcheck"]).await;
    assert!(matches!(&result, Err(Error::Cli(m)) if m.contains("503")), "{result:?}");
}

/// 0.1.1 live NIT 3: `--help`, `help`, `<built-in> --help` and `--version` need no configuration
/// and no database (`run_main`); every other command loads the configuration first.
#[tokio::test]
async fn help_and_version_need_no_configuration() {
    async fn main_with(args: &[&str]) -> (Result<(), Error>, String, bool) {
        let mut output = Vec::new();
        let mut argv = vec!["game-server"];
        argv.extend_from_slice(args);
        let loaded = std::sync::atomic::AtomicBool::new(false);
        let load = || {
            loaded.store(true, std::sync::atomic::Ordering::SeqCst);
            Err(Error::Config(vec!["database.url (or database.url_file) is required".into()]))
        };
        let result = NetBackendServer::run_main_with_output(argv, load, |config| NetBackendServer::new(config).module(Scores), &mut output).await;
        (result, String::from_utf8_lossy(&output).into_owned(), loaded.load(std::sync::atomic::Ordering::SeqCst))
    }
    for args in [&["--help"][..], &["-h"], &["help"], &["migrate", "--help"], &["healthcheck", "-h"]] {
        let (result, out, loaded) = main_with(args).await;
        assert!(result.is_ok() && !loaded, "{args:?}: {result:?}");
        assert!(out.contains("Usage:"), "{args:?}: {out}");
    }
    let (result, out, loaded) = main_with(&["--help"]).await;
    assert!(result.is_ok() && !loaded && out.contains("healthcheck") && out.contains("--version"), "{out}");
    for flag in ["--version", "-V"] {
        let (result, out, loaded) = main_with(&[flag]).await;
        assert!(result.is_ok() && !loaded, "{result:?}");
        assert_eq!(out.trim(), format!("net_backend_server {}", env!("CARGO_PKG_VERSION")));
    }
    // Anything else loads the configuration (and reports its error).
    for args in [&[][..], &["migrate"], &["healthcheck"], &["scores:x", "-h"]] {
        let (result, _, loaded) = main_with(args).await;
        assert!(loaded && matches!(&result, Err(Error::Config(_))), "{args:?}: {result:?}");
    }
}
