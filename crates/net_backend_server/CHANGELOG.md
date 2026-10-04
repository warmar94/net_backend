# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a key dependency bump).

## [0.2.0] - 2026-10-04

### Added

- The `Leaderboards` module (feature `leaderboards`, `[modules.leaderboards]`): boards from the
  settings (`BoardSpec`: key, name, mode best / latest / sum, order desc / asc, period all-time /
  daily / weekly, `client_submit`), the routes `GET /v1/leaderboards`, `GET /v1/leaderboards/{board}`
  (pages, best first, with a cursor), `POST /v1/leaderboards/{board}/scores`, `GET .../me`,
  `GET .../around`, each read for any period through `at`; unique ranks (ties: who reached the score
  first, then the lower account id); one submission at a time per player (the account lock, then
  one UPDATE or INSERT); a per-user submit rate (`submit_rate`, 30 per 60 s); score metadata
  (`max_metadata_bytes`, 1 KiB); the hooks `BeforeScoreSubmit` (check, change or refuse) and
  `AfterScoreSubmit`; `LeaderboardService` for server code (`submit` on any board for any player,
  `top`, `rank`, `around`, `boards`, `remove`, `purge`); a background purge of periods older than
  `keep_periods` (8). Table `leaderboard_scores` for MySQL, PostgreSQL and SQLite.
- The `Notifications` module (feature `notifications`, `[modules.notifications]`): notifications
  stored per player and pushed live (`notify.new`, through the hub's `Broadcaster`);
  `NotificationService::send` (and `send_with`) for server code and other modules with a
  `NewNotification` (kind, text, data, sender); the player's routes `GET /v1/notifications`
  (newest first, cursor pages, `unread_only`), `GET /v1/notifications/count`,
  `POST /v1/notifications/mark` (ids or all, read or unread), `DELETE /v1/notifications/{id}` and
  their WebSocket twins `notify.list`, `notify.count`, `notify.mark`, `notify.delete`; a per-player
  cap (`max_per_user`, 200: the oldest go), a retention purge (`retention_days`, 30), `max_data_bytes`
  (4 KiB); the hooks `BeforeNotify` (change or refuse) and `AfterNotify`. Table `notifications` for
  MySQL, PostgreSQL and SQLite.
- The `Friends` module (feature `friends`, `[modules.friends]`): friend requests by account id,
  display name (exact; several matches answer 409) or friend code (8 characters, made on first use,
  replaceable), accept, decline, cancel, remove, block and unblock (a block ends a friendship and the
  open requests between the two; the blocked player's requests answer 403), the lists of friends,
  requests (received or sent) and blocks; the routes under `/v1/friends`; the online state of
  friends: a WebSocket connection or a heartbeat (`POST /v1/friends/presence`) within
  `online_window_secs` (90), kept fresh for every instance by a background task, and the
  `friends.presence` push to a player's friends when its first connection opens and its last one
  closes (in that order per instance, also when a socket closes while its "online" is being sent),
  counted over every instance (a row per instance and connected player in `friend_presence`); the
  limits `max_friends` (200), `max_pending` (50) and `max_blocks` (500) (403 `quota_exceeded`) and a
  per-player request rate (`request_rate`, 10 per 60 s); `friends.request` / `friends.accepted`
  notifications when the `Notifications` module is registered (`notify`); the hooks
  `BeforeFriendRequest` and `AfterFriendChange`; `FriendService` for server code (`state_of`,
  `are_friends`, `is_blocked`, `friends_of`, `is_online`, and the players' actions). Tables
  `friend_links`, `friend_profiles` and `friend_presence` and an index on `auth_users
  (display_name)` for MySQL, PostgreSQL and SQLite. Heartbeats, friend-code resets and settings
  changes share a per-player rate (`update_rate`, 30 per 60 s).
- Friends, Steam IDs: `POST /v1/friends/steam` answers which of a list of Steam IDs (decimal
  strings, at most `steam_max_ids` = 500) belong to accounts here, for players who linked Steam:
  the account id, the display name and the caller's relation (friend / sent / received), in the
  request's order. It works when the `Auth` module has Steam login (else 404) and the caller has a
  Steam account linked (else 403). Never found: the caller, banned accounts, players with a block
  between them and the caller (either direction), players who turned `steam_findable` off
  (`GET` / `PUT /v1/friends/settings`; findable by default). A per-player rate (`steam_rate`: a
  burst of 3, then one every 5 minutes; 429), the hook `BeforeSteamMatch` (refuse, or remove Steam
  IDs), one audit entry per lookup (`friends.steam_match`, the counts only); `FriendService`
  `steam_match`, `steam_id_of`, `settings`, `update_settings`, `set_steam_findable`. Table
  `friends_settings` for MySQL, PostgreSQL and SQLite.
- The `Groups` module (feature `groups`, `[modules.groups]`): groups (guilds, clans) with a name
  (3-32 characters, unique without regard to case), a description, `open`, the game's metadata;
  the routes under `/v1/groups` (list with a name search, create, mine, invitations, get, change,
  delete, members, join, leave, invite, accept, decline, withdraw, kick, role, transfer); roles
  owner / admin / member (admins change the group, invite and remove members; the owner sets roles
  and hands the group over; the owner leaves last); a chat group room per group with the `Chat`
  module (`chat_room`), members added and removed with the group, the room deleted with it;
  `groups.invite` / `groups.kicked` notifications with the `Notifications` module (`notify`); the
  limits `max_members` (100), `max_groups_per_user` (10), `max_invites` (50), `max_metadata_bytes`
  (2 KiB), a create rate (`create_rate`, 3 per hour) and an invitation rate per player
  (`invite_rate`, 20 per `invite_rate_window_secs` 600); no invitation to a player who blocked the
  inviter (with the `Friends` module: 403); a background upkeep (`upkeep_interval_secs`, 3600) that
  gives a group whose owner's account was deleted the oldest admin, else the oldest member, as
  owner, deletes a group with no member left and deletes the module's chat rooms no group names
  (older than an hour); the hooks `BeforeGroupCreate`, `BeforeGroupUpdate` (after the rights
  check), `BeforeGroupJoin`, `BeforeGroupInvite` (after the rights check) and `AfterGroupChange`
  (`actor` `None` for the upkeep's changes); `GroupService` for server code (`role_of`,
  `members_of`, `upkeep`, and the players' actions). Tables `game_groups`, `game_group_members` and
  `game_group_invites` for MySQL, PostgreSQL and SQLite.
- The `OAuth` module (feature `oauth`, `[modules.oauth]`): OpenID Connect logins with an identity
  provider's ID token, `POST /v1/auth/oauth/{provider}` → `AuthSession`. Providers by name
  (`ProviderConfig`: `preset = "google"`, or `issuer` with discovery or `jwks_uri`, `client_ids`,
  `accepted_issuers`, `algorithms`); RS256 / ES256 signatures checked with ring (no other algorithm,
  no `crit` header); `iss`, `aud` (+ `azp`), `exp`, `iat` (`max_token_age_secs`), `nbf` with
  `clock_skew_secs`, `sub`, and the nonce (`require_nonce`, on by default); each nonce (or token)
  accepted once on every instance (table `oauth_used_tokens`, purged after expiry); the provider's
  keys fetched over HTTPS (hyper + rustls with ring), kept for the answer's `max-age`, fetched again
  for an unknown key id (at most once per `jwks_refetch_secs`), the last keys kept for
  `jwks_stale_secs` when a fetch fails (503 `unavailable` without usable keys); the first login
  creates an account, a Bearer token of a recent login links the provider account (409 for a
  provider account of another account, one per provider), unlinking through
  `DELETE /v1/account/identities/{provider}`; the hook `BeforeOAuthLogin` (the verified claims);
  `login_per_minute` per client address (10); `OAuthService::purge`. The reference server registers
  it (no provider configured: logins answer 404).
- The `Lobbies` module (feature `lobbies`, `[modules.lobbies]`): lobbies with a host, created with a
  visibility (`public`, `private`, `friends` with the `Friends` module), a size and the game's
  metadata (text keys and values); a join code per lobby (8 characters of the friend-code alphabet,
  unique, valid while the lobby exists, replaced by the host; also a number below 2^40); joining by
  id (public lobbies, a friend's friends-only lobby) or by code (any visibility), refused for a
  player the host blocked; ready flags; the host's changes (metadata key by key, merged in memory
  with only the changed keys written, at most two changes per allowed key; size, visibility, state
  `open` / `in_game` / `closed`; back to `open` resets the ready flags; `closed` removes the
  lobby), kicks and a new host (a player who is not a member gets 404 for these, a member 403); a
  leaving host passes the lobby to the member who joined first, the last member's leaving removes
  it; a search of open lobbies by metadata filters (public ones, or those of the caller's friends);
  the pushes `lobby.member` and `lobby.changed` to the members; a chat group room per lobby with the
  `Chat` module, deleted with the lobby; a player whose last WebSocket connection on the instance
  closes leaves its lobbies after `disconnect_grace_secs` (`leave_on_disconnect`); the limits
  `max_players` (64), `max_lobbies_per_user` (1), `max_metadata_keys` (32), `max_metadata_bytes`
  (4 KiB), the rates `create_rate`, `join_rate`, `bad_code_rate` (codes that match no lobby, 20 per
  hour) and `update_rate` (changes and new codes, 30 per 60 s); a purge of lobbies left without
  members (a lobby joined meanwhile stays) that also gives a lobby whose host's account was deleted
  the member who joined first as host and deletes the module's chat rooms no lobby names (older
  than an hour); the hooks `BeforeLobbyCreate`, `BeforeLobbyUpdate` (after the rights check),
  `BeforeLobbyJoin` and `AfterLobbyChange`; `LobbyService` for server code (`lobby`, `members_of`,
  `add_player`, `leave_all`, `purge`, the players' actions; host-only actions through `LobbyActor`).
  Tables `lobbies`, `lobby_members` and `lobby_metadata` for MySQL, PostgreSQL and SQLite.
- The `Matchmaking` module (feature `matchmaking`, `[modules.matchmaking]`): queues from the
  settings (`QueueSpec`: key, players per match, ticket timeout); one ticket per player
  (`POST` / `GET` / `DELETE /v1/matchmaking/ticket`, `GET /v1/matchmaking/queues`) with
  `attributes` (`max_attributes_bytes`, 1 KiB); a round every `interval_ms` (1000): the
  `MatchmakingRound` hook (the game's rules) gets the waiting tickets and the default proposal
  (first come, first matched) and sets the matches, each with optional `data` for the players; the
  pushes `match.found` and `match.expired`; a matched ticket readable for `matched_keep_secs` (60);
  a player whose last WebSocket connection on the instance closes leaves its queue
  (`cancel_on_disconnect`); a per-player ticket rate (`ticket_rate`, 10 per 60 s); the hooks
  `BeforeTicketCreate`, `MatchmakingRound` and `AfterMatchFound`; `MatchmakingService` for server
  code (`run_round` among them). Tickets live in the instance's memory (no tables).
- Permissions (`permissions`): named rights finer than roles. `Module::permissions` and
  `NetBackendServer::permission` declare them (`Permission`: a dotted name, a description, the roles
  that hold it by default); `[permissions]` in the configuration maps a role to its permissions
  (replacing that role's defaults); `admin` holds every declared permission; a permission nobody
  declared is never held. Checks: the `RequirePermission<P>` extractor (`PermissionName`),
  `AuthContext::has_permission` / `require_permission` (403 `forbidden`), and the `Permissions` state
  value (`allows`, `require`, `declared`, `of_roles`, `roles_with`). An undeclared name in
  `[permissions]`, `admin` listed there, a permission declared twice or a module permission without
  the module's name as its prefix stop the build. The `Lobbies` module declares `lobbies.manage`
  (`admin` and `moderator` by default): act as the host of every lobby.
- The `Files` module (feature `files`, `[modules.files]`): players' binary files. `POST /v1/files`
  (`multipart/form-data`: an optional `meta` part, then the `file` part) streams the bytes to a
  `FileStore` while counting their size and SHA-256 (`max_file_bytes` 16 MiB → 413; the player's
  remaining bytes → 403 `quota_exceeded`; an expected SHA-256 that differs → 422; nothing kept of a
  refused or broken upload; an upload is timed by its data, see the upload time limits below);
  `GET /v1/files/{file}/content` streams them back (`ETag` = the SHA-256,
  `If-None-Match` → 304, an attachment with `nosniff` and a sandbox CSP); `GET /v1/files` (own files,
  or `?owner=` another player's readable ones), `GET /v1/files/usage`, `GET` / `PATCH` / `DELETE
  /v1/files/{file}`. Visibility private / public / friends (with the `Friends` module) / shared with
  chosen accounts (`max_shared_with` 50); quotas `max_files_per_user` (100) and `max_bytes_per_user`
  (256 MiB) counted under the account lock; `upload_rate` (10 per 60 s); `allowed_content_types`;
  metadata up to `max_metadata_bytes` (4 KiB). The `FileStore` trait (put all or nothing, get,
  delete; `keys` lists stored keys for the purge) with `LocalFileStore` (a folder: `dir`) and
  `Files::store` for an app's own; the hooks `BeforeFileUpload`, `BeforeFileUpdate` (a settings
  change: change the name, the visibility or the share list, or refuse) and `AfterFileChange`; a
  settings change runs under the file row's lock; `FileService` for server code. Every
  `purge_interval_secs` (3600; 0 = never) the module deletes stored bytes no file row names that are
  older than an hour (an account deleted straight from the database, a server stopped mid-upload);
  `FileService::purge_orphans` runs it from server code. Tables `stored_files` and
  `stored_file_shares` for MySQL, PostgreSQL and SQLite. The reference server registers it (the
  deployment files keep the bytes on a data volume / in the systemd state folder).
- Upload time limits: a route with `http::upload_timeout()` (the files module's upload has it) is
  timed by its data while the body arrives instead of `http.request_timeout_secs`: 503 `unavailable`
  when no data arrives for `http.upload_idle_timeout_secs` (30) or the body takes longer than
  `http.upload_timeout_secs` (3600; 0 = no overall limit); after the body, the handler has
  `http.request_timeout_secs` for the rest (`NBS__HTTP__UPLOAD_IDLE_TIMEOUT_SECS`,
  `NBS__HTTP__UPLOAD_TIMEOUT_SECS`).
- Storage objects have a visibility: `private` (the default), `public` or `friends` (with the
  `Friends` module), set by a write (`PutObject.visibility`, also in batches and admin / server
  writes) and kept by writes without it; other players read them through
  `GET /v1/users/{user}/storage/{collection}` and `.../{key}` (404 / left out when they may not);
  `StorageService::get_visible` / `list_visible`; `BeforeStorageWrite::visibility` shows a write's
  visibility to the hooks, which may change it (checked again) or refuse. A new storage migration
  adds the column (existing objects stay private).
- `LoginMethod::OpenId` and `BeforeLogin::identity` (the provider account of an OpenID Connect
  login).
- The `healthcheck` command in every server started with `NetBackendServer::run()`: it asks the
  running server's `/readyz` on `server.bind` (an unspecified address means loopback; `[::]` tries
  `::1`, then `127.0.0.1`) and exits 0 on `200`, 1 otherwise, within 4 seconds in all (below the
  5-second timeout of the Docker health checks) and without the database. Container images without a
  shell or `curl` use it as their health check. The reference server `examples/server.rs` uses the
  framework's command. An app command named `healthcheck` (an `AppCommand` registered by the app or a
  module) replaces the built-in one: it runs, and the built-in is hidden from `--help`.
- `--version` / `-V` (the framework's version) and `NetBackendServer::run_main(make)`, a whole `main`:
  `--help` and `--version` work without a configuration file or a database, every other command loads
  the configuration (`Config::load`) and runs the server `make` builds. The reference server uses it.
- Chat extras in the `Chat` module (feature `chat`; migrations on all three databases add
  `chat_rooms.is_public`, `chat_members.role`, `chat_messages.edited_at` / `edited_by` and the table
  `chat_reads`):
  - **Message editing:** `chat.edit` and `PATCH /v1/chat/rooms/{room}/messages/{message}` by the
    sender within `edit_window_secs` (900, 0 = no limit; `allow_edit`; counted on the send rate) or by
    a holder of the new permission `chat.moderate` (`chat::MODERATE`, declared by the module, held by
    `admin` and `moderator` by default; audited as `chat.message_edited`). `BeforeChatSend` runs for
    edits too (its new field `edit`), the push `chat.edited` goes to the room, the history shows the
    latest text with `edited_at`; `ChatService::edit_message` for server code; hook `AfterChatEdit`.
  - **Read markers:** `chat.mark_read` and `PUT /v1/chat/rooms/{room}/read` ("read up to message X",
    forward only; `read_rate` 30 per `read_window_secs` 60), `chat.receipts` and
    `GET /v1/chat/rooms/{room}/receipts` (DM, group and player rooms), `chat.unread` and
    `POST /v1/chat/unread` (up to 100 rooms, counts capped at 1000). DM, group and player rooms push
    `chat.read` (`read_receipts`), at most one per user, room and `read_push_interval_ms` (2000), the
    newest marker winning. Hook `AfterChatRead`.
  - **Typing indicators:** `chat.set_typing` (never stored), the push `chat.typing` at most once per
    user, room and `typing_interval_ms` (3000) with `typing_ttl_ms` (6000) for the clients, none in
    rooms over `typing_max_members` (50) online users; a sent message clears the user's state. Hook
    `BeforeChatTyping`.
  - **Rooms created by players** (`player_rooms`, on by default): `POST /v1/chat/rooms`,
    `GET /v1/chat/rooms/mine`, `GET /v1/chat/rooms/public`, `GET` / `PATCH` / `DELETE
    /v1/chat/rooms/{room}`, `POST …/join`, `POST …/leave`, `GET …/members`, `POST …/invites`,
    `DELETE …/members/{user}`, `PUT …/members/{user}/role`, `POST …/owner`. Public or private rooms,
    an owner and moderators, invitations (a `chat.invite` notification with the `Notifications`
    module, `notify`), kicks as bans until invited again, hand-on, the next owner when the owner
    leaves, deletion with the last member; `chat.join` makes the caller a member of a public room or
    accepts an invitation. Limits `max_rooms_per_player` (10), `max_player_room_members` (100),
    `room_create_rate` (5 per `room_create_window_secs` 3600). Every change is a `chat.room` push to
    the members and invited players; holders of `chat.moderate` act as the owner of every player room
    (they open, read and join private ones too; a room deleted that way is audited as
    `chat.room_deleted`); the background task gives a room whose owner's account was deleted a new
    owner. Invitations: `invite_rate` (20 per `invite_rate_window_secs` 600) per player, none to a
    player who blocked the inviter (with the `Friends` module: 403). Hooks `BeforeRoomCreate`,
    `BeforeRoomUpdate`, `BeforeRoomInvite` (after the rights check), `AfterRoomChange`; joining a
    player room runs `BeforeChatJoin`. `ChatService` methods for server code: `create_player_room`,
    `get_room`, `leave_player_room`, `delete_room`, `my_rooms`, `public_player_rooms`,
    `room_upkeep`, `receipts`, `unread`.
  - Message deletion is also open to holders of `chat.moderate` and, in a player room, to its owner
    and moderators. A sender deletes its own messages while it can still read the room.
  - `ChatService::delete_group_room` deletes a group room with its messages (the groups and lobbies
    modules use it); `chat_rooms.origin` (a new migration, with an index) names the module that
    created a group room.
- A per-account session cap: `modules.auth.max_sessions_per_user` (default 100, 1 to 100000). A login
  over it revokes the account's oldest live sessions (`RevocationReason::SessionLimit`, stored as
  `session_limit`; their sockets close with 4001, `AfterSessionsRevoked` runs for each).
- `AuthService::revocations_after(state, at, session_id)` and `AuthService::REVOCATIONS_PAGE` (1000):
  the page after an entry of `revocations_since`, for reading any number of revocations.
- `database.statement_timeout_secs` (`NBS__DATABASE__STATEMENT_TIMEOUT_SECS`, default 0 = no limit):
  the database cancels a longer statement (PostgreSQL `statement_timeout`, MySQL `max_execution_time`
  for read-only `SELECT`s, MariaDB `max_statement_time`); migrations run without it on their own
  short-lived connections.
- `KeyedBuckets::refund` (and `refund_at`): give back a token taken for a key.

### Changed

- **Breaking: the default cargo feature is `postgres`** (it was `mysql`). `cargo add
  net_backend_server` now builds the PostgreSQL backend. A server on MySQL / MariaDB that relied on
  the default adds the feature: `cargo add net_backend_server --no-default-features --features
  mysql` (or `default-features = false, features = ["mysql", …]` in `Cargo.toml`). Without it, a
  `mysql://` URL is refused at start with a message naming the missing feature. A missing
  `database.url` stays an error (no fallback to another database).
- **Breaking: a server with `modules.auth.revocation_poll_secs = 0` and the WebSocket hub on
  (`ws.enabled = true`, the default) no longer starts**, and every command and `config check` refuse
  that configuration: set it to 1 or more (the default is 5). Without the poll, a ban or revocation
  made by the command line or another instance never closed open WebSockets. With `ws.enabled =
  false`, 0 is allowed.
- **Breaking:** `net_backend_protocol` 0.2.0 (re-exported as `net_backend_server::protocol`).
- **Breaking:** `asyncapi` is in `command::BUILT_IN_COMMANDS`: an app command with that name is
  refused (the built-in `asyncapi export` always ran instead of it).
- **SQLite runs with `synchronous = NORMAL`** (it was SQLite's default, FULL), in WAL mode as
  before: a commit no longer waits for the disk, so a SQLite server stores far more writes per
  second. A power loss or an operating-system crash can lose the last moments of writes; the file is
  never corrupted. The new setting `database.sqlite_synchronous = "normal" | "full"`
  (`NBS__DATABASE__SQLITE_SYNCHRONOUS`; ignored for MySQL and PostgreSQL) brings back FULL: every
  commit waits for the disk.
- SQLite's busy timeout (how long a write waits for another connection's write lock) is
  `database.acquire_timeout_secs` (default 5 s) instead of a fixed 5 s, so one setting decides how
  long a request waits before it fails.
- **Modules shut down at the same time**, all within one `server.module_shutdown_timeout_secs` (10 s)
  (before: one after the other in reverse order, each with its own limit). The `shutdown` calls still
  start in reverse registration order. The deployment files' `stop_grace_period` (Compose) and
  `TimeoutStopSec` (systemd) are 60 s (were 90 s): the 20 s shutdown grace, the modules' 10 s and the
  pool close, with room.
- `/v1/ws` answers the WebSocket upgrade itself (the same checks and answers as before) and reads the
  socket through a layer that takes every `auth` message out before tungstenite reads it.
- The chat module's background task runs whenever `purge_interval_secs` is above 0 (it also keeps
  player rooms owned); with `history_retention_days = 0` it deletes no messages, as before.
- The chat module's `max_group_members` is 500 by default (it was 200): the cap counts connections,
  and a group of 100 players has up to 5 each.
- **`serve` refuses to start when a published module's migrations lack migrations of this
  version** (the error names the module, the missing files and `migrations publish <module>`);
  `migrate` still warns only. Before, the server started on the old schema and answered 500.
- **`serve` refuses to start while a migration is pending** (embedded or published) and
  `database.migrate_on_start` is off: the error names each pending migration and says to run
  `migrate`. Before, the server started on the old schema and answered 500. A database that cannot
  be read at start (`connect_lazy`, down) is not checked (a WARN says so).
- `log.format = "pretty"` writes colours only when the output is a terminal (`docker logs`, journald and
  files get plain text).
- **The reference server `examples/server.rs` registers every module** (`Auth`, `Storage`, `Chat`,
  `Leaderboards`, `Notifications`, `Friends`, `Groups`, `OAuth`, `Lobbies`, `Matchmaking`, `Files`;
  `required-features` lists them all), and so do the Docker image's default build (`CARGO_ARGS`) and
  the deployment configurations: `deploy/config/config.toml`, the Compose files' `x-nbs-config` and
  the image's `trial.toml` have `[modules.leaderboards]` with a `highscore` board,
  `[modules.notifications]`, `[modules.friends]`, `[modules.groups]`, `[modules.lobbies]` and
  `[modules.matchmaking]` with a `duel` queue, `[modules.oauth]` (no provider) and `[modules.files]`.
  An existing deployment gets the new modules' tables with its next `migrate`; `/v1/info` lists the
  new modules.
- The deployment Caddyfiles (systemd and the Compose files' inline one) allow request bodies up to
  17 MB (were 5 MB), so a file upload of the files module's default `max_file_bytes` (16 MiB) passes,
  and also delete an `access_token` query parameter from the access log (the server accepts
  `?access_token=` as another name of `?token=` when `ws.query_token` is on).
- The README and the API documentation describe what the crate has and does.
- `GET /readyz` checks the database at most once per second: one check at a time, its answer reused
  for 1 s by every request. A failed check is logged as a WARN when the state changes (then DEBUG
  while it lasts), the recovery as INFO.
- Until a WebSocket authenticated, a frame or message may have at most 16 KiB (or
  `ws.max_message_bytes` if smaller; bigger: close 1009); after `auth.ok` the limit is
  `ws.max_message_bytes`.
- Pongs count against a socket's frame rate limit like text, binary and ping frames.
- `/v1/ws` answers 426 to an upgrade whose `Sec-WebSocket-Key` is not 16 bytes in base64.
- `ws.handshakes_per_ip_per_minute` is checked before the authenticators look at the handshake's
  token.
- Module settings from the environment (`NBS__MODULES__…`): a value is a TOML number or boolean only
  when it reads back as the same text (`1e5`, `0042`, `+5` and dates stay text); a string in TOML
  quotes is that string. A `SecretString` setting takes a number or boolean as its text.
- Login attempts count against the lockout limits when they start (given back when the password was
  right), and so do password-change attempts.
- A session's `last_used_at` update runs once per session and minute, however many requests of the
  session arrive at once.
- The revocation poll re-reads the last 30 s (was 2 s) for revocations whose transaction committed
  late.
- An incoming text message is parsed into a JSON document once (the `auth` check reads only the
  top-level `id` and `type`).

### Fixed

- Revocations (logout, password change, ban, admin, refresh-token reuse) are applied to the WebSocket
  hub before the call that made them returns, those of other processes as soon as the revocation poll
  reads them: a socket that authenticates after a ban returned never receives `auth.ok`.
- A ban that lands while a socket authenticates is answered `auth.failed` with the code `banned`
  (it was `unauthorized`), matching the close code 4003.
- MySQL: simultaneous registrations no longer deadlock on `auth_email_tokens`. A new email token
  replaces the unused ones through a plain read and a delete by id; the ranged delete before took gap
  locks on the user index.
- A token checked longer ago than the hub's memory of revocations (30 s; e.g. behind a slow connect
  hook), or before a revocation that memory had to drop (more than 4096 within 30 s), is checked again
  before its socket is registered, so a ban that landed meanwhile is answered `banned`, never `auth.ok`.
- MySQL: simultaneous resends of one account's verification or reset mail leave one unused token (the
  account row is locked first); a password reset deletes the account's unused reset tokens by id.
- Deployment files of the repository: two backups started at the same instant each keep their dump
  under their own name, and a run that fails after taking a name leaves no empty file; a PostgreSQL
  restore keeps psql's query results and notices out of the log (errors stay); after a PostgreSQL load
  that failed, the restore says that nothing was replaced and starts the server again; in Docker mode a
  `db` service that is not running is reported as such, Compose's progress lines stay out of the
  restore log, and `migrate` runs once (through `docker compose up`).
- The revocation poll reads revocations page by page within one poll: more than 1000 sessions revoked
  at the same instant (a logout everywhere, a ban or a command-line revoke of an account with many
  sessions) no longer stopped it for good; every such session and every later revocation reaches
  `subscribe_revocations` receivers and closes its sockets.
- A secret given by environment variable that looks like a number or boolean
  (`NBS__MODULES__AUTH__SMTP_PASSWORD=12345678`) is accepted as its text; it was refused, and the error
  printed the value.
- An upload handler that stops reading the body early has `http.request_timeout_secs` for the rest of
  its work (it stayed on the upload idle limit).

### Security

- The access token of a first-message `auth` no longer appears in tungstenite's TRACE `log` records
  (`Received message …`, `received frame …`), which hold every frame and message the server receives:
  tungstenite receives a stand-in text in place of each `auth` message. The README's WebSocket
  section ("Logs") says what tungstenite still logs.
- A frame the WebSocket layer refuses (reserved bits set, an unknown opcode, a frame out of sequence, an
  unmasked or fragmented control frame) reaches tungstenite as its header with an empty payload, and
  nothing after it: an `auth` sent in such a frame is not in tungstenite's TRACE records either.
- Configuration errors never show a configured value: numbers, booleans and enum variants that serde
  quotes in backticks are replaced by `<hidden>`, like strings before.
- `DbError::describe` replaces the value in MySQL's `Duplicate entry '…'` message by `<hidden>` (it
  named, for example, an email address).
- A socket waiting for `auth` holds a few KiB instead of up to several copies of
  `ws.max_message_bytes` (the 16 KiB limit before authentication).

### Upgrading from 0.1.0

- A server that runs the embedded migrations (nothing published): `migrate` (or
  `database.migrate_on_start`) adds the new tables and columns.
- A server that published the `storage` or `chat` migrations (`migrations/storage/`,
  `migrations/chat/`): run `migrations publish storage` and `migrations publish chat` (each adds
  only the new files and keeps your edits), then `migrate`, before serving. `serve` refuses to
  start until then.

### Deployment files: what changes for a 0.1.0 Docker install

- The Docker install is one Compose file per database (`deploy/docker/compose.postgres.yaml`,
  `compose.mysql.yaml`) with the prebuilt server image; the database and server secrets are generated
  into Docker volumes on the first start. `deploy/docker/setup.sh`, `compose.yaml`, `Caddyfile`,
  `.env.example` and `postgres-init.sql` are removed: run `docker compose down` in the old folder
  BEFORE updating the checkout, because the old files run that install.
- The backup and restore scripts read the database from the running `db` service (no
  `secrets/database_url` file); their default Compose folder (`NBS_COMPOSE_DIR`) is `/opt/net-backend`
  (it was `/opt/net_backend/deploy/docker`).
- `backup/install.sh --mode docker` needs `--compose-dir <the folder with compose.yaml and .env>`.
- "Moving from a `setup.sh` install" in
  [deploy/README.md](https://github.com/warmar94/net_backend/tree/main/deploy#path-a-docker-compose)
  lists the steps: the new folder, the old secrets copied into the new volumes, the configuration and
  published migrations carried over, and the backup timer re-installed.

## [0.1.0] - 2026-10-02

### Added

- `config check` builds the server without touching the database, so every registered module reads and
  checks its own `[modules.<name>]` section: a typo there fails the check instead of the next start.
- CORS (when `cors.allowed_origins` is set) allows the request headers `If-Match` / `If-None-Match` and
  exposes `ETag` / `Retry-After`, so pages on another origin can use conditional storage writes and read
  versions and wait times from the headers.
- The OpenAPI document describes a storage `value` as any JSON value (it was an object), as the protocol
  defines it.
- The reference server `examples/server.rs` (`Auth`, `Storage` and `Chat` from the configuration, plus a
  `healthcheck` command that asks `/readyz`) and the repository's deployment files: Docker Compose
  (distroless non-root image, a one-shot `migrate` service, MySQL 8.4 or PostgreSQL 16, Caddy) or a
  hardened systemd service (`ExecStartPre` migrations, `LimitNOFILE`, sandboxing), Caddy with HTTPS + WSS,
  a 5 MB body limit and token-free access logs, daily backups with a restore command, an SSH hardening
  guide and the `load_test` load generator.
- Typed routes (`http::call`): the extractor `Call<C>` (path parameters + JSON body or query of a
  protocol `HttpCall`, shape-checked), the answer `Reply<C>` / `CallResult<C>` (with headers), the
  handler marker `CallHandler`, `documented` / `undocumented` / `method_router`, the macro
  `call_route!` and the builder method `.call::<C, ..>(handler)`: a route is mounted at
  `C::ROUTE` with its method, and its OpenAPI path and method always come from the protocol. Every
  auth, account and admin route is mounted this way now.
- The storage module (feature `storage`, `storage::Storage`, settings `[modules.storage]` /
  `StorageConfig`): `/v1/storage` (list without values, get / put / delete with `ETag`, batch get /
  put in one transaction), optimistic versions (`if_version`, `If-Match: "N"`, `If-None-Match: *`,
  409 `version_conflict` with the current version and the failing batch index; exactly one winner
  among simultaneous conditional writes), the server write lock (`write: "server"`), the value
  size limit and per-route body limits, the per-user quota (exact under concurrent creates), hooks
  (`BeforeStorageWrite`, `InStorageWriteTx`, `AfterStorageWrite`, `BeforeStorageDelete`,
  `AfterStorageDelete`), audited admin access (`/v1/admin/users/{user}/storage/...`),
  `StorageService` for server code, migrations for the three databases, OpenAPI schemas.
- The chat module (feature `chat`, `chat::Chat`, settings `[modules.chat]` / `ChatConfig`,
  `RoomSpec`): public rooms from the settings, group rooms and DM rooms; the WebSocket kinds
  `chat.join` / `leave` / `send` / `history` / `members` and the pushes `chat.message` /
  `chat.deleted` / `chat.presence` (registered for the AsyncAPI document with schemas); the HTTP
  routes for rooms, history, DMs and message deletion; the answer before the sender's echo (with
  its `nonce`); member caps (exact under simultaneous joins), the per-user send rate, the text
  rules, the history retention with a purge task; presence per user with a cap and a per-room
  rate; moderation hooks (`BeforeChatJoin`, `BeforeChatSend`, `AfterChatSend`,
  `BeforeDirectOpen`, `AfterChatDelete`), sender / moderator deletion (audited); `ChatService` for
  server code (rooms, groups, members, `send_as`, `delete_message`, `online`, `purge`); migrations
  for the three databases.
- `in_tx` hooks: `Hooks::in_tx` / `run_in_tx` / `in_tx_count` and `NetBackendServer::in_tx` (inside a
  module's transaction; time limit, contained panics, the transaction rolls back on refusal).
- `Module::depends_on` (a module's required modules, registered before it; checked at build).
- `Db::begin_write` (a transaction that writes; SQLite `BEGIN IMMEDIATE`).
- `db::Retry`, `DbError::is_retryable`, `db::is_retryable_error` and `DbTx::finish`: a write
  transaction the database aborts as a deadlock (MySQL 1213, PostgreSQL `40P01` / `40001`, a busy
  SQLite) runs again from the start, bounded; every framework write transaction uses it.
- Storage: `StorageConfig::max_bytes_per_user` (4 MiB of values per user; only growth is checked),
  `write_rate` / `write_rate_window_secs` (60 owner writes per 60 s per user; 429), and
  `server_collections` (default `["server"]`: owners never create, change or delete objects there;
  new ones are server-locked). The quotas bind only the owner's writes. A batch item's failure
  carries its `index` also for a hook's refusal and a quota.
- Chat: `ChatConfig::dm_open_rate` / `dm_open_window_secs` (20 DM opens per 600 s per user; 429).
- Hub: `Control` deliveries (`Delivery::control`, `Control::LeaveRoom`) through the `Broadcaster`
  and `Hub::remove_from_room(user, room)`: that user's sockets leave the room on every instance.

- The WebSocket hub at `/v1/ws` (module `ws`, settings `[ws]` / `WsConfig`, on by default): the
  protocol's envelope (requests, answers, pushes; malformed frames with an id answered
  `bad_request`, `unknown_type` for unregistered kinds); authentication at the handshake through
  the app's authenticators (`Authorization: Bearer`, or `?token=`) or by first-message `auth` with
  exactly one `auth.ok` / `auth.failed` per `auth` and a deadline (close 1008); refused handshakes
  never answer 400 (401 / 401 `token_expired` / 403 `banned`, upgrade + close 4010 for an
  unsupported protocol version, 503 / 429 with `Retry-After`); open sockets survive token expiry and
  close on revocation (4001) or ban (4003), also when another process revoked; request handlers by
  kind for the game (`NetBackendServer::ws`, `ws_call`, `ws_handler`) and modules
  (`Module::ws_handlers`), typed through the protocol's `WsCall` or on JSON, with `WsCtx`
  (`AuthContext`, connection) and `KindDoc` documentation; pushes to a connection / user / room /
  everyone (`Hub`, `AppState::ws`, encoded once), rooms with member and per-connection caps; global
  and per-user connection caps (4009 replaced), per-address handshake and per-socket frame rate
  limits (`rate_limited`, close 1008), bounded outboxes (close 1013) and a write deadline,
  heartbeats with dead-peer detection, a message size limit (close 1009), tuned socket buffers
  (8 KiB read buffer, no write buffering; ~15 KiB heap per idle socket measured), hooks
  (`ws::events`: `BeforeWsConnect`, `AfterWsConnect`, `AfterWsDisconnect`, `BeforeWsFrame`), close
  1001 for every socket on shutdown within the grace period, metrics (`nbs_ws_*`), the `Broadcaster`
  seam (`LocalBroadcaster`) for several instances.
- WebSocket hub hardening (review and live-test round): request handlers run while the socket keeps writing
  pushes (only a handler's pushes to its own socket wait for its answer); pushes over `ws.max_message_bytes` are
  refused (`PushError::TooLarge`); a revocation or ban landing while a socket authenticates is applied; a temporary
  failure during first-message `auth` (5xx / 429) closes with 1013 without `auth.failed` (clients retry); open
  sockets get role changes (`AuthService::subscribe_role_changes`, `roles_of_users`, `ws.roles_refresh_secs`);
  `Target::Connection` pushes never leave the process and `Target` / `Delivery` / `ConnectionId` are serde types;
  `ws.max_connections_per_ip` (100, 429) and `ws.max_pending_connections` (1000, 503); the per-user cap closes the
  oldest socket of the same session first; an `auth` over the rate limit is still answered; unique AsyncAPI keys and
  the handshake statuses in the AsyncAPI description; `BeforeWsConnect::origin`; unsendable close codes become
  1011; `WsCtx::for_tests`; `HubStats::pending`; `ConnectionInfo::roles`; the registry is a `RwLock`;
  `request_timeout_secs < idle_timeout_secs - ping_interval_secs` is validated.
- The AsyncAPI 3.0 document of the WebSocket endpoint at `/v1/asyncapi.json` (`ASYNCAPI_PATH`,
  `PreparedServer::asyncapi_json`, command `asyncapi export`), generated from the registered kinds.
- The accounts module `Auth` (name `auth`, settings `[modules.auth]` / `AuthConfig`): the protocol's
  `/v1/auth/*` and `/v1/account*` routes (register, login, Steam login, refresh, logout, email
  verification and resend, password forgot / reset, account read / update, password change) and
  `/v1/admin/*` (list / get users, ban / unban, revoke sessions, grant / revoke roles, audit log).
  Passwords: argon2id (19 MiB, t=2, p=1 by default) on the blocking pool behind a concurrency
  limit, dummy hashing for unknown accounts, rehash on login. Tokens: opaque `nbsa_` / `nbsr_`
  tokens stored as SHA-256; 1 h / 30 days; refresh rotation with the 30-second grace window (the
  same pair, derived with HMAC-SHA-256 under a stored server key) and family revocation on later
  reuse; `token_expired` vs `unauthorized` vs `banned`. Sessions per login, revocation with
  `AuthService::subscribe_revocations` (close codes 4001 / 4003) and `authenticate_token` for the
  WebSocket hub. Case-insensitive unique emails on every database (normalised column). 24
  migrations per dialect (one statement each), publishable. Steam: the `SteamVerifier` trait,
  `FakeSteamVerifier`, `SteamWebApiVerifier` (feature `steam`). Mail: `mail::{Mailer, Mail,
  LogMailer, MemoryMailer}`, `SmtpMailer` (feature `smtp`), a bounded background queue. Roles
  (`RequireRole`, `RequireAdmin`, `AuthContext::require_role`), the audit log (`auth::audit`),
  rate limits and a failed-login lockout, hooks (`auth::events`), commands `user:create`,
  `user:role`, `user:ban`, `user:unban`, `sessions:revoke`, a periodic purge of expired rows.
- `rate_limit::{KeyedBuckets, MemoryRateLimiter, RateRule, RateScope}`: in-memory token buckets
  with bounded memory.
- `http.trusted_proxies` and the `ClientIp` extractor (the client address from `X-Forwarded-For`
  of trusted proxies only); rate limiters key on it.
- App commands: `command::{AppCommand, CommandCtx, CommandArgs}`, `NetBackendServer::command`,
  `Module::commands`; `--help` lists them.
- `Module::setup` (with `module::Setup`): modules register state values, authenticators and rate
  limiters at build time.
- Features `steam` and `smtp`.

- Security hardening of the accounts module (review round): strict `local@domain` emails (no
  display-name / angle-bracket forms) with Unicode NFC, mail sent to the bare validated address;
  NFC passwords; Steam linking needs a recent login (`link_reauth_secs`, 403
  `reauthentication_required`), one Steam account per account, the ban check and `BeforeLogin` hook
  before the link, an owner notification, unlink routes (`DELETE /v1/account/identities/{provider}`,
  `DELETE /v1/admin/users/{user}/identities/{provider}`), a password reset unlinks providers; the
  configured `steam_identity` (now required with any verifier) goes to Steam, accepted tickets are
  refused for 10 minutes, only Steam's documented success shape counts; a stale Bearer on
  `/v1/auth/steam` is refused (`MaybeAuth` extractor); the lockout counts per (address, client
  network) plus an account-wide ceiling that known networks bypass; IPv6 clients count by /64
  (`rate_limit_ipv6_prefix`); a random per-rotation nonce in the refresh derivation, forgotten after
  the grace window; failed logins do the same awaited work for known and unknown addresses (one
  query, background audit); banned accounts get 403 `banned` from tokens and refresh;
  revocations of other processes reach `subscribe_revocations` by a database poll
  (`revocation_poll_secs`; `AuthService::revocations_since`); indexes on the token tables'
  `session_id` and on session revocation / expiry (29 migrations); `audit_retention_days`; logout
  "everywhere" needs a current refresh token; a password change voids pending reset mails; last-admin
  and admin-ban guards; a warning when a local proxy's `X-Forwarded-For` is not trusted; the dummy
  hash is built at start; `AuthContext::session_started_at`, `rate_limit::{ip_key, RecentSet}`.

### Changed

- Path-parameter errors of the auth / account / admin routes come from the protocol's
  `HttpCall::from_parts`: an invalid role is 400 `bad_request` (as before) with a message naming
  the parameter; a provider name that is not a provider name (`DELETE
  /v1/account/identities/Steam`, also the admin route) is now 400 `bad_request` before any lookup
  (it answered 404 "no such linked login" before).
- Typed routes enforce `ROUTE.auth`: a route marked "token required" answers 401 (or the
  authenticator's 403) before its handler runs, also when the handler never takes an
  `AuthContext` (a game's own `HttpCall` included). A route with a method this version cannot
  serve answers 405, and `NetBackendServer::call` refuses it at build.
- Storage: every write and delete takes the owner's account lock, reads the object, then updates
  the existing row or inserts (no statement on an absent row): first saves of different players
  no longer deadlock on MySQL; conditional writes take the lock too. PostgreSQL locks the account
  `FOR NO KEY UPDATE`.
- Chat: a DM send updates the room before inserting the message (concurrent DM sends in one room
  no longer deadlock on MySQL). `chat.members` on a DM room lists only the caller (a DM never
  reveals whether the peer is online). A removed group member is refused at once on every
  instance (the membership is checked on every group send and re-checked after a join) and leaves
  the room everywhere. A DM's push and the history show the `nonce` to its sender only. Room and
  group names are validated (1-64 characters, no invisible characters); group members are
  deduplicated, an unknown one is 404. Cached rooms refresh after 60 s. The retention purge
  deletes in batches of 1000. The join looks up the display name before joining the hub room.
- Tests wait for conditions with generous upper bounds instead of tight fixed timeouts (a CI
  runner raced a push against the socket's registration).

- `ws.query_token` defaults to `false` (reverse proxies log URLs with their query). `Hub::publish` returns
  `Result<(), PushError>`.
- `/v1/ws` is the WebSocket hub (it answered 403 while reserved; with `ws.enabled = false` it
  still does). `AppState` holds the hub.
- Several authenticators and rate limiters may be installed (asked in order). An authenticator's
  error no longer answers the request at once: the request continues anonymously, and handlers
  that need a user (`AuthContext`) answer that error; `Option<AuthContext>` sees `None`.
- `AuthContext` has `session_id` and `roles`.

- The app builder `NetBackendServer`: `new(config)`, `module`, `route` / `routes` (documented) /
  `nest` / `merge`, `state`, `before` / `after` / `on_start` / `on_shutdown` hooks, `clock`, `db`,
  `authenticator`, `rate_limiter`, `publish_migrations`, `build` → `PreparedServer` (`router`,
  `migrate`, `migration_status`, `serve`, `serve_with_shutdown`), `run` / `run_with_args` /
  `run_with_output` (the command line).
- The `Module` trait (name, migrations per dialect, routes, OpenAPI parts, hooks, `start`,
  `shutdown`; every method but the name has a default), module name rules, deterministic registration
  order.
- Hooks: typed `before` (continue, modify, reject) and `after` hooks per `Event` type, start and
  shutdown hooks, a time limit per call (503 `hook_timeout`), contained panics.
- `AppState` (configuration, database, clock, hooks, the app's own state values, the shutdown
  signal), `Clock` with `SystemClock` and `ManualClock`, the extractors `ApiJson`, `Ext`,
  `RequestId`, `AuthContext`.
- `Config`: TOML file + `NBS__SECTION__KEY` environment overrides + `database.url_file`, strict
  keys, validation that reports every problem, `SecretString` (redacted `Debug`), per-module
  sections.
- Errors: `AppError` (the protocol's error body and status table; internal causes logged with the
  request id, never sent) and the framework `Error`.
- Databases: features `mysql` (default), `postgres`, `sqlite`, additive; the `Db` / `DbTx` enums
  over the sqlx pools with sea-query statements (`execute`, `fetch_all`, `fetch_optional`,
  `fetch_one`, `insert_id`, `execute_script`, transactions with the same methods), `check_ready`,
  `Dialect`, SQLite files switched to WAL once at connect (safe for parallel starts on a new file),
  portable column helpers (`db::schema`; MySQL tables `utf8mb4_bin`, case-sensitive),
  `DbError` with unique / foreign-key checks and `describe()`; unsigned values above `i64::MAX` are
  refused. TLS through rustls with ring.
- Migrations: plain SQL per dialect, ordered by module then version, the `nbs_migrations`
  tracking table with line-ending-independent SHA-256 checksums, safe concurrent runs (MySQL
  `GET_LOCK` per database and PostgreSQL advisory lock, both with `database.migrate_lock_timeout_secs`;
  SQLite `BEGIN IMMEDIATE`; a re-check of the tracking row everywhere), MySQL migrations run statement
  by statement with a precise recovery message on failure (every statement that ran counts once a
  DDL statement committed), `-- nbs:no-transaction`, `-- nbs:single-statement` and
  `-- nbs:statement-begin` / `-- nbs:statement-end` for MySQL `BEGIN … END` bodies, a read-only
  status (applied, pending, modified, missing), publishing a module's SQL into the app (the app copy
  then wins; an existing file, also a non-UTF-8 one, is never overwritten without `force`).
- HTTP: `GET /healthz`, `GET /readyz`, `GET /v1/info` (the protocol's `ServerInfo`), the protocol
  version header on every answer and the version check (400 `unsupported_protocol` on plain
  routes), `/v1/ws` reserved (403, never 400), 64 KiB default body limit, per-route limits and a
  32 MiB hard cap, a header-read timeout (slow clients, idle keep-alive), request ids, request
  tracing without query strings, request timeouts (503), panic safety (500 without details),
  error-body normalising with the protocol's statuses, optional CORS.
- OpenAPI at `/v1/openapi.json` (utoipa 6), an optional `/v1/docs` page (a pinned third-party
  script with Subresource Integrity and a Content-Security-Policy); optional Prometheus metrics at
  `/metrics` on their own listener (`metrics.bind`, loopback by default).
- The command line: `serve`, `migrate [up|status]`, `migrations publish <module> [--dialect]
  [--force]`, `config check [--connect]`, `openapi export [--output]`.
- Graceful shutdown on SIGTERM / Ctrl-C with an enforced grace deadline (remaining connections and
  their handlers are dropped), module start / shutdown with time limits and contained panics,
  module shutdown in reverse order, shutdown hooks, pool closing.
- The authentication seam (`Authenticator`) and the rate-limit seam (`RateLimiter`, asked before
  and after authentication).
- Configuration: module sections print only their key names in `Debug`, errors never quote
  configured values, unknown `[modules.*]` sections are refused, `config::resolve_secret` for
  `<name>` / `<name>_file` secrets.
- Dependencies use caret minimums (no exact pins), so apps can use newer compatible axum, sqlx and
  utoipa releases.
- The example `minimal` (a headless game server with one custom route).
