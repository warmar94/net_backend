# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a key dependency bump).

## [0.2.0] - 2026-10-04

### Added

- The leaderboard calls of the protocol (`ListBoards`, `GetLeaderboard`, `PostScore`, `GetMyRank`,
  `GetAroundMe`) through `Client::call` (and the blocking client).
- The notification calls of the protocol (`NotificationQuery`, `CountNotifications`,
  `MarkNotifications`, `DeleteNotification`) through `Client::call` and as WebSocket requests
  (`ws.request`), and the `notify.new` push as `ws.subscribe::<Notification>()`.
- The friends calls of the protocol (`ListFriends`, `AddFriend`, `AcceptFriend`, `DeclineFriend`,
  `CancelFriendRequest`, `RemoveFriend`, `ListFriendRequests`, `BlockUser`, `UnblockUser`,
  `ListBlocks`, `GetFriendCode`, `ResetFriendCode`, `FriendsHeartbeat`) through `Client::call`, and
  the `friends.presence` push as `ws.subscribe::<FriendPresence>()`.
- The Steam ID lookup and the friends settings of the protocol (`SteamMatch`, `GetFriendSettings`,
  `UpdateFriendSettings`) through `Client::call`.
- The groups calls of the protocol (`ListGroups`, `CreateGroup`, `MyGroups`, `ListGroupInvites`,
  `GetGroup`, `EditGroup`, `DeleteGroup`, `ListGroupMembers`, `JoinGroup`, `LeaveGroup`,
  `InviteToGroup`, `AcceptGroupInvite`, `DeclineGroupInvite`, `RevokeGroupInvite`, `KickMember`,
  `SetMemberRole`, `TransferGroup`) through `Client::call`.
- The lobby and matchmaking calls of the protocol (`CreateLobby`, `MyLobbies`, `LobbySearch`,
  `JoinLobbyByCode`, `GetLobby`, `EditLobby`, `JoinLobby`, `LeaveLobby`, `SetLobbyReady`,
  `NewLobbyCode`, `TransferLobby`, `KickFromLobby`; `ListQueues`, `CreateTicket`, `GetTicket`,
  `CancelTicket`) through `Client::call`, and the pushes `lobby.member`, `lobby.changed`,
  `match.found` and `match.expired` as `ws.subscribe::<LobbyMemberUpdate>()` (`LobbyUpdate`,
  `MatchFound`, `TicketExpired`).
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
- Extra root certificates: `ClientBuilder::root_certificates_pem` and `root_certificates_file`
  (every `CERTIFICATE` block of the PEM; checked at `build`), for HTTPS and the WebSocket, on top of
  webpki-roots or the operating system's store. One TLS configuration per client, shared by HTTP
  and the WebSocket.
- Feature `os-certificates`: `ClientBuilder::os_certificates(true)` trusts the operating system's
  certificate store and checks (`rustls-platform-verifier` with ring) instead of webpki-roots.
- Feature `http2`: `ClientBuilder::http2(true)` offers HTTP/2 to HTTPS servers through ALPN, with
  HTTP/1.1 as the fallback.
- The token file: `ClientBuilder::token_file(path)` resumes the stored session at `build` and
  writes every new pair (login, registration, refresh, `resume`), deleting the file when the
  session ends; `TokenFile` (`load`, `save`, `remove`) on its own. JSON with the server URL (a file
  of another server is not used), written atomically, owner-only on Unix (`0600`), the folder's
  ACL on Windows; its text buffers are wiped. The async calls write it on tokio's blocking pool;
  `resume` and `forget_session` on the calling thread.
- OpenID Connect logins: `Client::login_oauth` / `link_oauth` (and the blocking client's) with the
  protocol's `OAuthLogin`. Feature `oauth`: the desktop sign-in in the system browser, module
  `oauth`: `OAuthFlow` (`new` with the provider's endpoints, `google`, `client_secret`, `scopes`,
  `timeout`, `param`, `sign_in`) runs the authorization code flow with PKCE (S256), `state` and a
  nonce over a one-time loopback redirect (`http://127.0.0.1:<port>/callback`), handing the sign-in
  URL to a callback the app gives (`print_url` prints it; the crate never starts a browser), and
  exchanges the code at the token endpoint with the client's TLS settings and the proxy decided
  for the token endpoint's own host (`sign_in` alone: the environment's proxy settings); each
  connection to the loopback address is read on its own, so an idle one (a browser's
  pre-connection) never holds up the redirect; `sign_in` returns `SignedIn` (the ID token, the
  nonce, and the provider's `access_token` and `refresh_token` as `Secret`s, `token_type`,
  `expires_in`, `scope`). `Client::sign_in_oauth` / `link_oauth_sign_in` (and
  `blocking::Client::sign_in_oauth` / `link_oauth_sign_in`) then log in at the server;
  `blocking::Client::start_sign_in_oauth` / `start_link_oauth_sign_in` return a `Reply` at once for
  game loops, and `Reply::cancel` ends the sign-in (the loopback listener closed;
  `Error::Cancelled` with `sent: Some(false)` until the code went to the token endpoint). New error kind `Error::OAuth`
  (the sign-in did not finish).
- A call's own deadline: `Client::call_with_timeout` (and `blocking::Client::call_with_timeout` /
  `send_with_timeout`), shorter or longer than `ClientBuilder::timeout`, clamped to 1 ms..=1 h.
- Files (module `files`): `FileUpload` (from a path, read in 64 KiB pieces while it is sent, or from
  memory; content type, file name, the typed `FileMeta` JSON part, a time limit of 1 ms..=24 h,
  `MAX_TRANSFER_TIMEOUT`) sent as `multipart/form-data` with an exact `Content-Length` by
  `Client::upload_file` / `start_upload`; `download_file` (into memory, `DownloadOptions::max_bytes`),
  `download_file_to` / `start_download_to` (streamed to a part file of its own next to the path,
  `<name>.<process>-<n>.part`, renamed when complete); downloads checked against the server's
  SHA-256 and size. `FileTransfer` reports `TransferProgress` (`next_progress`,
  `try_progress`) and the result (`finish`, `try_finish`, `wait`); dropping it cancels. The same
  methods on `blocking::Client`. One refresh and one more attempt on a 401. The files calls of the
  protocol (`ListFiles`, `GetFile`, `EditFile`, `DeleteFile`, `GetFileUsage`) and the storage reads
  of other players' objects (`GetPlayerObject`, `ListPlayerObjects`) through `Client::call`.
- The chat extras of the protocol through `Client::call` (`EditMessage`, `MarkRead`, `ListReceipts`,
  `UnreadQuery`, and the player-room calls `CreateRoom`, `MyRooms`, `PublicRooms`, `GetRoom`,
  `EditRoom`, `DeleteRoom`, `JoinChatRoom`, `LeaveChatRoom`, `ListRoomMembers`, `InviteToRoom`,
  `KickFromRoom`, `SetRoomRole`, `TransferRoom`) and `ws.request` (`EditMessage`, `MarkRead`,
  `ListReceipts`, `UnreadQuery`, `SetTyping`); the pushes `MessageEdited`, `ReadReceipt`,
  `TypingUpdate` and `RoomUpdate` through `ws.subscribe`.
- `Reconnect::with_tls_retry(true)` (default `false`): keep retrying a WebSocket reconnect after a
  TLS or certificate error.

### Changed

- **Breaking:** `net_backend_protocol` 0.2.0 (re-exported as `net_backend_client::protocol`).
- **Breaking:** HTTP: a TLS or certificate error is now `Error::Tls`; 0.1.0 reported it as
  `Error::Network` (hyper-rustls wraps the rustls error twice).
- **Breaking:** WebSocket: a TLS or certificate error (`Error::Tls`) on a reconnect attempt now ends
  the connection (`WsEvent::Closed` with the error); 0.1.0 kept retrying it with the backoff.
  `Reconnect::with_tls_retry(true)` restores the retries.
- WebSocket: the crate writes the upgrade request (and, through a proxy, the `CONNECT` request)
  and the first-message `auth` frame itself and checks the server's answer, so neither the
  `Authorization` header nor the token passes through tungstenite (which logs its handshake
  request and every frame it sends at TRACE).
- SFTP downloads keep 16 reads of 64 KiB in flight (at most 1 MiB asked for or waiting to be
  written) and write them in file order; a file the server reports over the transfer limit is
  refused before any data is read.
- SFTP: each SFTP request waits at most the operation timeout (`with_sftp_timeout`, at least 1 s).
- WebSocket 4001: a refresh that fails for a passing reason (network, 5xx, timeout) goes on with
  the reconnect backoff; 0.1.0 ended the connection. With `without_reconnect`, 4001 ends the
  connection at once, as every 4000–4099 code; 0.1.0 still refreshed and reconnected once.
- The README and the API documentation describe what the crate has and does.

### Fixed

- SFTP: a download whose remote file ends before the size the server reported when it was opened
  now fails with `Error::Ssh("SFTP: the remote file was cut short during the download (expected N
  bytes, its size when it was opened; received M)")` and leaves no local file; 0.1.0 returned the
  shorter data as a success. A file that reports a size of 0, or none, is still read to its end.
- SFTP: an operation whose connection is lost ends with `Error::Disconnected` (`sent: Some(true)`
  once the server had answered part of a transfer, `Some(false)` when the connection was lost
  before the operation's first request, else `None`), as a running command does, at once (within
  200 ms), also when a request sent while the SFTP channel was closing is never answered.
- SFTP: the remote file handle is closed after a cancelled, timed-out or failed transfer too, and
  a download's part file is removed after a cancel or timeout as well.
- `Reply::wait` (and `FileTransfer::wait`) inside a current-thread tokio runtime, for an async
  client's reply whose work runs on a current-thread runtime, answers `InvalidRequest` at once;
  0.1.0 waited forever (the work could not run while the only thread waited).
- Session: a token refresh that completes while a logout runs no longer brings the tokens back:
  the refreshed pair is checked and stored under one lock, and a successful logout drops the pair
  of its session also when a refresh replaced it meanwhile (0.1.0 could stay logged in with
  revoked tokens, with `token_updates` ending at the dead pair).
- WebSocket: `WsSettings::with_push_buffer` / `with_event_buffer` take 1..=65 536 (a larger value
  is clamped); 0.1.0 panicked or aborted in `connect_ws` on a huge value.
- WebSocket: pushes and events are buffered only for existing streams; 0.1.0 kept the last 256
  pushes (and 64 events) in memory also with no stream.
- `call(&LogoutRequest::…)` sends the access token (unless expired), as `logout` does; 0.1.0 sent
  none, so a logout without the refresh token in the body was answered 401.

### Security

- The protocol's secret types overwrite their memory with zeros when they are
  dropped; the client also wipes its own copies: the `Bearer …` header text, the JSON request
  bodies it encodes, the WebSocket's upgrade request and its proxy `CONNECT` request, the
  first-message WebSocket `auth` frame, the OpenID Connect sign-in's texts (the sign-in URL, the
  redirect with the code, the PKCE verifier, the code exchange's body with the client secret) and
  SSH key file text. The README section "Secrets in memory" lists what is wiped and what is not.
- No password, token, authorization code, PKCE verifier, client secret or passphrase goes into a
  log record of the client's side at any level (its own `tracing` events and its libraries' `log`
  records). The README section "Secrets in logs" says what the libraries log.

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
