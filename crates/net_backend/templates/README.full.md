# {{name}}

A game backend built with [net_backend](https://net-backend.com): a server with accounts, saves and
chat, and a Rust client that uses them.

## Run it

In this folder, the same in PowerShell, cmd, bash and zsh:

```text
cargo run              # the server on http://127.0.0.1:8080 (Ctrl-C stops it)
cargo run -p client    # in a second terminal: the client
```

On its first start the server creates the SQLite database `game.db` in this folder and applies the
migrations. `http://127.0.0.1:8080/v1/info` in a browser shows the server's protocol and modules.

The client logs in to a development account (`player@example.com`, registered on its first run),
writes and reads a save, joins the public chat room `world`, sends a message and prints the room's
messages for 10 seconds. Another server: `cargo run -p client -- https://api.example.com` (or the
variable `NET_BACKEND_URL`) with the variables `NET_BACKEND_EMAIL` and `NET_BACKEND_PASSWORD` set;
plain `http://` goes to this machine only (127.0.0.1, ::1, localhost).

In Docker (Docker Desktop or Docker Engine), the server alone:

```text
docker compose up -d --build    # http://127.0.0.1:8080, the database in the volume `data`
```

## What is in it

```text
{{name}}/
├── Cargo.toml         the workspace: `cargo run` = the server, `cargo run -p client` = the client
├── config.toml        the server's configuration (development)
├── config.docker.toml the server's configuration in the Docker image
├── server/src/main.rs the server: Auth (accounts), Storage (saves), Chat; your routes go here
├── client/src/main.rs the client: login, a save, chat
├── Dockerfile         the server in a small non-root Linux image
└── compose.yaml       the image, port 127.0.0.1:8080 and the volume `data`
```

## The server

`server/src/main.rs` builds a `NetBackendServer` with the `Auth`, `Storage` and `Chat` modules. Add
your game's HTTP routes, hooks and WebSocket handlers there; the
[net_backend_server guide](https://docs.rs/net_backend_server) describes each.

The server program has a command line (`cargo run -- --help`):

```text
cargo run -- user:create you@example.com --admin   # an admin account
cargo run -- config check --connect                # check config.toml and the database
cargo run -- migrate                               # apply the migrations
```

`config.toml` holds the address (`127.0.0.1:8080`), the database (`sqlite:game.db`), migrations on
start, the log, the mail log (development: whole mails with their links) and the chat room `world`.
An environment variable `NBS__<SECTION>__<KEY>` overrides any key. PostgreSQL or MySQL: add the
feature `postgres` or `mysql` to `net_backend_server` in `server/Cargo.toml` and set `database.url`.

The Docker image takes its configuration from `config.docker.toml`: the address `0.0.0.0:8080`
inside the container, the database `/data/game.db`, migrations on start (with this project's
`migrations/`, when present), JSON logs and a mail log with recipient and subject only. The image sets
no `NBS__*` variable, so a Compose file that mounts its own configuration over `/etc/net-backend` gets
exactly that one. Commands inside the container:
`docker compose exec server net-backend-server user:create you@example.com --admin`.

The production Compose files of net_backend
([deploy/docker](https://github.com/warmar94/net_backend/tree/main/deploy/docker): PostgreSQL or
MySQL, Caddy with HTTPS) run this image too, set as `NBS_IMAGE`: add the feature `postgres` (or
`mysql`) to `net_backend_server` first; their configuration lists the same three modules.

## The client

`client/src/main.rs` uses [net_backend_client](https://docs.rs/net_backend_client) with its
WebSocket (feature `ws`). The account comes from the variables `NET_BACKEND_EMAIL` and
`NET_BACKEND_PASSWORD`; without them, a server on this machine gets the development account
`player@example.com` (its password is in `client/src/main.rs`, so the client never uses it for a
server elsewhere).

Bevy games use [bevy_net_backend](https://github.com/warmar94/bevy_net_backend); other languages
use the HTTP + WebSocket API directly
([API.md](https://github.com/warmar94/net_backend/blob/main/API.md)).
