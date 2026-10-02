//! The project generator: the embedded templates, the name rules and the file writing.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The version of the net_backend crates the generated projects depend on (this crate's own: the
/// generator is released together with the server, the protocol and the client).
pub const CRATES_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What `new*` generates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A workspace with `server/` and `client/` (`net-backend new`).
    Full,
    /// A server project (`net-backend new-server`).
    Server,
    /// A client project (`net-backend new-client`).
    Client,
}

impl Kind {
    /// What the success message calls it.
    pub fn describe(self) -> &'static str {
        match self {
            Kind::Full => "server + client",
            Kind::Server => "server",
            Kind::Client => "client",
        }
    }
}

// The templates (embedded: generating a project needs no network and no files besides the binary).
// Manifests carry `.tmpl` so that Cargo never treats a template folder as a package.
const WORKSPACE_MANIFEST: &str = include_str!("../templates/workspace.Cargo.toml.tmpl");
const SERVER_MANIFEST: &str = include_str!("../templates/server/Cargo.toml.tmpl");
const SERVER_MAIN: &str = include_str!("../templates/server/main.rs");
const CLIENT_MANIFEST: &str = include_str!("../templates/client/Cargo.toml.tmpl");
const CLIENT_MAIN: &str = include_str!("../templates/client/main.rs");
const CONFIG: &str = include_str!("../templates/config.toml");
const CONFIG_DOCKER: &str = include_str!("../templates/config.docker.toml");
const DOCKERFILE: &str = include_str!("../templates/Dockerfile");
const COMPOSE: &str = include_str!("../templates/compose.yaml");
const GITIGNORE: &str = include_str!("../templates/gitignore");
const GITIGNORE_CLIENT: &str = include_str!("../templates/gitignore-client");
const DOCKERIGNORE: &str = include_str!("../templates/dockerignore");
const README_FULL: &str = include_str!("../templates/README.full.md");
const README_SERVER: &str = include_str!("../templates/README.server.md");
const README_CLIENT: &str = include_str!("../templates/README.client.md");

/// Package names the generated projects must not take (their own dependencies) or that Cargo
/// refuses or warns about.
const RESERVED: &[&str] = &[
    // Dependencies of the generated projects.
    "net_backend_server",
    "net_backend_client",
    "net_backend_protocol",
    "tokio",
    "serde_json",
    // Cargo: the standard library's crates and its own build folders.
    "std",
    "core",
    "alloc",
    "proc_macro",
    "test",
    "build",
    "deps",
    "examples",
    "incremental",
    // Rust keywords (a package name becomes a crate name).
    "abstract",
    "as",
    "async",
    "await",
    "become",
    "box",
    "break",
    "const",
    "continue",
    "crate",
    "do",
    "dyn",
    "else",
    "enum",
    "extern",
    "false",
    "final",
    "fn",
    "for",
    "gen",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "macro",
    "match",
    "mod",
    "move",
    "mut",
    "override",
    "priv",
    "pub",
    "ref",
    "return",
    "self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "try",
    "type",
    "typeof",
    "union",
    "unsafe",
    "unsized",
    "use",
    "virtual",
    "where",
    "while",
    "yield",
];

/// File names Windows refuses (with or without an extension).
const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7",
    "lpt8", "lpt9",
];

/// The longest accepted name (crates.io's limit).
pub const MAX_NAME_LEN: usize = 64;

/// Why a name is refused, or why the project could not be written.
#[derive(Debug)]
pub enum GenError {
    /// The name breaks a rule (the message says which).
    InvalidName(String),
    /// The target exists and is not an empty folder.
    NotEmpty(PathBuf),
    /// A file system error.
    Io(PathBuf, io::Error),
}

impl fmt::Display for GenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GenError::InvalidName(problem) => f.write_str(problem),
            GenError::NotEmpty(path) => write!(f, "`{}` already exists and is not an empty folder; nothing was written", path.display()),
            GenError::Io(path, error) => write!(f, "cannot write `{}`: {error}", path.display()),
        }
    }
}

impl std::error::Error for GenError {}

/// Check a project name against Cargo's package-name rules (kept strict so that the same name works
/// as a folder, a package, a binary and a Docker image on every OS): 1 to 64 characters of `a-z`,
/// `0-9`, `-` and `_`, starting with a letter, not a Rust keyword, a standard-library crate, a
/// dependency of the project or a reserved Windows file name.
pub fn validate_name(name: &str) -> Result<(), GenError> {
    let refuse = |why: String| Err(GenError::InvalidName(format!("`{name}` cannot be a project name: {why}")));
    if name.is_empty() {
        return Err(GenError::InvalidName("the project name is empty".into()));
    }
    if name.len() > MAX_NAME_LEN {
        return refuse(format!("it is longer than {MAX_NAME_LEN} characters"));
    }
    if let Some(bad) = name.chars().find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_')) {
        return refuse(format!("`{bad}` is not allowed (use a-z, 0-9, `-` and `_`)"));
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return refuse("it must start with a letter (a-z)".into());
    }
    let normalized = name.replace('-', "_");
    if RESERVED.contains(&normalized.as_str()) {
        return refuse("the name is reserved (a Rust keyword, a standard-library crate or a dependency of the project)".into());
    }
    if WINDOWS_RESERVED.contains(&name) {
        return refuse("Windows reserves this file name".into());
    }
    Ok(())
}

/// The files of a project, as (path relative to the project folder with `/` separators, content).
/// Every file uses LF line endings.
pub fn files(kind: Kind, name: &str) -> Vec<(&'static str, String)> {
    let render = |template: &str, package: &str, client_run: &str| {
        template
            .replace("\r\n", "\n")
            .replace("{{name}}", name)
            .replace("{{package}}", package)
            .replace("{{version}}", CRATES_VERSION)
            .replace("{{client_run}}", client_run)
    };
    match kind {
        Kind::Full => vec![
            ("Cargo.toml", render(WORKSPACE_MANIFEST, name, "")),
            ("README.md", render(README_FULL, name, "")),
            (".gitignore", render(GITIGNORE, name, "")),
            (".dockerignore", render(DOCKERIGNORE, name, "")),
            ("config.toml", render(CONFIG, name, "")),
            ("config.docker.toml", render(CONFIG_DOCKER, name, "")),
            ("Dockerfile", render(DOCKERFILE, "server", "")),
            ("compose.yaml", render(COMPOSE, "server", "")),
            ("server/Cargo.toml", render(SERVER_MANIFEST, "server", "")),
            ("server/src/main.rs", render(SERVER_MAIN, "server", "")),
            ("client/Cargo.toml", render(CLIENT_MANIFEST, "client", "")),
            ("client/src/main.rs", render(CLIENT_MAIN, "client", "cargo run -p client")),
        ],
        Kind::Server => vec![
            ("Cargo.toml", render(SERVER_MANIFEST, name, "")),
            ("README.md", render(README_SERVER, name, "")),
            (".gitignore", render(GITIGNORE, name, "")),
            (".dockerignore", render(DOCKERIGNORE, name, "")),
            ("config.toml", render(CONFIG, name, "")),
            ("config.docker.toml", render(CONFIG_DOCKER, name, "")),
            ("Dockerfile", render(DOCKERFILE, name, "")),
            ("compose.yaml", render(COMPOSE, name, "")),
            ("src/main.rs", render(SERVER_MAIN, name, "")),
        ],
        Kind::Client => vec![
            ("Cargo.toml", render(CLIENT_MANIFEST, name, "")),
            ("README.md", render(README_CLIENT, name, "")),
            (".gitignore", render(GITIGNORE_CLIENT, name, "")),
            ("src/main.rs", render(CLIENT_MAIN, name, "cargo run")),
        ],
    }
}

/// The project name a target path gives: its last component.
pub fn name_of(target: &Path) -> Result<&str, GenError> {
    match target.file_name() {
        Some(name) => name.to_str().ok_or_else(|| GenError::InvalidName(format!("`{}` is not valid UTF-8", target.display()))),
        None => Err(GenError::InvalidName(format!("`{}` does not end in a project name", target.display()))),
    }
}

/// Write a project of this kind into `target` (created; refused when it exists and is not an empty
/// folder). The project name is the last component of `target`. Returns the written files.
pub fn generate(kind: Kind, target: &Path) -> Result<Vec<PathBuf>, GenError> {
    let name = name_of(target)?;
    validate_name(name)?;
    match fs::read_dir(target) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(GenError::NotEmpty(target.to_path_buf()));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        // A file (or anything else that is not a readable folder) is never overwritten.
        Err(_) if target.exists() => return Err(GenError::NotEmpty(target.to_path_buf())),
        Err(error) => return Err(GenError::Io(target.to_path_buf(), error)),
    }
    let created = !target.exists();
    write_or_remove(target, created, &files(kind, name))
}

/// Write the files; on an error remove what this run wrote (the whole folder when `created`, else the
/// files' top-level entries in the folder, which was empty) and return the error.
fn write_or_remove(target: &Path, created: bool, files: &[(&str, String)]) -> Result<Vec<PathBuf>, GenError> {
    write_files(target, files).inspect_err(|_| {
        if created {
            let _ = fs::remove_dir_all(target);
        } else {
            for (relative, _) in files {
                let path = target.join(relative.split('/').next().unwrap_or(relative));
                let _ = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
            }
        }
    })
}

fn write_files(target: &Path, files: &[(&str, String)]) -> Result<Vec<PathBuf>, GenError> {
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

    /// A fresh scratch folder under the target directory (never the system temp folder).
    fn scratch(test: &str) -> PathBuf {
        let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
        let dir = target.join("net_backend-unit").join(test).join("with space");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names() {
        for good in ["mygame", "my-game", "my_game", "g", "game2", "a-1_b", &"a".repeat(64)] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "MyGame",
            "2game",
            "-game",
            "_game",
            "my game",
            "my.game",
            "my/game",
            "spiel\u{e9}",
            "test",
            "std",
            "self",
            "fn",
            "gen",
            "tokio",
            "net_backend_server",
            "net-backend-client",
            "con",
            "lpt1",
            "build",
            &"a".repeat(65),
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn file_sets() {
        let paths = |kind| files(kind, "mygame").into_iter().map(|(path, _)| path).collect::<Vec<_>>();
        assert_eq!(
            paths(Kind::Full),
            [
                "Cargo.toml",
                "README.md",
                ".gitignore",
                ".dockerignore",
                "config.toml",
                "config.docker.toml",
                "Dockerfile",
                "compose.yaml",
                "server/Cargo.toml",
                "server/src/main.rs",
                "client/Cargo.toml",
                "client/src/main.rs"
            ]
        );
        assert_eq!(
            paths(Kind::Server),
            ["Cargo.toml", "README.md", ".gitignore", ".dockerignore", "config.toml", "config.docker.toml", "Dockerfile", "compose.yaml", "src/main.rs"]
        );
        assert_eq!(paths(Kind::Client), ["Cargo.toml", "README.md", ".gitignore", "src/main.rs"]);
    }

    #[test]
    fn rendering() {
        let version = format!("version = \"{CRATES_VERSION}\"");
        for kind in [Kind::Full, Kind::Server, Kind::Client] {
            for (path, content) in files(kind, "mygame") {
                assert!(!content.contains("{{") && !content.contains("}}"), "{kind:?} {path}: a placeholder is left");
                assert!(!content.contains('\r'), "{kind:?} {path}: CRLF");
                assert!(content.ends_with('\n'), "{kind:?} {path}: no final newline");
                if path.ends_with("Cargo.toml") && !content.contains("[workspace]") {
                    assert!(content.contains(&version), "{kind:?} {path}: not pinned to {CRATES_VERSION}");
                }
            }
        }
        let full = files(Kind::Full, "mygame");
        let get = |set: &[(&str, String)], path: &str| set.iter().find(|(p, _)| *p == path).unwrap().1.clone();
        assert!(get(&full, "Cargo.toml").contains("default-members = [\"server\"]"));
        assert!(get(&full, "server/Cargo.toml").contains("name = \"server\""));
        assert!(get(&full, "client/Cargo.toml").contains("name = \"client\""));
        assert!(get(&full, "client/src/main.rs").contains("cargo run -p client"));
        assert!(get(&full, "Dockerfile").contains("--bin server"));
        // Multi-line instructions keep their continuations: an indented line follows a line ending in
        // `\`, and no instruction is a run of joined lines.
        let dockerfile = get(&full, "Dockerfile");
        let lines: Vec<&str> = dockerfile.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.starts_with("    ") {
                assert!(i > 0 && lines[i - 1].ends_with(" \\"), "Dockerfile line {}: no continuation before it", i + 1);
            }
            assert!(line.starts_with('#') || !line.trim_start().contains("    "), "Dockerfile line {}: joined lines", i + 1);
        }
        assert!(dockerfile.contains("--retries=3 \\\n    CMD [\"/usr/local/bin/net-backend-server\", \"healthcheck\"]"));
        assert!(dockerfile.contains("RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \\\n"));
        // The image sets no setting by environment (a production Compose file's configuration must win).
        let env: Vec<String> = get(&full, "Dockerfile").lines().filter(|l| l.starts_with("ENV")).map(String::from).collect();
        assert_eq!(env, ["ENV NBS_CONFIG=/etc/net-backend/config.toml"]);
        assert!(get(&full, "config.docker.toml").contains("url = \"sqlite:/data/game.db\""));
        assert!(get(&full, "config.toml").contains("app_name = \"mygame\""));
        let server = files(Kind::Server, "my-game");
        assert!(get(&server, "Cargo.toml").contains("name = \"my-game\""));
        assert!(get(&server, "Dockerfile").contains("ARG BINARY=target/release/my-game"));
        let client = files(Kind::Client, "my-client");
        assert!(get(&client, "Cargo.toml").contains("name = \"my-client\""));
        assert!(get(&client, "src/main.rs").contains("//! cargo run -- https://"));
    }

    /// The commands in the generated READMEs and doc comments work in every shell: no environment
    /// variable syntax, no shell scripts, no line continuations, no Unix-only tools.
    #[test]
    fn no_shell_specific_steps() {
        let shell_specific = ["$env:", "export ", "set NBS", "%NBS", "#!/", "bash -", "sh -c", ".sh ", "sudo", "chmod", "&&", " \\\n", " `\n", "curl "];
        for kind in [Kind::Full, Kind::Server, Kind::Client] {
            for (path, content) in files(kind, "mygame") {
                if path == "Dockerfile" {
                    continue; // runs inside the Linux build container, not in the user's shell
                }
                for token in shell_specific {
                    assert!(!content.contains(token), "{kind:?} {path}: shell-specific `{}`", token.escape_debug());
                }
            }
        }
    }

    #[test]
    fn writes_and_never_overwrites() {
        let dir = scratch("writes");
        let target = dir.join("mygame");
        let written = generate(Kind::Full, &target).unwrap();
        assert_eq!(written.len(), files(Kind::Full, "mygame").len());
        assert!(target.join("server").join("src").join("main.rs").is_file());
        assert_eq!(fs::read_to_string(target.join("config.toml")).unwrap(), files(Kind::Full, "mygame")[4].1);

        // A second run into the same (now non-empty) folder writes nothing.
        let before = fs::read_to_string(target.join("Cargo.toml")).unwrap();
        assert!(matches!(generate(Kind::Server, &target), Err(GenError::NotEmpty(_))));
        assert_eq!(fs::read_to_string(target.join("Cargo.toml")).unwrap(), before);
        assert!(!target.join("src").exists());

        // A file of that name is refused too.
        fs::write(dir.join("afile"), "keep").unwrap();
        assert!(matches!(generate(Kind::Client, &dir.join("afile")), Err(GenError::NotEmpty(_))));
        assert_eq!(fs::read_to_string(dir.join("afile")).unwrap(), "keep");

        // An existing empty folder is used.
        fs::create_dir(dir.join("empty")).unwrap();
        generate(Kind::Client, &dir.join("empty")).unwrap();
        assert!(dir.join("empty").join("src").join("main.rs").is_file());

        // A failure half-way removes what was written: a new folder entirely, an empty one's new entries.
        let broken = [("a/b.txt", String::from("1")), ("c.txt", String::from("2")), ("c.txt", String::from("3"))];
        let fresh = dir.join("fresh");
        assert!(matches!(write_or_remove(&fresh, true, &broken), Err(GenError::Io(..))));
        assert!(!fresh.exists());
        fs::create_dir(dir.join("kept")).unwrap();
        assert!(write_or_remove(&dir.join("kept"), false, &broken).is_err());
        assert!(dir.join("kept").is_dir());
        assert_eq!(fs::read_dir(dir.join("kept")).unwrap().count(), 0);

        // A bad name writes nothing.
        assert!(matches!(generate(Kind::Full, &dir.join("Bad Name")), Err(GenError::InvalidName(_))));
        assert!(!dir.join("Bad Name").exists());
    }
}
