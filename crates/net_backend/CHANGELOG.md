# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Its version is the version of
`net_backend_server`, `net_backend_protocol` and `net_backend_client` the generated projects use.

## [0.2.0] - 2026-10-04

The first release (its version follows the crates it generates for).

### Added

- The interactive installer `net-backend new [<name>]`: in a terminal it asks how the game talks to
  the server (Rust client, Bevy plugin, your own client, pure API), the database (SQLite, PostgreSQL,
  MySQL), the server modules (auth, storage, chat, leaderboards, notifications, friends, groups,
  oauth, lobbies, matchmaking, files; every module adds auth), a demo app (Rust and Bevy), for Bevy
  whether the demo goes into the project or next to an existing game, and whether to start it; at
  the end it prints the equivalent one-line command. Every question is a flag
  (`--client rust|bevy|protocol|api`, `--db sqlite|postgres|mysql`, `--modules <list>|none`, `--demo`
  / `--no-demo`, `--existing <path>`, `--run`, `-y` / `--yes`); with any flag, or without a terminal,
  nothing is asked and the defaults (Rust client, SQLite, every module but oauth, the demo, no run)
  fill the rest. Prompts through `inquire`, colors through `crossterm`.
- The terminal look: in a terminal, a "NET BACKEND" banner and a section per question (a `›` cursor,
  `●` / `○` module boxes), INFO / WARN / DONE / ERROR badges, a `✓` line per part written, a spinner
  during the builds of `--run` and a summary box with the one-line command and the next steps.
  `--no-color` (any command) or `NO_COLOR` give plain text, as does output that is not a terminal.
- `new-server <name>` (the same as `new <name> --client api`) and `new-client <name>` (a Rust client
  alone), `--help`, `--version`.
- The generated server: the chosen modules from one module table (registration in the order of
  net_backend_server's reference server, cargo features, a configuration section per module: the
  board `highscore`, the matchmaking queues `duel` and `solo`, the players' files folder `files`, the
  OpenID Connect module registered with a commented Google provider and off until a provider is
  set), the chosen database in `server/Cargo.toml` and written out in `config.toml`
  (`sqlite:game.db`, or a development PostgreSQL / MySQL URL with the README's instructions to create
  that database), migrations on start, the framework's command line with its `healthcheck`.
- Docker files for every server, matched to its database: SQLite gets a `Dockerfile` (non-root
  distroless image with `config.docker.toml` built in, no `NBS__*` variables, the same build
  arguments as net_backend's `deploy/docker` image) and a hardened `compose.yaml`; PostgreSQL and
  MySQL get net_backend's production Compose file for that database (Caddy with HTTPS, generated
  secrets, 60 s to stop), set to build the project's server and configured with its modules (the
  volume `server_data` for the players' files with the files module). The SQLite image keeps the
  players' files in its `/data` volume.
- The clients: Rust (`net_backend_client`: login or register, a save, `world` chat for 10 seconds;
  a development account on loopback only), Bevy (a headless app on `bevy_net_backend` with the
  protocol's typed WebSocket requests and pushes; the same account rule), protocol (the message
  types, printed without a network connection). The demos fill in the development password for a
  server on this machine only.
- The demos: a small panel per module the server reports (account; saves; chat with message
  editing, typing and read markers; leaderboards; notifications; friends; groups; lobbies;
  matchmaking; files) and a log of what the client sent, what came back and every module's pushes
  over the WebSocket (also printed in the terminal): a small egui window for the Rust client, a
  small Bevy app for the Bevy plugin (codes and player ids typed into an input line). Both take a
  second player's email as an argument (`cargo run -p demo -- player2@example.com`).
- The demos' cargo feature `steam` (off by default): a Steam panel with the Steam friends who have an
  account on the server (and a friend request to them), the findable setting, Steam login / linking,
  "Join Game" and Steam invites for the current lobby, and joins from Steam (an invite, "Join Game",
  a start with `+nb_lobby <number>`; a demo in another lobby leaves it first); a lobby's join code travels as its number. Bevy through
  `bevy_steam_kit` (features `lobby`, `friends`, `auth`, `overlay`, `steam`: the callback pump, the
  friends list, the login ticket, invites, the overlay, the joins); egui through `steamworks`
  directly, with guards for the callbacks it panics on when Steam quits. Both go on without Steam
  when Steam quits. App id `STEAM_APP_ID` (default 480).
- `--existing <game>` (Bevy): the demo goes next to an existing game as a standalone project
  (`net_backend_demo/` beside the game's resolved folder, its own `[workspace]`; the game's files are
  not changed), with the two dependency lines for the game printed.
- `--run`: builds the project, starts the server in a new terminal window (Windows console, macOS
  Terminal, the first Linux terminal found), waits for `/v1/info`, then starts the demo (or the
  client); without a window, or when the window's launcher fails (its message is printed), it runs
  the server in the same terminal.
- PostgreSQL / MySQL projects: the production Compose file reads the project's own migrations from
  the image (`/app/migrations`). Image names are `<name>-server`, with `-` / `_` runs Docker refuses
  turned into one `-`.
- Project names checked against Cargo's package-name rules; an existing non-empty folder or file
  is never overwritten, and a failed run removes what it wrote. The templates are embedded;
  generating needs no network.
