# net_backend_client

<p>
  <a href="https://crates.io/crates/net_backend_client"><img alt="crates.io" src="https://img.shields.io/crates/v/net_backend_client.svg"></a>
  <a href="https://docs.rs/net_backend_client"><img alt="docs.rs" src="https://img.shields.io/docsrs/net_backend_client"></a>
  <a href="https://net-backend.com"><img alt="Website: net-backend.com" src="https://img.shields.io/badge/website-net--backend.com-informational"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

A Rust client for
[`net_backend_server`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_server),
built on the shared message types of
[`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol).

For Rust apps that do not use Bevy: other engines, tools, bots and command-line programs. Bevy
games use [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend), which offers the same
features as Bevy plugins.

## Contents

- [What it does](#what-it-does)
- [Features](#features)
- [Install](#install)
- [Quick start (async)](#quick-start-async)
- [Quick start (blocking, game loops)](#quick-start-blocking-game-loops)
- [The session: tokens, refresh, logout](#the-session-tokens-refresh-logout)
- [Typed calls and errors](#typed-calls-and-errors)
- [WebSocket (`ws`)](#websocket-ws)
- [SSH and SFTP (`ssh`, `sftp`)](#ssh-and-sftp-ssh-sftp)
- [How it works](#how-it-works)
- [Defaults and limits](#defaults-and-limits)
- [Clients](#clients)
- [Compatibility](#compatibility)
- [Testing](#testing)
- [FAQ](#faq)
- [License](#license)
- [Contributing](#contributing)

## What it does

- **Every HTTP route, typed.** `client.call(&request)` takes any request of the protocol (or a
  game's own route described the same way, see [`HttpCall`](https://docs.rs/net_backend_protocol/latest/net_backend_protocol/http_call/trait.HttpCall.html))
  and answers its typed response. Errors carry the server's stable error code.
- **The session, handled.** Log in (email + password or a Steam ticket) or register; the access
  token is refreshed before it expires, one refresh at a time for every caller; a call answered
  401 is retried once after a refresh; rotating refresh tokens are reported to your app so it can
  store them; logout and logout everywhere.
- **A WebSocket that reconnects by the rules** (feature `ws`): typed requests and answers, typed
  server pushes, heartbeats, automatic reconnects with backoff that never retry after a "do not
  come back" close code.
- **SSH and SFTP to the server machine** (features `ssh`, `sftp`) for admin tools: strict host key
  checking, key files, the SSH agent, passwords and keyboard-interactive logins.
- **Async or blocking.** One async core on tokio, plus a blocking interface for programs without a
  runtime, where nothing ever blocks a game frame.
- **Honest answers.** Every call gets exactly one answer; an error says whether the request may
  have reached the server (`was_sent()`); secrets never show in `Debug` or `Display`; nothing
  panics on bad input.

## Features

| Feature | What |
|---|---|
| (always) | HTTP(S): log in, keep the session fresh, call the server's routes with the protocol's typed requests; the blocking interface |
| `ws` | the WebSocket: typed requests and answers, server pushes, heartbeats, reconnects |
| `ssh` | SSH to the server machine's OpenSSH (admin tools, commands) |
| `sftp` | file transfer over SSH (implies `ssh`) |
| `ssh-rsa` | also accept old RSA host keys and RSA key files (opt-in) |

- One async core on tokio, plus a blocking interface for game loops and simple programs.
- TLS with rustls + ring.
- A small footprint: HTTP and the WebSocket use the same crates as the server (hyper, rustls with
  ring, tungstenite), so an app that also runs the server adds no new crate for them.

`default = []`: the HTTP client and the blocking interface are always there; everything else is
opt-in.

## Install

```toml
[dependencies]
net_backend_client = { version = "0.1.0" }
# or with the WebSocket:
# net_backend_client = { version = "0.1.0", features = ["ws"] }
# an admin tool with SSH and SFTP:
# net_backend_client = { version = "0.1.0", features = ["ws", "sftp"] }
```

The protocol's types are re-exported as `net_backend_client::protocol`, so the versions always
match; depending on `net_backend_protocol` yourself works too (use the version this crate uses).

The async API needs a tokio runtime (`tokio = { version = "1", features = ["rt-multi-thread", "macros"] }`
in your app); the blocking API brings its own.

## Quick start (async)

```rust,no_run
use net_backend_client::protocol::auth::{GetAccount, LoginRequest};
use net_backend_client::protocol::storage::{GetObject, PutObject, WriteObject};
use net_backend_client::{Client, Error};

# async fn run() -> Result<(), Error> {
let client = Client::new("https://api.example.com")?;
client.login(LoginRequest::new("player@example.com", "a long password")).await?;

let me = client.call(&GetAccount::new()).await?;
client.call(&WriteObject::new("saves", "slot-1", PutObject::new(serde_json::json!({"level": 3})))).await?;
let save = client.call(&GetObject::new("saves", "slot-1")).await?;
println!("{:?} is on level {}", me.display_name, save.value["level"]);
# Ok(())
# }
```

`examples/quickstart.rs` is the same as a program (`cargo run -p net_backend_client --example quickstart`,
with `NET_BACKEND_URL`, `NET_BACKEND_EMAIL` and `NET_BACKEND_PASSWORD` set).

## Quick start (blocking, game loops)

```rust,no_run
use net_backend_client::blocking::Client;
use net_backend_client::protocol::auth::{GetAccount, LoginRequest};

# fn run() -> Result<(), net_backend_client::Error> {
let client = Client::new("https://api.example.com")?;
client.login(LoginRequest::new("player@example.com", "a long password"))?; // blocks (start-up)

let mut me = client.send(GetAccount::new()); // never blocks
loop {
    // ... one frame of the game ...
    if let Some(answer) = me.try_take() {
        println!("hello {:?}", answer?.display_name);
        break;
    }
#   break;
}
# Ok(())
# }
```

- `client.call(&request)` blocks until the answer; `client.send(request)` returns a
  [`Reply`](https://docs.rs/net_backend_client/latest/net_backend_client/struct.Reply.html) at once:
  poll it with `try_take()` every frame, `.await` it in async code, or `wait()` for it.
- Every blocking client (and its clones) shares one private thread with a small tokio runtime; it
  stops when the last clone is dropped.
- Blocking calls work on any thread, also inside tokio's `spawn_blocking` (the place for blocking
  work in a tokio program). In async code use the async client instead (`client.async_client()`,
  same session) or `.await` the `Reply`: tokio offers no public way to tell an async worker from a
  `spawn_blocking` thread, so a blocking call there is not refused; it stalls that worker until the
  answer, but never deadlocks or panics (the client's work runs on its own thread). The one
  refusal (`InvalidRequest`) is a blocking call on the client's own runtime thread (e.g. from an
  SSH prompt responder), which would wait for itself.

## The session: tokens, refresh, logout

| Method | What |
|---|---|
| `register(RegisterRequest)` / `login(LoginRequest)` / `login_steam(SteamLoginRequest)` | log in; the client keeps the tokens |
| `link_steam(SteamLoginRequest)` | link Steam to the logged-in account (needs a login younger than 10 minutes) |
| `resume(TokenPair)` | use the tokens your app stored (also `Client::builder(url).tokens(pair)`) |
| `token_updates()` | every change of the tokens: `Some(pair)` after a login and every refresh, `None` when the session ended |
| `refresh()` | get a new pair now (normally automatic) |
| `logout()` / `logout_everywhere()` | revoke this session / every session of the account; works with an expired access token |
| `forget_session()` | drop the tokens locally without telling the server |
| `tokens()` / `is_logged_in()` | the current pair |

**Store every new pair.** The refresh token rotates: each one works once. Persist the pair that
`token_updates()` reports (it is secret: store it like a password), and pass it to `resume` at the
next start:

```rust,no_run
# async fn run(client: net_backend_client::Client) {
let mut updates = client.token_updates();
tokio::spawn(async move {
    while let Some(change) = updates.changed().await {
        match change {
            Some(pair) => { /* save `pair` (serde: TokenPair is Serialize + Deserialize) */ let _ = pair; }
            None => { /* the session ended: delete the stored pair, show the login screen */ }
        }
    }
});
# }
```

A game loop calls `updates.try_changed()` each frame instead (no runtime needed).

How the refresh works:

- Before a call that needs a token, the access token is refreshed when less than 60 seconds of it
  are left (the protocol's margin; `ClientBuilder::refresh_margin`), but never before half of its
  lifetime has passed (a server with very short tokens does not get a refresh before every call).
  The expiry is measured against the server's clock (its `Date` header), so a wrong local clock
  does not matter.
- **One refresh at a time:** every caller that needs a token while a refresh runs waits for that
  same refresh. A refresh runs as its own task: a caller that gives up never cancels a refresh
  that may already have reached the server.
- A call answered 401 (`token_expired`, or a revoked access token) gets one refresh and is sent
  once more (a 401 means the server did not run it).
- A refresh the server refuses (`refresh_token_reused`, `unauthorized`, `banned`) ends the session:
  the tokens are dropped, `token_updates()` reports `None`, and the call answers
  `Error::SessionEnded` (`error.needs_login()` is true).
- The server answers the SAME new pair to a refresh token presented again within 30 seconds, and
  revokes the session for a reuse after that. So a refresh whose fate is unknown (a timeout, a
  reset connection) is retried at once only while it is less than 20 seconds old; after that the
  client keeps using the current access token until it really expires, and tries once more only
  then. A refresh that never left the machine (connection refused) is never a risk.
- A refresh that fails for a passing reason (network, 5xx) while the old access token is still
  valid keeps the old token: the call goes ahead.
- **Two programs must not share one stored refresh token**: the second one to refresh (after the
  grace window) ends the session of both. Log in once per program or device.

## Typed calls and errors

`call` works with every request type of the protocol: auth (`GetAccount`, `UpdateAccountRequest`,
`ChangePasswordRequest`, …), storage (`GetObject`, `WriteObject`, `ListObjects`, `RemoveObject`,
`BatchGet`, `BatchPut`), chat (`ListRooms`, `ListMessages`, `OpenDirect`, `ListDirects`,
`DeleteMessage`), admin (`GetUser`, `BanUser`, …) and `GetServerInfo` (also `client.info()`).
A game's own routes work the same way: implement the protocol's `HttpCall` for your request type.

| Error | Meaning |
|---|---|
| `Api { status, error, retry_after }` | the server refused the request with the protocol's error body: branch on `error.code()` (`codes::NOT_FOUND`, `codes::VERSION_CONFLICT`, …); `error.api_error()` has the `details` |
| `Status { status, .. }` | a non-2xx answer without the protocol's error body (a proxy's 502 page, a redirect) |
| `Decode { .. }` | a success answer that is not the expected JSON |
| `Network { sent, .. }`, `Tls(..)`, `Timeout { sent, .. }` | transport failures |
| `BodyTooLarge` / `RequestTooLarge` | the answer / the request over its limit |
| `NotLoggedIn`, `SessionEnded { code }` | log in (again) |
| `InvalidRequest(..)` | refused before sending: a bad URL, a path parameter that would need escaping, a call from the wrong place (an async call outside tokio, a blocking call on the client's own thread) |

Helpers on `Error`: `code()`, `is(code)`, `status()`, `retry_after()` (from
`details.retry_after_ms` or `Retry-After`: wait that long after a 429), `was_sent()`
(`Some(false)` never sent, `Some(true)` it reached the server, `None` maybe), `needs_login()`,
`close_code()`.

```rust,no_run
use net_backend_client::protocol::codes;
use net_backend_client::protocol::storage::{ObjectVersion, PutObject, VersionConflict, WriteObject};

# async fn run(client: net_backend_client::Client) -> Result<(), net_backend_client::Error> {
let save = WriteObject::new("saves", "slot-1", PutObject::new(serde_json::json!({"level": 4})).if_version(ObjectVersion::new(3)));
match client.call(&save).await {
    Ok(ack) => println!("saved as version {}", ack.version.get()),
    Err(error) if error.is(codes::VERSION_CONFLICT) => {
        let current = error.api_error().and_then(|e| e.details_as::<VersionConflict>()).and_then(|d| d.current_version);
        println!("another device saved first (now version {current:?}): reload and merge");
    }
    Err(error) if error.is(codes::RATE_LIMITED) => println!("wait {:?}", error.retry_after()),
    Err(error) => return Err(error),
}
# Ok(())
# }
```

## WebSocket (`ws`)

```rust,no_run
use net_backend_client::protocol::chat::{ChatMessage, JoinRoom, SendMessage};
use net_backend_client::ws::{WsEvent, WsSettings};

# async fn run(client: net_backend_client::Client) -> Result<(), net_backend_client::Error> {
let ws = client.connect_ws(WsSettings::default()).await?; // waits for the first connection
let mut messages = ws.subscribe::<ChatMessage>();
let mut events = ws.events();

let room = ws.request(&JoinRoom::new("world")).await?;
ws.request(&SendMessage::new(room.id, "hello")).await?;

loop {
    tokio::select! {
        Some(message) = messages.next() => println!("{}", message?.text),
        Some(event) = events.next() => match event {
            // Membership ends with every connection: join again, reload what you may have missed.
            WsEvent::Connected { reconnected: true } => { ws.request(&JoinRoom::new("world")).await?; }
            WsEvent::Closed { error } => { println!("closed for good: {error:?}"); break; }
            _ => {}
        },
        else => break,
    }
}
# Ok(())
# }
```

- **Authentication:** the `Authorization: Bearer` header on the handshake by default;
  `WsSettings::with_auth(WsAuthMode::FirstMessage)` sends the first-message `auth` instead and
  waits for `auth.ok` (`WsAuthMode::Both` does both). The token is refreshed first when it is about
  to expire; a handshake (or `auth`) refused with `token_expired` / `unauthorized` gets ONE refresh
  and one more try.
- **Requests:** `ws.request(&call)` returns a `Reply` with the call's typed answer (or
  `Error::Api` with the server's code). The request is queued at once, so answers come in the
  order requests were made. Each has a timeout (10 s, `with_request_timeout`, or
  `request_with_timeout`). `request_raw(kind, json)` sends a kind without a type.
- **Pushes:** `ws.subscribe::<P>()` is a stream of one push kind, decoded; `ws.pushes()` every push
  untyped. Each stream buffers 256 pushes; a reader that falls further behind gets
  `Error::Lagged { missed }` (resync then).
- **Events:** `ws.events()`: `Connected { reconnected }`, `Reconnecting { attempt, retry_in, error }`,
  `Closed { error }` (final, `None` when the app closed it). `ws.state()` is the current state.
- **Exactly one answer per request.** A request that went out on a link that was then lost is
  answered `Error::Disconnected` with `sent: Some(true)` and never resent (it may have run).
  Requests made while reconnecting wait and go out on the new link (or time out, `sent: Some(false)`).
  A request over 1 MiB is refused unsent (`RequestTooLarge`).
- **Heartbeats:** a ping every 15 s; a link from which nothing arrived for 45 s (checked at each ping)
  counts as lost and reconnects.
- `ws.close()` closes for every clone (code 1000); dropping the last clone does too.

| Close / answer | What the client does |
|---|---|
| 1000, 1001, 1006, 1008, 1009, 1011, 1013, a network loss, a silent link | reconnects with exponential backoff and full jitter (500 ms, doubling, up to 30 s; reset after 10 s connected) |
| handshake 429 / 503 | reconnects, waiting at least `Retry-After` |
| 4001 (revoked) | ONE refresh, then one new connection at once; a failed refresh or a second 4001 ends it |
| 4003 (banned), 4009 (replaced: another device or window took over), 4010 (unsupported protocol), any other 4000–4099 | closed for good, no reconnect (`Error::Closed { code, .. }`) |
| handshake 401 after its one refresh, 403 | closed for good (`Error::Api`) |

Your app rejoins its chat rooms after a reconnect (it knows which rooms still matter): do it on
`WsEvent::Connected { reconnected: true }`, as above.

From a game loop (blocking interface): `blocking_client.connect_ws(settings)` blocks until
connected; then `ws.request(..)` returns a `Reply` (`try_take()`), and streams have `try_next()`.

## SSH and SFTP (`ssh`, `sftp`)

**For admin tools only.** An SSH key (or agent access) inside a program you give to players is
shell access for anyone who extracts it. Every connect is refused in a release build unless the
target opts in with `SshTarget::allow_in_release(true)`. SSH talks to the server machine's
OpenSSH, never through `net_backend_server`.

```rust,no_run
use net_backend_client::ssh::{SshAuth, SshSession, SshTarget};

# async fn run() -> Result<(), net_backend_client::Error> {
let target = SshTarget::new("server.example.com", "deploy")
    .with_auth(SshAuth::agent())
    .with_known_hosts_file("/home/admin/.ssh/known_hosts");
let ssh = SshSession::connect(target).await?;

let status = ssh.run("systemctl is-active net-backend").await?;
println!("{} (exit {:?})", status.stdout_text().trim(), status.exit.status);

// feature `sftp`
ssh.upload("backups/notes.txt", b"hello".to_vec()).await?;
for entry in ssh.list_dir("backups").await? {
    println!("{:?} {:?}", entry.kind, entry.safe_file_name());
}
ssh.close().await;
# Ok(())
# }
```

- **Host keys:** always checked against known_hosts files (`with_known_hosts_file`; default
  `~/.ssh/known_hosts`; read-only, nothing is ever written) or a fingerprint pinned in code
  (`trust_host_key_fingerprint("SHA256:…")`, checked on the server first). Patterns, hashed hosts,
  `[host]:port` and `@revoked` are understood. An unknown, changed or revoked key is
  `Error::HostKey` with the fingerprint, never accepted silently.
- **Terrapin (CVE-2023-48795):** AES-GCM is preferred; a server without strict key exchange that
  would only agree on an exposed cipher is refused unless `allow_terrapin_vulnerable(true)`. The
  refusal and its opt-out are tested against an in-process mock server (a server without strict key
  exchange); OpenSSH 9.6+ (and distribution backports) supports strict key exchange and connects.
- **Logins** (tried in order): `SshAuth::key_file(path)`, `key_file_with_passphrase`,
  `SshAuth::agent()` (`SSH_AUTH_SOCK`; on Windows the OpenSSH agent pipe, then Pageant),
  `SshAuth::password(..)`, `SshAuth::keyboard_interactive(SshPromptAnswers::new().answer_containing("code", ..))`.
  RSA keys need `ssh-rsa`; SHA-1 RSA signatures are never used. `SshTarget::from_ssh_config("alias")`
  reads `HostName`, `Port`, `User`, `IdentityFile` and `ConnectTimeout` from `~/.ssh/config`.
- **Commands:** `run(cmd)` collects stdout + stderr (8 MiB limit) and the exit; `run_streaming(cmd)`
  gives chunks as they arrive (`next_chunk()` / `try_next_chunk()`, then `finish()`). A non-zero
  exit is still `Ok`. `SshCommand::new(..).with_stdin(..).with_timeout(..)`. Dropping a running
  command sends `TERM` and closes its channel. Several commands run at once (8 channels).
- **SFTP:** `upload`, `upload_file`, `download`, `download_file` (through a part file, renamed when
  complete), `list_dir`, `create_dir`, `remove_file`, `remove_dir`, `rename`. Names in a listing
  come from the server: use `entry.safe_file_name()` before turning one into a local path.
- **Blocking:** `net_backend_client::blocking::SshSession` has the same methods, blocking.
- After a lost SSH connection, connect again.

## How it works

- **One async core** on tokio. HTTP is hyper 1 (HTTP/1.1) through hyper-util's pooled client and
  hyper-rustls; the WebSocket is tokio-tungstenite over the crate's own TCP / TLS stream; SSH is
  russh. TLS is rustls with ring, handed in explicitly (never a process-wide default), with
  Mozilla's root certificates (webpki-roots).
- **One deadline per call:** waiting for a token refresh, connecting, sending and reading the
  whole answer together (15 s by default).
- Every HTTP request carries `x-net-backend-protocol: 1` (the WebSocket handshake too); the access
  token goes in `Authorization: Bearer`, never in a URL. Redirects are never followed. Answers come
  uncompressed (the client asks for no compression).
- **The blocking interface** runs the same core on one private thread (`net-backend-client`) with
  a current-thread runtime.
- **The WebSocket** is owned by one background task per connection: it is the only thing that
  answers requests, so every request gets exactly one answer even when the link dies.
- Nothing panics on bad input or from the wrong place: an async call outside tokio answers `InvalidRequest`;
  blocking calls work from any thread but the client's own.
- One async `Client` belongs to one tokio runtime (its pooled connections belong to the runtime
  that opened them).

## Defaults and limits

| Setting | Default | Change with |
|---|---|---|
| deadline of one HTTP call | 15 s | `ClientBuilder::timeout` |
| largest HTTP answer body | 10 MiB | `ClientBuilder::max_response_bytes` |
| refresh margin | 60 s | `ClientBuilder::refresh_margin` |
| plain `http://` | loopback only | `ClientBuilder::allow_insecure_http` |
| WebSocket connect (TCP + TLS + handshake + `auth.ok`) | 10 s | `WsSettings::with_connect_timeout` |
| WebSocket request timeout | 10 s | `WsSettings::with_request_timeout` |
| heartbeat | ping 15 s, lost after 45 s silent | `WsSettings::with_heartbeat` |
| reconnect backoff | 500 ms doubling to 30 s, full jitter, no attempt limit, reset after 10 s | `WsSettings::with_reconnect` / `without_reconnect` |
| WebSocket message size | 1 MiB (the server's) | `WsSettings::with_max_message_bytes` |
| buffered pushes per stream / events per stream | 256 / 64 | `with_push_buffer` / `with_event_buffer` |
| requests waiting or running per connection | 256 | `WsSettings::with_max_pending` |
| SSH connect (TCP + key exchange + host key + login) | 15 s | `SshTarget::with_connect_timeout` |
| SSH command time / output | 60 s / 8 MiB | `with_command_timeout` / `with_max_output_bytes` |
| SSH command line | 64 KiB | (fixed) |
| SSH channels at once | 8 | `with_max_channels` |
| SSH keepalive | every 15 s, lost after 3 unanswered | `with_keepalive` |
| SFTP operation time / transfer size | 5 min / 256 MiB | `with_sftp_timeout` / `with_max_transfer_bytes` |

## Clients

| Your client is… | Use |
|---|---|
| a **Rust** app | this crate + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) |
| a **Bevy** game | [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) |
| **other** | the server's HTTP / WebSocket API directly ([API reference](https://github.com/warmar94/net_backend/blob/main/API.md), OpenAPI + WebSocket reference) |

## Compatibility

| `net_backend_client` | `net_backend_protocol` | `net_backend_server` | Rust | tokio |
|---|---|---|---|---|
| 0.1.0 | 0.1.0 (protocol version 1) | 0.1.0 | 1.95+ | 1.x |

## Testing

The crate's tests run the real `net_backend_server` in the same process on 127.0.0.1 (SQLite in
memory, auth + storage + chat, a clock the tests move forward to expire tokens), and an in-process
mock SSH server (russh's server side, an in-memory file system for SFTP; it never executes
anything). Nothing talks to the internet; keys and known_hosts files are generated at runtime.

```text
cargo test -p net_backend_client                       # HTTP, the session, blocking
cargo test -p net_backend_client --features ws         # + the WebSocket
cargo test -p net_backend_client --all-features        # + SSH and SFTP
```

They cover: typed calls and every kind of error (404, 409 with details, 422, 403, 429 with
`retry_after`), refresh before expiry with twenty concurrent calls sharing one refresh,
`token_expired` → refresh → one retry, refresh-token reuse ending the session, logout and logout
everywhere, body limits, unreachable and silent servers, the blocking interface (also from `spawn_blocking` and
inside a runtime), WebSocket header and first-message auth, pushes, chat, close codes (1001
reconnects; 4001 one refresh + reconnect, then final; 4003 and 4009 final), a refused handshake
token, exactly one answer when the link dies, a silent peer, a busy handshake with `Retry-After`,
SSH commands, limits, timeouts, host keys (unknown, changed, pinned), every login kind, the
Terrapin refusal and its opt-out, and SFTP.

## FAQ

**Do I need `net_backend_protocol` as a dependency?** No: it is re-exported as
`net_backend_client::protocol`. Add it yourself only if you prefer the shorter path (same version).

**How do I keep the player logged in between runs?** Store the pair from `token_updates()` every
time it changes, `resume` it at start-up. An expired access token is refreshed at the first call.

**My game has its own HTTP routes and WebSocket kinds.** Describe a route with the protocol's
`HttpCall` (method, path, payload, response) and call it with `client.call`; a WebSocket kind with
`WsCall` (`ws.request`) and a push with `ServerPush` (`ws.subscribe`).

**Can I use it from a game loop without async?** Yes: `blocking::Client`, `send` + `Reply::try_take`,
and for the WebSocket `request` + `try_take` and `try_next` on the streams.

**What happens when the server restarts?** HTTP calls fail with a network error (`was_sent()`
tells whether to retry); the WebSocket reconnects by itself (`WsEvent::Connected { reconnected: true }`).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

Issues and pull requests are welcome. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.

Website: [net-backend.com](https://net-backend.com) · Contact: [info@net-backend.com](mailto:info@net-backend.com)
