# net_backend

<p>
  <a href="https://net-backend.com"><img alt="Website: net-backend.com" src="https://img.shields.io/badge/website-net--backend.com-informational"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

**Build your own game backend in Rust: server, protocol and client.**

Website: [net-backend.com](https://net-backend.com) · Contact: [info@net-backend.com](mailto:info@net-backend.com)

`net_backend` is a family of Rust crates for game backends: a framework you build your own server
with, the shared message types both sides speak, a small Rust client, and a generator that writes
a server and a client project that run at once. The server speaks
plain HTTP + WebSocket + JSON, so any client in any language can use it.

```text
Your client (Rust)                          Your server (Rust binary)
┌─────────────────────────────┐             ┌───────────────────────────────┐
│ your game / app / tool      │             │ your rules / hooks / logic    │
│        │                    │             │        │                      │
│ HTTP + WebSocket client ────┼─ HTTP / WS ▶│ net_backend_server            │
│  · Rust: net_backend_client │             │  core: auth, sessions, WS hub │
│  · Bevy: bevy_net_backend   │             │  modules: storage, chat       │
│  · Other: any HTTP/WS lib   │             └───────────────────────────────┘
└─────────────────────────────┘                                ▲
           ▲                                                   │
           └──── net_backend_protocol (shared message types) ──┘

Not Rust? Use the HTTP / WebSocket API directly (see API.md).
```

## Get started

With [Rust](https://rustup.rs) installed, the same commands in PowerShell, cmd, bash and zsh:

```text
cargo install net_backend
net-backend new mygame
cd mygame
cargo run              # the server on http://127.0.0.1:8080: accounts, saves, chat on SQLite
cargo run -p client    # in a second terminal: a client that logs in, saves and chats
```

`net-backend new` writes a workspace with the server, the client, a development `config.toml`, a
`Dockerfile` and a `compose.yaml`; `net-backend new-server` and `net-backend new-client` write one
side alone. The [`net_backend` README](crates/net_backend/README.md) describes the generated files.

Without the generator, add the framework to your own project (here with SQLite and the storage and
chat modules):

```text
cargo add net_backend_server --no-default-features --features sqlite,storage,chat
cargo add tokio --features rt-multi-thread,macros
```

The [server README](crates/net_backend_server/README.md#install) continues from there (the
database features, the configuration, the modules).

## The crates

| Crate | What | Version |
|---|---|---|
| [`net_backend_server`](crates/net_backend_server/README.md) | The game-backend framework: tokio + axum, modules with hooks, accounts and sessions, a WebSocket hub, storage (saves) and chat, MySQL / PostgreSQL / SQLite, migrations, OpenAPI + AsyncAPI. A library you build your own server binary with. | 0.1.1 · [crates.io](https://crates.io/crates/net_backend_server) · [docs](https://docs.rs/net_backend_server) |
| [`net_backend_protocol`](crates/net_backend_protocol/README.md) | The shared message types: plain Rust + serde data types and pure helpers. Used by the server and by Rust clients. | 0.1.1 · [crates.io](https://crates.io/crates/net_backend_protocol) · [docs](https://docs.rs/net_backend_protocol) |
| [`net_backend_client`](crates/net_backend_client/README.md) | A small Rust client for apps that do not use Bevy, built on the protocol. | 0.1.1 · [crates.io](https://crates.io/crates/net_backend_client) · [docs](https://docs.rs/net_backend_client) |
| [`net_backend`](crates/net_backend/README.md) | The project generator: the `net-backend` command (`new`, `new-server`, `new-client`) writes a server and a client that run at once. Its version is the version of the crates it generates for. | 0.1.1 · [crates.io](https://crates.io/crates/net_backend) |

Each crate has its own version, changelog and README (the full manual).

## Clients

The server does not care which client connects; the JSON on the wire is the contract.

| Your client is… | Use |
|---|---|
| a **Rust** app (tool, bot, CLI, other engine) | [`net_backend_client`](crates/net_backend_client/README.md) for the connection (HTTP, WebSocket, SSH / SFTP) + [`net_backend_protocol`](crates/net_backend_protocol/README.md) for the message types. |
| a **Bevy** game | [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) for the connection (HTTP, WebSocket, SSH / SFTP) + [`net_backend_protocol`](crates/net_backend_protocol/README.md) for the message types. |
| **other** | [`net_backend_protocol`](crates/net_backend_protocol/README.md) + any HTTP / WebSocket library (for example reqwest, ureq, tokio-tungstenite). |
| **not Rust** (C#, GDScript, JavaScript, …) | The API directly: [API.md](API.md) is the complete HTTP + WebSocket reference with examples; the server's OpenAPI document describes every HTTP route and can generate typed clients; its AsyncAPI document (`/v1/asyncapi.json`) and README describe every WebSocket frame. |

## Repository layout

```text
crates/
├── net_backend_server/     the server framework
├── net_backend_protocol/   the shared message types
├── net_backend_client/     the Rust client
├── net_backend/            the project generator (`net-backend new`)
├── e2e_tests/              end-to-end tests with the Bevy client (never published)
└── load_test/              a load generator for HTTP(S) / WS(S) (never published)
deploy/                     Docker Compose or systemd, Caddy, migrations, backups, SSH hardening
```

One Cargo workspace with one lockfile and one CI workflow. Releases are per crate: each crate has
its own version and tags named `<crate>-X.Y.Z`.

```sh
cargo test -p net_backend_protocol
cargo test -p net_backend_server --no-default-features --features sqlite
cargo test -p e2e_tests          # the server driven by bevy_net_backend in a headless app
cargo test -p net_backend -- --include-ignored --skip published   # the generator, then its projects built and run
```

## Deployment

[`deploy/`](deploy/) runs a server on one Linux machine, with the install path chosen once: **Docker
Compose** (server, MySQL or PostgreSQL, Caddy) or **systemd** (a hardened service, MySQL / PostgreSQL /
SQLite). Both get HTTPS + WSS through Caddy, migrations on every deploy, daily backups with a restore
command, and an SSH hardening guide. The [deployment guide](deploy/README.md) walks through both.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

Issues and pull requests are welcome. Please run `cargo fmt --all`, `cargo clippy --workspace
--all-targets -- -D warnings` and the tests of the crate you changed (see its README) before
opening a pull request. Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

Website: [net-backend.com](https://net-backend.com) · Contact: [info@net-backend.com](mailto:info@net-backend.com)
