//! The project generator: the embedded templates put together for the chosen options, and the file
//! writing (never over an existing file; a failed run removes what it wrote).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::compose;
use crate::names::{image_name, name_of, validate_name, GenError};
use crate::options::{ClientKind, Database, Options};
use crate::readme;

/// The version of the net_backend crates the generated projects depend on (this crate's own: the
/// installer is released together with the server, the protocol and the client).
pub const CRATES_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The bevy_net_backend release the Bevy projects use (it is published on its own; Bevy 0.19.0).
pub const BEVY_NET_BACKEND_VERSION: &str = "0.2.0";

// The templates (embedded: generating a project needs no network and no files besides the binary).
// Manifests carry `.tmpl` so that Cargo never treats a template folder as a package.
const SERVER_MANIFEST: &str = include_str!("../templates/server/Cargo.toml.tmpl");
const SERVER_MAIN: &str = include_str!("../templates/server/main.rs");
const RUST_CLIENT_MANIFEST: &str = include_str!("../templates/client/Cargo.toml.tmpl");
const RUST_CLIENT_MAIN: &str = include_str!("../templates/client/main.rs");
const PROTOCOL_CLIENT_MANIFEST: &str = include_str!("../templates/protocol/Cargo.toml.tmpl");
const PROTOCOL_CLIENT_MAIN: &str = include_str!("../templates/protocol/main.rs");
const BEVY_CLIENT_MANIFEST: &str = include_str!("../templates/bevy/client.Cargo.toml.tmpl");
const BEVY_CLIENT_MAIN: &str = include_str!("../templates/bevy/client.rs");
const EGUI_DEMO_MANIFEST: &str = include_str!("../templates/demo-egui/Cargo.toml.tmpl");
const EGUI_DEMO_MAIN: &str = include_str!("../templates/demo-egui/main.rs");
const BEVY_DEMO_MANIFEST: &str = include_str!("../templates/demo-bevy/Cargo.toml.tmpl");
const BEVY_DEMO_MAIN: &str = include_str!("../templates/demo-bevy/main.rs");
const EGUI_DEMO_STEAM: &str = include_str!("../templates/demo-egui/steam.rs");
const BEVY_DEMO_STEAM: &str = include_str!("../templates/demo-bevy/steam.rs");
/// The join-code helpers both demos share (`{{protocol}}`: the path of the protocol crate).
const DEMO_INVITE: &str = include_str!("../templates/demo-shared/invite.rs");
const CONFIG: &str = include_str!("../templates/config.toml");
const CONFIG_DOCKER: &str = include_str!("../templates/config.docker.toml");
const DOCKERFILE: &str = include_str!("../templates/Dockerfile");
const COMPOSE_SQLITE: &str = include_str!("../templates/compose.yaml");
const GITIGNORE: &str = include_str!("../templates/gitignore");
const GITIGNORE_CLIENT: &str = include_str!("../templates/gitignore-client");
const DOCKERIGNORE: &str = include_str!("../templates/dockerignore");
const README_CLIENT_ONLY: &str = include_str!("../templates/README.client.md");

/// Bevy's recommended development profile: the dependencies optimized, the game quick to build.
const BEVY_PROFILE: &str = "
# Bevy: dependencies optimized (a usable frame rate), this project's own code quick to build.
[profile.dev]
opt-level = 1

[profile.dev.package.\"*\"]
opt-level = 3
";

/// One folder to write: its path and its files (relative paths with `/`, contents with LF).
#[derive(Debug)]
pub struct Folder {
    pub dir: PathBuf,
    pub files: Vec<(String, String)>,
}

/// Replaces the `{{…}}` placeholders; every template is written with LF line endings.
fn render(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut text = template.replace("\r\n", "\n");
    for (key, value) in pairs {
        text = text.replace(&format!("{{{{{key}}}}}"), value);
    }
    text
}

/// `render` with the name and the versions, plus `extra`.
fn render_named(template: &str, name: &str, extra: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(&str, &str)> = vec![("name", name), ("version", CRATES_VERSION), ("bevy_net_backend_version", BEVY_NET_BACKEND_VERSION)];
    pairs.extend_from_slice(extra);
    render(template, &pairs)
}

/// Whether the project is a workspace (server/ + client/ ...) rather than a server at the root.
pub fn is_workspace(options: &Options) -> bool {
    options.client != ClientKind::Api
}

/// The server's package (and binary) name.
pub fn server_package<'a>(options: &Options, name: &'a str) -> &'a str {
    if is_workspace(options) {
        "server"
    } else {
        name
    }
}

/// Whether the demo is a member of the project's workspace (not placed next to an existing game).
pub fn demo_in_workspace(options: &Options) -> bool {
    options.demo && options.existing.is_none()
}

/// The folders `net-backend new` writes for these options: the project, and for `--existing` the
/// demo next to the game.
pub fn plan(options: &Options) -> Result<Vec<Folder>, GenError> {
    let name = name_of(&options.target)?;
    validate_name(name)?;
    let mut files: Vec<(String, String)> = Vec::new();
    let mut add = |path: &str, content: String| files.push((path.to_string(), content));
    let workspace = is_workspace(options);
    let prefix = if workspace { "server/" } else { "" };
    let package = server_package(options, name);
    let image = image_name(name);

    if workspace {
        add("Cargo.toml", workspace_manifest(options));
    }
    add("README.md", readme::project(options, name));
    // The players' uploads of the development server (`[modules.files] dir`) stay out of git and
    // out of the image's build context.
    let uploads_kept = options.has("files");
    let uploads = if uploads_kept { "# Players' files of the development server (config.toml: modules.files.dir).\n/files\n" } else { "" };
    add(".gitignore", format!("{}{uploads}", render(GITIGNORE, &[])));
    add(".dockerignore", format!("{}{}", render(DOCKERIGNORE, &[]), if uploads_kept { "files\n" } else { "" }));
    add("config.toml", config(options, name));
    let sqlite = options.database == Database::Sqlite;
    if sqlite {
        add("config.docker.toml", render(CONFIG_DOCKER, &[("name", name), ("module_sections", &module_sections(options, name, true))]));
    }
    let compose_note = if sqlite {
        "compose.yaml: the image, the port and the volume".to_string()
    } else {
        format!("compose.yaml: {}, the server and Caddy; DOMAIN in .env", options.database.name())
    };
    add(
        "Dockerfile",
        render(
            DOCKERFILE,
            &[
                ("name", name),
                ("image", &image),
                ("package", package),
                ("compose_note", &compose_note),
                ("docker_config", if sqlite { "config.docker.toml" } else { "config.toml" }),
            ],
        ),
    );
    let compose = match compose::production(options, name, package).map_err(GenError::Template)? {
        Some(production) => production,
        None => render(COMPOSE_SQLITE, &[("name", name), ("image", &image)]),
    };
    add("compose.yaml", compose);
    add(&format!("{prefix}Cargo.toml"), server_manifest(options, package));
    add(&format!("{prefix}src/main.rs"), server_main(options, name));

    let render_with = |template: &str, extra: &[(&str, &str)]| render_named(template, name, extra);
    match options.client {
        ClientKind::Rust => {
            add("client/Cargo.toml", render_with(RUST_CLIENT_MANIFEST, &[("package", "client")]));
            add("client/src/main.rs", render_with(RUST_CLIENT_MAIN, &[("client_run", "cargo run -p client")]));
        }
        ClientKind::Bevy => {
            add("client/Cargo.toml", render_with(BEVY_CLIENT_MANIFEST, &[("package", "client")]));
            add("client/src/main.rs", render_with(BEVY_CLIENT_MAIN, &[]));
        }
        ClientKind::Protocol => {
            add("client/Cargo.toml", render_with(PROTOCOL_CLIENT_MANIFEST, &[("package", "client")]));
            add("client/src/main.rs", render_with(PROTOCOL_CLIENT_MAIN, &[]));
        }
        ClientKind::Api => {}
    }
    if demo_in_workspace(options) {
        match options.client {
            ClientKind::Rust => {
                add("demo/Cargo.toml", render_with(EGUI_DEMO_MANIFEST, &[("package", "demo")]));
                add("demo/src/main.rs", render_with(EGUI_DEMO_MAIN, &[]));
                add("demo/src/invite.rs", render_with(DEMO_INVITE, &[("protocol", "net_backend_client::protocol")]));
                add("demo/src/steam.rs", render_with(EGUI_DEMO_STEAM, &[]));
            }
            ClientKind::Bevy => {
                add("demo/Cargo.toml", render_with(BEVY_DEMO_MANIFEST, &[("package", "demo"), ("standalone", ""), ("profile", "")]));
                add("demo/src/main.rs", render_with(BEVY_DEMO_MAIN, &[("demo_run", "cargo run -p demo")]));
                add("demo/src/invite.rs", render_with(DEMO_INVITE, &[("protocol", "net_backend_protocol")]));
                add("demo/src/steam.rs", render_with(BEVY_DEMO_STEAM, &[("demo_run", "cargo run -p demo")]));
            }
            _ => {}
        }
    }
    let mut folders = vec![Folder { dir: options.target.clone(), files }];

    if let Some(dir) = options.standalone_demo_dir() {
        let standalone = "\n# Standalone: its own workspace, part of no other (the game's folder is never touched).\n[workspace]\n";
        let files = vec![
            (
                "Cargo.toml".to_string(),
                render_with(BEVY_DEMO_MANIFEST, &[("package", crate::options::STANDALONE_DEMO), ("standalone", standalone), ("profile", BEVY_PROFILE)]),
            ),
            ("src/main.rs".to_string(), render_with(BEVY_DEMO_MAIN, &[("demo_run", "cargo run")])),
            ("src/invite.rs".to_string(), render_with(DEMO_INVITE, &[("protocol", "net_backend_protocol")])),
            ("src/steam.rs".to_string(), render_with(BEVY_DEMO_STEAM, &[("demo_run", "cargo run")])),
            ("README.md".to_string(), readme::standalone_demo(options, name)),
            (".gitignore".to_string(), render(GITIGNORE_CLIENT, &[])),
        ];
        folders.push(Folder { dir, files });
    }
    Ok(folders)
}

/// The legacy client-only project (`net-backend new-client`).
pub fn client_only(target: &Path) -> Result<Vec<Folder>, GenError> {
    let name = name_of(target)?;
    validate_name(name)?;
    let pairs = [("name", name), ("package", name), ("version", CRATES_VERSION), ("client_run", "cargo run")];
    let files = vec![
        ("Cargo.toml".to_string(), render(RUST_CLIENT_MANIFEST, &pairs)),
        ("README.md".to_string(), render(README_CLIENT_ONLY, &pairs)),
        (".gitignore".to_string(), render(GITIGNORE_CLIENT, &pairs)),
        ("src/main.rs".to_string(), render(RUST_CLIENT_MAIN, &pairs)),
    ];
    Ok(vec![Folder { dir: target.to_path_buf(), files }])
}

fn workspace_manifest(options: &Options) -> String {
    let mut members = vec!["server"];
    if options.client != ClientKind::Api {
        members.push("client");
    }
    if demo_in_workspace(options) {
        members.push("demo");
    }
    let list = members.iter().map(|m| format!("\"{m}\"")).collect::<Vec<_>>().join(", ");
    let mut text = format!(
        "[workspace]\nmembers = [{list}]\n# `cargo run` and `cargo build` in this folder mean the server; `-p client`{} picks another member.\ndefault-members = [\"server\"]\nresolver = \"3\"\n",
        if demo_in_workspace(options) { " / `-p demo`" } else { "" }
    );
    if options.client == ClientKind::Bevy {
        text.push_str(BEVY_PROFILE);
    }
    text
}

fn server_manifest(options: &Options, package: &str) -> String {
    let mut features = vec![options.database.flag()];
    features.extend(options.modules.iter().filter_map(|m| m.feature));
    let list = features.iter().map(|f| format!("\"{f}\"")).collect::<Vec<_>>().join(", ");
    let modules = if options.modules.is_empty() {
        "no module (the core server)".to_string()
    } else {
        format!("the modules {}", options.modules.iter().map(|m| m.name).collect::<Vec<_>>().join(", "))
    };
    let doc = format!("{} with {modules}", options.database.name());
    render(SERVER_MANIFEST, &[("package", package), ("version", CRATES_VERSION), ("features", &list), ("features_doc", &doc)])
}

/// The server's `main.rs`: the modules' imports and registrations, formatted as rustfmt does.
fn server_main(options: &Options, name: &str) -> String {
    let mut imports: Vec<&str> = vec!["use net_backend_server::NetBackendServer;"];
    imports.extend(options.modules.iter().map(|m| m.import));
    imports.sort_unstable();
    let imports = imports.iter().map(|line| format!("{line}\n")).collect::<String>();
    let body = match options.modules.as_slice() {
        [] => "    NetBackendServer::run_main(|config| NetBackendServer::new(config)).await".to_string(),
        modules => {
            let one_line = format!(
                "    NetBackendServer::run_main(|config| NetBackendServer::new(config){}).await",
                modules.iter().map(|m| format!(".module({})", m.register)).collect::<String>()
            );
            let chain = format!("NetBackendServer::new(config){}", modules.iter().map(|m| format!(".module({})", m.register)).collect::<String>());
            // rustfmt keeps a chain of up to 60 characters on one line (within 100 columns).
            if chain.len() <= 60 && one_line.len() <= 100 {
                one_line
            } else {
                let mut text = String::from("    NetBackendServer::run_main(|config| {\n        NetBackendServer::new(config)\n");
                for m in modules {
                    text.push_str(&format!("            .module({})\n", m.register));
                }
                text.push_str("    })\n    .await");
                text
            }
        }
    };
    let modules_doc = match options.modules.as_slice() {
        [] => "none (the core server)".to_string(),
        modules => modules.iter().map(|m| format!("`{}`", m.register.trim_end_matches("::new()"))).collect::<Vec<_>>().join(", "),
    };
    // `user:create` is a command of the auth module.
    let user_create = if options.has("auth") { "//! cargo run -- user:create you@example.com --admin\n" } else { "" };
    render(SERVER_MAIN, &[("name", name), ("imports", &imports), ("main_body", &body), ("modules_doc", &modules_doc), ("user_create", user_create)])
}

/// The `[modules.*]` sections of the configuration (`docker`: the image's variant: mails without
/// their links, the players' files under /data).
fn module_sections(options: &Options, name: &str, docker: bool) -> String {
    let mut text = String::new();
    for module in &options.modules {
        let mut section = module.config.replace("{{name}}", name);
        if docker {
            for (from, to) in module.docker {
                section = section.replace(from, to);
            }
        }
        text.push('\n');
        text.push_str(&section);
    }
    text
}

/// The development `config.toml`: the database written out explicitly.
fn config(options: &Options, name: &str) -> String {
    let database = match options.database {
        Database::Sqlite => "# A SQLite file in the current folder, created on the first start.\nurl = \"sqlite:game.db\"".to_string(),
        Database::Postgres => format!(
            "# PostgreSQL on this machine: the role `game` with this password and the database `game`\n# (README.md: how to create them).\nurl = \"{}\"",
            dev_url(Database::Postgres)
        ),
        Database::Mysql => format!(
            "# MySQL on this machine: the user `game` with this password and the database `game` (README.md:\n# how to create them).\nurl = \"{}\"",
            dev_url(Database::Mysql)
        ),
    };
    render(CONFIG, &[("name", name), ("database", &database), ("module_sections", &module_sections(options, name, false))])
}

/// The development database URL of `config.toml`.
pub fn dev_url(database: Database) -> &'static str {
    match database {
        Database::Sqlite => "sqlite:game.db",
        Database::Postgres => "postgres://game:game-dev-password@127.0.0.1:5432/game",
        Database::Mysql => "mysql://game:game-dev-password@127.0.0.1:3306/game",
    }
}

/// Checks that every folder is new or empty, then writes them all; on an error removes what this
/// run wrote and returns the error.
pub fn write(folders: &[Folder]) -> Result<Vec<PathBuf>, GenError> {
    for folder in folders {
        match fs::read_dir(&folder.dir) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Err(GenError::NotEmpty(folder.dir.clone()));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            // A file (or anything else that is not a readable folder) is never overwritten.
            Err(_) if folder.dir.exists() => return Err(GenError::NotEmpty(folder.dir.clone())),
            Err(error) => return Err(GenError::Io(folder.dir.clone(), error)),
        }
    }
    let mut written = Vec::new();
    let mut done: Vec<(&Folder, Option<PathBuf>)> = Vec::new();
    for folder in folders {
        // The highest folder this run creates (`a` for a new `a/b/mygame`): removed on a failure.
        let created = folder.dir.ancestors().take_while(|a| !a.as_os_str().is_empty() && !a.exists()).last().map(Path::to_path_buf);
        match write_files(&folder.dir, &folder.files) {
            Ok(paths) => {
                written.extend(paths);
                done.push((folder, created));
            }
            Err(error) => {
                done.push((folder, created));
                for (folder, created) in done {
                    remove(&folder.dir, created.as_deref(), &folder.files);
                }
                return Err(error);
            }
        }
    }
    Ok(written)
}

/// Removes what a run wrote: the highest folder it created (the project folder and any new parent
/// folders), else the files' top-level entries in the folder (which was empty).
fn remove(dir: &Path, created: Option<&Path>, files: &[(String, String)]) {
    if let Some(created) = created {
        let _ = fs::remove_dir_all(created);
    } else {
        for (relative, _) in files {
            let path = dir.join(relative.split('/').next().unwrap_or(relative));
            let _ = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
        }
    }
}

fn write_files(target: &Path, files: &[(String, String)]) -> Result<Vec<PathBuf>, GenError> {
    let mut written = Vec::new();
    for (relative, content) in files {
        let path = relative.split('/').fold(target.to_path_buf(), |path, part| path.join(part));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| GenError::Io(parent.to_path_buf(), e))?;
        }
        // `create_new`: never replace a file, even one that appeared since the check above.
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| GenError::Io(path.clone(), e))?;
        io::Write::write_all(&mut file, content.as_bytes()).map_err(|e| GenError::Io(path.clone(), e))?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Answers;

    fn options(args: &[&str]) -> Options {
        Answers::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>()).unwrap().complete().unwrap()
    }

    fn paths(folder: &Folder) -> Vec<&str> {
        folder.files.iter().map(|(p, _)| p.as_str()).collect()
    }

    fn get<'a>(folder: &'a Folder, path: &str) -> &'a str {
        &folder.files.iter().find(|(p, _)| p == path).unwrap_or_else(|| panic!("{path} missing")).1
    }

    /// A fresh scratch folder (never the system temp folder): under `NET_BACKEND_E2E_DIR`, else the
    /// target folder.
    fn scratch(test: &str) -> PathBuf {
        let base = std::env::var_os("NET_BACKEND_E2E_DIR").map(PathBuf::from).unwrap_or_else(|| {
            let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
            target.unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")).join("net_backend-unit")
        });
        let dir = base.join(test).join("with space");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_sets() {
        let rust = &plan(&options(&["g"])).unwrap()[0];
        let common = ["README.md", ".gitignore", ".dockerignore", "config.toml"];
        assert_eq!(
            paths(rust),
            [
                &["Cargo.toml"][..],
                &common,
                &["config.docker.toml", "Dockerfile", "compose.yaml", "server/Cargo.toml", "server/src/main.rs"],
                &["client/Cargo.toml", "client/src/main.rs", "demo/Cargo.toml", "demo/src/main.rs", "demo/src/invite.rs", "demo/src/steam.rs"]
            ]
            .concat()
        );
        let api = &plan(&options(&["g", "--client", "api", "--db", "mysql"])).unwrap()[0];
        assert_eq!(paths(api), [&common[..], &["Dockerfile", "compose.yaml", "Cargo.toml", "src/main.rs"]].concat());
        let protocol = &plan(&options(&["g", "--client", "protocol"])).unwrap()[0];
        assert!(paths(protocol).contains(&"client/src/main.rs") && !paths(protocol).iter().any(|p| p.starts_with("demo/")));
        let bevy = plan(&options(&["g", "--client", "bevy", "--existing", "../game"])).unwrap();
        assert_eq!(bevy.len(), 2);
        assert!(!paths(&bevy[0]).iter().any(|p| p.starts_with("demo/")));
        assert_eq!(bevy[1].dir, Path::new("..").join(crate::options::STANDALONE_DEMO));
        assert_eq!(paths(&bevy[1]), ["Cargo.toml", "src/main.rs", "src/invite.rs", "src/steam.rs", "README.md", ".gitignore"]);
        assert!(get(&bevy[1], "Cargo.toml").contains("\n[workspace]\n") && get(&bevy[1], "Cargo.toml").contains("opt-level = 3"));
        assert_eq!(paths(&client_only(Path::new("c")).unwrap()[0]), ["Cargo.toml", "README.md", ".gitignore", "src/main.rs"]);
    }

    #[test]
    fn rendering_every_combination() {
        // None, each module alone (+ auth), the defaults, all of them.
        let all = crate::modules::names_list().replace(", ", ",");
        let defaults = crate::modules::defaults().iter().map(|m| m.name).collect::<Vec<_>>().join(",");
        let mut module_sets: Vec<&str> = vec!["none", &defaults, &all];
        module_sets.extend(crate::modules::MODULES.iter().map(|m| m.name));
        for client in ["rust", "bevy", "protocol", "api"] {
            for db in ["sqlite", "postgres", "mysql"] {
                for &modules in &module_sets {
                    for demo in ["--demo", "--no-demo"] {
                        let mut args = vec!["mygame", "--client", client, "--db", db, "--modules", modules];
                        if matches!(client, "rust" | "bevy") {
                            args.push(demo);
                        }
                        let o = options(&args);
                        for folder in plan(&o).unwrap() {
                            for (path, content) in &folder.files {
                                let at = format!("{args:?} {path}");
                                assert!(!content.contains("{{"), "{at}: a placeholder is left");
                                assert!(!content.contains('\r'), "{at}: CRLF");
                                assert!(content.ends_with('\n'), "{at}: no final newline");
                                assert!(!content.contains("ghcr.io"), "{at}: the published image");
                            }
                            if folder.dir != o.target {
                                continue;
                            }
                            let manifest = get(&folder, if client == "api" { "Cargo.toml" } else { "server/Cargo.toml" });
                            assert!(manifest.contains(&format!("version = \"{CRATES_VERSION}\"")));
                            assert!(manifest.contains(&format!("features = [\"{db}\"")), "{args:?}");
                            let config = get(&folder, "config.toml");
                            assert!(config.contains(&format!("url = \"{}\"", dev_url(o.database))), "{args:?}");
                            let main = get(&folder, if client == "api" { "src/main.rs" } else { "server/src/main.rs" });
                            for module in crate::modules::MODULES {
                                let name = module.name;
                                let has = o.has(name);
                                assert_eq!(has, modules.split(',').any(|m| m == name) || (name == "auth" && modules != "none"), "{args:?} {name}");
                                assert_eq!(config.contains(&format!("[modules.{name}]")), has, "{args:?} config {name}");
                                assert_eq!(main.contains(&format!(".module({})", module.register)), has, "{args:?} main {name}");
                                assert_eq!(main.contains(module.import), has, "{args:?} import {name}");
                                if let Some(feature) = module.feature {
                                    assert_eq!(manifest.contains(&format!("\"{feature}\"")), has, "{args:?} feature {name}");
                                }
                            }
                            assert_eq!(get(&folder, ".gitignore").contains("/files\n"), o.has("files"));
                            if db == "sqlite" {
                                let docker = get(&folder, "config.docker.toml");
                                assert_eq!(docker.contains("dir = \"/data/files\""), o.has("files"), "{args:?}");
                                assert!(!docker.contains("dir = \"files\"") && !docker.contains("log_mailer_show_links = true"));
                            }
                            let readme = get(&folder, "README.md");
                            assert!(readme.contains(&o.command_line()), "{args:?}: README without the command");
                            // The demo's Steam feature: how to try it, only with a demo.
                            assert_eq!(readme.contains("--features steam"), o.demo, "{args:?}");
                            if o.demo {
                                let demo = get(&folder, "demo/Cargo.toml");
                                assert!(demo.contains("steam = [\"dep:"));
                                if client == "bevy" {
                                    // The kit does Steam; steamworks only starts it.
                                    assert!(demo.contains(
                                        "bevy_steam_kit = { version = \"0.2.0\", features = [\"lobby\", \"friends\", \"auth\", \"overlay\", \"steam\"], optional = true }"
                                    ));
                                    assert!(demo.contains("steamworks = { version = \"=0.12.2\", optional = true }"));
                                    assert_eq!(get(&folder, "demo/src/steam.rs").matches("steamworks::").count(), 1, "steamworks only for init_app");
                                } else {
                                    assert!(demo.contains("steamworks = { version = \"=0.12.2\", features = [\"raw-bindings\"], optional = true }"));
                                }
                                assert!(get(&folder, "demo/src/main.rs").contains(
                                    "#[cfg(feature = \"steam\")]
mod steam;"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn server_main_layouts() {
        let main = |modules: &str| server_main(&options(&["g", "--modules", modules]), "g");
        assert!(main("none").contains("    NetBackendServer::run_main(|config| NetBackendServer::new(config)).await\n"));
        // `user:create` (an auth command) only with auth.
        assert!(
            !main("none").contains("user:create") && main("auth").contains("//! cargo run -- user:create you@example.com --admin\n//! cargo run -- config")
        );
        assert!(main("auth").contains("    NetBackendServer::run_main(|config| NetBackendServer::new(config).module(Auth::new())).await\n"));
        let all = main("auth,storage,chat");
        assert!(all.contains("        NetBackendServer::new(config)\n            .module(Auth::new())\n            .module(Storage::new())\n            .module(Chat::new())\n    })\n    .await\n"));
        assert!(all.contains(
            "use net_backend_server::Auth;\nuse net_backend_server::NetBackendServer;\nuse net_backend_server::chat::Chat;\nuse net_backend_server::storage::Storage;\n"
        ));
    }

    #[test]
    fn docker_files_follow_the_database() {
        let sqlite = &plan(&options(&["g"])).unwrap()[0];
        assert!(get(sqlite, "Dockerfile").contains("ARG CONFIG=config.docker.toml"));
        assert!(get(sqlite, "compose.yaml").contains("data:/data"));
        assert!(get(sqlite, "compose.yaml").contains("\n    build: .\n    # The image built from this folder.\n    image: g-server:local\n"));
        assert!(get(sqlite, "Dockerfile").contains("docker build -t g-server ."));
        // A name Docker refuses in an image (`game_-server`) gives a valid image name.
        let odd = &plan(&options(&["game_"])).unwrap()[0];
        assert!(get(odd, "compose.yaml").contains("image: game-server:local") && get(odd, "Dockerfile").contains("docker build -t game-server ."));
        let env: Vec<&str> = get(sqlite, "Dockerfile").lines().filter(|l| l.starts_with("ENV")).collect();
        assert_eq!(env, ["ENV NBS_CONFIG=/etc/net-backend/config.toml"]);
        let docker = get(sqlite, "config.docker.toml");
        assert!(docker.contains("url = \"sqlite:/data/game.db\"") && docker.contains("log_mailer_show_links = false"));
        for db in ["postgres", "mysql"] {
            let project = &plan(&options(&["g", "--db", db])).unwrap()[0];
            assert!(!paths(project).contains(&"config.docker.toml"));
            assert!(get(project, "Dockerfile").contains("ARG CONFIG=config.toml"));
            assert!(get(project, "compose.yaml").contains("caddy:"), "{db}");
        }
        // Multi-line instructions keep their continuations.
        let dockerfile = get(sqlite, "Dockerfile");
        let lines: Vec<&str> = dockerfile.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.starts_with("    ") {
                assert!(i > 0 && lines[i - 1].ends_with(" \\"), "Dockerfile line {}: no continuation before it", i + 1);
            }
        }
        assert!(dockerfile.contains("--retries=3 \\\n    CMD [\"/usr/local/bin/net-backend-server\", \"healthcheck\"]"));
    }

    /// The commands in the generated READMEs and doc comments work in every shell: no environment
    /// variable syntax, no shell scripts, no line continuations, no Unix-only tools.
    #[test]
    fn no_shell_specific_steps() {
        let shell_specific = ["$env:", "export ", "set NBS", "%NBS", "#!/", "bash -", "sh -c", ".sh ", "sudo", "chmod", "&&", " \\\n", " `\n", "curl "];
        for client in ["rust", "bevy", "protocol", "api"] {
            for db in ["sqlite", "postgres"] {
                for folder in plan(&options(&["g", "--client", client, "--db", db])).unwrap() {
                    for (path, content) in &folder.files {
                        // The README and the doc comments (the commands a user types); the Dockerfile
                        // and compose.yaml run inside Linux containers, not in the user's shell.
                        let typed: String = if path.ends_with(".md") {
                            content.clone()
                        } else if path.ends_with(".rs") {
                            content
                                .lines()
                                .filter(|l| l.starts_with("//!"))
                                .map(|l| {
                                    format!(
                                        "{l}
"
                                    )
                                })
                                .collect()
                        } else {
                            continue;
                        };
                        for token in shell_specific {
                            assert!(!typed.contains(token), "{client} {db} {path}: shell-specific `{}`", token.escape_debug());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn writes_and_never_overwrites() {
        let dir = scratch("writes");
        let target = dir.join("mygame");
        let folders = plan(&options(&[target.to_str().unwrap()])).unwrap();
        let written = write(&folders).unwrap();
        assert_eq!(written.len(), folders[0].files.len());
        assert!(target.join("server").join("src").join("main.rs").is_file());

        // A second run into the same (now non-empty) folder writes nothing.
        let before = fs::read_to_string(target.join("Cargo.toml")).unwrap();
        assert!(matches!(write(&plan(&options(&[target.to_str().unwrap(), "--client", "api"])).unwrap()), Err(GenError::NotEmpty(_))));
        assert_eq!(fs::read_to_string(target.join("Cargo.toml")).unwrap(), before);
        assert!(!target.join("src").exists());

        // A file of that name is refused too.
        fs::write(dir.join("afile"), "keep").unwrap();
        assert!(matches!(write(&client_only(&dir.join("afile")).unwrap()), Err(GenError::NotEmpty(_))));
        assert_eq!(fs::read_to_string(dir.join("afile")).unwrap(), "keep");

        // An existing empty folder is used.
        fs::create_dir(dir.join("empty")).unwrap();
        write(&client_only(&dir.join("empty")).unwrap()).unwrap();
        assert!(dir.join("empty").join("src").join("main.rs").is_file());

        // A failure half-way removes what was written: a new folder entirely, an empty one's new entries.
        let broken = |dir: PathBuf| Folder { dir, files: vec![("a/b.txt".into(), "1".into()), ("c.txt".into(), "2".into()), ("c.txt".into(), "3".into())] };
        assert!(matches!(write(&[broken(dir.join("fresh"))]), Err(GenError::Io(..))));
        assert!(!dir.join("fresh").exists());
        // New parent folders (`a/b/fresh`) go too.
        assert!(write(&[broken(dir.join("a").join("b").join("fresh"))]).is_err());
        assert!(!dir.join("a").exists());
        fs::create_dir(dir.join("kept")).unwrap();
        assert!(write(&[broken(dir.join("kept"))]).is_err());
        assert!(dir.join("kept").is_dir());
        assert_eq!(fs::read_dir(dir.join("kept")).unwrap().count(), 0);
        // Two folders: the second fails, the first is removed as well.
        let ok = Folder { dir: dir.join("first"), files: vec![("x.txt".into(), "x".into())] };
        assert!(write(&[ok, broken(dir.join("second"))]).is_err());
        assert!(!dir.join("first").exists() && !dir.join("second").exists());
        // A non-empty second folder: nothing at all is written.
        fs::create_dir_all(dir.join("full")).unwrap();
        fs::write(dir.join("full").join("keep.txt"), "k").unwrap();
        let ok = Folder { dir: dir.join("third"), files: vec![("x.txt".into(), "x".into())] };
        let full = Folder { dir: dir.join("full"), files: vec![("y.txt".into(), "y".into())] };
        assert!(matches!(write(&[ok, full]), Err(GenError::NotEmpty(_))));
        assert!(!dir.join("third").exists());

        // A bad name writes nothing.
        assert!(matches!(plan(&options(&[dir.join("Bad Name").to_str().unwrap()])), Err(GenError::InvalidName(_))));
        assert!(!dir.join("Bad Name").exists());
    }
}
