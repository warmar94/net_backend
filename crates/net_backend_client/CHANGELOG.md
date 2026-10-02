# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a key dependency bump).

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
