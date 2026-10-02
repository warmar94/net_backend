# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a key dependency bump).

## [0.1.1] - Unreleased

### Added

- Cancel: `Reply::cancel` and `Reply::cancel_handle` (`CancelHandle`) for `blocking::Client::send`
  and WebSocket requests; the answer is the new `Error::Cancelled { sent }` (`sent: Some(false)`:
  never sent; a WebSocket request that was written: `Some(true)`; an HTTP request handed to a
  connection: `None`). A WebSocket request still waiting for the connection is never sent.
- Proxies: HTTP calls and the WebSocket go through an HTTP CONNECT proxy taken from
  `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` (never for a loopback server), or set
  with `ClientBuilder::proxy` (with `user:password@` for `Proxy-Authorization: Basic`);
  `ClientBuilder::no_proxy` connects directly. A proxy URL that is not `http://` is refused at
  `build`.
- SSH (feature `ssh`): opt-in automatic reconnect, `SshTarget::with_reconnect(SshReconnect)`
  (backoff 1 s doubling to 30 s with full jitter, `with_max_attempts`, `with_stable_after`); a lost
  command is answered `Disconnected` and never run again, commands started while reconnecting
  wait for the new connection, host key / login / protocol errors end the session.
  `SshSession::state`, `events` (`SshEvent`, `SshEvents`) and `closed`, also on
  `blocking::SshSession`.
- SFTP (feature `sftp`): transfers with progress, `start_upload`, `start_upload_file`,
  `start_download`, `start_download_file` returning an `SftpTask` (`next_progress`,
  `try_progress`, `progress`, `finish`, `try_finish`; dropping it cancels the transfer), also on
  `blocking::SshSession`.

### Changed

- The README and the API documentation describe what the crate has and does.
- SFTP downloads keep 16 reads of 64 KiB in flight (at most 1 MiB asked for or waiting to be
  written) and write them in file order; a file the server reports over the transfer limit is
  refused before any data is read.
- SFTP: an operation whose connection is lost ends with `Error::Disconnected` (`sent: Some(true)`
  once the server had answered part of a transfer, else `None`), as a running command does.
- WebSocket: the upgrade request goes out through hyper, and the first-message `auth` frame is
  written by the crate, so neither passes through tungstenite (which logs its handshake request
  and every frame it sends at TRACE).
- SFTP: the remote file handle is closed after a cancelled, timed-out or failed transfer too, and
  a download's part file is removed after a cancel or timeout as well. Each SFTP request waits at
  most the operation timeout (`with_sftp_timeout`, at least 1 s).
- `net_backend_protocol` 0.1.1 or newer.

### Security

- The secret types of `net_backend_protocol` 0.1.1 overwrite their memory with zeros when they are
  dropped; the client also wipes its own copies: the `Bearer …` header text, the JSON request
  bodies it encodes, the first-message WebSocket `auth` frame and SSH key file text. The README
  section "Secrets in memory" lists what is wiped and what is not.
- A test captures every `tracing` event of the process and every `log` record of the client's side
  at every level while the session, the WebSocket and SSH run, and finds no password, token or
  passphrase. The README section "Secrets in logs" says what the libraries log.

## [0.1.0] - 2026-10-02

### Added

- `Client` (async, tokio): typed calls for every route of `net_backend_protocol` (`call` with any
  `HttpCall`), `info`, one deadline per call, a capped answer body, errors as the server's codes
  (`Error::Api` with `code()`, `retry_after()`, `was_sent()`).
- The session: `register`, `login`, `login_steam`, `link_steam`, `resume`, `refresh`, `logout`,
  `logout_everywhere`; automatic refresh before expiry (one refresh at a time, measured against the
  server's clock), one refresh + one retry after a 401, rotating refresh tokens reported through
  `token_updates()`, the server's reuse grace window respected.
- `blocking::Client`: the same client on a private runtime thread; `send` + `Reply::try_take` for
  game loops.
- Feature `ws`: `Client::connect_ws` with header or first-message authentication, typed requests
  (`WsCall`), typed push streams (`ServerPush`), events, heartbeats, reconnects with backoff that
  obey the close codes (never after 4000–4099; one refresh + reconnect after 4001), exactly one
  answer per request.
- Features `ssh`, `sftp`, `ssh-rsa`: `ssh::SshSession` (and `blocking::SshSession`) to the server
  machine's OpenSSH: strict known_hosts checking or pinned fingerprints, the Terrapin refusal and
  its opt-out, key files, the agent, password and keyboard-interactive logins, a release-build
  guard; commands (collected or streamed) and SFTP file operations.
- TLS with rustls + ring, Mozilla's root certificates.
- Examples: `quickstart`, `blocking_loop`, `chat` (`ws`), `admin_ssh` (`sftp`).
