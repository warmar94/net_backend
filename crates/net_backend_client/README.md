# net_backend_client

<p>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

> **Status: planned.** This crate is an empty placeholder with no API yet. It is not published.

A small Rust client for
[`net_backend_server`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_server),
built on the shared message types of
[`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol).

## What it will be

- A small, simple client for Rust apps that do not use Bevy (other engines, tools, bots, command-line
  programs): log in, keep the session fresh, call the HTTP routes and use the WebSocket with the
  protocol's typed messages.
- The smallest footprint we can manage, built on `net_backend_protocol`.
- It comes after the first release of the server and the protocol.

## Until then

| Your client is… | Use |
|---|---|
| a **Bevy** game | [`bevy_net_backend`](https://crates.io/crates/bevy_net_backend) (recommended) + `net_backend_protocol` |
| **another Rust** app | `net_backend_protocol` + the HTTP / WebSocket library you already use |
| **not Rust** | the server's HTTP / WebSocket API directly (OpenAPI + WebSocket reference) |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

Issues and pull requests are welcome. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
