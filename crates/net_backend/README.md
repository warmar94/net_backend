# net_backend

<p>
  <a href="https://crates.io/crates/net_backend"><img alt="crates.io" src="https://img.shields.io/crates/v/net_backend.svg"></a>
  <a href="https://net-backend.com"><img alt="Website: net-backend.com" src="https://img.shields.io/badge/website-net--backend.com-informational"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

The installer of [net_backend](https://github.com/warmar94/net_backend): `net-backend new` asks a few
questions and writes a game backend that runs at once: a
[`net_backend_server`](https://crates.io/crates/net_backend_server) with the modules you pick on SQLite,
PostgreSQL or MySQL, a client for your kind of game, and a demo app with buttons to try it.

```text
cargo install net_backend
net-backend new mygame
```

```text
? How will your game talk to the server?
  > Rust client      (server + protocol + net_backend_client)
    Bevy plugin      (server + protocol + bevy_net_backend)
    Your own client  (server + protocol, your HTTP / WebSocket library)
    Pure API         (the server only; any language, API.md)
? Database?                 SQLite / PostgreSQL / MySQL
? Server modules?           auth, storage, chat, leaderboards, notifications, friends, groups,
                            oauth, lobbies, matchmaking, files   (all but oauth ticked)
? Add a demo app to try it? yes / no              (Rust client, Bevy plugin)
? Where does the Bevy demo go?  this project / next to an existing Bevy game
? Start the server (and the demo) when done?      yes / no
```

Every question is also a flag, and at the end the installer prints the one-line command that gives
the same project:

```text
net-backend new mygame --client rust --db sqlite --modules auth,storage,chat,leaderboards --demo --run
```

The commands are the same in PowerShell, cmd, bash and zsh, on Windows, macOS and Linux.

## Contents

- [Install](#install)
- [Commands and flags](#commands-and-flags)
- [What it writes](#what-it-writes)
- [The server](#the-server)
- [The clients](#the-clients)
- [The demos](#the-demos)
- [Bevy: next to an existing game](#bevy-next-to-an-existing-game)
- [Start it now (`--run`)](#start-it-now---run)
- [Docker](#docker)
- [Modules](#modules)
- [Versions](#versions)
- [How it works](#how-it-works)
- [License](#license)
- [Contributing](#contributing)

## Install

```text
cargo install net_backend
```

It installs the `net-backend` command; Rust itself comes from [rustup.rs](https://rustup.rs). Its
dependencies are [`inquire`](https://crates.io/crates/inquire) (the questions) and
[`crossterm`](https://crates.io/crates/crossterm) (colors in the terminal); generating a project
needs no network.

## Commands and flags

| Command | What it writes |
|---|---|
| `net-backend new [<name>] [flags]` | A project; in a terminal it asks every question no flag answered. |
| `net-backend new-server <name> [flags]` | The same as `new <name> --client api`: the server alone. |
| `net-backend new-client <name>` | A Rust client alone, for a server that exists. |
| `net-backend --help` / `--version` | The usage / the version (the same as the crates the projects use). |

| Flag of `new` | Values | Default |
|---|---|---|
| `--client` | `rust` (server + protocol + net_backend_client), `bevy` (server + protocol + bevy_net_backend), `protocol` (server + protocol, your HTTP / WebSocket library), `api` (the server only) | `rust` |
| `--db` | `sqlite`, `postgres`, `mysql` | `sqlite` |
| `--modules` | a comma-separated list of `auth`, `storage`, `chat`, `leaderboards`, `notifications`, `friends`, `groups`, `oauth`, `lobbies`, `matchmaking`, `files`, or `none`; every module adds `auth` | all but `oauth` |
| `--demo` / `--no-demo` | the demo app (with `rust` and `bevy`) | `--demo` |
| `--existing <path>` | `bevy`: the demo goes next to this existing game | none |
| `--run` | build, start the server in a new terminal window, then the demo (or the client) | off |
| `-y`, `--yes` | no questions: the defaults for everything not given | |
| `--no-color` | plain text without colors (with every command) | |

Questions are asked only when no flag is given and both standard input and output are a terminal;
otherwise (a flag, a script, CI) the defaults fill every answer not given. `<name>` is the folder to
create; a path works too (`net-backend new games/mygame`), its last part is the project name. Names
follow Cargo's package-name rules, kept strict so that one name works as a folder, a package, a
binary and a Docker image everywhere: 1 to 64 characters of `a-z`, `0-9`, `-` and `_`, starting
with a letter, not a Rust keyword, a standard-library crate or a dependency of the project. An
existing folder must be empty; nothing is ever overwritten, and a failed run removes what it wrote.

In a terminal the installer shows a banner, a section per question, colored badges and a summary
box; `--no-color`, the `NO_COLOR` environment variable or output that is not a terminal give plain text.

## What it writes

`net-backend new mygame` (the defaults):

```text
mygame/
├── Cargo.toml          the workspace: members server, client, demo; `cargo run` = the server
├── README.md           how to run, deploy and extend it, and the one-line command
├── config.toml         the server's configuration (development)
├── config.docker.toml  the server's configuration in the Docker image (SQLite)
├── Dockerfile          the server in a small non-root Linux image
├── compose.yaml        SQLite: the image on 127.0.0.1:8080; PostgreSQL / MySQL: production with Caddy
├── .gitignore, .dockerignore   (with the files module: the players' files folder `files/` too)
├── server/             net_backend_server (the database's feature, the modules' features), tokio
├── client/             the client for the chosen path
└── demo/               the demo app (rust, bevy)
```

| `--client` | `client/` | `demo/` |
|---|---|---|
| `rust` | a program on `net_backend_client` (feature `ws`) | a small egui window on `net_backend_client` |
| `bevy` | a headless Bevy app on `bevy_net_backend` + `net_backend_protocol` (feature `bevy_net_backend`) | a small Bevy app with buttons |
| `protocol` | a crate on `net_backend_protocol` (plain Rust + serde, no network) | none |
| `api` | none: the server is the project itself (`Cargo.toml` and `src/main.rs` at the root) | none |

Every generated package has `publish = false` and edition 2024. A Bevy workspace also gets Bevy's
development profile (dependencies optimized, the game's own code quick to build).

## The server

`server/src/main.rs` builds a `NetBackendServer` and registers the chosen modules. It has the
framework's command line (`serve` by default, `migrate`, `config check`, `user:create`, ...,
`healthcheck`, which the Docker image uses).

The development `config.toml`:

| Setting | Value |
|---|---|
| `server.bind` | `127.0.0.1:8080` |
| `database.url` | written out for the chosen database: `sqlite:game.db` (a file in the folder `cargo run` runs in, created on the first start), `postgres://game:game-dev-password@127.0.0.1:5432/game`, or `mysql://game:game-dev-password@127.0.0.1:3306/game` |
| `database.migrate_on_start` | `true` |
| `log` | `info`, `pretty` |
| `[modules.*]` | a section per chosen module, with its defaults in a comment: `auth` with `app_name` = the project name and `log_mailer_show_links = true` (development: mails with their links in the log), `storage`, `chat` with the public room `world`, `leaderboards` with the board `highscore`, `notifications`, `friends`, `groups`, `oauth` with a commented Google provider (registered, off until a provider is set: every OpenID Connect login answers 404 until then), `lobbies`, `matchmaking` with the queues `duel` (2 players) and `solo` (1 player: a match at once), `files` with `dir = "files"` (the players' files in the project folder) |

For PostgreSQL and MySQL the project's README shows how to create that role (or user) and database:
one `docker run` line for a development database, and the SQL for an installed server. Any key can
be overridden by an environment variable `NBS__<SECTION>__<KEY>`.

## The clients

- **Rust** (`client/`): checks the server's protocol (a mismatch is an error), logs in to a
  development account (registering it on the first run, so repeated runs never hit the registration
  limit), writes a save and reads it back, joins the chat room `world` over the WebSocket, sends a
  message, prints the room's messages for 10 seconds and logs out; it skips what the server's
  modules do not offer. The server comes from the first argument, else `NET_BACKEND_URL`, else
  `http://127.0.0.1:8080`. The account comes from `NET_BACKEND_EMAIL` / `NET_BACKEND_PASSWORD`;
  without them, a server on this machine gets the development account `player@example.com` (its
  password is in the generated source), and any other server needs the two variables.
  `net_backend_client` sends plain `http://` only to loopback addresses (127.0.0.1, ::1, localhost).
- **Bevy** (`client/`): a headless Bevy app (`MinimalPlugins`) on `bevy_net_backend`: the server's
  protocol and modules, login (or register) of the development account, the WebSocket, `world`
  joined, a message sent, the room's messages printed for 10 seconds. It needs the modules `auth`
  and `chat`; `NET_BACKEND_URL` names another server. The account follows the Rust client's rule:
  `NET_BACKEND_EMAIL` / `NET_BACKEND_PASSWORD`, else the development account on this machine only.
- **Protocol** (`client/`): `cargo run -p client` prints what a client sends (method, path, JSON
  body, WebSocket frames, the protocol header) for a few calls, without a network connection: the
  starting point for your own HTTP / WebSocket library.

## The demos

Both demos show a small panel per module of the server, only for the modules it reports in
`/v1/info` (the ones you picked), and a log of what the client sent (`->`), what came back (`<-`)
and what the server pushed (`<<`), also printed in the demo's terminal; the server's own terminal
shows the other side. After a login the demo opens the WebSocket, so every module's pushes arrive in
the log.

| Module | Buttons | Pushes in the log |
|---|---|---|
| `auth` | register / login / refresh / logout | |
| `storage` | save / load / list (the collection `saves`) | |
| `chat` | join / send / history / presence (the room `world`), edit my last message / typing / read marker | `chat.message`, `chat.presence`, `chat.edited`, `chat.typing`, `chat.read` |
| `leaderboards` | submit / top / around me (the board `highscore`) | |
| `notifications` | list / mark all read | `notify.new` |
| `friends` | my code / add by code / list (friends and received requests) / accept | `friends.presence` |
| `groups` | create / invite (a player id) / list | |
| `lobbies` | create / join by code / ready / leave | `lobby.member`, `lobby.changed` |
| `matchmaking` | queues / queue (per queue) / cancel | `match.found`, `match.expired` |
| `files` | upload (a small text file) / list / download | |

Two players: start the demo twice (two windows) with two accounts (the second one
`cargo run -p demo -- player2@example.com`), then add each other by friend code, join a lobby by its
code or queue both in `duel`.

- **Rust:** a small [egui](https://crates.io/crates/eframe) window (eframe on OpenGL) on
  `net_backend_client`'s blocking interface, each call on its own thread. Email (the argument with an
  `@`, else `player@example.com`), password (the development password is filled in for a server on
  this machine only), the save, the message, the board and score, the friend code, the group name,
  the player id and the join code are editable; the server comes from the argument with `http://` or
  `https://`, else `NET_BACKEND_URL`. The first build takes about 1 to 2 minutes.
- **Bevy:** a small Bevy app (`bevy_ui`) on `bevy_net_backend` with the protocol's typed
  requests and pushes (plain HTTP and WebSocket requests; the upload is a `multipart/form-data`
  request with the file part). It uses a development account (`player@example.com`, or the email
  given as the argument: `cargo run -p demo -- player2@example.com`); its password is used on this
  machine only, and another server needs `NET_BACKEND_PASSWORD`. Friend codes, join codes and player
  ids are typed on the keyboard into its input line. The first build takes 5 to 10 minutes.

**Steam in the demos.** Both demos have a cargo feature `steam`, off by default (without it nothing of
Steam is built or linked): `cargo run -p demo --features steam`. It adds a Steam panel: Steam login
(a Web API ticket for `POST /v1/auth/steam`; logged in, it links Steam to the account), the Steam
friends who have an account on the server (`POST /v1/friends/steam`, with a friend request to each),
the "findable through Steam" setting (`PUT /v1/friends/settings`), "Join Game" and Steam invites for
the current lobby, and joining a lobby from Steam (an accepted invite, "Join Game", or a start with
`+nb_lobby <number>` on the command line; `POST /v1/lobbies/join` with the code; a demo in another
lobby leaves it first). A lobby's
8-character join code travels through Steam as its number (`+nb_lobby 590122524587` = `K7M2-Q9XD`;
`demo/src/invite.rs`, with tests).

- **Bevy:** [bevy_steam_kit](https://crates.io/crates/bevy_steam_kit) (features `lobby`, `friends`,
  `auth`, `overlay`, `steam`) does the Steam side: the one callback pump, the friends list
  (`SteamFriends`), the login ticket (`AuthRequest`), invites (`InviteToGame`), the invite dialog
  (`OpenOverlay`), rich presence and the joins (`ConnectRequested`, `JoinRequested`); `steamworks`
  only starts Steam. The server calls go through `bevy_net_backend`.
- **Rust (egui):** [steamworks](https://crates.io/crates/steamworks) directly, in one file
  (`demo/src/steam.rs`), pumped once per frame, with guards for the callbacks `steamworks` panics on
  when Steam quits.

Steam must be running and logged in (otherwise the demo says so once and works as without the
feature); when Steam quits while a demo runs, the demo says so and goes on without Steam. The app id
is `STEAM_APP_ID` (480, Valve's test app "Spacewar", when not set), handed to Steam by the demo (no
`steam_appid.txt`). Steam login and the Steam friends lookup need Steam login enabled on the server
(its cargo feature `steam` and `steam_app_id`, `steam_identity`, `steam_web_api_key_file` in
`[modules.auth]`); join codes, invites and "Join Game" work without it. The generated project's
README says how to try it with two Steam accounts and where Steam's library goes next to a built
program.

## Bevy: next to an existing game

`--client bevy --existing <game folder>` (or the answer "next to an existing Bevy game") writes the
demo into its own folder `net_backend_demo/` beside the game (the folder's real path is resolved
first, so `.` or `..` work too): a standalone project with its own `[workspace]`. The game's files
are never read beyond the check that its `Cargo.toml` exists and never changed; deleting
`net_backend_demo/` removes the demo completely. The server project is written as usual, and the
installer prints the two lines a game adds to its `Cargo.toml` to use the client, with the versions
the project uses:

```toml
bevy_net_backend = { version = "<version>", features = ["http", "json", "ws"] }
net_backend_protocol = { version = "<version>", features = ["bevy_net_backend"] }
```

## Start it now (`--run`)

After writing the project, `--run` (or "yes" to the last question):

1. builds it (`cargo build --workspace`; the warning before names the first build's time);
2. starts the server (`cargo run -p server`) in a **new terminal window**: on Windows a new console
   (`cmd /C start … cmd /K`), on macOS a Terminal window (`osascript`), on Linux the first of
   `x-terminal-emulator`, `gnome-terminal`, `konsole`, `xfce4-terminal`, `kitty`, `alacritty`,
   `xterm` found on `PATH`;
3. waits until `http://127.0.0.1:8080/v1/info` answers;
4. starts the demo here (or the client, for `protocol` and without a demo).

The window starts in the project's folder (its absolute path; on Windows the path is the console's
working folder, never part of a command line). Without a window (an SSH session, Linux without
`DISPLAY` / `WAYLAND_DISPLAY`, no terminal found) the server runs in this terminal instead, and the
client replaces the demo window when there is no screen. When the program that opens the window
fails, the installer prints its message and runs the server in this terminal. A server already
answering on `127.0.0.1:8080` is used as it is.

## Docker

The installer asks nothing about Docker; every generated server includes Docker files for its
database, and its README says how to deploy:

- **SQLite:** `docker compose up -d --build`. The `Dockerfile` builds the server in
  `rust:1.96.0-slim-bookworm` (BuildKit cache mounts) and copies the binary
  (`/usr/local/bin/net-backend-server`), `config.docker.toml` (as `/etc/net-backend/config.toml`:
  `0.0.0.0:8080` inside the container, `sqlite:/data/game.db`, migrations on start, JSON logs, mails
  without their links) and the project's `migrations/` (when present, as `/app/migrations`) into
  `gcr.io/distroless/cc-debian12:nonroot` (uid 65532, no shell). `compose.yaml` publishes it on
  `127.0.0.1:8080`, keeps `/data` in the volume `data` (the database, and with the files module the
  players' files in `/data/files`), gives it 60 s to stop, and runs it read-only (a `/tmp` in
  memory), without capabilities and with `no-new-privileges`.
- **PostgreSQL / MySQL:** `compose.yaml` is net_backend's production file from
  [deploy/docker](https://github.com/warmar94/net_backend/tree/main/deploy/docker) for that database
  (generated passwords in Docker volumes, the database, migrations, Caddy with HTTPS and WSS), set to
  build this server (`build:` with `--bin server`, the image `<name>-server:local`) and with only the
  chosen modules in its `x-nbs-config` (the volume `server_data` for the players' files only with
  the files module; `migrations_dir = "/app/migrations"`, the project's own migrations built into
  the image). On a Linux server: a `.env` with `DOMAIN=api.example.com`, then
  `docker compose up -d --build`.

The image of a project is `<name>-server` (SQLite: `<name>-server:local` in `compose.yaml`); where a
name has `-` / `_` runs Docker refuses in an image name (`game_`, `a_-b`), those become one `-`
(`game-server`).

The image sets no `NBS__*` variable and checks its health with `net-backend-server healthcheck`; its
build arguments `CARGO_ARGS`, `BINARY` and `CONFIG` are those of the repository's
`deploy/docker/Dockerfile`.

## Modules

The modules are rows of one table in the installer (`src/modules.rs`): the name in `--modules`, what
the question shows, the modules it needs, whether it is in the default set, its cargo feature, its
`use` line and registration in `main.rs`, and its `config.toml` section (and its Docker variant).
`main.rs` registers them in the order of net_backend_server's reference server. Permissions are part
of the core (no module), and chat's message editing, read markers, typing and players' rooms are part
of `chat`.

| Module | Needs | Works with (when picked too) | Default | Feature | In `main.rs` |
|---|---|---|---|---|---|
| `auth` | | | yes | (core) | `Auth::new()` |
| `storage` | `auth` | | yes | `storage` | `Storage::new()` |
| `chat` | `auth` | `notifications` (room invitations) | yes | `chat` | `Chat::new()` |
| `leaderboards` | `auth` | | yes | `leaderboards` | `Leaderboards::new()` |
| `notifications` | `auth` | | yes | `notifications` | `Notifications::new()` |
| `friends` | `auth` | `notifications` (requests) | yes | `friends` | `Friends::new()` |
| `groups` | `auth` | `chat` (a room per group), `notifications` (invitations) | yes | `groups` | `Groups::new()` |
| `oauth` | `auth` | | no: it needs a provider (e.g. a Google OAuth client) set up first | `oauth` | `OAuth::new()` |
| `lobbies` | `auth` | `chat` (a room per lobby), `friends` (friends-only lobbies) | yes | `lobbies` | `Lobbies::new()` |
| `matchmaking` | `auth` | | yes | `matchmaking` | `Matchmaking::new()` |
| `files` | `auth` | `friends` (friends-only files) | yes | `files` | `Files::new()` |

## Versions

The version of `net_backend` is the version of `net_backend_server`, `net_backend_protocol` and
`net_backend_client` in the projects it generates (`net-backend --version` shows it); the Bevy path
uses the `bevy_net_backend` release made for it. The generated manifests ask for these versions with
caret requirements, so `cargo update` in a project moves to newer compatible releases; Bevy is
pinned exactly, as bevy_net_backend pins it.

## How it works

The templates live in [`templates/`](templates/) and are embedded in the binary with
`include_str!`: generating a project reads no network and no other file. The PostgreSQL and MySQL
Compose files are copies of `deploy/docker/compose.{postgres,mysql}.yaml` (a test keeps them equal)
changed at generation time. Placeholders are replaced by plain text substitution, the server's
`main.rs` is put together from the module table, line endings are always LF, and every file is
created with `create_new`, so an existing file is never replaced.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

Issues and pull requests are welcome. Please run `cargo fmt --all`, `cargo clippy -p net_backend
--all-targets -- -D warnings`, `cargo test -p net_backend` and the end-to-end tests
(`cargo test -p net_backend --test e2e -- --ignored`) before opening a pull request. Unless you
explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by
you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
