# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Its version is the version of
`net_backend_server`, `net_backend_protocol` and `net_backend_client` the generated projects use.

## [0.1.1] - Unreleased

The first release (its version follows the crates it generates for).

### Added

- The `net-backend` command: `new <name>` (a workspace with `server/` and `client/`, `cargo run` =
  the server, `cargo run -p client` = the client), `new-server <name>`, `new-client <name>`,
  `--help`, `--version`.
- The generated server: `Auth`, `Storage` and `Chat` on SQLite (`net_backend_server` 0.1.1), a
  development `config.toml` (`127.0.0.1:8080`, `sqlite:game.db`, migrations on start, the chat room
  `world`, mail links in the log), the framework's command line with its `healthcheck`, a
  `Dockerfile` (non-root distroless image with `config.docker.toml` built in, no `NBS__*` variables,
  database in `/data`, the same build arguments as net_backend's `deploy/docker` image) and a hardened `compose.yaml` (read-only, no
  capabilities), `.gitignore`, `.dockerignore`, a README.
- The generated client: `net_backend_client` 0.1.1 (feature `ws`): register or log in, a save
  written and read back, the room `world` joined, a message sent, the room's messages printed for
  10 seconds; the server from an argument, `NET_BACKEND_URL` or `http://127.0.0.1:8080`; the account
  from `NET_BACKEND_EMAIL` / `NET_BACKEND_PASSWORD`, or a development account on loopback only.
- Project names checked against Cargo's package-name rules; an existing non-empty folder or file
  is never overwritten, and a failed run removes what it wrote. The templates are embedded;
  generating needs no network.
