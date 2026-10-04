//! Project names: Cargo's package-name rules, kept strict, and the errors of writing a project.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Package names the generated projects must not take (their own dependencies) or that Cargo
/// refuses or warns about.
const RESERVED: &[&str] = &[
    // Dependencies of the generated projects.
    "net_backend_server",
    "bevy",
    "bevy_net_backend",
    "eframe",
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
    /// An embedded template does not have the expected shape (a bug of the installer).
    Template(String),
}

impl fmt::Display for GenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GenError::InvalidName(problem) => f.write_str(problem),
            GenError::NotEmpty(path) => write!(f, "`{}` already exists and is not an empty folder; nothing was written", path.display()),
            GenError::Io(path, error) => write!(f, "cannot write `{}`: {error}", path.display()),
            GenError::Template(problem) => f.write_str(problem),
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

/// The Docker image of the project's server, `<name>-server`, with the name changed only where
/// Docker refuses it: Docker allows `_`, `__` or any number of `-` between letters and digits, so
/// other runs of `-` / `_` (`_-`, `___`) become `-` and a trailing one goes (`game_` → `game-server`).
pub fn image_name(name: &str) -> String {
    let mut image = String::new();
    let mut run = String::new();
    for c in name.chars() {
        if c == '-' || c == '_' {
            run.push(c);
            continue;
        }
        if !run.is_empty() {
            let allowed = run == "_" || run == "__" || run.chars().all(|c| c == '-');
            image.push_str(if allowed { &run } else { "-" });
            run.clear();
        }
        image.push(c);
    }
    format!("{image}-server")
}

/// The project name a target path gives: its last component.
pub fn name_of(target: &Path) -> Result<&str, GenError> {
    match target.file_name() {
        Some(name) => name.to_str().ok_or_else(|| GenError::InvalidName(format!("`{}` is not valid UTF-8", target.display()))),
        None => Err(GenError::InvalidName(format!("`{}` does not end in a project name", target.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn image_names() {
        for (name, image) in [
            ("mygame", "mygame-server"),
            ("my-game", "my-game-server"),
            ("my_game", "my_game-server"),
            ("my__game", "my__game-server"),
            ("my---game", "my---game-server"),
            ("game_", "game-server"),
            ("game--", "game-server"),
            ("a_-b", "a-b-server"),
            ("a-_b", "a-b-server"),
            ("a___b", "a-b-server"),
            ("a_b_", "a_b-server"),
        ] {
            assert_eq!(image_name(name), image, "{name}");
            assert!(docker_component(image), "{image}");
        }
    }

    /// Docker's rule for a name component: letters and digits joined by `.`, `_`, `__` or `-`+.
    fn docker_component(text: &str) -> bool {
        let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
        let mut runs: Vec<String> = Vec::new();
        let mut last_alnum = None;
        for c in text.chars() {
            if last_alnum != Some(alnum(c)) {
                runs.push(String::new());
                last_alnum = Some(alnum(c));
            }
            runs.last_mut().unwrap().push(c);
        }
        let ends = text.starts_with(alnum) && text.ends_with(alnum);
        ends && runs.iter().filter(|r| !r.starts_with(alnum)).all(|r| r == "." || r == "_" || r == "__" || r.chars().all(|c| c == '-'))
    }
}
