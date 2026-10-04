//! The README of a generated project, put together for its options.

use crate::generate::{demo_in_workspace, dev_url, is_workspace, BEVY_NET_BACKEND_VERSION, CRATES_VERSION};
use crate::options::{ClientKind, Database, Options, STANDALONE_DEMO};

/// The two dependency lines a Bevy game adds to use the client with the server's types.
pub fn bevy_dependency_lines() -> String {
    format!(
        "bevy_net_backend = {{ version = \"{BEVY_NET_BACKEND_VERSION}\", features = [\"http\", \"json\", \"ws\"] }}\n\
         net_backend_protocol = {{ version = \"{CRATES_VERSION}\", features = [\"bevy_net_backend\"] }}"
    )
}

/// The project's README.md.
pub fn project(options: &Options, name: &str) -> String {
    let mut text = format!("# {name}\n\nA game backend built with [net_backend](https://net-backend.com): {}.\n\n", summary(options));

    text.push_str("## Run it\n\nIn this folder, the same in PowerShell, cmd, bash and zsh:\n\n```text\n");
    text.push_str("cargo run              # the server on http://127.0.0.1:8080 (Ctrl-C stops it)\n");
    if is_workspace(options) {
        text.push_str("cargo run -p client    # in a second terminal: the client\n");
    }
    if demo_in_workspace(options) {
        text.push_str("cargo run -p demo      # in a second terminal: the demo window\n");
    }
    text.push_str("```\n\n");
    text.push_str(&database_note(options.database));
    text.push_str("`http://127.0.0.1:8080/v1/info` in a browser shows the server's protocol and modules.\n\n");
    text.push_str(&client_note(options));

    text.push_str("## Deploy with Docker\n\n");
    text.push_str(&docker_note(options));

    text.push_str("## What is in it\n\n```text\n");
    text.push_str(&tree(options, name));
    text.push_str("```\n\n");

    text.push_str("## The server\n\n");
    let main = if is_workspace(options) { "server/src/main.rs" } else { "src/main.rs" };
    let modules = if options.modules.is_empty() {
        "no module (the core server: `/v1/info`, health, the WebSocket hub)".to_string()
    } else {
        format!("the modules {}", options.modules.iter().map(|m| format!("`{}`", m.name)).collect::<Vec<_>>().join(", "))
    };
    text.push_str(&format!(
        "`{main}` builds a `NetBackendServer` with {modules}. Add your game's HTTP routes, hooks and WebSocket\n\
         handlers there; the [net_backend_server guide](https://docs.rs/net_backend_server) describes each.\n\n\
         The server program has a command line (`cargo run -- --help`):\n\n\
         ```text\n\
         cargo run -- config check --connect                # check config.toml and the database\n\
         cargo run -- migrate                               # apply the migrations\n"
    ));
    if options.has("auth") {
        text.push_str("cargo run -- user:create you@example.com --admin   # an admin account\n");
    }
    text.push_str("```\n\n");
    text.push_str(&format!(
        "`config.toml` holds the address (`127.0.0.1:8080`), the database (`{}`), migrations on start, the log{}.\n\
         An environment variable `NBS__<SECTION>__<KEY>` overrides any key. Another database: its feature\n\
         (`sqlite`, `postgres`, `mysql`) on `net_backend_server` in `{}` and `database.url`.\n\n",
        dev_url(options.database),
        module_config_note(options),
        if is_workspace(options) { "server/Cargo.toml" } else { "Cargo.toml" }
    ));

    text.push_str("## The same project without questions\n\n```text\n");
    text.push_str(&options.command_line());
    text.push_str("\n```\n\n");
    if let Some(note) = options.command_line_note() {
        text.push_str(&format!("{note}\n\n"));
    }
    text.push_str("`net-backend --help` lists every flag.\n");
    text
}

fn summary(options: &Options) -> String {
    let client = match options.client {
        ClientKind::Rust => "a server and a Rust client on `net_backend_client`",
        ClientKind::Bevy => "a server and a Bevy client on `bevy_net_backend`",
        ClientKind::Protocol => "a server and a client crate with the server's message types (`net_backend_protocol`), ready for your HTTP / WebSocket library",
        ClientKind::Api => "the server; clients in any language use its HTTP + WebSocket API",
    };
    let demo = match (options.client, options.demo, options.existing.is_some()) {
        (ClientKind::Rust, true, _) => ", plus a demo window",
        (ClientKind::Bevy, true, false) => ", plus a demo app",
        (ClientKind::Bevy, true, true) => ", plus a demo app next to an existing Bevy game",
        _ => "",
    };
    format!("{client}{demo}, on {}", options.database.name())
}

fn database_note(database: Database) -> String {
    match database {
        Database::Sqlite => {
            "On its first start the server creates the SQLite database `game.db` in this folder and applies the\nmigrations.\n\n"
                .to_string()
        }
        Database::Postgres => format!(
            "The server needs its PostgreSQL database first: the role `game` with the password\n\
             `game-dev-password` and the database `game` (`{url}` in `config.toml`). With Docker, one command\n\
             starts one on this machine:\n\n\
             ```text\n\
             docker run -d --name game-postgres -e POSTGRES_USER=game -e POSTGRES_PASSWORD=game-dev-password -e POSTGRES_DB=game -p 127.0.0.1:5432:5432 postgres:16\n\
             ```\n\n\
             On an installed PostgreSQL, as its superuser (`psql -U postgres`):\n\n\
             ```sql\n\
             CREATE ROLE game LOGIN PASSWORD 'game-dev-password';\n\
             CREATE DATABASE game OWNER game;\n\
             ```\n\n\
             The server applies its migrations on every start.\n\n",
            url = dev_url(Database::Postgres)
        ),
        Database::Mysql => format!(
            "The server needs its MySQL database first: the user `game` with the password `game-dev-password`\n\
             and the database `game` (`{url}` in `config.toml`). With Docker, one command starts one on\n\
             this machine:\n\n\
             ```text\n\
             docker run -d --name game-mysql -e MYSQL_ROOT_PASSWORD=game-root-password -e MYSQL_DATABASE=game -e MYSQL_USER=game -e MYSQL_PASSWORD=game-dev-password -p 127.0.0.1:3306:3306 mysql:8.4\n\
             ```\n\n\
             On an installed MySQL, as root (`mysql -u root -p`):\n\n\
             ```sql\n\
             CREATE DATABASE game;\n\
             CREATE USER 'game'@'%' IDENTIFIED BY 'game-dev-password';\n\
             GRANT ALL PRIVILEGES ON game.* TO 'game'@'%';\n\
             ```\n\n\
             The server applies its migrations on every start.\n\n",
            url = dev_url(Database::Mysql)
        ),
    }
}

fn client_note(options: &Options) -> String {
    let mut text = String::new();
    let account = "a development account (`player@example.com`, registered on its first run)";
    match options.client {
        ClientKind::Rust => text.push_str(&format!(
            "The client (`client/src/main.rs`, on [net_backend_client](https://docs.rs/net_backend_client)) logs in to\n\
             {account}, writes and reads a save, joins the public chat room `world`, sends a message and prints the\n\
             room's messages for 10 seconds; it skips what the server's modules do not offer. Another server:\n\
             `cargo run -p client -- https://api.example.com` (or the variable `NET_BACKEND_URL`) with the\n\
             variables `NET_BACKEND_EMAIL` and `NET_BACKEND_PASSWORD`; plain `http://` goes to this machine only.\n\n"
        )),
        ClientKind::Bevy => text.push_str(&format!(
            "The client (`client/src/main.rs`) is a headless Bevy app on\n\
             [bevy_net_backend](https://github.com/warmar94/bevy_net_backend) with the server's message types from\n\
             [net_backend_protocol](https://docs.rs/net_backend_protocol) (feature `bevy_net_backend`): it logs in to\n\
             {account}, joins the public chat room `world`, sends a message and prints the room's messages for\n\
             10 seconds (it needs the modules `auth` and `chat`). The variable `NET_BACKEND_URL` names another server,\n\
             with the variables `NET_BACKEND_EMAIL` and `NET_BACKEND_PASSWORD`: the development account is used on\n\
             this machine only.\n\n"
        )),
        ClientKind::Protocol => text.push_str(
            "The client crate (`client/`) has the server's message types from\n\
             [net_backend_protocol](https://docs.rs/net_backend_protocol): every HTTP call is a type with its route\n\
             and payload, every WebSocket request a type with its kind, every answer a type to decode into. Add the\n\
             HTTP / WebSocket library of your choice (reqwest, ureq, tokio-tungstenite, ...). `cargo run -p client`\n\
             prints what a client sends, without a network connection.\n\n",
        ),
        ClientKind::Api => text.push_str(
            "Clients in any language use the HTTP + WebSocket API:\n\
             [API.md](https://github.com/warmar94/net_backend/blob/main/API.md) describes it with examples; the\n\
             running server serves its OpenAPI document at `/v1/openapi.json` and its AsyncAPI document (the\n\
             WebSocket) at `/v1/asyncapi.json`.\n\n",
        ),
    }
    if options.demo {
        let buttons = demo_panels(options);
        match (options.client, options.existing.as_ref()) {
            (ClientKind::Rust, _) => text.push_str(&format!(
                "The demo (`demo/src/main.rs`) is a small egui window on net_backend_client: a panel per module of\n\
                 the server ({buttons}) and a log of what the client sent, what came back and what the server\n\
                 pushed (also printed in the terminal). It shows the panels of the modules the server reports in\n\
                 `/v1/info`. The password field is filled in for a server on this machine only. {}\n\n",
                two_players("cargo run -p demo")
            )),
            (ClientKind::Bevy, None) => text.push_str(&format!(
                "The demo (`demo/src/main.rs`) is a small Bevy app on bevy_net_backend: a row of buttons per\n\
                 module of the server ({buttons}) and a log of what the client sent, what came back and what the\n\
                 server pushed (also printed in the terminal). It shows the rows of the modules the server reports\n\
                 in `/v1/info`; friend codes, join codes and player ids are typed on the keyboard into its input\n\
                 line. Another server: `NET_BACKEND_URL` with `NET_BACKEND_PASSWORD` (the development password is\n\
                 for this machine only). Its first build takes 5 to 10 minutes (Bevy). {}\n\n",
                two_players("cargo run -p demo")
            )),
            (ClientKind::Bevy, Some(game)) => text.push_str(&format!(
                "The demo is a small Bevy app in its own folder `{STANDALONE_DEMO}`, next to the game (`{}`):\n\
                 `cargo run` there, with the server running. It is a project of its own; the game's files are not\n\
                 changed. Its first build takes 5 to 10 minutes (Bevy). To use the client in the game, add these two\n\
                 lines to its `[dependencies]`:\n\n\
                 ```toml\n{}\n```\n\n",
                game.display(),
                bevy_dependency_lines()
            )),
            _ => {}
        }
        if options.client.has_demo() {
            let run = if options.existing.is_some() { "cargo run --features steam" } else { "cargo run -p demo --features steam" };
            text.push_str(&steam_note(options.client, run, "###"));
        }
    }
    text
}

/// How a second player joins in: a second window with another account.
fn two_players(run: &str) -> String {
    format!(
        "Two players: a second window with another account, `{run} -- player2@example.com`\n\
         (Register once per account, then Login); the friend codes, join codes and player ids one window's\n\
         log shows are typed into the other."
    )
}

/// The demo's `steam` feature: how to try it.
pub fn steam_note(client: ClientKind, run: &str, heading: &str) -> String {
    let (how, file) = if client == ClientKind::Bevy {
        (
            "a Steam row through [bevy_steam_kit](https://docs.rs/bevy_steam_kit) (its friends list, Web API\n\
             ticket, invites, overlay and joins)",
            "src/steam.rs",
        )
    } else {
        ("a Steam panel through [steamworks](https://docs.rs/steamworks)", "src/steam.rs")
    };
    let file = if run.contains("-p demo") { format!("demo/{file}") } else { file.to_string() };
    let binary = if run.contains("-p demo") { "demo" } else { "net_backend_demo" };
    format!(
        "{heading} Steam in the demo\n\n\
         The demo's cargo feature `steam` (off by default; without it nothing of Steam is built) adds {how}\n\
         (`{file}`): Steam login (or linking Steam to the account logged in), the Steam friends who have an\n\
         account here (and a friend request to them), whether others find you by your Steam account,\n\
         \"Join Game\" and Steam invites for your lobby (its join code travels as `+nb_lobby <number>`), and\n\
         joining a lobby from Steam: an accepted invite, \"Join Game\", or a start with `+nb_lobby <number>` on\n\
         the command line (a demo in another lobby leaves it first).\n\n\
         ```text\n\
         {run}\n\
         {run} -- player2@example.com        # the second player, on the second machine\n\
         {run} -- +nb_lobby 590122524587     # start and join the lobby K7M2-Q9XD\n\
         ```\n\n\
         - Steam must be running and logged in before the demo starts; otherwise the demo says so once and\n  \
           works as without the feature. When Steam quits while the demo runs, the log says so and the demo\n  \
           goes on without Steam.\n\
         - The app id: the variable `STEAM_APP_ID`, or 480 (Valve's test app \"Spacewar\") when it is not set.\n  \
           The demo hands the id to Steam when it starts, so no `steam_appid.txt` is needed.\n\
         - Invites and \"Join Game\" need two Steam accounts that are Steam friends: one per machine, each with\n  \
           Steam logged in and the demo running, each demo logged in to this server with its own email. The\n  \
           second machine reaches the server over HTTPS (a deployed server: `NET_BACKEND_URL`, and the account's\n  \
           own password, as the development password is for this machine only) or through an SSH tunnel to\n  \
           this machine's `127.0.0.1:8080`: plain `http://` goes to the local machine only.\n\
         - Steam login, linking and the Steam friends lookup need Steam login on the server: its cargo feature\n  \
           `steam` and `steam_app_id`, `steam_identity` and `steam_web_api_key_file` in `[modules.auth]` (the\n  \
           net_backend_server guide). The demo's ticket uses the identity `NET_BACKEND_STEAM_IDENTITY` (the\n  \
           project's name when not set). Join codes, invites and \"Join Game\" work without it.\n\
         - Steam's library: `cargo run` finds it by itself. The built program (`target/debug/{binary}`) started\n  \
           any other way needs it next to the program: `steam_api64.dll` on Windows, `libsteam_api.dylib` on\n  \
           macOS; on Linux `libsteam_api.so` on the library search path (next to the program with\n  \
           `LD_LIBRARY_PATH=.`). The build puts the file in `target/debug/build/steamworks-sys-*/out/`.\n\n"
    )
}

fn docker_note(options: &Options) -> String {
    match options.database {
        Database::Sqlite => "With Docker (Docker Desktop or Docker Engine), in this folder:\n\n\
             ```text\n\
             docker compose up -d --build    # http://127.0.0.1:8080, the database in the volume `data`\n\
             ```\n\n\
             The `Dockerfile` builds the server into a small non-root Linux image with `config.docker.toml` as its\n\
             configuration (the address `0.0.0.0:8080` inside the container, the database `/data/game.db`,\n\
             migrations on start, JSON logs). `compose.yaml` publishes it on `127.0.0.1:8080` of this machine and\n\
             keeps `/data` in the volume `data`.\n\n"
            .to_string(),
        database => format!(
            "On a Linux server with Docker, in this folder (its DNS name pointing at the machine):\n\n\
             1. write a file `.env` with one line: `DOMAIN=api.example.com`\n\
             2. `docker compose up -d --build`\n\n\
             `compose.yaml` is net_backend's production setup for {db}: it builds this server (`Dockerfile`),\n\
             generates the database passwords into Docker volumes on the first start, starts {db}, applies the\n\
             migrations and puts Caddy in front (HTTPS and WSS for `DOMAIN`, certificates from Let's Encrypt). The\n\
             server's production configuration is `x-nbs-config` in that file. The deployment guide:\n\
             [deploy/README.md](https://github.com/warmar94/net_backend/blob/main/deploy/README.md).\n\n",
            db = database.name()
        ),
    }
}

fn module_config_note(options: &Options) -> String {
    let notes = [
        ("auth", "the mail log (development: whole mails with their links)"),
        ("chat", "the chat room `world`"),
        ("leaderboards", "the board `highscore`"),
        ("oauth", "the OpenID Connect providers (none: every OpenID Connect login answers 404 until one is set)"),
        ("matchmaking", "the queues `duel` and `solo`"),
        ("files", "the players' files folder `files`"),
    ];
    notes.iter().filter(|(module, _)| options.has(module)).map(|(_, note)| format!(", {note}")).collect()
}

/// The demo's panels for the project's modules: `account (register / login / …), saves (…)`.
pub fn demo_panels(options: &Options) -> String {
    let panels = [
        ("auth", "account: register / login / refresh / logout"),
        ("storage", "saves: save / load / list"),
        ("chat", "chat: join / send / history / presence / edit / typing / read marker"),
        ("leaderboards", "leaderboards: submit / top / around me"),
        ("notifications", "notifications: list / mark read"),
        ("friends", "friends: my code / add by code / list / accept"),
        ("groups", "groups: create / invite / list"),
        ("lobbies", "lobbies: create / join by code / ready"),
        ("matchmaking", "matchmaking: queue / cancel"),
        ("files", "files: upload / list / download"),
    ];
    let chosen: Vec<&str> = panels.iter().filter(|(module, _)| options.has(module)).map(|(_, panel)| *panel).collect();
    if chosen.is_empty() {
        "the server's info".to_string()
    } else {
        chosen.join("; ")
    }
}

fn tree(options: &Options, name: &str) -> String {
    let mut rows: Vec<(String, &str)> = Vec::new();
    let workspace = is_workspace(options);
    if workspace {
        rows.push(("Cargo.toml".into(), "the workspace: `cargo run` = the server"));
    } else {
        rows.push(("Cargo.toml".into(), "the server's dependencies"));
    }
    rows.push(("config.toml".into(), "the server's configuration (development)"));
    if options.database == Database::Sqlite {
        rows.push(("config.docker.toml".into(), "the server's configuration in the Docker image"));
    }
    rows.push((if workspace { "server/src/main.rs" } else { "src/main.rs" }.into(), "the server: your routes go here"));
    match options.client {
        ClientKind::Rust => rows.push(("client/src/main.rs".into(), "the client (net_backend_client)")),
        ClientKind::Bevy => rows.push(("client/src/main.rs".into(), "the client (bevy_net_backend, headless)")),
        ClientKind::Protocol => rows.push(("client/src/main.rs".into(), "the message types (net_backend_protocol)")),
        ClientKind::Api => {}
    }
    if demo_in_workspace(options) {
        rows.push(("demo/src/main.rs".into(), if options.client == ClientKind::Rust { "the demo window (egui)" } else { "the demo app (Bevy)" }));
    }
    rows.push(("Dockerfile".into(), "the server in a small non-root Linux image"));
    rows.push((
        "compose.yaml".into(),
        if options.database == Database::Sqlite { "the image on 127.0.0.1:8080, the volume `data`" } else { "production: the database, the server, Caddy" },
    ));
    let width = rows.iter().map(|(path, _)| path.len()).max().unwrap_or(0) + 2;
    let mut text = format!("{name}/\n");
    for (index, (path, what)) in rows.iter().enumerate() {
        let branch = if index + 1 == rows.len() { "└──" } else { "├──" };
        text.push_str(&format!("{branch} {path:width$}{what}\n"));
    }
    text
}

/// The README of a Bevy demo placed next to an existing game.
pub fn standalone_demo(options: &Options, name: &str) -> String {
    let text = format!(
        "# {STANDALONE_DEMO}\n\n\
         A small Bevy app on [bevy_net_backend](https://github.com/warmar94/bevy_net_backend) that talks to the {name}\n\
         server: a row of buttons per module of the server ({panels}) and a log of what the client sent,\n\
         what came back and what the server pushed.\n\n\
         It is a project of its own, next to the game (`{game}`): the game's files are not changed, and deleting\n\
         this folder removes it completely.\n\n\
         ## Run it\n\n\
         With the server running (`cargo run` in `{server}`), in this folder:\n\n\
         ```text\n\
         cargo run\n\
         ```\n\n\
         The first build takes 5 to 10 minutes (Bevy). The variable `NET_BACKEND_URL` names another server than\n\
         `http://127.0.0.1:8080`, with `NET_BACKEND_PASSWORD` (the development password is for this machine\n\
         only). The log is also printed in the terminal. {players}\n\n\
         ## In the game\n\n\
         Two lines in the game's `Cargo.toml`, under `[dependencies]`:\n\n\
         ```toml\n{lines}\n```\n\n\
         `src/main.rs` shows the plugin, the typed requests and the pushes.\n",
        game = options.existing.as_deref().map(|p| p.display().to_string()).unwrap_or_default(),
        // Absolute: this README is in another folder than the one the installer ran in.
        server = std::path::absolute(&options.target).unwrap_or_else(|_| options.target.clone()).display(),
        lines = bevy_dependency_lines(),
        panels = demo_panels(options),
        players = two_players("cargo run"),
    );
    text + &steam_note(ClientKind::Bevy, "cargo run --features steam", "##")
}
