# net_backend_client

<p>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

A small Rust client for
[`net_backend_server`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_server),
built on the shared message types of
[`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol).

For Rust apps that do not use Bevy: other engines, tools, bots and command-line programs. Bevy
games use [`bevy_net_backend`](https://crates.io/crates/bevy_net_backend), which offers the same
features as Bevy plugins.

## Features

| Feature | What |
|---|---|
| (always) | HTTP(S): log in, keep the session fresh, call the server's routes with the protocol's typed requests |
| `ws` | the WebSocket: typed requests and answers, server pushes, heartbeats, reconnects |
| `ssh` | SSH to the server machine's OpenSSH (admin tools, commands) |
| `sftp` | file transfer over SSH (implies `ssh`) |
| `ssh-rsa` | also accept old RSA host keys (opt-in) |

- One async core on tokio, plus a blocking interface for game loops and simple programs.
- TLS with rustls + ring; no OpenSSL.
- The smallest footprint we can manage: it shares its dependencies with the server.

## Clients

| Your client is… | Use |
|---|---|
| a **Rust** app | this crate + `net_backend_protocol` |
| a **Bevy** game | [`bevy_net_backend`](https://crates.io/crates/bevy_net_backend) + `net_backend_protocol` |
| **other** | the server's HTTP / WebSocket API directly (OpenAPI + WebSocket reference) |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

Issues and pull requests are welcome. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
