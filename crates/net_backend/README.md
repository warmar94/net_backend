# net_backend

<p>
  <a href="https://crates.io/crates/net_backend"><img alt="crates.io" src="https://img.shields.io/crates/v/net_backend.svg"></a>
  <a href="https://net-backend.com"><img alt="Website: net-backend.com" src="https://img.shields.io/badge/website-net--backend.com-informational"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

The project generator of [net_backend](https://github.com/warmar94/net_backend): the `net-backend`
command writes a game backend that runs at once, a
[`net_backend_server`](https://crates.io/crates/net_backend_server) with accounts, saves and chat on
SQLite, and a [`net_backend_client`](https://crates.io/crates/net_backend_client) program that uses
them.

```text
cargo install net_backend
net-backend new mygame
cd mygame
cargo run              # the server on http://127.0.0.1:8080
cargo run -p client    # in a second terminal: the client
```

The commands are the same in PowerShell, cmd, bash and zsh, on Windows, macOS and Linux. No
environment variables, no scripts, no database to install.

## Contents

- [Install](#install)
- [Commands](#commands)
- [The generated projects](#the-generated-projects)
- [The server](#the-server)
- [The client](#the-client)
- [Docker](#docker)
- [Versions](#versions)
- [How it works](#how-it-works)
- [License](#license)
- [Contributing](#contributing)

## Install

```text
cargo install net_backend
```

It installs the `net-backend` command (a small program without dependencies; it compiles in
seconds). Rust itself comes from [rustup.rs](https://rustup.rs).

## Commands

| Command | What it writes |
|---|---|
| `net-backend new <name>` | A workspace with `server/` and `client/`. `cargo run` runs the server, `cargo run -p client` the client. |
| `net-backend new-server <name>` | A server project. `cargo run` runs it. |
| `net-backend new-client <name>` | A client project. `cargo run` runs it against `http://127.0.0.1:8080`. |
| `net-backend --help` | The usage. |
| `net-backend --version` | The version (the same as the crates the projects use). |

`<name>` is the folder to create; a path works too (`net-backend new games/mygame`), its last part is
the project name. Names follow Cargo's package-name rules, kept strict so that one name works as a
folder, a package, a binary and a Docker image everywhere: 1 to 64 characters of `a-z`, `0-9`, `-`
and `_`, starting with a letter, not a Rust keyword, a standard-library crate or a dependency of the
project. An existing folder must be empty; nothing is ever overwritten.

## The generated projects

`net-backend new mygame`:

```text
mygame/
├── Cargo.toml          the workspace (members server, client; default member server)
├── README.md           how to run, configure and extend it
├── config.toml         the server's configuration (development)
├── config.docker.toml  the server's configuration in the Docker image
├── Dockerfile          the server in a small non-root Linux image
├── compose.yaml        the image on 127.0.0.1:8080, the database in the volume `data`
├── .gitignore          target/ and the SQLite files
├── .dockerignore
├── server/
│   ├── Cargo.toml      net_backend_server (sqlite, storage, chat), tokio
│   └── src/main.rs     Auth + Storage + Chat and the framework's command line
└── client/
    ├── Cargo.toml      net_backend_client (ws), tokio, serde_json
    └── src/main.rs     login, a save, chat
```

`net-backend new-server mygame` writes the server alone (`Cargo.toml`, `src/main.rs`, `config.toml`,
`config.docker.toml`, `Dockerfile`, `compose.yaml`, `.gitignore`, `.dockerignore`, `README.md`); `net-backend new-client
mygame` the client alone (`Cargo.toml`, `src/main.rs`, `.gitignore`, `README.md`). In these the
package is named after the project; in the workspace the members are `server` and `client`.

Every generated package has `publish = false` and edition 2024.

## The server

The generated `main.rs` is the reference server of `net_backend_server`: a `NetBackendServer` with
the `Auth` (accounts, sessions, email verification and password reset), `Storage` (per-player JSON
saves) and `Chat` (rooms, messages, history) modules. It has the framework's command line
(`serve` by default, `migrate`, `config check`, `user:create`, ...), including `healthcheck` (exit 0
when the running server's `/readyz` answers 200), which the Docker image uses.

The development `config.toml`:

| Setting | Value |
|---|---|
| `server.bind` | `127.0.0.1:8080` |
| `database.url` | `sqlite:game.db` (a file in the folder `cargo run` runs in, created on the first start) |
| `database.migrate_on_start` | `true` |
| `log` | `info`, `pretty` |
| `modules.auth` | `app_name` = the project name, `log_mailer_show_links = true` (mails with their links in the log) |
| `modules.chat.rooms` | the public room `world` |

Any key can be overridden by an environment variable `NBS__<SECTION>__<KEY>`. The
[net_backend_server guide](https://docs.rs/net_backend_server) describes every key, routes, hooks
and WebSocket handlers.

## The client

The generated client checks the server's protocol (a mismatch is an error), logs in (registering
the account on the first run, so repeated runs never hit the registration limit), writes a save and
reads it back, joins the chat room `world` over the WebSocket, sends a message, prints the room's
messages for 10 seconds and logs out. The server comes from the first argument, else
`NET_BACKEND_URL`, else `http://127.0.0.1:8080`. `net_backend_client` sends plain `http://` only to
loopback addresses (127.0.0.1, ::1, localhost); a server elsewhere is reached over `https://`.

The account comes from `NET_BACKEND_EMAIL` / `NET_BACKEND_PASSWORD`. Without them, a server on
this machine gets the development account `player@example.com`, whose password is in the generated
source; for any other server the client asks for the two variables instead.

## Docker

In a server or full project, with Docker Desktop or Docker Engine:

```text
docker compose up -d --build
```

The `Dockerfile` builds the server in `rust:1.96.0-slim-bookworm` (BuildKit cache mounts keep the
registry and the build folder between builds) and copies the binary (`/usr/local/bin/net-backend-server`),
`config.docker.toml` (as `/etc/net-backend/config.toml`) and the project's `migrations/` (when
present, as `/app/migrations`) into `gcr.io/distroless/cc-debian12:nonroot` (uid 65532, no shell).
`config.docker.toml` binds `0.0.0.0:8080` inside the container, uses `sqlite:/data/game.db`, applies
migrations on start, logs JSON and writes only recipient and subject of mails. The image sets no
`NBS__*` variable, and checks its health with `net-backend-server healthcheck`. `compose.yaml`
publishes the port on `127.0.0.1:8080`, keeps `/data` in the named volume `data`, and runs the
container read-only (a `/tmp` in memory), without capabilities and with `no-new-privileges`.

The build arguments `CARGO_ARGS`, `BINARY` and `CONFIG`, the binary path, the configuration path and
the `healthcheck` command are those of the repository's
[deploy/docker](https://github.com/warmar94/net_backend/tree/main/deploy/docker) image, so its
production Compose files (PostgreSQL or MySQL, Caddy with HTTPS) run a generated server's image as
`NBS_IMAGE`; they mount their own configuration over `/etc/net-backend`. The server then needs the
feature `postgres` (or `mysql`) of `net_backend_server`.

## Versions

The version of `net_backend` is the version of the crates it generates for:

| net_backend | net_backend_server | net_backend_protocol | net_backend_client |
|---|---|---|---|
| 0.1.1 | 0.1.1 | 0.1.1 | 0.1.1 |

The generated manifests ask for that version with a caret requirement (`version = "0.1.1"`), so
`cargo update` in a project moves to newer compatible releases.

## How it works

The templates live in [`templates/`](templates/) and are embedded in the binary with
`include_str!`: generating a project reads no network and no other file. `{{name}}`, `{{package}}`
and `{{version}}` are replaced by plain text substitution, line endings are always LF, and every
file is created with `create_new`, so an existing file is never replaced. The CI job `cli` builds
the crate, runs its tests and then an end-to-end test: it generates all three kinds of project in a
path with a space, builds them against the repository's crates, runs both generated servers on
loopback (SQLite), checks `GET /v1/info` and runs the generated clients against them. A second
end-to-end test (`published_crates_build_and_run`, run by hand) builds and runs a generated project
against the crates on crates.io, without the repository's code.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

Issues and pull requests are welcome. Please run `cargo fmt --all`, `cargo clippy -p net_backend
--all-targets -- -D warnings`, `cargo test -p net_backend` and the end-to-end test
(`cargo test -p net_backend --test e2e -- --ignored generated_projects`) before opening a pull request. Unless you
explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by
you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
