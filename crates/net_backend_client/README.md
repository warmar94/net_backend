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
transport features (HTTP, WebSocket, SSH / SFTP, uploads and downloads with progress, the OpenID
Connect sign-in) as Bevy plugins; logging in and refreshing tokens are the game's own requests there.

## Contents

- [What it does](#what-it-does)
- [Features](#features)
- [Install](#install)
- [Quick start (async)](#quick-start-async)
- [Quick start (blocking, game loops)](#quick-start-blocking-game-loops)
- [The session: tokens, refresh, logout](#the-session-tokens-refresh-logout)
- [Sign-in with Google or another OpenID Connect provider (`oauth`)](#sign-in-with-google-or-another-openid-connect-provider-oauth)
- [Typed calls and errors](#typed-calls-and-errors)
- [Files: uploads and downloads with progress](#files-uploads-and-downloads-with-progress)
- [WebSocket (`ws`)](#websocket-ws)
- [SSH and SFTP (`ssh`, `sftp`)](#ssh-and-sftp-ssh-sftp)
- [Proxies](#proxies)
- [Certificates and HTTP/2](#certificates-and-http2)
- [Secrets in memory](#secrets-in-memory)
- [Secrets in logs](#secrets-in-logs)
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
- **The session, handled.** Log in (email + password, a Steam ticket, or an OpenID Connect
  provider's ID token such as Google's) or register; the access
  token is refreshed before it expires, one refresh at a time for every caller; a call answered
  401 is retried once after a refresh; rotating refresh tokens are reported to your app so it can
  store them, or kept in a token file by the client; logout and logout everywhere.
- **Files with progress.** Uploads streamed from disk (or memory) as `multipart/form-data` with a
  typed JSON settings part and progress reports; downloads into memory or streamed to a file with
  progress, checked against the SHA-256 the server sends.
- **A WebSocket that reconnects by the rules** (feature `ws`): typed requests and answers, typed
  server pushes, heartbeats, automatic reconnects with backoff that never retry after a "do not
  come back" close code.
- **SSH and SFTP to the server machine** (features `ssh`, `sftp`) for admin tools: strict host key
  checking, key files, the SSH agent, passwords and keyboard-interactive logins, opt-in automatic
  reconnects, pipelined transfers with progress.
- **Async or blocking.** One async core on tokio, plus a blocking interface for programs without a
  runtime, where nothing ever blocks a game frame.
- **Proxies.** HTTP and the WebSocket go through an HTTP CONNECT proxy from the environment
  (`HTTPS_PROXY`, `NO_PROXY`, …) or the builder.
- **Certificates.** Mozilla's root certificates by default; extra root certificates (a self-signed
  development server, a company CA) from PEM text or a file; the operating system's certificate
  store (feature `os-certificates`). HTTP/2 through ALPN (feature `http2`).
- **Honest answers.** Every call gets exactly one answer; an error says whether the request may
  have reached the server (`was_sent()`); a cancelled request says whether it went out; secrets
  never show in `Debug`, `Display` or the logs ([Secrets in logs](#secrets-in-logs)), and are
  overwritten in memory when dropped; nothing panics on bad input.

## Features

| Feature | What |
|---|---|
| (always) | HTTP(S): log in, keep the session fresh, call the server's routes with the protocol's typed requests; the blocking interface |
| `ws` | the WebSocket: typed requests and answers, server pushes, heartbeats, reconnects |
| `ssh` | SSH to the server machine's OpenSSH (admin tools, commands) |
| `sftp` | file transfer over SSH (implies `ssh`) |
| `ssh-rsa` | also accept old RSA host keys and RSA key files (opt-in) |
| `os-certificates` | `ClientBuilder::os_certificates(true)`: trust the operating system's certificate store (`rustls-platform-verifier`, ring) |
| `http2` | `ClientBuilder::http2(true)`: offer HTTP/2 to HTTPS servers through ALPN (hyper's `h2`) |
| `oauth` | the desktop sign-in at an OpenID Connect provider (Google and others) in the system browser: PKCE with a loopback redirect (module `oauth`, `Client::sign_in_oauth`) |

- One async core on tokio, plus a blocking interface for game loops and simple programs.
- TLS with rustls + ring.
- A small footprint: HTTP and the WebSocket use the same crates as the server (hyper, rustls with
  ring, tungstenite); next to the server they add one small crate, `ipnet` (the proxy rules).
  `os-certificates` adds `rustls-platform-verifier` and the platform's own crates (`windows-sys`
  on Windows, `security-framework` on macOS / iOS, `rustls-native-certs` on Linux / BSD, `jni` on
  Android); `http2` adds `h2`; `oauth` uses `ring` and `base64`, which every build already has.
  No OpenSSL, no aws-lc, no native-tls with any feature.

`default = []`: the HTTP client and the blocking interface are always there; everything else is
opt-in.

## Install

```toml
[dependencies]
net_backend_client = { version = "0.2.0" }
# or with the WebSocket:
# net_backend_client = { version = "0.2.0", features = ["ws"] }
# an admin tool with SSH and SFTP:
# net_backend_client = { version = "0.2.0", features = ["ws", "sftp"] }
```

The protocol's types are re-exported as `net_backend_client::protocol`, so the versions always
match; depending on `net_backend_protocol` yourself works too (use the version this crate uses).

The async API needs a tokio runtime in your app (`cargo add tokio --features rt-multi-thread,macros`);
the blocking API brings its own.

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
  `call_with_timeout` and `send_with_timeout` give one call its own deadline.
- `reply.cancel()` (or a `reply.cancel_handle()` kept elsewhere) cancels a request made with
  `send`: its answer becomes `Error::Cancelled { sent }`, with `sent: Some(false)` when the request
  had not been handed to a connection (it is never sent) and `None` after (it may have reached
  the server). Dropping a `Reply` does not cancel the request.
- Every blocking client (and its clones) shares one private thread with a small tokio runtime; it
  stops when the last clone is dropped.
- Blocking calls work on any thread, also inside tokio's `spawn_blocking` (the place for blocking
  work in a tokio program). In async code use the async client instead (`client.async_client()`,
  same session) or `.await` the `Reply`: tokio offers no public way to tell an async worker from a
  `spawn_blocking` thread, so a blocking call there is not refused; it stalls that worker until the
  answer, but never deadlocks or panics (the client's work runs on its own thread). The one
  refusal (`InvalidRequest`) is a blocking call on the client's own runtime thread (e.g. from an
  SSH prompt responder), which would wait for itself.
- The async client's `Reply` and `FileTransfer` are made to be awaited. Their `wait()` inside a
  current-thread tokio runtime (`#[tokio::main(flavor = "current_thread")]`, a `block_on`) is
  refused with `InvalidRequest` when their work runs on a current-thread runtime (the work could
  not go on while that thread waits); `.await` them there.

## The session: tokens, refresh, logout

| Method | What |
|---|---|
| `register(RegisterRequest)` / `login(LoginRequest)` / `login_steam(SteamLoginRequest)` | log in; the client keeps the tokens |
| `link_steam(SteamLoginRequest)` | link Steam to the logged-in account (needs a login younger than 10 minutes) |
| `login_oauth(OAuthLogin)` / `link_oauth(OAuthLogin)` | log in (or link) with an OpenID Connect provider's ID token; `sign_in_oauth` / `link_oauth_sign_in` (feature `oauth`) get the token in the system browser first |
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

**Or let the client keep the file.** `ClientBuilder::token_file(path)` loads the stored session at
`build` and writes every new pair to the file (login, registration, every refresh, `resume`); a
logout, a refused refresh or `forget_session()` deletes it. The async calls write it on tokio's
blocking pool (never on an async worker) before they return; `resume` and `forget_session` write
it on the calling thread:

```rust,no_run
# fn run() -> Result<(), net_backend_client::Error> {
let client = net_backend_client::Client::builder("https://api.example.com")
    .token_file("saves/session.json") // e.g. under %APPDATA% / ~/.local/share / ~/Library/Application Support
    .build()?;
if !client.is_logged_in() {
    // show the login screen
}
# Ok(())
# }
```

- The file is JSON (`format`, the server URL, the `TokenPair`), written atomically: a new file next
  to it, flushed to disk, then renamed over the old one.
- Unix: created owner-only (`0600`; a missing folder `0700`). Windows: the file gets the
  permissions (ACL) of its folder; under the user's profile (`%APPDATA%`, `%LOCALAPPDATA%`) that is
  the user, `SYSTEM` and the Administrators group. Keep it there.
- A file written for another server URL is not used (its tokens never go to a different server).
  A damaged file is not used either: a warning is logged and the next login overwrites it. A file
  that exists but cannot be read makes `build` fail with `InvalidRequest`.
- A failed write is logged as a warning (the file name and the I/O error, never a token); the
  session goes on and `token_updates()` still reports every pair.
- `TokenFile` (`load`, `save`, `remove`) is the same file for apps that store the session
  themselves.

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

## Sign-in with Google or another OpenID Connect provider (`oauth`)

A server with the `oauth` module accepts a provider's ID token. With the feature `oauth` the client
gets one the way desktop apps should (RFC 8252): the authorization code flow with PKCE in the
player's system browser, back to a one-time loopback address. You pass a callback that opens the
browser (or shows the URL); the crate never starts a browser itself.

```rust,no_run
use net_backend_client::oauth::OAuthFlow;
use net_backend_client::Client;

# async fn run() -> Result<(), net_backend_client::Error> {
let client = Client::new("https://api.example.com")?;
// The client id and secret of your Google OAuth client of type "Desktop app" (the secret of a
// desktop client is not confidential; Google's token endpoint asks for it).
let flow = OAuthFlow::google("1234-abc.apps.googleusercontent.com").client_secret("GOCSPX-desktop-secret");
let session = client
    .sign_in_oauth("google", &flow, |url| {
        // open the system browser here (e.g. with the `open` crate); this example prints the URL
        println!("Sign in: {url}");
        Ok(())
    })
    .await?;
println!("logged in as account {}", session.account.id);
# Ok(())
# }
```

1. A listener on `http://127.0.0.1:<free port>/callback` (it lives only for this sign-in).
2. A random PKCE verifier (S256), `state` and nonce from the operating system's random source.
3. Your callback gets the sign-in URL. The provider's page asks the player and sends the browser
   back to the loopback address; a visit with a wrong `state` gets an error page and is ignored,
   the provider's `error` (the player declined) ends the flow with `Error::OAuth`, other paths get
   404. The player sees a page saying to return to the game.
4. The code and the verifier go to the provider's token endpoint (`https://`, or `http://` only on
   loopback) with this client's TLS settings and proxy setting, the proxy decided for the token
   endpoint's own host (the environment's `HTTPS_PROXY` / `NO_PROXY`, the builder's proxy, or
   none; `flow.sign_in(open)` alone uses the environment's); the answer's `id_token` and the nonce
   go to `POST /v1/auth/oauth/{provider}` and the session keeps the tokens.

`OAuthFlow::new(authorization_endpoint, token_endpoint, client_id)` is any other provider (the
endpoints are in its `/.well-known/openid-configuration`); `scopes([...])` (always with `openid`),
`timeout` (5 minutes by default; then `Error::OAuth`), `param("prompt", "select_account")`.
`link_oauth_sign_in` links the provider account to the logged-in account instead (a login younger
than 10 minutes). `flow.sign_in(open)` alone returns a `SignedIn` for
`login_oauth(OAuthLogin::new(provider, signed.token()))`, which also takes a token your app got
another way (for example the device flow). `SignedIn` holds the ID token, the nonce, and what else
the provider's token endpoint sent: `access_token` and `refresh_token` (as `Secret`s, for the
provider's own API), `token_type`, `expires_in` and `scope`. The ID token is checked by the server,
not by the client.

The blocking client has `sign_in_oauth` and `link_oauth_sign_in` (both block until the player is
back), and `start_sign_in_oauth` / `start_link_oauth_sign_in` for game loops: they return a `Reply`
at once, polled with `try_take()` every frame. `reply.cancel()` ends the sign-in: the loopback
listener is closed and the answer is `Error::Cancelled { sent: Some(false) }` while the code had not
gone to the provider's token endpoint (`None` after).

```rust,no_run
use net_backend_client::blocking::Client;
use net_backend_client::oauth::OAuthFlow;

# fn run() -> Result<(), net_backend_client::Error> {
let client = Client::new("https://api.example.com")?;
let flow = OAuthFlow::google("1234-abc.apps.googleusercontent.com").client_secret("GOCSPX-desktop-secret");
let mut sign_in = client.start_sign_in_oauth("google", &flow, |url| {
    println!("Sign in: {url}"); // open the system browser here
    Ok(())
});
loop {
    // ... one frame of the game; a "Cancel" button calls `sign_in.cancel()` ...
    if let Some(answer) = sign_in.try_take() {
        println!("logged in as account {}", answer?.account.id);
        break;
    }
#   break;
}
# Ok(())
# }
```

## Typed calls and errors

`call` works with every request type of the protocol: auth (`GetAccount`, `UpdateAccountRequest`,
`ChangePasswordRequest`, …), storage (`GetObject`, `WriteObject`, `ListObjects`, `RemoveObject`,
`BatchGet`, `BatchPut`), chat (`ListRooms`, `ListMessages`, `OpenDirect`, `ListDirects`,
`DeleteMessage`, `EditMessage`, `MarkRead`, `ListReceipts`, `UnreadQuery`; rooms created by players:
`CreateRoom`, `MyRooms`, `PublicRooms`, `GetRoom`, `EditRoom`, `DeleteRoom`, `JoinChatRoom`,
`LeaveChatRoom`, `ListRoomMembers`, `InviteToRoom`, `KickFromRoom`, `SetRoomRole`, `TransferRoom`),
leaderboards (`ListBoards`, `GetLeaderboard`, `PostScore`, `GetMyRank`, `GetAroundMe`),
notifications (`NotificationQuery`, `CountNotifications`, `MarkNotifications`, `DeleteNotification`), friends (`ListFriends`, `AddFriend`, `AcceptFriend`, `DeclineFriend`,
`CancelFriendRequest`, `RemoveFriend`, `ListFriendRequests`, `BlockUser`, `UnblockUser`, `ListBlocks`,
`GetFriendCode`, `ResetFriendCode`, `FriendsHeartbeat`, `SteamMatch`, `GetFriendSettings`,
`UpdateFriendSettings`), groups (`ListGroups`, `CreateGroup`, `MyGroups`,
`GetGroup`, `EditGroup`, `DeleteGroup`, `ListGroupMembers`, `JoinGroup`, `LeaveGroup`, `InviteToGroup`,
`AcceptGroupInvite`, `DeclineGroupInvite`, `ListGroupInvites`, `RevokeGroupInvite`, `KickMember`,
`SetMemberRole`, `TransferGroup`), lobbies (`CreateLobby`, `MyLobbies`, `LobbySearch`, `JoinLobbyByCode`,
`GetLobby`, `EditLobby`, `JoinLobby`, `LeaveLobby`, `SetLobbyReady`, `NewLobbyCode`, `TransferLobby`,
`KickFromLobby`), matchmaking (`ListQueues`, `CreateTicket`, `GetTicket`, `CancelTicket`), admin
(`GetUser`, `BanUser`, …) and
`GetServerInfo` (also `client.info()`).
A game's own routes work the same way: implement the protocol's `HttpCall` for your request type.
`call_with_timeout(&request, timeout)` is `call` with its own deadline instead of the builder's
(shorter or longer; 1 ms to 1 h).

```rust,no_run
use net_backend_client::protocol::leaderboards::{GetAroundMe, PostScore, SubmitScore};

# async fn run(client: net_backend_client::Client) -> Result<(), net_backend_client::Error> {
let ack = client.call(&PostScore::new("highscore", SubmitScore::new(1200))).await?;
println!("rank {} (stored score {}, changed: {})", ack.rank, ack.score, ack.changed);
let around = client.call(&GetAroundMe::new("highscore")).await?;
for entry in around.items {
    println!("#{} {:?} {}", entry.rank, entry.name, entry.score);
}
# Ok(())
# }
```

On a server with Steam login, a player who linked Steam finds which of its Steam friends have an
account there. The Steam IDs come from the game's own Steam SDK (its friends list):

```rust,no_run
use net_backend_client::protocol::friends::{AddFriend, SteamMatch, UpdateFriendSettings};

# async fn run(client: net_backend_client::Client, steam_friends: Vec<u64>) -> Result<(), net_backend_client::Error> {
let found = client.call(&SteamMatch::new(steam_friends)).await?;
for player in found.players {
    println!("{} plays here as {:?} (account {})", player.steam_id, player.name, player.user);
    if player.state.is_none() {
        client.call(&AddFriend::by_id(player.user)).await?;
    }
}
// A player who does not want to be found by Steam ID turns it off:
client.call(&UpdateFriendSettings::new().steam_findable(false)).await?;
# Ok(())
# }
```

| Error | Meaning |
|---|---|
| `Api { status, error, retry_after }` | the server refused the request with the protocol's error body: branch on `error.code()` (`codes::NOT_FOUND`, `codes::VERSION_CONFLICT`, …); `error.api_error()` has the `details` |
| `Status { status, .. }` | a non-2xx answer without the protocol's error body (a proxy's 502 page, a redirect) |
| `Decode { .. }` | a success answer that is not the expected JSON |
| `Network { sent, .. }`, `Tls(..)`, `Timeout { sent, .. }` | transport failures |
| `BodyTooLarge` / `RequestTooLarge` | the answer / the request over its limit |
| `NotLoggedIn`, `SessionEnded { code }` | log in (again) |
| `Cancelled { sent }` | the app cancelled the request (`Reply::cancel`) |
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

## Files: uploads and downloads with progress

For a server with the `files` module. The settings and lists are typed calls (`ListFiles`,
`GetFile`, `EditFile`, `DeleteFile`, `GetFileUsage` through `call`); the bytes go with these methods:

```rust,no_run
use net_backend_client::files::{DownloadOptions, FileUpload};
use net_backend_client::protocol::files::{FileMeta, FileVisibility};

# async fn run(client: net_backend_client::Client) -> Result<(), net_backend_client::Error> {
let upload = FileUpload::path("screenshots/boss.png")
    .content_type("image/png")
    .meta(FileMeta::new().with_visibility(FileVisibility::Public));
let mut transfer = client.start_upload(upload);
while let Some(progress) = transfer.next_progress().await {
    println!("{} / {:?} bytes", progress.done, progress.total);
}
let file = transfer.finish().await?;
let bytes = client.download_file(file.id, DownloadOptions::default()).await?;
client.download_file_to(file.id, "downloads/boss.png").await?;
# let _ = bytes;
# Ok(())
# }
```

| Method | What |
|---|---|
| `upload_file(FileUpload)` / `start_upload(FileUpload)` | `POST /v1/files`; `start_upload` returns a `FileTransfer` with progress (the file's bytes handed to the connection) and the `FileInfo` |
| `download_file(id, DownloadOptions)` | the bytes into memory (at most `max_bytes`, 64 MiB by default) |
| `download_file_to(id, path)` / `start_download_to(id, path, options)` | streamed to a part file next to `path` (`<name>.<process>-<n>.part`, one per download), renamed to `path` when complete; with progress; `max_bytes` does not apply |

- **`FileUpload`:** `path(..)` (opened, measured and read in 64 KiB pieces while the request is
  sent: never loaded whole; the name defaults to the file name) or `bytes(name, ..)`;
  `content_type`, `file_name`, `meta(FileMeta)` (the typed JSON part: name, visibility, share list,
  metadata, an expected SHA-256), `timeout` (10 minutes by default for the whole upload; 1 ms to
  24 hours, longer values such as `Duration::MAX` are 24 hours; `DownloadOptions::timeout` the
  same). The body is two parts, `meta` (only when a setting is given) then `file`, with an exact
  `Content-Length` and a boundary of 128 random bits. A file that changes size while it is sent fails the upload.
- **`FileTransfer`:** `next_progress().await` / `try_progress()` (`done`, `total`; `done` only grows),
  `finish().await` / `try_finish()` / `wait()` (blocking programs). Dropping it cancels the
  transfer; a cancelled download leaves no part file.
- **Checks:** a download is compared with the server's SHA-256 (its `ETag`) and `Content-Length`;
  a mismatch is an error and the part file is removed. The server's refusals arrive as
  `Error::Api` (413 too large, 403 `quota_exceeded`, 422 a SHA-256 that does not match, 404 a file
  the player may not read).
- **Sessions:** the access token is refreshed before a transfer when it is about to expire; a 401
  gets one refresh and one more attempt (the upload body is built again).
- The blocking client has the same methods (`start_upload` / `start_download_to` return the same
  `FileTransfer`, for `try_progress` / `try_finish` in a game loop).

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
- **Cancel:** `reply.cancel()` answers a request `Error::Cancelled`: `sent: Some(false)` when it was
  still waiting for the connection (it is never sent), `sent: Some(true)` when it had been written
  (it may have run; its late answer is dropped).
- **Pushes:** `ws.subscribe::<P>()` is a stream of one push kind, decoded (e.g. `ChatMessage`,
  `Notification` for `notify.new`, `FriendPresence` for `friends.presence`, `LobbyMemberUpdate` /
  `LobbyUpdate` for `lobby.member` / `lobby.changed`, `MatchFound` / `TicketExpired` for `match.found` /
  `match.expired`, `MessageEdited`, `ReadReceipt`, `TypingUpdate`, `RoomUpdate` for `chat.edited`,
  `chat.read`, `chat.typing`, `chat.room`); `ws.pushes()` every push untyped. The notification
  requests (`NotificationQuery`, `CountNotifications`, `MarkNotifications`, `DeleteNotification`)
  and the chat requests `EditMessage`, `MarkRead`, `ListReceipts`, `UnreadQuery` work as WebSocket
  requests too; `SetTyping` (`chat.set_typing`) is WebSocket only. Each stream buffers 256 pushes; a
  reader that falls further behind gets `Error::Lagged { missed }` (resync then).
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
| 4001 (revoked) | ONE refresh, then one new connection at once; a refused refresh (the session ended) or a second 4001 ends it; a refresh that fails for a passing reason (network, 5xx) goes on with the backoff; without a reconnect policy (`without_reconnect`) 4001 ends it |
| 4003 (banned), 4009 (replaced: another device or window took over), 4010 (unsupported protocol), any other 4000–4099 | closed for good, no reconnect (`Error::Closed { code, .. }`) |
| handshake 401 after its one refresh, 403 | closed for good (`Error::Api`) |
| a TLS or certificate error on a reconnect attempt | closed for good (`Error::Tls`); `Reconnect::with_tls_retry(true)` retries it with the backoff instead |

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
  would only agree on an exposed cipher is refused unless `allow_terrapin_vulnerable(true)`.
  OpenSSH 9.6+ (and distribution backports) supports strict key exchange and connects.
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
- **Transfers:** a download keeps 16 reads of 64 KiB in flight (at most 1 MiB asked for or waiting
  to be written, whatever the file size) and writes them in file order; an upload keeps 16 writes
  of 32 KiB in flight. A file the server reports over the transfer limit is refused before any
  data is read. A remote file that ends before the size the server reported when it was opened
  is an error (`Error::Ssh("SFTP: the remote file was cut short during the download (expected N
  bytes, its size when it was opened; received M)")`; no local file is left); a file that reports
  a size of 0, or none, is read to its end. Each SFTP request waits at most the operation timeout (`with_sftp_timeout`, at
  least 1 s).
- **Progress:** `start_upload`, `start_upload_file`, `start_download`, `start_download_file` return
  an `SftpTask`: `next_progress()` / `try_progress()` (bytes done and the size; at most about 10
  reports a second, only growing, the last one is the whole size), then `finish()` / `try_finish()`.
  Dropping the task (or the future of `upload`, `download`, …) cancels the transfer.
- **Cancel, timeout, error:** the remote file handle is closed after every transfer, also one that
  was cancelled, timed out or failed; a download's part file is removed.
- **Reconnect (opt-in):** `SshTarget::with_reconnect(SshReconnect::default())` opens a new
  connection after a lost one: backoff from 1 s doubling up to 30 s with full jitter
  (`with_base`, `with_cap`, `with_max_attempts`, `with_stable_after`). A command that was running
  is answered `Error::Disconnected` (`sent: Some(true)` once it had started) and never run again;
  a transfer that was running ends with `Error::Disconnected` (`sent: Some(true)` once the server
  had answered part of it, else `None`) and is not repeated either; commands and transfers started
  while it reconnects wait for the new connection (bounded by their own timeout); the SFTP channel is opened again on it. Host key,
  login, protocol (`Error::Ssh`) and settings errors end the session. `ssh.state()` and
  `ssh.events()` (`Reconnecting { attempt, retry_in, error }`, `Connected { reconnected }`,
  `Closed { error }`) show what happens. Without it, a lost connection ends the session
  (`Closed`); connect again.
- **Blocking:** `net_backend_client::blocking::SshSession` has the same methods, blocking, plus the
  `start_*` transfers polled with `try_progress()` / `try_finish()` and `events().try_next()`.

```rust,no_run
use net_backend_client::ssh::{SshAuth, SshReconnect, SshSession, SshTarget};

# async fn run() -> Result<(), net_backend_client::Error> {
let target = SshTarget::new("server.example.com", "deploy")
    .with_auth(SshAuth::agent())
    .with_reconnect(SshReconnect::default().with_max_attempts(Some(10)));
let ssh = SshSession::connect(target).await?;

// feature `sftp`: a download with progress
let mut download = ssh.start_download_file("backups/db.sql.gz", "db.sql.gz");
while let Some(progress) = download.next_progress().await {
    println!("{} of {:?} bytes", progress.done, progress.total);
}
let bytes = download.finish().await?;
println!("{bytes} bytes saved");
# Ok(())
# }
```

## Proxies

HTTP calls and the WebSocket go through an HTTP proxy as an HTTP CONNECT tunnel: TLS runs end to
end through it (for an `https://` server the proxy sees only the host and port). The proxy is
decided for each destination host: the server, and an OpenID Connect provider's token endpoint
(feature `oauth`). Where the proxy comes from:

| Setting | What |
|---|---|
| (default) the environment, read at `build` | `HTTPS_PROXY` for an `https://` server, `HTTP_PROXY` for `http://`, `ALL_PROXY` for both; `NO_PROXY` (comma-separated hosts, domains with their subdomains, IP addresses and ranges, `*` for all) skips it; the lowercase names work too |
| `ClientBuilder::proxy("http://host:port")` | this proxy instead of the environment's; `http://user:password@host:port` sends `Proxy-Authorization: Basic` |
| `ClientBuilder::no_proxy()` | always direct |

- A loopback server (`localhost`, `127.0.0.1`, `::1`) is always reached directly.
- A proxy URL that is not `http://` (e.g. `socks5://`) is refused at `build` with `InvalidRequest`,
  so a request never goes around a proxy that was asked for.
- A proxy that refuses the tunnel (e.g. `407 Proxy Authentication Required`) or cannot be reached
  is `Error::Network` with `sent: Some(false)`.
- SSH connects directly to its host.

## Certificates and HTTP/2

Which server certificates HTTPS and the WebSocket (`wss://`) trust; both use the same settings:

| Setting | What |
|---|---|
| (default) | Mozilla's root certificates (webpki-roots), checked by rustls |
| `ClientBuilder::root_certificates_pem(pem)` | also trust every `CERTIFICATE` in this PEM text (a self-signed development server, a company CA); other blocks are skipped; can be called more than once |
| `ClientBuilder::root_certificates_file(path)` | the same, read from a PEM file at `build` |
| `ClientBuilder::os_certificates(true)` (feature `os-certificates`) | the operating system's certificate store and checks instead of webpki-roots: Windows (CryptoAPI), macOS / iOS (Security framework), Linux / BSD (the system's CA files), Android; the extra roots above are added on top (not on Android) |

- A PEM source without a certificate, a file that cannot be read or a certificate that cannot be a
  root is `InvalidRequest` at `build`.
- A certificate the client does not trust is `Error::Tls` (`was_sent()` is `Some(false)`).
- `os_certificates` uses `rustls-platform-verifier` with ring, handed in explicitly. On Android
  that crate needs its JNI initialization first (see its documentation).

```rust,no_run
# fn run() -> Result<(), net_backend_client::Error> {
// A development server with a self-signed certificate.
let client = net_backend_client::Client::builder("https://dev-server.local:8443")
    .root_certificates_file("certs/dev-ca.pem")
    .build()?;
# let _ = client;
# Ok(())
# }
```

**HTTP/2** (feature `http2`): `ClientBuilder::http2(true)` offers `h2` and `http/1.1` through ALPN
to `https://` servers; the server picks, and a server without HTTP/2 is spoken to in HTTP/1.1. Plain
`http://` stays HTTP/1.1, and the WebSocket is not affected. `net_backend_server` itself speaks
HTTP/1.1; a reverse proxy in front of it (e.g. Caddy) speaks HTTP/2 to clients.

## Secrets in memory

The protocol's secret types (`Password`, `AccessToken`, `RefreshToken`, `Secret`) overwrite their
whole allocation with zeros when they are dropped (the `zeroize` crate); each clone is its own copy
and is wiped when it is dropped. That covers the tokens the session keeps and every request the
client builds from them.

The client also wipes, when it is done with them:

- the `Bearer …` text it builds for the `Authorization` header of HTTP requests;
- every JSON request body it encodes (a login's or registration's password, the refresh token of a
  refresh or a logout), once the HTTP connection has dropped the body;
- the WebSocket's upgrade request (with `Authorization: Bearer`) and, through a proxy, its
  `CONNECT` request (with `Proxy-Authorization`): the crate writes both itself;
- the first-message `auth` frame of the WebSocket (the crate masks and writes it itself);
- the OpenID Connect sign-in's own texts: the sign-in URL (`state`, nonce), the browser's request
  to the loopback address and the authorization code in it, the PKCE verifier, and the code
  exchange's request body (code, verifier, client secret); the tokens it hands over are `Secret`s;
- the text of an SSH key file it reads (decoded ed25519 and ECDSA keys wipe themselves); SSH
  passwords, passphrases and prompt answers are `Secret`s;
- the text of the token file it writes and reads.

Not wiped:

- the `Authorization` and `Proxy-Authorization` header values inside an HTTP request, the proxy's
  `Proxy-Authorization` value the client keeps while it lives, and the copies hyper, rustls and
  russh make while sending (write buffers, TLS records);
- answer bodies (a login or refresh answer holds the new tokens; the provider's token answer holds
  its tokens) and the input a decoder read;
- the token file on disk (deleted at logout, not overwritten first);
- SSH passwords and keyboard-interactive answers handed to russh;
- strings that stay with your app: the `&str` a secret was copied from (a `String` passed in is
  moved, and wiped), and what `into_inner`, `expose().to_string()`, serializing a `TokenPair` for
  storage or `TokenPair::authorization_header` return.

## Secrets in logs

The crate logs through `tracing` and never puts a password, token, passphrase or key in a log
line. Its libraries log too, through the `log` crate:

- tungstenite (the WebSocket) logs every frame it sends or receives, with its content, at TRACE.
  The WebSocket's secrets never pass through it: the crate writes the upgrade request with the
  `Authorization: Bearer` header itself, reads and checks the server's answer, and masks and
  writes the first-message `auth` frame itself before tungstenite takes the connection. Chat text
  and other requests and answers do show in those TRACE lines; the directive `tungstenite=debug` (in `RUST_LOG` or a `tracing_subscriber::EnvFilter`) leaves them
  out.
- russh (SSH) logs the protocol's steps (no key, password, passphrase or prompt answer); keep
  `russh` at `info` or below.

The crate installs no logger and no filter; that stays with your program.

## How it works

- **One async core** on tokio. HTTP is hyper 1 (HTTP/1.1; HTTP/2 with `http2`) through
  hyper-util's pooled client and hyper-rustls; the WebSocket is tokio-tungstenite over the crate's
  own TCP / TLS stream (the crate writes the upgrade request and a proxy's `CONNECT` itself); SSH is
  russh. TLS is rustls with ring, handed in explicitly (never a process-wide default), with
  Mozilla's root certificates (webpki-roots) or the operating system's store, plus the extra root
  certificates; one TLS configuration per client, shared by HTTP and the WebSocket.
- **One deadline per call:** waiting for a token refresh, connecting, sending and reading the
  whole answer together (15 s by default, or the call's own with `call_with_timeout`).
- Every HTTP request carries `x-net-backend-protocol: 1` (the WebSocket handshake too); the access
  token goes in `Authorization: Bearer`, never in a URL. Redirects are never followed. Answers come
  uncompressed (the client asks for no compression).
- **The blocking interface** runs the same core on one private thread (`net-backend-client`) with
  a current-thread runtime.
- **The WebSocket** is owned by one background task per connection: it is the only thing that
  answers requests (also a cancelled one), so every request gets exactly one answer even when the
  link dies.
- **An SSH session** has one supervisor task that watches its connection, reports the state and,
  with `with_reconnect`, opens the next connection; each command and each SFTP transfer runs as a
  task of its own.
- Nothing panics on bad input or from the wrong place: an async call outside tokio answers
  `InvalidRequest`; blocking calls work from any thread but the client's own.
- One async `Client` belongs to one tokio runtime (its pooled connections belong to the runtime
  that opened them).

## Defaults and limits

| Setting | Default | Change with |
|---|---|---|
| deadline of one HTTP call | 15 s | `ClientBuilder::timeout`; per call `call_with_timeout` (blocking also `send_with_timeout`) |
| largest HTTP answer body | 10 MiB | `ClientBuilder::max_response_bytes` |
| refresh margin | 60 s | `ClientBuilder::refresh_margin` |
| plain `http://` | loopback only | `ClientBuilder::allow_insecure_http` |
| proxy | from the environment (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`), never for loopback | `ClientBuilder::proxy` / `no_proxy` |
| trusted certificates | Mozilla's roots (webpki-roots) | `ClientBuilder::root_certificates_pem` / `root_certificates_file`, `os_certificates` |
| HTTP version | HTTP/1.1 | `ClientBuilder::http2` |
| token file | none (the app stores `token_updates()`) | `ClientBuilder::token_file` |
| WebSocket connect (TCP + TLS + handshake + `auth.ok`) | 10 s | `WsSettings::with_connect_timeout` |
| WebSocket request timeout | 10 s | `WsSettings::with_request_timeout` |
| heartbeat | ping 15 s, lost after 45 s silent | `WsSettings::with_heartbeat` |
| reconnect backoff | 500 ms doubling to 30 s, full jitter, no attempt limit, reset after 10 s; a TLS error is final | `WsSettings::with_reconnect` / `without_reconnect`, `Reconnect::with_tls_retry` |
| WebSocket message size | 1 MiB (the server's) | `WsSettings::with_max_message_bytes` |
| buffered pushes per stream / events per stream | 256 / 64 (each 1 to 65 536) | `with_push_buffer` / `with_event_buffer` |
| requests waiting or running per connection | 256 | `WsSettings::with_max_pending` |
| SSH connect (TCP + key exchange + host key + login) | 15 s | `SshTarget::with_connect_timeout` |
| SSH command time / output | 60 s / 8 MiB | `with_command_timeout` / `with_max_output_bytes` |
| SSH command line | 64 KiB | (fixed) |
| SSH channels at once | 8 | `with_max_channels` |
| SSH keepalive | every 15 s, lost after 3 unanswered | `with_keepalive` |
| SSH reconnect | off; when set: 1 s doubling to 30 s, full jitter, no attempt limit, reset after 10 s | `SshTarget::with_reconnect` |
| SFTP operation time / transfer size | 5 min / 256 MiB | `with_sftp_timeout` / `with_max_transfer_bytes` |
| SFTP request time (each read, write, …) | the operation time (at least 1 s) | `with_sftp_timeout` |
| SFTP reads / writes in flight | 16 × 64 KiB / 16 × 32 KiB | (fixed) |
| SFTP progress reports | at most about 10 a second, plus the last | (fixed) |

## Clients

| Your client is… | Use |
|---|---|
| a **Rust** app | this crate + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) |
| a **Bevy** game | [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) + [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) |
| **other Rust** code | [`net_backend_protocol`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_protocol) + any HTTP / WebSocket library (for example reqwest, ureq, tokio-tungstenite) |
| **not Rust** | the server's HTTP / WebSocket API directly ([API reference](https://github.com/warmar94/net_backend/blob/main/API.md), the server's OpenAPI and AsyncAPI documents) |

## Compatibility

| `net_backend_client` | `net_backend_protocol` | `net_backend_server` | Rust | tokio |
|---|---|---|---|---|
| 0.2.0 | 0.2 (≥ 0.2.0; protocol version 1) | 0.2 (0.2.0 or newer) | 1.95+ | 1.x |
| 0.1.0 | 0.1.0 (protocol version 1) | 0.1.0 | 1.95+ | 1.x |

## Testing

The crate's tests run the real `net_backend_server` in the same process on 127.0.0.1 (SQLite in
memory, auth + storage + chat, a clock the tests move forward to expire tokens), and an in-process
mock SSH server (russh's server side, an in-memory file system for SFTP; it never executes
anything). Nothing talks to the internet; keys and known_hosts files are generated at runtime.

```text
cargo test -p net_backend_client                       # HTTP, the session, blocking
cargo test -p net_backend_client --features ws         # + the WebSocket
cargo test -p net_backend_client --all-features        # + SSH and SFTP, the OS certificate store, HTTP/2, the OpenID Connect sign-in
```

The tests cover every error kind, the session (refresh, rotation, reuse, logout), the blocking
interface, cancels, proxies, the WebSocket (both authentications, pushes, close codes, reconnects),
SSH and SFTP (host keys, every login kind, the Terrapin refusal, reconnects, transfers with
progress, cancels, timeouts and cut-short files), certificates, HTTP/2, the token file, files and
the OpenID Connect sign-in against a local mock provider. A log-capture test runs every part with
every log level on and finds no password, token, authorization code, PKCE verifier, client secret
or passphrase.

## FAQ

**Do I need `net_backend_protocol` as a dependency?** No: it is re-exported as
`net_backend_client::protocol`. Add it yourself only if you prefer the shorter path (same version).

**How do I keep the player logged in between runs?** `ClientBuilder::token_file(path)` does it.
Or store the pair from `token_updates()` every time it changes and `resume` it at start-up. An
expired access token is refreshed at the first call.

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
