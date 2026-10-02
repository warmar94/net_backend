# load_test

A load generator for servers built with
[`net_backend_server`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_server) (the
reference server, or your own with the `Auth`, `Storage` and `Chat` modules). It speaks the protocol over
HTTP(S) and WS(S) with the protocol crate's own types, so a run measures the whole path: TLS, the reverse
proxy, the server and the database. Not published; build it from the repository:

```text
cargo build --release --locked -p load_test        # target/release/load_test
```

TLS is rustls with ring and the webpki roots (a public certificate is expected; for a plain loopback run
use `http://127.0.0.1:8080`).

## Scenarios

Every run prints a JSON result and a single `RESULT {…}` line. `--base` (or `LT_BASE`) is the server
without a path; `--users` the accounts file (default `users.jsonl`).

| Command | What it measures |
|---|---|
| `users --count N [--prefix lt] [--concurrency 32]` | registers `lt-<n>@example.com` (or logs in where the address exists) and writes the access tokens to the users file; the time per account is mostly the password hash |
| `sockets --count N [--hold 60] [--concurrency 200] [--reconnect]` | N authenticated WebSockets (users round-robin), held open; connect time, refusals by reason, closes during the hold. With `--reconnect` a socket the server closes (restart it during the hold: 1001) connects again at once: the reconnect storm of a redeploy, with its own latency and refusals |
| `chat --members M --senders S [--rate 0.5] [--duration 30] [--room world]` | M members join a public room, S of them send `rate` messages per second each; accepted sends, deliveries (every member gets every message), lost, deliveries per second, latency |
| `dm --pairs P [--messages 50]` | both members of P pairs send at once into their DM room; accepted, delivered, latency |
| `saves --count N [--bytes 4096]` | N players make their first save at the same moment (`if_absent`); answers, latency |
| `batch [--count 1] [--offset 0] [--objects 16] [--object-bytes 262144]` | 4 MiB batch puts through the proxy (the proxy's body limit must allow ~4.2 MB) |
| `http --route info\|account\|storage-get [--concurrency 32] [--duration 30]` | requests per second and latency on keep-alive connections |

Latency of chat and DM messages is the receive time minus the send time written into the message text:
run the tool on the server machine, or keep both clocks synchronised (NTP).

## A load-test server

One load machine is one client address and one set of players. The server's protections against a single
address and a single player would measure themselves, so a load-test server turns them off or up
(never on a public server):

```text
NBS__MODULES__AUTH__RATE_LIMITS=false            # registrations / logins per address
NBS__WS__HANDSHAKES_PER_IP_PER_MINUTE=0
NBS__WS__MAX_CONNECTIONS_PER_IP=100000
NBS__MODULES__CHAT__RATE_MESSAGES=1000           # chat sends per player
NBS__MODULES__CHAT__RATE_WINDOW_SECS=1
NBS__MODULES__STORAGE__WRITE_RATE=1000
```

`sockets` uses at most `ws.max_connections_per_user` (5) sockets per account: create `count / 5` users
or more. One source address can open roughly 28 000 connections to one port (the ephemeral port range);
for more, widen `net.ipv4.ip_local_port_range` or use several load machines. Access tokens live for an
hour: run `users` again (it logs the existing accounts in) before a later run.

## License

Dual-licensed under [MIT](../../LICENSE-MIT) or [Apache-2.0](../../LICENSE-APACHE), at your option.
