# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a key dependency bump).

## [0.2.0] - 2026-10-04

### Added

- `leaderboards`: boards (`BoardInfo` with `ScoreMode` best / latest / sum, `ScoreOrder` desc /
  asc, `Period` all-time / daily / weekly with `Period::start_of` / `end_of`, resets at 00:00 UTC,
  weeks on Monday), `SubmitScore` / `ScoreAck`, `LeaderboardEntry`, `LeaderboardPage`, `MyRank`,
  the queries `TopQuery`, `RankQuery`, `AroundQuery`, `is_valid_board_key`; the `HttpCall` types
  `ListBoards`, `GetLeaderboard`, `PostScore`, `GetMyRank`, `GetAroundMe`; the routes
  `routes::leaderboards` (`/v1/leaderboards`, `/{board}`, `/{board}/scores`, `/{board}/me`,
  `/{board}/around`) in `routes::ALL`.
- `notifications`: `Notification` (also the `notify.new` push), `NotificationQuery`
  (`notify.list`, `GET /v1/notifications`), `CountNotifications` / `NotificationCount`
  (`notify.count`, `GET /v1/notifications/count`), `MarkNotifications` / `MarkAck` (`notify.mark`,
  `POST /v1/notifications/mark`), `DeleteNotification` (`notify.delete`,
  `DELETE /v1/notifications/{id}`), `is_valid_kind`, `text_problem`; the id type `NotificationId`;
  the kinds `kinds::NOTIFY_*` in `kinds::ALL`; the routes `routes::notifications` in `routes::ALL`.
  With the feature `bevy_net_backend` the four requests implement `WsRequest` and `Notification`
  implements `WsPushMessage`.
- `friends`: `FriendEntry` / `FriendState` (friend, sent, received, blocked), `AddFriend` (by
  account id, display name or friend code), `RequestQuery` / `RequestDirection`, `FriendCode`,
  `FriendPresence` (the `friends.presence` push, `kinds::FRIENDS_PRESENCE` in `kinds::ALL`),
  `normalize_friend_code`, `FRIEND_CODE_ALPHABET`; the `HttpCall` types `ListFriends`,
  `RemoveFriend`, `ListFriendRequests`, `AddFriend`, `CancelFriendRequest`, `AcceptFriend`,
  `DeclineFriend`, `ListBlocks`, `BlockUser`, `UnblockUser`, `GetFriendCode`, `ResetFriendCode`,
  `FriendsHeartbeat`; the routes `routes::friends` in `routes::ALL`. With the feature
  `bevy_net_backend` `FriendPresence` implements `WsPushMessage`.
- `friends`, Steam IDs: `SteamMatch` (`POST /v1/friends/steam`: SteamID64s as decimal strings,
  `validate`, `ids`) → `SteamMatchResult` (`SteamPlayer`: `steam_id`, `user`, `name`, `state`),
  `parse_steam_id`, `is_individual_steam_id`, `MAX_STEAM_IDS` (2000); the settings `FriendSettings`
  (`steam_findable`, true by default), `GetFriendSettings` and `UpdateFriendSettings`
  (`GET` / `PUT /v1/friends/settings`); `routes::friends::STEAM` and `SETTINGS` in `routes::ALL`.
- `groups`: `GroupInfo`, `GroupRole` (owner, admin, member), `GroupMember`, `GroupInvite`,
  `GroupList`, `CreateGroup`, `UpdateGroup` (a present `"metadata":null` clears the metadata),
  `GroupQuery`, `Invitee`, `RoleChange`, `group_name_problem`, `description_problem`; the
  `HttpCall` types `ListGroups`, `CreateGroup`, `MyGroups`, `ListGroupInvites`, `GetGroup`,
  `EditGroup`, `DeleteGroup`, `ListGroupMembers`, `JoinGroup`, `LeaveGroup`, `InviteToGroup`,
  `AcceptGroupInvite`, `DeclineGroupInvite`, `RevokeGroupInvite`, `KickMember`, `SetMemberRole`,
  `TransferGroup`; the routes `routes::groups` in `routes::ALL`; the id type `GroupId`.
- `oauth`: OpenID Connect logins: `OAuthToken` (an identity provider's ID token and the nonce of
  the sign-in; `validate`, `ID_TOKEN_MAX_BYTES`, `NONCE_MAX_BYTES`) and the `HttpCall` `OAuthLogin`
  (`POST /v1/auth/oauth/{provider}` → `AuthSession`); the route `routes::auth::OAUTH` in
  `routes::ALL`, `routes::oauth_login_path`; the error code `codes::OAUTH_FAILED`
  (`oauth_failed`, 401).
- `files`: players' binary files: `FileInfo`, `FileVisibility` (private, public, friends, shared),
  `FileMeta` (the upload's JSON part), `UpdateFile` (a present `"metadata":null` clears),
  `FileQuery`, `FileUsage`, `name_problem`, `is_valid_content_type`, `is_sha256_hex`, the part names
  `UPLOAD_META_PART` / `UPLOAD_FILE_PART`; the `HttpCall` types `ListFiles`, `GetFileUsage`,
  `GetFile`, `EditFile`, `DeleteFile`; the routes `routes::files` (in `routes::ALL`) and
  `routes::BINARY` (the upload and the download: no JSON body or answer), `routes::file_path`,
  `routes::file_content_path`; the id type `FileId`.
- `storage`: `ObjectVisibility` (private, public, friends); `PutObject::visibility` /
  `with_visibility`, `StorageObject::visibility`, `StorageObjectInfo::visibility`,
  `AdminPutObject::visibility`; the `HttpCall` types `GetPlayerObject`
  (`GET /v1/users/{user}/storage/{collection}/{key}`) and `ListPlayerObjects`
  (`GET /v1/users/{user}/storage/{collection}`), in `routes::ALL`.
- `lobbies`: `LobbyInfo`, `LobbyVisibility` (public, private, friends), `LobbyState` (open,
  in_game, closed), `LobbyMember`, `LobbyList`, `LobbyCode` (the join code: `parse`, `grouped`, and
  its number below 2^40: `to_u64` / `from_u64`; `LobbyInfo` carries `code` and `code_number`),
  `CreateLobby`, `UpdateLobby` (metadata keys set or removed with `null`), `SetReady`, `LobbyPlayer`,
  `JoinLobbyByCode` (`from_number`), `LobbySearch` / `LobbyFilter`, `is_valid_meta_key`,
  `meta_value_problem`; the pushes `LobbyMemberUpdate` (`lobby.member`, `MemberChange`) and
  `LobbyUpdate` (`lobby.changed`, `LobbyChange`); the `HttpCall` types `CreateLobby`, `MyLobbies`,
  `LobbySearch`, `JoinLobbyByCode`, `GetLobby`, `EditLobby`, `JoinLobby`, `LeaveLobby`,
  `SetLobbyReady`, `NewLobbyCode`, `TransferLobby`, `KickFromLobby`; the routes `routes::lobbies` in
  `routes::ALL`; the id type `LobbyId`.
- `matchmaking`: `QueueInfo`, `Queues`, `CreateTicket`, `MatchTicket` (`TicketStatus`),
  `is_valid_queue_key`; the pushes `MatchFound` (`match.found`) and `TicketExpired`
  (`match.expired`); the `HttpCall` types `ListQueues`, `CreateTicket`, `GetTicket`,
  `CancelTicket`; the routes `routes::matchmaking` in `routes::ALL`; the id type `TicketId`.
- The kinds `lobby.member`, `lobby.changed`, `match.found` and `match.expired` in `kinds::ALL`. With
  the feature `bevy_net_backend`, `LobbyMemberUpdate`, `LobbyUpdate`, `MatchFound` and
  `TicketExpired` implement `WsPushMessage`.
- Chat extras in `chat`: `EditMessage` (`chat.edit`, and `PATCH` on the message path with a
  `MessageEdit` body) and the push `MessageEdited` (`chat.edited`); `ChatMessage::edited_at`;
  `MarkRead` (`chat.mark_read`, and `PUT /v1/chat/rooms/{room}/read` with a `ReadUpTo` body), the push
  `ReadReceipt` (`chat.read`), `ListReceipts` / `ReadReceipts` (`chat.receipts` and its `GET`),
  `UnreadQuery` / `UnreadCounts` / `UnreadCount` (`chat.unread` and `POST /v1/chat/unread`);
  `SetTyping` (`chat.set_typing`) and the push `TypingUpdate` (`chat.typing`); rooms created by
  players: `RoomKind::Player`, `RoomVisibility`, `RoomRole`, `RoomInfo::visibility` / `owner` /
  `role`, `CreateRoom`, `UpdateRoom`, `RoomUser`, `RoomRoleChange`, `RoomMembership`, the push
  `RoomUpdate` (`chat.room`, `RoomChange`) and the `HttpCall` types `MyRooms`, `PublicRooms`,
  `GetRoom`, `EditRoom`, `DeleteRoom`, `JoinChatRoom`, `LeaveChatRoom`, `ListRoomMembers`,
  `InviteToRoom`, `KickFromRoom`, `SetRoomRole`, `TransferRoom`; `room_name_problem` and the
  constants `DEFAULT_EDIT_WINDOW_SECS`, `MAX_LISTED_RECEIPTS`, `MAX_UNREAD_ROOMS`,
  `MAX_UNREAD_COUNT`, `DEFAULT_TYPING_TTL_MS`, `DEFAULT_TYPING_INTERVAL_MS`, `MAX_ROOM_NAME_CHARS`.
  New routes in `routes::chat` (`ROOMS_MINE`, `ROOMS_PUBLIC`, `ROOM`, `ROOM_JOIN`, `ROOM_LEAVE`,
  `ROOM_MEMBERS`, `ROOM_INVITES`, `ROOM_MEMBER`, `ROOM_ROLE`, `ROOM_OWNER`, `READ`, `RECEIPTS`,
  `UNREAD`; `ROOMS` also takes POST, `MESSAGE` also PATCH) and in `routes::ALL`; the kinds
  `chat.edit`, `chat.edited`, `chat.mark_read`, `chat.read`, `chat.receipts`, `chat.unread`,
  `chat.set_typing`, `chat.typing`, `chat.room` in `kinds::ALL`. With the feature
  `bevy_net_backend`, `EditMessage`, `MarkRead`, `ListReceipts`, `UnreadQuery` and `SetTyping`
  implement `WsRequest`; `MessageEdited`, `ReadReceipt`, `TypingUpdate` and `RoomUpdate` implement
  `WsPushMessage`.
- Feature `bevy_net_backend`: the module `bevy` with typed HTTP calls through that client.
  `bevy::request(&call)` turns any `HttpCall` (every route of `routes::ALL`, or a game's own) into
  its `OutgoingRequest` as `net_backend_client` sends it (method, path, JSON body or query string,
  `accept: application/json`, the protocol header; without the game's credentials on routes that
  need no token, logout excepted: it takes a Bearer token instead of the refresh token; a path
  parameter that would need escaping answered `InvalidRequest`, never sent);
  `bevy::HttpClientCalls::call(&call)` sends it with the answer decoded as `C::Response`;
  `bevy::api_error(&error)` reads the protocol's `ApiError` out of a refused answer.

### Changed

- **Breaking:** the feature `bevy_net_backend` uses `bevy_net_backend` 0.2.0 (the `WsRequest`,
  `WsPushMessage` and `Credentials` implementations are for that version).
- The README and the API documentation describe what the crate has and does.

### Security

- `Password`, `AccessToken`, `RefreshToken` and `Secret` overwrite their whole allocation with
  zeros when they are dropped (new dependency `zeroize`, no default features); each clone is wiped
  on its own. `into_inner` hands the `String` over unwiped.

## [0.1.0] - 2026-10-02

### Added

- `envelope`: the WebSocket frames compatible with `bevy_net_backend` 0.1.0's `JsonEnvelope`:
  `WsRequestFrame`, `WsResponseFrame`, `WsPushFrame` (typed or untyped payloads; `checked` refuses
  the reserved auth kinds), first-message `WsAuth` and `WsAuthOk`, the decoders `WsServerFrame` /
  `WsClientFrame` (the client's classification rules; a malformed request keeps its id in
  `FrameError` so it can be answered `bad_request`), `CloseCode` (1000, 1001, 1008, 1009, 1011,
  1013, 4001, 4003, 4009, 4010; `is_permanent` for 4000–4099), the traits `WsCall` and
  `ServerPush`, `Ack`, `MAX_MESSAGE_BYTES`, `AUTH_TIMEOUT_SECS`. Documented rules: every `auth`
  message gets exactly one `auth.ok` / `auth.failed`; a later `auth` re-authenticates; open
  sockets survive access-token expiry.
- `error`: `ApiError` (`code`, `message`, `details`), `ErrorBody` (`{"error":…}`), the stable
  `codes` (incl. `token_expired`, `method_not_allowed` 405, `unsupported_media_type` 415), `http_status_for`, `ValidationDetails`.
- `ids` (`UserId`, `RoomId`, `MessageId`), `time` (`UnixMillis`), `page` (`Cursor`,
  `PageRequest`, `Page<T>`, `DEFAULT_PAGE_LIMIT`, `MAX_PAGE_LIMIT`, `MAX_CURSOR_BYTES`).
- `auth`: register, login, Steam login (tickets up to 8192 hex characters), refresh (with a
  30-second reuse grace window), logout (by access token or refresh token), account read /
  update, password change, email verification, password reset; `TokenPair`, `AuthSession`,
  `Account`, `LinkedIdentity`; the redacted secret types `Password`, `AccessToken`,
  `RefreshToken`, `Secret` (decode errors never quote them); shared `validate()` rules and limits.
- `auth::is_valid_email` (a plain `local@domain` addr-spec subset; `validate()` uses it: display names,
  angle brackets, comments, quoted local parts, address literals are refused), `EMAIL_LOCAL_MAX_BYTES`;
  routes `account::IDENTITY` (DELETE, unlink a provider) and `admin::IDENTITY` with `account_identity_path` /
  `admin_identity_path`; code `reauthentication_required` (403).
- `admin`: the operator routes' types: `AdminUser` (account + `BanInfo` + last seen + open sessions),
  `UserListQuery`, `BanRequest`, `AuditEntry`, `AuditQuery`, `ADMIN_ROLE`, `is_valid_role`, limits; routes
  `routes::admin` (`USERS`, `USER`, `BAN`, `UNBAN`, `SESSIONS`, `ROLE`, `AUDIT`) in `routes::ALL`, and the path
  helpers `admin_user_path`, `admin_ban_path`, `admin_unban_path`, `admin_sessions_path`, `admin_role_path`.
- `storage`: `PutObject` (`if_version`), `DeleteObject`, `StorageObject`, `StorageObjectInfo`
  (listings without values), `ObjectAck`, `ObjectVersion` (optimistic concurrency, `ETag`),
  `VersionConflict` details (with the failing batch index), `WriteAccess`, batch get / put (16
  distinct objects, 4 MiB of values), `is_valid_name`, body-limit constants.
- `chat`: `JoinRoom`, `LeaveRoom`, `SendMessage` (optional `nonce`), `SendAck`, `ChatHistory`,
  `ChatMessage`, `MessageDeleted`, `RoomRef`, `RoomInfo` (with the DM `peer`), `RoomKind`,
  `OpenDirect`, `is_valid_room_key` and the default limits. Direct messages need no join.
- `text`: the character rules behind `validate()` (control, invisible and direction-changing
  characters).
- `kinds` (every WebSocket `type`, `is_reserved`) and `routes` (every `/v1` HTTP path, the route
  table `ALL`, `Route::new`, path helpers, `DEFAULT_BODY_LIMIT_BYTES`).
- `version`: `PROTOCOL_VERSION`, `PROTOCOL_HEADER`, `ServerInfo`, and how an unsupported version is
  refused (`HTTP_REFUSAL_STATUS`; on the WebSocket `WS_REFUSAL_CLOSE` 4010 or
  `WS_HANDSHAKE_REFUSAL_STATUS` 403, never 400).
- Feature `bevy_net_backend`: `WsRequest` / `WsPushMessage` for the chat messages and
  `Credentials` for `AccessToken`.
- Example `print_frames`.
- `http_call`: the `HttpCall` trait (route = method + path template + auth, `PayloadKind` = JSON
  body / query / nothing, `Response` type, `path()`, `path_params()`, `from_parts()`), `PathParams`
  (fill a template; ids and names checked, `bad_request`), `NoPayload`, `is_path_safe`,
  `placeholders`; re-exported at the root. Every route of `routes::ALL` has exactly one
  `HttpCall` type: the body / query types themselves (`LoginRequest`, `BatchPut`, `UserListQuery`,
  …) and new call types for routes with path parameters or without a payload (`GetServerInfo`,
  `auth::{ResendVerification, GetAccount, UnlinkIdentity}`, `storage::{ListObjects, GetObject,
  WriteObject, RemoveObject}`, `chat::{ListRooms, ListMessages, ListDirects, DeleteMessage}`,
  `admin::{GetUser, BanUser, UnbanUser, RevokeSessions, UnlinkUserIdentity, GrantRole,
  RevokeRole}`).
- Chat presence: the request `chat.members` (`ListMembers` → `RoomMembers` / `RoomMember`,
  `MAX_LISTED_MEMBERS`) and the push `chat.presence` (`Presence`, `PresenceEvent`), the defaults
  `DEFAULT_PRESENCE_MAX_MEMBERS` / `DEFAULT_PRESENCE_PER_SECOND`; kinds `CHAT_MEMBERS`,
  `CHAT_PRESENCE`; the feature `bevy_net_backend` implements the client traits for them.
- Message deletion: `routes::chat::MESSAGE` (`DELETE /v1/chat/rooms/{room}/messages/{message}`),
  `DeleteMessage`, `routes::chat_message_path`.
- A user's storage for operators: `routes::admin::USER_STORAGE` / `USER_OBJECT`,
  `admin::{ListUserObjects, GetUserObject, WriteUserObject, RemoveUserObject, AdminPutObject}`,
  `routes::admin_storage_path` / `admin_object_path`; `auth::is_valid_provider`.
- `http_call::query_pairs`: a `Query` payload as name / value pairs (absent fields left out, never
  `null`).
- `text::{is_tag, is_variation_selector, is_combining_mark, MAX_COMBINING_RUN}`.

### Changed

- `is_path_safe` admits only RFC 3986 path characters that need no escaping (letters, digits,
  `- . _ ~ ! $ & ' ( ) * + , ; = : @`); `"`, `<`, `>`, `\`, `^`, the backtick, `{`, `|`, `}` are no
  longer path-safe.
- Text rules: the Hangul fillers (U+115F, U+1160, U+3164, U+FFA0), the braille blank (U+2800) and
  the combining grapheme joiner (U+034F) count as invisible; names refuse tag characters; a chat
  text must show something (not only spaces, joiners, tags, variation selectors or combining marks)
  and may stack at most 8 combining marks.
- Docs: the chat send rate is a token bucket (a burst of 5, then one every 2 s); `chat.members` on a
  DM room lists only the caller; a DM's push and the history show the `nonce` to the sender only;
  the storage `ETag` comes with GET and PUT answers (not DELETE).
