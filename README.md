# net_backend

<p>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

**Build your own game backend in Rust: server, protocol and client.**

> **Status: in development.** Nothing is published yet. The server's core, its accounts module,
> its WebSocket hub and the storage and chat modules work and are tested; deployment comes next.
> APIs may still change before 0.1.0.

`net_backend` is a family of Rust crates for game backends: a framework you build your own server
with, the shared message types both sides speak, and (coming) a small client. The server speaks
plain HTTP + WebSocket + JSON, so any client in any language can use it.

```text
Your client (Rust)                        Your server (Rust binary)
┌───────────────────────────┐             ┌───────────────────────────────┐
│ your game / app / tool    │             │ your rules / hooks / logic    │
│        │                  │             │        │                      │
│ HTTP + WebSocket client ──┼─ HTTP / WS ▶│ net_backend_server            │
│  · Bevy: bevy_net_backend │             │  core: auth, sessions, WS hub │
│    (recommended)          │             │  modules: chat, leaderboards… │
│  · other: any HTTP/WS lib │             └───────────────────────────────┘
│    (net_backend_client:   │                          ▲
│     coming)               │                          │
└───────────────────────────┘                          │
           ▲                                           │
           └──── net_backend_protocol (shared message types) ────┘

Not Rust? Use the HTTP / WebSocket API directly (OpenAPI + WebSocket reference).
```

## The crates

| Crate | What | Status |
|---|---|---|
| [`net_backend_server`](crates/net_backend_server/README.md) | The game-backend framework: tokio + axum, modules with hooks, accounts and sessions, a WebSocket hub, storage (saves) and chat, MySQL / PostgreSQL / SQLite, migrations, OpenAPI + AsyncAPI. A library you build your own server binary with. | in development |
| [`net_backend_protocol`](crates/net_backend_protocol/README.md) | The shared message types: plain Rust + serde, no networking, no async runtime, no engine. Used by the server and by Rust clients. | in development |
| [`net_backend_client`](crates/net_backend_client/README.md) | A small Rust client for apps that do not use Bevy, built on the protocol. | planned |

Each crate has its own version, changelog and README (the full manual).

## Clients

The server does not care which client connects; the JSON on the wire is the contract.

| Your client is… | Use |
|---|---|
| a **Bevy** game | [`bevy_net_backend`](https://crates.io/crates/bevy_net_backend) for the connection (HTTP, WebSocket), the recommended path, + `net_backend_protocol` for the message types. |
| **another Rust** app (other engine, tool, bot, CLI) | `net_backend_protocol` + the HTTP / WebSocket library you already use (for example reqwest, ureq, tokio-tungstenite). `net_backend_client` is coming. |
| **not Rust** (C#, GDScript, JavaScript, …) | The API directly: the server's OpenAPI document describes every HTTP route and can generate typed clients; its AsyncAPI document (`/v1/asyncapi.json`) and README describe every WebSocket frame. |

## Repository layout

```text
crates/
├── net_backend_server/     the server framework
├── net_backend_protocol/   the shared message types
├── net_backend_client/     the client (planned)
└── e2e_tests/              end-to-end tests with the Bevy client (never published)
```

One Cargo workspace with one lockfile and one CI workflow. Releases are per crate: each crate has
its own version and tags named `<crate>-X.Y.Z`.

```sh
cargo test -p net_backend_protocol
cargo test -p net_backend_server --no-default-features --features sqlite
cargo test -p e2e_tests          # the server driven by bevy_net_backend in a headless app
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

Issues and pull requests are welcome. Please run `cargo fmt --all`, `cargo clippy --workspace
--all-targets -- -D warnings` and the tests of the crate you changed (see its README) before
opening a pull request. Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
