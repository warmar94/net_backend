# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a key dependency bump).

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
