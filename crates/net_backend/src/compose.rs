//! The production Compose file of a PostgreSQL / MySQL project: net_backend's own
//! `deploy/docker/compose.<db>.yaml` (copied into `templates/`), changed to build this project's
//! server and to configure only its modules.

use crate::modules::MODULES;
use crate::options::{Database, Options};

const POSTGRES: &str = include_str!("../templates/compose.postgres.yaml");
const MYSQL: &str = include_str!("../templates/compose.mysql.yaml");

/// The published image the deploy files use; a generated project builds its own instead.
const IMAGE: &str = "${NBS_IMAGE:-ghcr.io/warmar94/net_backend_server:0.2}";
/// The commented `build:` block of the deploy files.
const BUILD_COMMENTED: &str = "    # Your own server, built here from its project (the Dockerfile of deploy/docker, copied):
    # build:
    #   context: .
    #   args: { CARGO_ARGS: \"--bin mygame\", BINARY: target/release/mygame, CONFIG: config.toml }
    # pull_policy: build
";
/// The note on `NBS_IMAGE` in the deploy files' header.
const IMAGE_NOTE: &str = "#   NBS_IMAGE    the server image (default ghcr.io/warmar94/net_backend_server:0.2); your own server's
#                image, or see `build:` under `migrate`
";
/// The server's mount of the players' files volume, and the volume.
const FILES_MOUNT: &str = "      # The players' files (`[modules.files] dir`).\n      - server_data:/data\n";
const FILES_VOLUME: &str = "\n  server_data:\n";
const CONFIG_START: &str = "x-nbs-config: &nbs-config |\n";
/// The deploy files' migrations folder (mounted next to the configuration), and the project's: its
/// own `./migrations`, built into the image by its Dockerfile.
const MIGRATIONS: &str = "  migrations_dir = \"/etc/net-backend/migrations\"\n";
const PROJECT_MIGRATIONS: &str = "  # The project's own migrations (./migrations), built into the image.\n  migrations_dir = \"/app/migrations\"\n";

/// An error naming a part of the source file that is not there.
fn need_in(text: &str, part: &str) -> Result<(), String> {
    if text.contains(part) {
        Ok(())
    } else {
        Err(format!("compose template: `{}` not found", part.trim().lines().next().unwrap_or(part)))
    }
}
const CONFIG_END: &str = "\nservices:\n";

/// The source file (as in net_backend's `deploy/docker/`).
pub fn source(database: Database) -> Option<&'static str> {
    match database {
        Database::Sqlite => None,
        Database::Postgres => Some(POSTGRES),
        Database::Mysql => Some(MYSQL),
    }
}

/// The project's `compose.yaml` for PostgreSQL / MySQL (`None` for SQLite). `package` is the
/// server's binary. Errors name a part of the source file that was not found (a test keeps the
/// copies in `templates/` equal to `deploy/docker/`).
pub fn production(options: &Options, name: &str, package: &str) -> Result<Option<String>, String> {
    let Some(source) = source(options.database) else { return Ok(None) };
    let text = source.replace("\r\n", "\n");
    let need = |part: &str| if text.contains(part) { Ok(()) } else { Err(format!("compose template: `{}` not found", part.lines().next().unwrap_or(part))) };
    need(IMAGE)?;
    need(IMAGE_NOTE)?;
    need(BUILD_COMMENTED)?;
    need(CONFIG_START)?;
    need("  app_name = \"My Game\"\n")?;
    need(MIGRATIONS)?;
    let image = crate::names::image_name(name);
    let build = format!(
        "    # This project's server, built from this folder (Dockerfile).\n    build:\n      context: .\n      args: {{ CARGO_ARGS: \"--bin {package}\", BINARY: target/release/{package}, CONFIG: config.toml }}\n    pull_policy: build\n"
    );
    let image_note = format!(
        "#   NBS_IMAGE    the name of the image built from this folder (default {image}:local)
"
    );
    let text = text
        .replace(IMAGE_NOTE, &image_note)
        .replace(IMAGE, &format!("${{NBS_IMAGE:-{image}:local}}"))
        .replace(BUILD_COMMENTED, &build)
        .replacen("  app_name = \"My Game\"\n", &format!("  app_name = \"{name}\"\n"), 1)
        .replacen(MIGRATIONS, PROJECT_MIGRATIONS, 1);
    let mut text = keep_modules(&text, options)?;
    if !options.has("files") {
        // The players' files volume only serves `[modules.files] dir`.
        need_in(&text, FILES_MOUNT)?;
        need_in(&text, FILES_VOLUME)?;
        text = text.replacen(FILES_MOUNT, "", 1).replacen(FILES_VOLUME, "\n", 1);
    }
    let header = format!(
        "# The {name} server in production: {db}, the server built from this folder, and Caddy (HTTPS + WSS).\n\
         # Written by net-backend from net_backend's deploy/docker/compose.{flag}.yaml. In this folder:\n\
         #\n\
         #   1. write a file .env with one line:   DOMAIN=api.example.com\n\
         #   2. docker compose up -d --build\n\
         #\n\
         # The notes below come from that file (its steps 1-3 are the two above).\n\
         #\n",
        db = options.database.name(),
        flag = options.database.flag(),
    );
    Ok(Some(format!("{header}{text}")))
}

/// Removes the `[modules.<name>]` sections (and `[[modules.<name>.…]]` tables) of the modules the
/// project does not have from the `x-nbs-config` block.
fn keep_modules(text: &str, options: &Options) -> Result<String, String> {
    let start = text.find(CONFIG_START).ok_or("compose template: no x-nbs-config")? + CONFIG_START.len();
    let end = start + text[start..].find(CONFIG_END).ok_or("compose template: no services after x-nbs-config")?;
    let dropped: Vec<&str> = MODULES.iter().filter(|m| !options.has(m.name)).map(|m| m.name).collect();
    let mut kept = String::new();
    let mut skipping = false;
    for line in text[start..end].split_inclusive('\n') {
        if line.starts_with("  [") {
            let header = line.trim_start().trim_start_matches('[');
            skipping = dropped.iter().any(|name| header.starts_with(&format!("modules.{name}]")) || header.starts_with(&format!("modules.{name}.")));
        }
        if !skipping {
            kept.push_str(line);
        }
    }
    Ok(format!("{}{}{}", &text[..start], kept, &text[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Answers;

    fn options(args: &[&str]) -> Options {
        Answers::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>()).unwrap().complete().unwrap()
    }

    /// The copies in `templates/` are net_backend's deploy files (checked where the repository is
    /// present; a published package has no `deploy/`).
    #[test]
    fn templates_match_the_deploy_files() {
        let deploy = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/docker");
        for (file, template) in [("compose.postgres.yaml", POSTGRES), ("compose.mysql.yaml", MYSQL)] {
            if let Ok(original) = std::fs::read_to_string(deploy.join(file)) {
                assert_eq!(original.replace("\r\n", "\n"), template.replace("\r\n", "\n"), "templates/{file} differs from deploy/docker/{file}: copy it again");
            }
        }
    }

    #[test]
    fn production_files() {
        assert!(production(&options(&["g"]), "g", "server").unwrap().is_none());
        for db in ["postgres", "mysql"] {
            let text = production(&options(&["g", "--db", db]), "mygame", "server").unwrap().unwrap();
            assert!(!text.contains("ghcr.io"), "{db}");
            assert!(text.contains("${NBS_IMAGE:-mygame-server:local}"));
            assert!(text.contains("    build:\n      context: .\n      args: { CARGO_ARGS: \"--bin server\", BINARY: target/release/server, CONFIG: config.toml }\n    pull_policy: build\n"));
            assert!(text.contains("  app_name = \"mygame\"\n"));
            // The project's migrations: built into the image at /app/migrations (its Dockerfile).
            assert!(text.contains("\n  migrations_dir = \"/app/migrations\"\n") && !text.contains("/etc/net-backend/migrations"), "{db}");
            // An image name Docker accepts for any project name.
            let odd = production(&options(&["game_", "--db", db]), "game_", "server").unwrap().unwrap();
            assert!(odd.contains("${NBS_IMAGE:-game-server:local}") && odd.contains("(default game-server:local)") && !odd.contains("game_-server"), "{db}");
            // The defaults: every module but oauth.
            for module in MODULES {
                let section = format!("  [modules.{}]\n", module.name);
                assert_eq!(text.contains(&section), module.default, "{db} {section}");
            }
            for table in ["[[modules.chat.rooms]]", "[[modules.leaderboards.boards]]", "[[modules.matchmaking.queues]]"] {
                assert!(text.contains(table), "{db} {table}");
            }
            assert!(text.contains("  dir = \"/data/files\"\n") && text.contains(FILES_MOUNT) && text.contains(FILES_VOLUME));
            assert!(text.contains("stop_grace_period: 60s"));
            let all = crate::modules::names_list().replace(", ", ",");
            let every = production(&options(&["g", "--db", db, "--modules", &all]), "g", "g").unwrap().unwrap();
            assert!(every.contains("  [modules.oauth]\n  # OpenID Connect logins"));
            let few = production(&options(&["g", "--db", db, "--modules", "auth"]), "g", "g").unwrap().unwrap();
            assert!(few.contains("[modules.auth]") && !few.contains("[modules.storage]") && !few.contains("modules.chat"));
            assert!(!few.contains("server_data"), "{few}");
            assert!(few.contains("  server_files:\n  caddy_data:\n"), "{few}");
            let files = production(&options(&["g", "--db", db, "--modules", "files"]), "g", "g").unwrap().unwrap();
            assert!(files.contains("  [modules.files]\n") && files.contains(FILES_MOUNT) && !files.contains("[modules.friends]"));
            let none = production(&options(&["g", "--db", db, "--modules", "none"]), "g", "g").unwrap().unwrap();
            assert!(!none.contains("[modules.") && !none.contains("[[modules."));
            // Everything after the configuration block is unchanged (but the files volume).
            assert!(none.contains("\nservices:\n  # Writes db_password"));
            let source = source(if db == "mysql" { Database::Mysql } else { Database::Postgres }).unwrap().replace("\r\n", "\n");
            let services = |text: &str| text[text.find("\nservices:\n").unwrap()..].to_string();
            // The build block replaces the commented one line for line; the files mount (2 lines)
            // and the volume (1 line) go.
            assert_eq!(services(&every).lines().count(), services(&source).lines().count());
            assert_eq!(services(&none).lines().count(), services(&source).lines().count() - 3);
        }
    }
}
