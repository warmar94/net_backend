# {{name}}

A Rust client for a [net_backend](https://net-backend.com) server, built on
[net_backend_client](https://docs.rs/net_backend_client).

## Run it

With a server running (for example `cargo run` in a project from `net-backend new-server`), in this
folder, the same in PowerShell, cmd, bash and zsh:

```text
cargo run                                   # the server at http://127.0.0.1:8080
cargo run -- https://api.example.com        # another server (or the variable NET_BACKEND_URL)
```

The client logs in to a development account (`player@example.com`, registered on its first run),
writes and reads a save, joins the public chat room `world`, sends a message and prints the room's
messages for 10 seconds. Plain `http://` goes to this machine only (127.0.0.1, ::1, localhost); a
server elsewhere is reached over `https://`.

The account comes from the variables `NET_BACKEND_EMAIL` and `NET_BACKEND_PASSWORD`. Without them, a
server on this machine gets the development account `player@example.com` (its password is in
`src/main.rs`, so the client never uses it for a server elsewhere).

The server needs the `Auth`, `Storage` and `Chat` modules and a public chat room `world` (a project
from `net-backend new-server` has all of them).

## What is in it

```text
{{name}}/
├── Cargo.toml     net_backend_client (feature `ws`), tokio, serde_json
└── src/main.rs    login, a save, chat
```

`src/main.rs` is plain async Rust (tokio). The
[net_backend_client guide](https://docs.rs/net_backend_client) describes every call, the
WebSocket and the blocking interface for game loops.
