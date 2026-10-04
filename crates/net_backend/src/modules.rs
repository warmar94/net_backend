//! The server modules the installer offers: one row per module. A new module of net_backend_server
//! joins the installer by adding a row here (its flag, what it needs, and the pieces it puts into the
//! generated server).

/// One server module.
#[derive(Debug, PartialEq, Eq)]
pub struct Module {
    /// The name in `--modules` and in the server's `[modules.<name>]` configuration section.
    pub name: &'static str,
    /// What the question shows.
    pub label: &'static str,
    /// The modules it needs (selecting it selects these too).
    pub requires: &'static [&'static str],
    /// Part of the default set (`-y`, no `--modules`, ticked in the question). Off for a module that
    /// does nothing until it is set up outside the project.
    pub default: bool,
    /// The cargo feature of `net_backend_server` it needs (`None`: part of the core).
    pub feature: Option<&'static str>,
    /// The `use` line of the generated `main.rs`.
    pub import: &'static str,
    /// The expression registered with `.module(..)`.
    pub register: &'static str,
    /// Its section of the development `config.toml` (`{{name}}` = the project name).
    pub config: &'static str,
    /// Text replacements that turn `config` into the section of the Docker image's configuration.
    pub docker: &'static [(&'static str, &'static str)],
}

/// Every module, in registration order (a module comes after the ones it needs; the order of
/// net_backend_server's reference server).
pub const MODULES: &[Module] = &[
    Module {
        name: "auth",
        label: "accounts (auth): register, login, sessions, email verification, password reset",
        requires: &[],
        default: true,
        feature: None,
        import: "use net_backend_server::Auth;",
        register: "Auth::new()",
        config: "[modules.auth]\n\
                 app_name = \"{{name}}\"\n\
                 # Development: the log shows whole mails (verification and password-reset links). Mail through\n\
                 # SMTP: the feature `smtp` of net_backend_server and mailer = \"smtp\" (see its guide).\n\
                 log_mailer_show_links = true\n",
        docker: &[(
            "# Development: the log shows whole mails (verification and password-reset links). Mail through\n\
             # SMTP: the feature `smtp` of net_backend_server and mailer = \"smtp\" (see its guide).\n\
             log_mailer_show_links = true\n",
            "# The log mailer writes only the recipient (masked) and the subject of each mail.\n\
             log_mailer_show_links = false\n",
        )],
    },
    Module {
        name: "storage",
        label: "saves (storage): per-player JSON objects with versions",
        requires: &["auth"],
        default: true,
        feature: Some("storage"),
        import: "use net_backend_server::storage::Storage;",
        register: "Storage::new()",
        config: "[modules.storage]\n\
                 # Defaults: 256 KiB per object, 1000 objects and 4 MiB per player, 60 writes per minute.\n",
        docker: &[],
    },
    Module {
        name: "chat",
        label: "chat: rooms, messages, history, presence, edits, read markers, typing, players' rooms",
        requires: &["auth"],
        default: true,
        feature: Some("chat"),
        import: "use net_backend_server::chat::Chat;",
        register: "Chat::new()",
        config: "[modules.chat]\n\
                 # Defaults: 500 characters per message, history kept 30 days. Senders edit their messages for\n\
                 # 900 s; read markers and typing pushes are on; players create rooms (10 owned each).\n\
                 \n\
                 [[modules.chat.rooms]]\n\
                 key = \"world\"\n\
                 name = \"World\"\n",
        docker: &[],
    },
    Module {
        name: "leaderboards",
        label: "leaderboards: boards, scores, ranks, the players around you",
        requires: &["auth"],
        default: true,
        feature: Some("leaderboards"),
        import: "use net_backend_server::leaderboards::Leaderboards;",
        register: "Leaderboards::new()",
        config: "[modules.leaderboards]\n\
                 # A board per score your game keeps (mode best | latest | sum, order desc | asc, period\n\
                 # all_time | daily | weekly).\n\
                 \n\
                 [[modules.leaderboards.boards]]\n\
                 key = \"highscore\"\n\
                 name = \"High score\"\n\
                 mode = \"best\"\n\
                 order = \"desc\"\n\
                 period = \"all_time\"\n",
        docker: &[],
    },
    Module {
        name: "notifications",
        label: "notifications: stored per player, pushed live",
        requires: &["auth"],
        default: true,
        feature: Some("notifications"),
        import: "use net_backend_server::notifications::Notifications;",
        register: "Notifications::new()",
        config: "[modules.notifications]\n\
                 # Defaults: kept 30 days, at most 200 per player, 4 KiB of data each.\n",
        docker: &[],
    },
    Module {
        name: "friends",
        label: "friends: requests by id, name or friend code, blocks, online state",
        requires: &["auth"],
        default: true,
        feature: Some("friends"),
        import: "use net_backend_server::friends::Friends;",
        register: "Friends::new()",
        config: "[modules.friends]\n\
                 # Defaults: 200 friends, 50 open requests and 500 blocks per player. With the notifications\n\
                 # module, requests and acceptances are notifications too.\n",
        docker: &[],
    },
    Module {
        name: "groups",
        label: "groups: guilds / clans with roles, invitations, a chat room each",
        requires: &["auth"],
        default: true,
        feature: Some("groups"),
        import: "use net_backend_server::groups::Groups;",
        register: "Groups::new()",
        config: "[modules.groups]\n\
                 # Defaults: 100 members per group, 10 groups per player. With the chat module every group has\n\
                 # a chat room; with the notifications module, invitations are notifications too.\n",
        docker: &[],
    },
    Module {
        name: "oauth",
        label: "OpenID Connect logins (oauth): Google or another provider; off until one is set up in config.toml",
        requires: &["auth"],
        default: false,
        feature: Some("oauth"),
        import: "use net_backend_server::oauth::OAuth;",
        register: "OAuth::new()",
        config: "[modules.oauth]\n\
                 # OpenID Connect logins (POST /v1/auth/oauth/{provider}). Registered, but off until a provider is\n\
                 # set here: until then every login answers 404. Google, with your OAuth client id from the\n\
                 # Google Cloud console:\n\
                 # [modules.oauth.providers.google]\n\
                 # preset = \"google\"\n\
                 # client_ids = [\"1234-abc.apps.googleusercontent.com\"]\n",
        docker: &[],
    },
    Module {
        name: "lobbies",
        label: "lobbies: a host, join codes, ready flags, metadata, a chat room each",
        requires: &["auth"],
        default: true,
        feature: Some("lobbies"),
        import: "use net_backend_server::lobbies::Lobbies;",
        register: "Lobbies::new()",
        config: "[modules.lobbies]\n\
                 # Defaults: 64 players per lobby, 1 lobby per player at a time; a disconnected player leaves\n\
                 # after 30 s. With the chat module every lobby has a chat room.\n",
        docker: &[],
    },
    Module {
        name: "matchmaking",
        label: "matchmaking: queues, tickets, \"match found\" pushes (your rules through hooks)",
        requires: &["auth"],
        default: true,
        feature: Some("matchmaking"),
        import: "use net_backend_server::matchmaking::Matchmaking;",
        register: "Matchmaking::new()",
        config: "[modules.matchmaking]\n\
                 # Tickets live in the server's memory; a round every second. A queue per game mode:\n\
                 \n\
                 [[modules.matchmaking.queues]]\n\
                 key = \"duel\"\n\
                 players = 2\n\
                 \n\
                 # One player per match: a match at once (to try the flow alone).\n\
                 [[modules.matchmaking.queues]]\n\
                 key = \"solo\"\n\
                 players = 1\n",
        docker: &[],
    },
    Module {
        name: "files",
        label: "files: players' uploads with quotas, private / public / friends / shared",
        requires: &["auth"],
        default: true,
        feature: Some("files"),
        import: "use net_backend_server::files::Files;",
        register: "Files::new()",
        config: "[modules.files]\n\
                 # Players' files: the bytes in this folder, the rest in the database. Defaults: 16 MiB per\n\
                 # file, 100 files and 256 MiB per player.\n\
                 dir = \"files\"\n",
        docker: &[("dir = \"files\"\n", "dir = \"/data/files\"\n")],
    },
];

/// The module of this name.
pub fn find(name: &str) -> Option<&'static Module> {
    MODULES.iter().find(|m| m.name == name)
}

/// The default set: every module that works without setup outside the project.
pub fn defaults() -> Vec<&'static Module> {
    MODULES.iter().filter(|m| m.default).collect()
}

/// The names in table order, with every module's requirements added (`storage` → `auth, storage`).
/// Unknown names are an error.
pub fn resolve<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Vec<&'static Module>, String> {
    let mut wanted: Vec<&str> = Vec::new();
    let mut stack: Vec<&str> = Vec::new();
    for name in names {
        let module = find(name).ok_or_else(|| format!("unknown module `{name}` (known: {}, or none)", names_list()))?;
        stack.push(module.name);
    }
    while let Some(name) = stack.pop() {
        if !wanted.contains(&name) {
            wanted.push(name);
            if let Some(module) = find(name) {
                stack.extend(module.requires);
            }
        }
    }
    Ok(MODULES.iter().filter(|m| wanted.contains(&m.name)).collect())
}

/// `auth, storage, chat, …`.
pub fn names_list() -> String {
    MODULES.iter().map(|m| m.name).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(modules: &[&Module]) -> Vec<&'static str> {
        modules.iter().map(|m| m.name).collect()
    }

    #[test]
    fn requirements_are_added_in_table_order() {
        assert_eq!(names(&resolve(["chat"]).unwrap()), ["auth", "chat"]);
        assert_eq!(names(&resolve(["chat", "storage"]).unwrap()), ["auth", "storage", "chat"]);
        assert_eq!(names(&resolve(["auth"]).unwrap()), ["auth"]);
        assert_eq!(names(&resolve(["files", "groups"]).unwrap()), ["auth", "groups", "files"]);
        assert!(resolve([]).unwrap().is_empty());
        assert!(resolve(["guilds"]).unwrap_err().contains("unknown module `guilds`"));
        // Every module but auth needs auth.
        for module in &MODULES[1..] {
            assert_eq!(names(&resolve([module.name]).unwrap()), ["auth", module.name]);
        }
    }

    #[test]
    fn the_defaults_leave_out_oauth_only() {
        let defaults = names(&defaults());
        assert_eq!(defaults, ["auth", "storage", "chat", "leaderboards", "notifications", "friends", "groups", "lobbies", "matchmaking", "files"]);
        assert!(find("oauth").unwrap().config.contains("until then every login answers 404"));
    }

    #[test]
    fn the_table_is_consistent() {
        // The order of the reference server (examples/server.rs of net_backend_server).
        assert_eq!(names_list(), "auth, storage, chat, leaderboards, notifications, friends, groups, oauth, lobbies, matchmaking, files");
        for (index, module) in MODULES.iter().enumerate() {
            for other in module.requires {
                let at = MODULES.iter().position(|m| m.name == *other).expect("a named module is in the table");
                assert!(at < index, "{} must come after {other}", module.name);
            }
            assert!(module.config.starts_with(&format!("[modules.{}]", module.name)));
            assert!(module.config.ends_with('\n') && !module.config.contains("\n\n\n"));
            for (from, _) in module.docker {
                assert!(module.config.contains(from), "{}: the Docker replacement does not apply", module.name);
            }
            assert_eq!(module.feature.is_none(), module.name == "auth");
        }
    }
}
