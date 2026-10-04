# net_backend_protocol

<p>
  <a href="https://crates.io/crates/net_backend_protocol"><img alt="crates.io" src="https://img.shields.io/crates/v/net_backend_protocol.svg"></a>
  <a href="https://docs.rs/net_backend_protocol"><img alt="docs.rs" src="https://img.shields.io/docsrs/net_backend_protocol"></a>
  <a href="https://net-backend.com"><img alt="Website: net-backend.com" src="https://img.shields.io/badge/website-net--backend.com-informational"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.95+" src="https://img.shields.io/badge/rust-1.95%2B-orange">
</p>

Shared message types for [`net_backend_server`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_server):
plain Rust + serde, usable from any Rust client.

The server and its clients import the same types, so both sides agree on every request, answer
and push message, and on the exact JSON they become. A mismatch is a compile error instead of a
runtime surprise. The crate contains only data types and pure helpers (serde + serde_json, and
zeroize to wipe the secret types); a client library or the server sends them. The JSON on the
wire is the real contract: clients in other languages speak it directly; this crate is the
convenience for Rust and the single source of truth.

## How clients use this

This crate defines **what** is said; a client library decides **how** it is sent.

| Your client is… | Use |
|---|---|
| a **Rust** app | this crate + [`net_backend_client`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_client) for the connection. |
| a **Bevy** game | this crate + [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) for the connection (with the optional `bevy_net_backend` feature, the types plug straight into its WebSocket requests, and every HTTP route is a typed call there too). |
| **other** Rust code | this crate + any HTTP / WebSocket library (for example reqwest, ureq, tokio-tungstenite). |
| **not Rust** | the HTTP / WebSocket API directly: [API.md](https://github.com/warmar94/net_backend/blob/main/API.md) (plus the server's OpenAPI and AsyncAPI documents), with the same JSON. |

## Contents

- [How clients use this](#how-clients-use-this)
- [What is inside](#what-is-inside)
- [Features](#features)
- [Install](#install)
- [Quick start: a client](#quick-start-a-client)
- [Quick start: a server](#quick-start-a-server)
- [The WebSocket envelope](#the-websocket-envelope)
- [HTTP routes](#http-routes)
- [Typed HTTP calls (HttpCall)](#typed-http-calls-httpcall)
- [Errors](#errors)
- [Accounts and sessions](#accounts-and-sessions)
- [Storage (saves)](#storage-saves)
- [Files](#files)
- [Chat](#chat)
- [Leaderboards](#leaderboards)
- [Notifications](#notifications)
- [Friends](#friends)
- [Groups](#groups)
- [Lobbies](#lobbies)
- [Matchmaking](#matchmaking)
- [Ids, timestamps, pages](#ids-timestamps-pages)
- [Versioning and compatibility](#versioning-and-compatibility)
- [Optional integration with bevy_net_backend](#optional-integration-with-bevy_net_backend)
- [API design rules](#api-design-rules)
- [Testing](#testing)
- [FAQ](#faq)
- [License](#license)
- [Contributing](#contributing)

## What is inside

| Module | What |
|---|---|
| `envelope` | The WebSocket frames: `WsRequestFrame`, `WsResponseFrame`, `WsPushFrame`, first-message `WsAuth`, `WsAuthOk`, the decoders `WsServerFrame` / `WsClientFrame`, `CloseCode`, the traits `WsCall` (request kind → answer type) and `ServerPush`, `Ack`. |
| `error` | `ApiError { code, message, details }`, the HTTP body `ErrorBody`, the stable `codes`, `ValidationDetails`, `http_status_for`. |
| `ids`, `time`, `page` | `UserId`, `RoomId`, `MessageId`, `NotificationId`, `GroupId`, `LobbyId`, `TicketId`, `FileId` (`i64`), `UnixMillis` (`i64` milliseconds), cursor pagination (`PageRequest`, `Page<T>`, `Cursor`). |
| `auth` | Register, login, Steam login, refresh, logout, account, email verification, password reset; `TokenPair`; redacted `Password`, `AccessToken`, `RefreshToken`, `Secret`. |
| `admin` | Operator routes: list / inspect accounts (`AdminUser`), ban (`BanRequest`, `BanInfo`), revoke sessions, roles, the audit log (`AuditEntry`, `AuditQuery`), a user's storage (`AdminPutObject`). |
| `storage` | Per-user key-value objects (save slots): put / get / list / delete / batch with optimistic versions; a visibility (`ObjectVisibility`: private / public / friends) and reads of other players' objects (`GetPlayerObject`, `ListPlayerObjects`). |
| `files` | Players' binary files: `FileInfo`, `FileVisibility` (private / public / friends / shared), `FileMeta` (the upload's JSON part), `UpdateFile`, `FileQuery`, `FileUsage`, the calls `ListFiles`, `GetFileUsage`, `GetFile`, `EditFile`, `DeleteFile`; the part names `UPLOAD_META_PART` / `UPLOAD_FILE_PART`, `name_problem`, `is_valid_content_type`, `is_sha256_hex`. |
| `chat` | Rooms, direct messages, join / leave / send / history / members, the pushes `chat.message`, `chat.deleted` and `chat.presence`, message deletion; message editing (`EditMessage`, `chat.edited`), read markers and unread counts (`MarkRead`, `ListReceipts`, `UnreadQuery`, `chat.read`), typing (`SetTyping`, `chat.typing`), rooms created by players (`CreateRoom`, `RoomRole`, `RoomVisibility`, the room calls, `chat.room`). |
| `leaderboards` | Boards (`BoardInfo`: `ScoreMode`, `ScoreOrder`, `Period` with its reset times), `SubmitScore` / `ScoreAck`, the top (`LeaderboardPage`), the caller's rank (`MyRank`) and the ranks around it. |
| `notifications` | `Notification` (also the `notify.new` push), `NotificationQuery`, `CountNotifications` / `NotificationCount`, `MarkNotifications` / `MarkAck`, `DeleteNotification`: each a WebSocket request and an HTTP call. |
| `groups` | `GroupInfo`, `GroupRole`, `GroupMember`, `GroupInvite`, `GroupList`, `CreateGroup`, `UpdateGroup`, `GroupQuery`, `Invitee`, `RoleChange` and the calls (`ListGroups`, `MyGroups`, `GetGroup`, `EditGroup`, `DeleteGroup`, `ListGroupMembers`, `JoinGroup`, `LeaveGroup`, `InviteToGroup`, `AcceptGroupInvite`, `DeclineGroupInvite`, `RevokeGroupInvite`, `KickMember`, `SetMemberRole`, `TransferGroup`). |
| `lobbies` | `LobbyInfo` (`LobbyVisibility`, `LobbyState`, `LobbyMember`), `LobbyCode` (text and number forms), `CreateLobby`, `UpdateLobby`, `LobbySearch` (`LobbyFilter`), `SetReady`, `LobbyPlayer`, `JoinLobbyByCode`, the pushes `LobbyMemberUpdate` / `LobbyUpdate` and the calls (`MyLobbies`, `GetLobby`, `EditLobby`, `JoinLobby`, `LeaveLobby`, `SetLobbyReady`, `NewLobbyCode`, `TransferLobby`, `KickFromLobby`). |
| `matchmaking` | `QueueInfo` / `Queues`, `CreateTicket`, `MatchTicket` (`TicketStatus`), the pushes `MatchFound` / `TicketExpired` and the calls (`ListQueues`, `GetTicket`, `CancelTicket`). |
| `friends` | `FriendEntry` (`FriendState`), `AddFriend` (by id, name or code), `AcceptFriend`, `DeclineFriend`, `CancelFriendRequest`, `RemoveFriend`, `BlockUser` / `UnblockUser`, the lists (`ListFriends`, `ListFriendRequests`, `ListBlocks`), `FriendCode`, `FriendsHeartbeat`, the `friends.presence` push (`FriendPresence`), `normalize_friend_code`; the Steam ID lookup `SteamMatch` → `SteamMatchResult` (`SteamPlayer`), `parse_steam_id`, `is_individual_steam_id`, the settings `GetFriendSettings` / `UpdateFriendSettings` → `FriendSettings`. |
| `oauth` | OpenID Connect logins: `OAuthToken` (an identity provider's ID token + the nonce of the sign-in) and the call `OAuthLogin` (`POST /v1/auth/oauth/{provider}` → `AuthSession`). |
| `http_call` | `HttpCall` (route + payload + answer type of every HTTP route), `PathParams`, `PayloadKind`, `NoPayload`. |
| `text` | The character rules behind `validate()`: control, invisible and direction-changing characters. |
| `kinds`, `routes` | Every WebSocket `type` and every `/v1` HTTP path as constants (plus a route table). |
| `version` | `PROTOCOL_VERSION`, `PROTOCOL_HEADER`, `ServerInfo`. |

## Features

| Feature | Default | What |
|---|---|---|
| (none) | yes | serde, serde_json and zeroize only. |
| `bevy_net_backend` | no | Implements the client crate [`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend)'s `WsRequest` / `WsPushMessage` for this crate's WebSocket messages and its `Credentials` for `AccessToken`, and adds the `bevy` module: every `HttpCall` as a typed request of that client. Brings that crate and its dependencies; a server never enables it. |

## Install

```toml
[dependencies]
net_backend_protocol = { version = "0.2.0" }

# With the optional client integration:
# net_backend_protocol = { version = "0.2.0", features = ["bevy_net_backend"] }
```

Rust 1.95 or newer.

## Quick start: a client

Build a request, read whatever the server sends back:

```rust
use net_backend_protocol::chat::{ChatMessage, JoinRoom, RoomInfo, SendMessage};
use net_backend_protocol::{RoomId, WsAuth, WsRequestFrame, WsServerFrame};

// First message on a fresh socket (if the handshake carried no `Authorization` header).
let auth = WsAuth::new("the-access-token-from-login").to_message();
assert!(auth.starts_with(r#"{"type":"auth""#));

// A request: {"id":1,"type":"chat.send","data":{"room":12,"text":"hello"}}
let request = WsRequestFrame::call(1, SendMessage::new(RoomId(12), "hello"));
let text = serde_json::to_string(&request).unwrap();
assert_eq!(text, r#"{"id":1,"type":"chat.send","data":{"room":12,"text":"hello"}}"#);
let _join = WsRequestFrame::call(2, JoinRoom::new("world"));

// Whatever the server sends: an answer, a push, auth.ok or auth.failed.
let incoming = r#"{"type":"chat.message","data":{"id":981,"room":12,"sender":42,"text":"hi","sent_at":1790000000000}}"#;
match WsServerFrame::parse(incoming).unwrap() {
    WsServerFrame::Response(answer) => {
        // answer.id is the request's id; answer.result is Ok(data) or Err(ApiError).
        let _room: Option<RoomInfo> = answer.decode::<RoomInfo>().ok().and_then(Result::ok);
    }
    WsServerFrame::Push(push) if push.kind == "chat.message" => {
        let message: ChatMessage = push.data_as().unwrap();
        assert_eq!(message.text, "hi");
    }
    _ => {}
}
```

## Quick start: a server

Decode a client frame, route by kind, answer:

```rust
use net_backend_protocol::chat::{SendAck, SendMessage, DEFAULT_MAX_TEXT_CHARS};
use net_backend_protocol::{codes, kinds, ApiError, MessageId, UnixMillis, WsCall, WsClientFrame, WsResponseFrame};

fn handle(text: &str) -> Option<String> {
    let request = match WsClientFrame::parse(text) {
        Ok(WsClientFrame::Request(request)) => request,
        Ok(WsClientFrame::Auth(_auth)) => return None, // check the token, answer auth.ok / auth.failed
        Ok(_) => return None,
        // A broken frame WITH an id still gets its one answer (`bad_request`); without an id it
        // cannot be answered.
        Err(error) => return error.answer().and_then(|answer| serde_json::to_string(&answer).ok()),
    };
    let answer = match request.kind.as_str() {
        kinds::CHAT_SEND => match request.data_as::<SendMessage>() {
            Ok(send) => match send.validate(DEFAULT_MAX_TEXT_CHARS) {
                Ok(()) => WsResponseFrame::ok_serialize(request.id, &SendAck::new(MessageId(981), UnixMillis::now())).ok()?,
                Err(error) => WsResponseFrame::error(request.id, error),
            },
            Err(_) => WsResponseFrame::error(request.id, ApiError::new(codes::BAD_REQUEST, "bad chat.send payload")),
        },
        _ => WsResponseFrame::error(request.id, ApiError::new(codes::UNKNOWN_TYPE, "unknown type")),
    };
    serde_json::to_string(&answer).ok()
}

let reply = handle(r#"{"id":7,"type":"chat.send","data":{"room":12,"text":"hello"}}"#).unwrap();
assert!(reply.starts_with(r#"{"id":7,"ok":true,"data":{"message_id":981"#));
let refused = handle(r#"{"id":8,"type":"no.such.kind","data":null}"#).unwrap();
assert_eq!(refused, r#"{"id":8,"ok":false,"error":{"code":"unknown_type","message":"unknown type"}}"#);
let malformed = handle(r#"{"id":9,"type":7}"#).unwrap();
assert!(malformed.starts_with(r#"{"id":9,"ok":false,"error":{"code":"bad_request""#));
assert_eq!(<SendMessage as WsCall>::KIND, "chat.send");
```

## The WebSocket envelope

JSON objects in text frames, compatible with the default envelope (`JsonEnvelope`) of the client
crate `bevy_net_backend`:

| Direction | Frame | JSON |
|---|---|---|
| client → server | request | `{"id":7,"type":"chat.send","data":{…}}` |
| server → client | answer | `{"id":7,"ok":true,"data":{…}}` or `{"id":7,"ok":false,"error":{"code":…,"message":…}}` |
| server → client | push | `{"type":"chat.message","data":{…}}` |
| client → server | first-message auth | `{"type":"auth","data":{"token":"…","protocol":1}}` |
| server → client | auth accepted | `{"type":"auth.ok","data":{"user_id":42,"protocol":1}}` |
| server → client | auth refused | `{"type":"auth.failed","error":{…}}`, then close 4001 |

- `id` is an unsigned 64-bit JSON number, echoed unchanged.
- A frame with a numeric `id` and (an `ok` field or no `type`) is an answer; a missing `ok`
  means `true`, a missing `data` means `null`. Anything else with a `type` is a push or an auth
  result. A push never has an `ok` field, and the server never puts an `id` on a push.
- Key order inside an object carries no meaning.
- A malformed request that has a usable `id` is still answered (`bad_request`,
  `FrameError::id` / `FrameError::answer`); only frames without one cannot be answered.
- Authentication: `Authorization: Bearer <access token>` on the handshake (a bad token is
  refused with HTTP 401 before the upgrade, an expired one with 401 `token_expired`), or the
  first-message `auth` within 5 seconds (`AUTH_TIMEOUT_SECS`; otherwise close 1008).
- **Every `auth` message gets exactly one `auth.ok` or `auth.failed`**, also on a socket already
  authenticated by its header. A later `auth` re-authenticates an open socket with a fresh token;
  a token of another user is refused (`auth.failed`, close 4001). `auth.failed` is a definitive
  refusal; a temporary server failure instead closes with 1013 without an answer (the client
  reconnects and tries again).
- An open socket survives the expiry of its access token; only a revocation closes it (4001, or
  4003 for a ban). Client recipe: refresh shortly before expiry (`ACCESS_TOKEN_REFRESH_MARGIN_SECS`);
  after a `Disconnected` with a handshake 401 or close 4001, refresh once and connect again (if the
  refresh is refused, log in again).
- Messages are at most 1 MiB (`MAX_MESSAGE_BYTES`) in both directions.

**Close codes** (`CloseCode`). Clients reconnect after every code except 4000–4099:

| Code | Name | Meaning |
|---|---|---|
| 1000 | `NORMAL` | normal closure |
| 1001 | `GOING_AWAY` | server shutdown or redeploy: reconnect |
| 1008 | `POLICY_VIOLATION` | rate limit, no authentication in time, malformed traffic |
| 1009 | `MESSAGE_TOO_BIG` | a message over 1 MiB |
| 1011 | `INTERNAL_ERROR` | unexpected server error: reconnect |
| 1013 | `TRY_AGAIN_LATER` | overloaded, or the connection could not keep up: reconnect later |
| 4001 | `UNAUTHORIZED` | authentication refused or revoked (logout, password change): log in again |
| 4003 | `BANNED` | the account is banned |
| 4009 | `REPLACED` | replaced by a newer connection of the same user or session (e.g. over the server's per-user connection cap) |
| 4010 | `UNSUPPORTED_PROTOCOL` | the client's protocol version is not supported |

**WebSocket kinds** (`kinds`): `auth`, `auth.ok`, `auth.failed`; chat `chat.join`, `chat.leave`,
`chat.send`, `chat.history`, `chat.members`, `chat.edit`, `chat.mark_read`, `chat.receipts`,
`chat.unread`, `chat.set_typing` and the pushes `chat.message`, `chat.deleted`, `chat.presence`,
`chat.edited`, `chat.read`, `chat.typing`, `chat.room`; notifications `notify.list`, `notify.count`,
`notify.mark`, `notify.delete` and the push `notify.new`; the pushes `friends.presence`,
`lobby.member`, `lobby.changed`, `match.found` and `match.expired`.

## HTTP routes

Everything versioned is under `/v1` (`routes::PREFIX`). "auth" = `Authorization: Bearer <access token>`;
"admin" = the token of an account with the `admin` role (`admin::ADMIN_ROLE`).

| Method | Path | Auth | Body → answer |
|---|---|---|---|
| GET | `/v1/info` | no | → `ServerInfo` |
| POST | `/v1/auth/register` | no | `RegisterRequest` → `AuthSession` |
| POST | `/v1/auth/login` | no | `LoginRequest` → `AuthSession` |
| POST | `/v1/auth/steam` | no | `SteamLoginRequest` → `AuthSession` |
| POST | `/v1/auth/oauth/{provider}` | no (a Bearer token of a recent login links) | `OAuthToken` → `AuthSession` |
| POST | `/v1/auth/refresh` | no | `RefreshRequest` → `TokenPair` |
| POST | `/v1/auth/logout` | auth or `refresh_token` in the body | `LogoutRequest` → `Ack` |
| POST | `/v1/auth/email/verify` | no | `VerifyEmailRequest` → `Ack` |
| POST | `/v1/auth/email/resend` | auth | (none) → `Ack` |
| POST | `/v1/auth/password/forgot` | no | `ForgotPasswordRequest` → `Ack` (always) |
| POST | `/v1/auth/password/reset` | no | `ResetPasswordRequest` → `Ack` |
| GET | `/v1/account` | auth | → `Account` |
| PATCH | `/v1/account` | auth | `UpdateAccountRequest` → `Account` |
| POST | `/v1/account/password` | auth | `ChangePasswordRequest` → `Ack` |
| DELETE | `/v1/account/identities/{provider}` | auth (recent login) | → `Ack` (unlink a provider) |
| GET | `/v1/storage/{collection}` | auth | query `PageRequest` → `Page<StorageObjectInfo>` (no values) |
| GET | `/v1/storage/{collection}/{key}` | auth | → `StorageObject` |
| PUT | `/v1/storage/{collection}/{key}` | auth | `PutObject` → `ObjectAck` |
| DELETE | `/v1/storage/{collection}/{key}` | auth | query `DeleteObject` → `Ack` |
| POST | `/v1/storage/_batch/get` | auth | `BatchGet` → `BatchObjects` |
| POST | `/v1/storage/_batch/put` | auth | `BatchPut` → `BatchAcks` |
| GET | `/v1/users/{user}/storage/{collection}` | auth | query `PageRequest` → `Page<StorageObjectInfo>` (another player's objects the caller may read) |
| GET | `/v1/users/{user}/storage/{collection}/{key}` | auth | → `StorageObject` (public, or `friends` for the owner's friends) |
| GET | `/v1/chat/rooms` | auth | query `PageRequest` → `Page<RoomInfo>` |
| GET | `/v1/chat/rooms/{room}/messages` | auth | query `PageRequest` → `Page<ChatMessage>` |
| DELETE | `/v1/chat/rooms/{room}/messages/{message}` | auth (its sender or a moderator) | → `Ack` (the room gets `chat.deleted`) |
| POST | `/v1/chat/dm` | auth | `OpenDirect` → `RoomInfo` |
| GET | `/v1/chat/dms` | auth | query `PageRequest` → `Page<RoomInfo>` (the caller's DMs, with `peer`) |
| PATCH | `/v1/chat/rooms/{room}/messages/{message}` | auth (its sender in the edit window, or a moderator) | `MessageEdit` → `ChatMessage` (the room gets `chat.edited`) |
| PUT | `/v1/chat/rooms/{room}/read` | auth | `ReadUpTo` → `Ack` (the read marker) |
| GET | `/v1/chat/rooms/{room}/receipts` | auth (members) | → `ReadReceipts` |
| POST | `/v1/chat/unread` | auth | `UnreadQuery` → `UnreadCounts` |
| POST | `/v1/chat/rooms` | auth | `CreateRoom` → `RoomInfo` (a player room) |
| GET | `/v1/chat/rooms/mine` | auth | query `PageRequest` → `Page<RoomInfo>` (the caller's player rooms and invitations) |
| GET | `/v1/chat/rooms/public` | auth | query `PageRequest` → `Page<RoomInfo>` (public player rooms) |
| GET | `/v1/chat/rooms/{room}` | auth | → `RoomInfo` |
| PATCH | `/v1/chat/rooms/{room}` | auth (owner, moderators) | `UpdateRoom` → `RoomInfo` |
| DELETE | `/v1/chat/rooms/{room}` | auth (owner) | → `Ack` |
| POST | `/v1/chat/rooms/{room}/join` | auth | → `RoomInfo` (become a member, or accept an invitation) |
| POST | `/v1/chat/rooms/{room}/leave` | auth | → `Ack` |
| GET | `/v1/chat/rooms/{room}/members` | auth (members) | query `PageRequest` → `Page<RoomMembership>` |
| POST | `/v1/chat/rooms/{room}/invites` | auth (owner, moderators) | `RoomUser` → `Ack` |
| DELETE | `/v1/chat/rooms/{room}/members/{user}` | auth (owner, moderators) | → `Ack` (a kick: banned until invited again) |
| PUT | `/v1/chat/rooms/{room}/members/{user}/role` | auth (owner) | `RoomRoleChange` → `Ack` |
| POST | `/v1/chat/rooms/{room}/owner` | auth (owner) | `RoomUser` → `Ack` |
| GET | `/v1/leaderboards` | auth | → `Boards` |
| GET | `/v1/leaderboards/{board}` | auth | query `TopQuery` (`cursor`, `limit`, `at`) → `LeaderboardPage` (best first) |
| POST | `/v1/leaderboards/{board}/scores` | auth | `SubmitScore` → `ScoreAck` |
| GET | `/v1/leaderboards/{board}/me` | auth | query `RankQuery` (`at`) → `MyRank` |
| GET | `/v1/leaderboards/{board}/around` | auth | query `AroundQuery` (`above`, `below`, `at`) → `LeaderboardPage` |
| GET | `/v1/notifications` | auth | query `NotificationQuery` (`cursor`, `limit`, `unread_only`) → `Page<Notification>` (newest first) |
| GET | `/v1/notifications/count` | auth | → `NotificationCount` |
| POST | `/v1/notifications/mark` | auth | `MarkNotifications` → `MarkAck` |
| DELETE | `/v1/notifications/{id}` | auth | → `Ack` |
| GET | `/v1/friends` | auth | query `PageRequest` → `Page<FriendEntry>` (with the online state) |
| DELETE | `/v1/friends/{user}` | auth | → `Ack` |
| GET | `/v1/friends/requests` | auth | query `RequestQuery` (`direction`, `cursor`, `limit`) → `Page<FriendEntry>` |
| POST | `/v1/friends/requests` | auth | `AddFriend` → `FriendEntry` |
| DELETE | `/v1/friends/requests/{user}` | auth | → `Ack` |
| POST | `/v1/friends/requests/{user}/accept` | auth | → `FriendEntry` |
| POST | `/v1/friends/requests/{user}/decline` | auth | → `Ack` |
| GET | `/v1/friends/blocks` | auth | query `PageRequest` → `Page<FriendEntry>` |
| PUT / DELETE | `/v1/friends/blocks/{user}` | auth | → `Ack` (block / unblock) |
| GET / POST | `/v1/friends/code` | auth | → `FriendCode` (the code / a new one) |
| POST | `/v1/friends/presence` | auth | → `Ack` (online heartbeat) |
| POST | `/v1/friends/steam` | auth | `SteamMatch` → `SteamMatchResult` (Steam IDs → accounts here) |
| GET / PUT | `/v1/friends/settings` | auth | → `FriendSettings` / `UpdateFriendSettings` → `FriendSettings` |
| GET / POST | `/v1/groups` | auth | query `GroupQuery` (`query`, `cursor`, `limit`) → `Page<GroupInfo>`; `CreateGroup` → `GroupInfo` |
| GET | `/v1/groups/mine` | auth | → `GroupList` |
| GET | `/v1/groups/invites` | auth | query `PageRequest` → `Page<GroupInvite>` |
| GET / PATCH / DELETE | `/v1/groups/{group}` | auth | → `GroupInfo`; `UpdateGroup` → `GroupInfo`; → `Ack` |
| GET | `/v1/groups/{group}/members` | auth | query `PageRequest` → `Page<GroupMember>` |
| POST | `/v1/groups/{group}/join`, `/leave` | auth | → `GroupInfo`, `Ack` |
| POST | `/v1/groups/{group}/invites` | auth | `Invitee` → `Ack` |
| POST | `/v1/groups/{group}/invites/accept`, `/decline` | auth | → `GroupInfo`, `Ack` |
| DELETE | `/v1/groups/{group}/invites/{user}`, `/v1/groups/{group}/members/{user}` | auth | → `Ack` |
| PUT | `/v1/groups/{group}/members/{user}/role` | auth | `RoleChange` → `Ack` |
| POST | `/v1/groups/{group}/transfer` | auth | `Invitee` → `Ack` |
| POST | `/v1/lobbies` | auth | `CreateLobby` → `LobbyInfo` |
| GET | `/v1/lobbies/mine` | auth | → `LobbyList` |
| POST | `/v1/lobbies/search` | auth | `LobbySearch` → `Page<LobbyInfo>` |
| POST | `/v1/lobbies/join` | auth | `JoinLobbyByCode` → `LobbyInfo` |
| GET / PATCH | `/v1/lobbies/{lobby}` | auth | → `LobbyInfo`; `UpdateLobby` → `LobbyInfo` (host) |
| POST | `/v1/lobbies/{lobby}/join`, `/leave` | auth | → `LobbyInfo`, `Ack` |
| PUT | `/v1/lobbies/{lobby}/ready` | auth | `SetReady` → `Ack` |
| POST | `/v1/lobbies/{lobby}/code` | auth | → `LobbyInfo` (host: a new join code) |
| POST | `/v1/lobbies/{lobby}/host` | auth | `LobbyPlayer` → `Ack` (host) |
| DELETE | `/v1/lobbies/{lobby}/members/{user}` | auth | → `Ack` (host) |
| GET | `/v1/matchmaking/queues` | auth | → `Queues` |
| POST / GET / DELETE | `/v1/matchmaking/ticket` | auth | `CreateTicket` → `MatchTicket`; → `MatchTicket`; → `Ack` |
| POST | `/v1/files` | auth | `multipart/form-data`: `meta` (JSON `FileMeta`, optional, first) + `file` (the bytes) → `FileInfo` (in `routes::BINARY`) |
| GET | `/v1/files` | auth | query `FileQuery` (`owner`, `cursor`, `limit`) → `Page<FileInfo>` |
| GET | `/v1/files/usage` | auth | → `FileUsage` |
| GET / PATCH / DELETE | `/v1/files/{file}` | auth | → `FileInfo`; `UpdateFile` → `FileInfo` (owner); → `Ack` (owner) |
| GET | `/v1/files/{file}/content` | auth | → the bytes (in `routes::BINARY`) |
| GET | `/v1/admin/users` | admin | query `UserListQuery` (`q`, `cursor`, `limit`) → `Page<AdminUser>` |
| GET | `/v1/admin/users/{user}` | admin | → `AdminUser` |
| POST | `/v1/admin/users/{user}/ban` | admin | `BanRequest` → `Ack` (sessions revoked, sockets close 4003) |
| POST | `/v1/admin/users/{user}/unban` | admin | (none) → `Ack` |
| DELETE | `/v1/admin/users/{user}/sessions` | admin | → `Ack` (every session revoked) |
| DELETE | `/v1/admin/users/{user}/identities/{provider}` | admin | → `Ack` (unlink a provider) |
| PUT / DELETE | `/v1/admin/users/{user}/roles/{role}` | admin | → `Ack` (grant / revoke) |
| GET | `/v1/admin/audit` | admin | query `AuditQuery` (`user`, `action`, `cursor`, `limit`) → `Page<AuditEntry>` |
| GET | `/v1/admin/users/{user}/storage/{collection}` | admin | query `PageRequest` → `Page<StorageObjectInfo>` |
| GET | `/v1/admin/users/{user}/storage/{collection}/{key}` | admin | → `StorageObject` |
| PUT | `/v1/admin/users/{user}/storage/{collection}/{key}` | admin | `AdminPutObject` (may set `write`) → `ObjectAck` |
| DELETE | `/v1/admin/users/{user}/storage/{collection}/{key}` | admin | query `DeleteObject` → `Ack` |
| GET (upgrade) | `/v1/ws` | header or first message | the WebSocket |

Unversioned: `/healthz` (liveness) and `/readyz` (readiness). `routes::ALL` holds the table as data
(method, path, auth; `Route::new` builds entries for your own routes); every route has an
[`HttpCall`](#typed-http-calls-httpcall) type, and `routes::storage_object_path` and friends fill in
path parameters. Request body limits: 64 KiB for JSON routes
(`routes::DEFAULT_BODY_LIMIT_BYTES`), 272 KiB for a storage PUT, 4.06 MiB for a batch put; every
answer stays far below the 10 MiB `bevy_net_backend` accepts by default.

## Typed HTTP calls (HttpCall)

`HttpCall` is the HTTP twin of `WsCall`: a request type knows its route (method, path template,
whether a Bearer token is needed), its payload (a JSON body, a query string, or nothing) and its
answer type. Every route of `routes::ALL` has exactly one `HttpCall` type (a golden test checks
it), and the server mounts its handlers from the same types, so client and server cannot drift.

- Routes without path parameters use their body / query type directly: `LoginRequest`,
  `RegisterRequest`, `BatchGet`, `BatchPut`, `OpenDirect`, `UserListQuery`, `AuditQuery`, ….
- Routes with path parameters (or without a payload) have a small call type: `GetObject`,
  `WriteObject` (its `PutObject` body), `RemoveObject`, `ListObjects`, `ListMessages`,
  `DeleteMessage`, `ListRooms`, `ListDirects`, `GetAccount`, `ResendVerification`,
  `UnlinkIdentity`, `GetServerInfo`, and in `admin` `GetUser`, `BanUser`, `UnbanUser`,
  `RevokeSessions`, `UnlinkUserIdentity`, `GrantRole`, `RevokeRole`, `ListUserObjects`,
  `GetUserObject`, `WriteUserObject`, `RemoveUserObject`. They are not wire types: what travels
  is the path, the payload and the answer.

A client sends `C::ROUTE.method` to `call.path()` with `call.payload()` as JSON
(`PayloadKind::Json`), as the query (`PayloadKind::Query`) or nothing (`PayloadKind::Empty`), plus
the Bearer token when `C::ROUTE.auth`; a 2xx answer decodes as `C::Response`, every error as
`ErrorBody`:

```rust
use net_backend_protocol::storage::{GetObject, ObjectVersion, PutObject, StorageObject, WriteObject};
use net_backend_protocol::{HttpCall, PayloadKind};

let put = WriteObject::new("saves", "slot-1", PutObject::new(serde_json::json!({"level": 3})).if_version(ObjectVersion(2)));
assert_eq!(WriteObject::ROUTE.method.as_str(), "PUT");
assert!(WriteObject::ROUTE.auth);
assert_eq!(put.path().as_deref(), Some("/v1/storage/saves/slot-1"));
assert_eq!(WriteObject::PAYLOAD, PayloadKind::Json);
let body = serde_json::to_string(put.payload()).unwrap(); // {"value":{"level":3},"if_version":2}
assert!(body.contains("if_version"));

// The answer type is part of the call: `GetObject` answers a `StorageObject`.
fn decode<C: HttpCall>(json: &str) -> C::Response {
    serde_json::from_str(json).unwrap()
}
let object: StorageObject = decode::<GetObject>(r#"{"collection":"saves","key":"slot-1","owner":42,"value":{"level":3},"version":3,"write":"owner","updated_at":1790000000000}"#);
assert_eq!(object.version, ObjectVersion(3));

// Only path-safe values are sent: a name that would need escaping gives no path.
assert_eq!(GetObject::new("saves", "../etc").path(), None);
```

A server rebuilds a call from what it received with `C::from_parts(&path_params, payload)`, which
checks the parameters' shape (an id that is not a number, an invalid name: `bad_request`). Your
own routes can have `HttpCall` types too (`Route::new(HttpMethod::Get, "/v1/game/…", true)`;
`net_backend_server` enforces the `auth` flag: no valid token, no handler).
`PathParams`, `is_path_safe` (RFC 3986 path characters that need no escaping: letters, digits,
`- . _ ~ ! $ & ' ( ) * + , ; = : @`), `http_call::placeholders` and `http_call::query_pairs` (a
`Query` payload as name / value pairs for your HTTP library; absent fields are left out, never
sent as `null`) are the helpers.

## Errors

Every error is an `ApiError`: a stable `code`, a human-readable `message`, optional `details`.
Over HTTP the body is `{"error":{…}}` (`ErrorBody`); over WebSocket it is the `error` of an
answer or of `auth.failed`.

```rust
use net_backend_protocol::{codes, error::http_status_for, ApiError, ErrorBody, ValidationDetails};

let body: ErrorBody = serde_json::from_str(
    r#"{"error":{"code":"validation_failed","message":"the request is invalid","details":{"fields":{"password":["is too short"]}}}}"#,
)
.unwrap();
assert!(body.error.is(codes::VALIDATION_FAILED));
let fields: ValidationDetails = body.error.details_as().unwrap();
assert_eq!(fields.fields["password"], ["is too short"]);
assert_eq!(http_status_for(codes::VERSION_CONFLICT), Some(409));
assert_eq!(ApiError::new(codes::NOT_FOUND, "no such save").to_string(), "not_found: no such save");
```

Codes (`codes`): `bad_request` 400, `validation_failed` 422, `unauthorized` 401, `forbidden` 403,
`token_expired` 401, `not_found` 404, `method_not_allowed` 405, `unsupported_media_type` 415, `conflict` 409, `version_conflict` 409, `payload_too_large` 413, `rate_limited`
429, `quota_exceeded` 403, `unknown_type` (WebSocket only), `unsupported_protocol` 400,
`invalid_credentials` 401, `refresh_token_reused` 401, `email_taken` 409, `email_not_verified`
403, `invalid_token` 400, `banned` 403, `reauthentication_required` 403, `steam_auth_failed` 401, `oauth_failed` 401, `room_full` 409, `not_a_member`
403, `hook_timeout` 503, `unavailable` 503, `internal` 500. Codes never change meaning; server
modules and games may add their own, so treat an unknown code like its HTTP status.
`unsupported_protocol` is 400 on plain HTTP routes only (see versioning). `email_taken` tells that
an address has an account: a deliberate trade-off for a clear sign-up answer, bounded by rate limits.

## Accounts and sessions

Register or log in (email + password, or a Steam Web API ticket) and get an `AuthSession`: the
`Account` and a `TokenPair`. The access token lives 1 hour by default
(`DEFAULT_ACCESS_TOKEN_TTL_SECS`), the refresh token 30 days (`DEFAULT_REFRESH_TOKEN_TTL_SECS`);
a server may configure both. Refresh tokens **rotate**: each works once. Presenting the same
refresh token again within 30 seconds of its first use (`REFRESH_REUSE_GRACE_SECS`: an HTTP retry,
two systems refreshing at once) answers the same new pair; a later reuse revokes the whole session
(`refresh_token_reused`). Refresh single-flight anyway and keep the old pair until the new one is
stored. Logout takes the Bearer token or, when it already expired, the refresh token in the body.

```rust
use net_backend_protocol::auth::{LoginRequest, RegisterRequest, TokenPair};

let register = RegisterRequest::new("ada@example.com", "correct horse battery").with_display_name("Ada");
assert!(register.validate().is_ok()); // the same rules the server applies
assert!(RegisterRequest::new("ada@example.com", "short").validate().is_err());

// Secrets never show in Debug output (but do serialize: they must reach the server).
let login = LoginRequest::new("ada@example.com", "my password");
assert!(!format!("{login:?}").contains("my password"));

let tokens: TokenPair = serde_json::from_str(
    r#"{"token_type":"Bearer","access_token":"a","access_expires_at":1,"refresh_token":"r","refresh_expires_at":2}"#,
)
.unwrap();
assert_eq!(tokens.authorization_header(), "Bearer a");
```

Rules shared by both sides: passwords 10 characters to 128 bytes, no control characters, not only
whitespace; emails up to 254 bytes (shape only: the server normalises them and sends a verification
mail); display names up to 32 characters, trimmed. Emails must be a plain `local@domain`
(`auth::is_valid_email`): display names, angle brackets, comments, quoted local parts and address
literals are refused, so a server never re-parses an address with a lenient mail parser. Emails
and display names refuse control characters and invisible or direction-changing characters (bidi
controls, zero-width characters, BOM: `net_backend_protocol::text`), which would allow
impersonation. Steam tickets up to 8192 hex characters (Steam's buffer is 2560 bytes = 5120 hex).
Password-reset requests always answer the same way, whether or not the address has an account. Steam: the game sends the hex ticket from `GetAuthTicketForWebApi` with
its identity string; the server checks it with Steam and creates the account on the first login.

OpenID Connect (a server with the `oauth` module): the game signs the player in at the provider
(Google, or another provider the server is configured for), gets the provider's ID token and sends
it with the nonce of that sign-in as `OAuthLogin`; the server checks the token and answers an
`AuthSession`. The provider account becomes a `LinkedIdentity` (`provider` = the server's name for
the provider). With the Bearer token of a recent login the same call links the provider account
instead; unlinking is `UnlinkIdentity`.

```rust
use net_backend_protocol::oauth::{OAuthLogin, OAuthToken};
use net_backend_protocol::HttpCall;

let login = OAuthLogin::new("google", OAuthToken::new("eyJhbGciOi.eyJpc3Mi.c2lnbmF0dXJl").with_nonce("nonce-of-this-sign-in"));
assert!(login.token.validate().is_ok());
assert_eq!(login.path().as_deref(), Some("/v1/auth/oauth/google"));
assert!(!format!("{login:?}").contains("eyJhbGciOi")); // the token never shows in Debug
```

## Storage (saves)

Objects live at `(owner, collection, key)` and hold one JSON value; every route addresses the
caller's own objects. Every write bumps the object's `ObjectVersion` (1 for a new object). The
default is **last write wins**. A write that names the version it expects (`if_version`) gets 409
`version_conflict` with `{"current_version":N}` (`VersionConflict`; for a batch also `"index":i`,
the failing item) if the stored one differs, and nothing changes: only then two devices cannot
silently overwrite each other's save. The server also honours `If-Match: "N"` / `If-None-Match: *`
on single-object PUT and DELETE and sends an `ETag` with every object it returns or writes (GET,
PUT; not on DELETE). Deleting an object that does not exist answers `Ack` (unless `if_version` is
given).

```rust
use net_backend_protocol::storage::{ObjectVersion, PutObject, DEFAULT_MAX_OBJECT_BYTES};
use net_backend_protocol::routes;

#[derive(serde::Serialize)]
struct Save { level: u32, gold: u64 }

let put = PutObject::from_serialize(&Save { level: 3, gold: 250 }).unwrap().if_version(ObjectVersion(2));
assert!(put.validate(DEFAULT_MAX_OBJECT_BYTES).is_ok());
assert_eq!(serde_json::to_string(&put).unwrap(), r#"{"value":{"gold":250,"level":3},"if_version":2}"#);
assert_eq!(routes::storage_object_path("saves", "slot-1").as_deref(), Some("/v1/storage/saves/slot-1"));
assert_eq!(routes::storage_object_path("saves", "../etc"), None); // not a valid name
let _create_only = PutObject::new(serde_json::json!({})).if_absent(); // only if it does not exist
```

Names: 1–128 bytes of ASCII letters, digits, `_`, `-`, `.`, starting with a letter or digit
(`storage::is_valid_name`). Defaults: 256 KiB per value, 1000 objects per user. Batches: at most 16
distinct objects (`MAX_BATCH`) and 4 MiB of values (`MAX_BATCH_BYTES`), all or nothing; a batch
read over 4 MiB is refused with 413. Collection listings return `StorageObjectInfo` (version, size,
time, no value), so a page stays small. `write` is `owner` or `server`: only server-side game code
sets `server`, and clients then get 403 on writes. `visibility` (`ObjectVisibility`) is `private`
(the default), `public` or `friends`: `PutObject::with_visibility` sets it, a write without it keeps
the stored one, and `GetPlayerObject` / `ListPlayerObjects` read another player's objects the caller
may read (404 / left out otherwise). Binary data: the `files` module (below), or a string you
encode yourself (e.g. base64, +33 %, which counts against the size limit).

## Files

A server with the `files` module keeps players' binary files. The upload and the download carry
bytes, not JSON, so they have no `HttpCall` (`routes::BINARY`); everything else is typed.

```rust
use net_backend_protocol::files::{FileMeta, FileVisibility, ListFiles, FileQuery, UpdateFile, UPLOAD_META_PART};
use net_backend_protocol::{routes, FileId, HttpCall, UserId};

// The upload's `meta` part (JSON), sent before the `file` part.
let meta = FileMeta::new().with_name("caves.level").shared_with(vec![UserId(7), UserId(9)]);
assert!(meta.validate().is_ok());
assert_eq!(UPLOAD_META_PART, "meta");
// Another player's readable files; a change of the settings.
assert_eq!(ListFiles::new().with_query(FileQuery::of(UserId(7))).path().as_deref(), Some("/v1/files"));
let _public = UpdateFile::new().with_visibility(FileVisibility::Public);
assert_eq!(routes::file_content_path(FileId(12)), "/v1/files/12/content");
```

`FileVisibility`: `private` (the owner), `public` (every logged-in player), `friends` (the owner's
friends), `shared` (the accounts in `shared_with`). `FileInfo.sha256` is the lower-case hex SHA-256
of the bytes (the download's `ETag`); `FileMeta::with_sha256` makes the server check the upload.
Names: 1–255 bytes without slashes or control / invisible characters (`files::name_problem`);
content types: a plain `type/subtype` (`files::is_valid_content_type`).

## Chat

Joining, leaving, sending, history and live messages go over the WebSocket; the room list,
history pages and opening a direct-message room are also HTTP routes.

| Kind | Request → answer |
|---|---|
| `chat.join` | `JoinRoom` → `RoomInfo` (by id `{"room":12}` or public key `{"room":"world"}`) |
| `chat.leave` | `LeaveRoom` → `Ack` |
| `chat.send` | `SendMessage` → `SendAck` |
| `chat.history` | `ChatHistory` → `Page<ChatMessage>` (newest first) |
| push `chat.message` | `ChatMessage`, to every member, the sender included |
| push `chat.deleted` | `MessageDeleted` (moderation) |
| `chat.members` | `ListMembers` → `RoomMembers` (who is online: each user once, up to 200 listed, the total `count`; a DM room lists only the caller: a DM never reveals the peer's online state) |
| push `chat.presence` | `Presence` (`joined` / `left`, with the user's name and the room's online `count`) |
| `chat.edit` | `EditMessage` → `ChatMessage` (the edited message, with `edited_at`) |
| push `chat.edited` | `MessageEdited` |
| `chat.mark_read` | `MarkRead` → `Ack` (the caller's read marker; forward only) |
| push `chat.read` | `ReadReceipt` (a member's marker moved; DM, group and player rooms; coalesced) |
| `chat.receipts` | `ListReceipts` → `ReadReceipts` |
| `chat.unread` | `UnreadQuery` → `UnreadCounts` (up to 100 rooms, counts stop at 1000) |
| `chat.set_typing` | `SetTyping` → `Ack` (never stored) |
| push `chat.typing` | `TypingUpdate` (throttled; a client drops it after `expires_in_ms`) |
| push `chat.room` | `RoomUpdate` (a player room changed: `RoomChange`) |

Public and group rooms: membership lasts as long as the connection; after a reconnect, join
again. **Direct messages** need no join: their `chat.message` goes to every open connection of
both members, and `GET /v1/chat/dms` lists the caller's DM rooms (`RoomInfo::peer`). The sender
gets its own `chat.message` too (the final text after server hooks, also on its other devices); the
`SendAck` answer comes first, games dedupe by `message_id`, and an optional `nonce` on
`SendMessage`, echoed in the push, matches a send whose answer was lost (in a public or group room
the other members' pushes carry it too, so use a random value; in a DM and in the history only the
sender sees it). Text: line breaks and tabs are allowed; other control characters and invisible /
direction-changing characters are refused (zero-width joiners stay allowed for emoji), a text must
show something (not only spaces, joiners, tags or marks), and at most 8 combining marks may stack. Room keys: 1–64 bytes of `a-z`, `0-9`, `_ - .`.
`ChatHistory` is WebSocket-only; over HTTP the room is in the path and the page in the query.
Defaults (a server may configure others): 500 characters per message, a send rate per user of a
burst of 5 messages, then one every 2 seconds (5 per 10 s as a token bucket; `rate_limited` carries
`retry_after_ms`), 30 days of history, 200 connections per public room, 16 joined rooms per
connection. Public rooms are capped on
purpose: one huge room multiplies every message by its member count.

**Presence:** when a user's first connection joins a room, the room's members (the joiner's own
connections too) get `chat.presence` `joined`; when its last one leaves (or disconnects), `left`.
It is best effort on purpose: rooms with more online users than the server's presence cap
(`DEFAULT_PRESENCE_MAX_MEMBERS`, 100) get none, and a burst beyond the per-room rate
(`DEFAULT_PRESENCE_PER_SECOND`, 10) is not pushed; `chat.members` always answers the current list.
**Deleting** a message (`DELETE /v1/chat/rooms/{room}/messages/{message}`, its sender or a
moderator) pushes `chat.deleted` and removes it from the history.

**Editing** (`EditMessage`, over the WebSocket or HTTP): the sender within the server's edit window
(`DEFAULT_EDIT_WINDOW_SECS`, 900) or a moderator; the room gets `chat.edited`, and the history shows
the latest text with `ChatMessage::edited_at`. **Read markers** (`MarkRead`: "read up to message X",
forward only) feed `UnreadQuery`; in DM, group and player rooms the members get `ReadReceipt` pushes
and `ListReceipts` answers the current markers. **Typing** (`SetTyping`) is never stored; a
`TypingUpdate` carries how long to show it.

**Rooms created by players** (`RoomKind::Player`): `CreateRoom` (a name, `RoomVisibility` public or
private) makes the caller the owner. Public rooms are listed (`PublicRooms`) and open to anyone not
banned; private rooms take invited players. Membership is stored: `JoinChatRoom` (or a `chat.join`)
makes the caller a member and accepts an invitation, `LeaveChatRoom` ends it. Roles (`RoomRole`):
the owner renames, changes the visibility, sets roles (`SetRoomRole`), hands the room on
(`TransferRoom`) and deletes it (`DeleteRoom`); moderators rename (`EditRoom`), invite
(`InviteToRoom`), kick (`KickFromRoom`: banned until invited again) and delete messages in the room.
`MyRooms` lists the caller's rooms and invitations with its role, `ListRoomMembers` the rows of a
room (`RoomMembership`). Every change is a `RoomUpdate` push (`chat.room`).

```rust
use net_backend_protocol::chat::{CreateRoom, EditMessage, InviteToRoom, MarkRead, RoomUser, UnreadQuery};
use net_backend_protocol::{HttpCall, MessageId, RoomId, UserId, WsCall};

let create = CreateRoom::new("Night Owls").public();
assert_eq!(serde_json::to_string(&create).ok().as_deref(), Some(r#"{"name":"Night Owls","visibility":"public"}"#));
let invite = InviteToRoom::new(RoomId(40), RoomUser::new(UserId(77)));
assert_eq!(invite.path().as_deref(), Some("/v1/chat/rooms/40/invites"));
// The same request over the WebSocket and over HTTP.
let edit = EditMessage::new(RoomId(12), MessageId(981), "hello again");
assert_eq!(<EditMessage as WsCall>::KIND, "chat.edit");
assert_eq!(edit.path().as_deref(), Some("/v1/chat/rooms/12/messages/981"));
assert_eq!(serde_json::to_string(&MarkRead::new(RoomId(12), MessageId(981))).ok().as_deref(), Some(r#"{"room":12,"message":981}"#));
assert!(UnreadQuery::new(vec![RoomId(12)]).validate().is_ok());
```

## Leaderboards

The server configures its boards; players submit scores (`PostScore` with a `SubmitScore`) and read
them (`ListBoards`, `GetLeaderboard`, `GetMyRank`, `GetAroundMe`). A board (`BoardInfo`) has a key
(1–64 bytes of `a-z`, `0-9`, `_ - .`; `leaderboards::is_valid_board_key`), a `ScoreMode` (`best`
keeps the better score, `latest` replaces it, `sum` adds to it), a `ScoreOrder` (`desc`: higher is
better; `asc`: lower is better) and a `Period` (`all_time`, `daily`, `weekly`: resets at 00:00 UTC,
weeks on Monday). `Period::start_of` / `end_of` compute a period's bounds; every read takes `at`, a
time inside the period to show.

```rust
use net_backend_protocol::leaderboards::{AroundQuery, GetAroundMe, Period, PostScore, SubmitScore};
use net_backend_protocol::{HttpCall, UnixMillis};

let submit = PostScore::new("weekly-race", SubmitScore::new(61_250).with_metadata(serde_json::json!({"car": "red"})));
assert_eq!(submit.path().as_deref(), Some("/v1/leaderboards/weekly-race/scores"));
let around = GetAroundMe::new("weekly-race").with_query(AroundQuery::new().with_counts(2, 2));
assert_eq!(around.path().as_deref(), Some("/v1/leaderboards/weekly-race/around"));
// The week of 2026-10-02 started on Monday 2026-09-28 at 00:00 UTC.
assert_eq!(Period::Weekly.start_of(UnixMillis(1_790_953_200_000)), Some(UnixMillis(1_790_553_600_000)));
```

Ranks (`LeaderboardEntry::rank`) start at 1 and are unique: equal scores rank by who reached the
score first (`achieved_at`), then by the lower account id. `ScoreAck` says the stored score, the
submitted one (after the server's hooks), whether the stored score changed and the rank. A board
with `client_submit: false` takes scores from the server's own code only (players get 403). A score
is any `i64` except `i64::MIN`; metadata is a small JSON value (`DEFAULT_MAX_METADATA_BYTES`, 1 KiB,
unless the server configures another). `AroundQuery` asks for up to `MAX_AROUND` (50) entries above
and below the caller (default `DEFAULT_AROUND`, 5).

## Notifications

The server stores notifications per player (a reward, an invitation, a system notice) and pushes
each new one as `notify.new` to the player's open connections. Only the server creates them; a
player reads, marks and deletes their own, over the WebSocket or HTTP (same answers):

| Kind | Request → answer | HTTP |
|---|---|---|
| push `notify.new` | `Notification` | |
| `notify.list` | `NotificationQuery` → `Page<Notification>` (newest first) | `GET /v1/notifications` |
| `notify.count` | `CountNotifications` → `NotificationCount` (`unread`, `total`) | `GET /v1/notifications/count` |
| `notify.mark` | `MarkNotifications` → `MarkAck` (`changed`, `unread`) | `POST /v1/notifications/mark` |
| `notify.delete` | `DeleteNotification` → `Ack` | `DELETE /v1/notifications/{id}` |

```rust
use net_backend_protocol::notifications::{MarkNotifications, NotificationQuery};
use net_backend_protocol::{HttpCall, NotificationId, WsCall};

let unread = NotificationQuery::new().unread_only();
assert_eq!(<NotificationQuery as WsCall>::KIND, "notify.list");
assert_eq!(serde_json::to_string(&unread).unwrap(), r#"{"unread_only":true}"#);
let mark = MarkNotifications::read(vec![NotificationId(31), NotificationId(32)]);
assert!(mark.validate().is_ok());
assert_eq!(mark.path().as_deref(), Some("/v1/notifications/mark"));
assert!(MarkNotifications::all_read().validate().is_ok());
```

A `Notification` has its `id`, the game's `kind` (1–64 bytes of `a-z`, `0-9`, `_ . : -`, starting
with a letter; `notifications::is_valid_kind`), an optional `text` (at most `MAX_TEXT_CHARS`, 1000,
with the chat text rules), optional `data` (JSON; `DEFAULT_MAX_DATA_BYTES`, 4 KiB, unless the server
configures another), the `sender` account if any, `created_at` and `read`. `MarkNotifications` names
1 to `MAX_MARK_IDS` (100) distinct ids or `all`; ids of other players are skipped. A client that was
offline lists what it missed (`unread_only`); while connected it gets `notify.new`.

## Friends

Friends by account. A player sends a request by account id, display name or friend code
(`AddFriend`); the other accepts (`AcceptFriend`) or declines (`DeclineFriend`); the sender may
withdraw it (`CancelFriendRequest`); either ends the friendship (`RemoveFriend`). `BlockUser` ends a
friendship and every open request between the two, and refuses the blocked player's requests. Every
entry is a `FriendEntry` (`user`, `name`, `state`: `friend` / `sent` / `received` / `blocked`,
`since`; for friends `online` and `last_seen`). HTTP only, plus one push:

| Push | Data |
|---|---|
| `friends.presence` | `FriendPresence` (`user`, `online`, `last_seen`), to the player's friends when it comes online or goes offline |

```rust
use net_backend_protocol::friends::{normalize_friend_code, AddFriend, ListFriendRequests, RequestQuery};
use net_backend_protocol::HttpCall;

let add = AddFriend::by_code("k7m2-q9xd");
assert!(add.validate().is_ok());
assert_eq!(normalize_friend_code("k7m2-q9xd").as_deref(), Some("K7M2Q9XD"));
assert_eq!(serde_json::to_string(&add).unwrap(), r#"{"code":"k7m2-q9xd"}"#);
let sent = ListFriendRequests::sent().with_query(RequestQuery::sent().with_limit(20));
assert_eq!(sent.path().as_deref(), Some("/v1/friends/requests"));
```

A friend code is 8 characters of `FRIEND_CODE_ALPHABET` (`2-9`, `A-Z` without `O` and `I`); case,
spaces and dashes do not matter when it is typed in (`normalize_friend_code`). A name is matched
exactly against display names, which are not unique: several matches answer 409 `conflict`. A
friend is online while it has a WebSocket connection, or for the server's online window (90 s by
default) after its last `FriendsHeartbeat`.

**Steam IDs:** on a server with Steam login, a player who linked a Steam account sends Steam IDs,
e.g. its Steam friends list (`SteamMatch`), and gets the ones that belong to accounts there
(`SteamMatchResult`: `SteamPlayer` with `steam_id`, `user`, `name`, and `state` when the caller
already relates to that player). Steam IDs travel as decimal strings (`parse_steam_id`: an
individual SteamID64, no sign, spaces or leading zeros); a SteamID64 is larger than the integers a
JSON number carries exactly. Players who turned `FriendSettings::steam_findable` off
(`UpdateFriendSettings`) are never found; it is on by default.

```rust
use net_backend_protocol::friends::{parse_steam_id, SteamMatch, UpdateFriendSettings};
use net_backend_protocol::HttpCall;

let lookup = SteamMatch::new([76_561_201_960_265_729]);
assert!(lookup.validate().is_ok());
assert_eq!(serde_json::to_string(&lookup).unwrap(), r#"{"steam_ids":["76561201960265729"]}"#);
assert_eq!(lookup.path().as_deref(), Some("/v1/friends/steam"));
assert_eq!(parse_steam_id("76561201960265729"), Some(76_561_201_960_265_729));
let hide = UpdateFriendSettings::new().steam_findable(false);
assert_eq!(serde_json::to_string(&hide).unwrap(), r#"{"steam_findable":false}"#);
```

## Groups

Groups (guilds, clans). A player creates one (`CreateGroup`) and owns it; others join by invitation
(`InviteToGroup`, then `AcceptGroupInvite` or `DeclineGroupInvite`) or directly when it is open
(`JoinGroup`). Roles (`GroupRole`): the owner, admins (change the group, invite, remove members) and
members; `SetMemberRole` and `TransferGroup` are the owner's. HTTP only.

```rust
use net_backend_protocol::groups::{CreateGroup, EditGroup, GroupRole, SetMemberRole, UpdateGroup};
use net_backend_protocol::{GroupId, HttpCall, UserId};

let create = CreateGroup::new("Night Owls").with_description("We play at night").open();
assert!(create.validate().is_ok());
assert!(CreateGroup::new("No").validate().is_err(), "3 to 32 characters");
let clear = EditGroup::new(GroupId(5), UpdateGroup::new().with_description(""));
assert_eq!(clear.path().as_deref(), Some("/v1/groups/5"));
let promote = SetMemberRole::new(GroupId(5), UserId(42), GroupRole::Admin);
assert_eq!(promote.path().as_deref(), Some("/v1/groups/5/members/42/role"));
```

A `GroupInfo` has the name (3 to `MAX_GROUP_NAME_CHARS`, 32, characters; unique on the server
without regard to case), an optional description (`MAX_DESCRIPTION_CHARS`, 500), `open`, the game's
`metadata` (JSON; `DEFAULT_MAX_METADATA_BYTES`, 2 KiB, unless the server configures another), the
owner, the member count and the server's `max_members`, the group's `chat_room` (when the server
runs the chat module) and the caller's `role` when it is a member. In an `UpdateGroup`, an empty
description removes it and a present `"metadata":null` removes the metadata.

## Lobbies

Lobbies (`lobbies`): a player creates one (`CreateLobby`) and hosts it; others join by id
(`JoinLobby`: public lobbies, or a friends-only one of a friend) or with the join code
(`JoinLobbyByCode`, any visibility); members set a ready flag (`SetLobbyReady`); the host changes
the metadata, size, visibility and state (`EditLobby` with an `UpdateLobby`), kicks
(`KickFromLobby`), hands the lobby over (`TransferLobby`) and replaces the code (`NewLobbyCode`).
`LobbySearch` lists open lobbies by metadata filters. HTTP, plus two pushes:

| Push | Data |
|---|---|
| `lobby.member` | `LobbyMemberUpdate` (`lobby`, `change`: `joined` / `left` / `kicked` / `ready`, `member`), to every member |
| `lobby.changed` | `LobbyUpdate` (`changes`: `host` / `metadata` / `settings` / `state` / `code`, `lobby` without its member list), to every member |

```rust
use net_backend_protocol::lobbies::{CreateLobby, EditLobby, JoinLobbyByCode, LobbyCode, LobbySearch, LobbyState, LobbyVisibility, UpdateLobby};
use net_backend_protocol::{HttpCall, LobbyId};

let create = CreateLobby::new(4).with_visibility(LobbyVisibility::Private).with_meta("mode", "ranked");
assert!(create.validate().is_ok());
// A join code is also a number below 2^40, for platforms that carry a number.
let code = LobbyCode::parse("k7m2-q9xd").unwrap();
assert_eq!(code.to_u64(), 590_122_524_587);
assert_eq!(LobbyCode::from_u64(590_122_524_587), Some(code));
assert_eq!(JoinLobbyByCode::from_number(590_122_524_587).unwrap().code, "K7M2Q9XD");
let start = EditLobby::new(LobbyId(7), UpdateLobby::new().with_state(LobbyState::InGame).remove_meta("password"));
assert_eq!(start.path().as_deref(), Some("/v1/lobbies/7"));
assert_eq!(serde_json::to_string(&start.update).unwrap(), r#"{"state":"in_game","metadata":{"password":null}}"#);
let search = LobbySearch::new().with_filter("mode", "ranked");
assert!(search.validate().is_ok());
```

A `LobbyInfo` has the visibility (`public`, `private`, `friends`), the state (`open`, `in_game`,
`closed`: the last `lobby.changed` of a lobby that is gone), the host, `max_players`, the member
count, the metadata (text keys of 1 to `MAX_META_KEY_BYTES`, 64, bytes; values of at most
`MAX_META_VALUE_CHARS`, 256, characters), and for members the join code in both forms (`code`,
`code_number`) and the `chat_room` (when the server runs the chat module); answers about one lobby
list its `players` (`LobbyMember`: `user`, `name`, `ready`, `joined_at`). A join code is 8
characters of `FRIEND_CODE_ALPHABET`; case, spaces and dashes do not matter when typed
(`LobbyCode::parse`); `LobbyCode::grouped` shows it as `K7M2-Q9XD`.

## Matchmaking

Matchmaking (`matchmaking`): a player puts a ticket (`CreateTicket`: a queue and `attributes` the
game's rules read) into one of the server's queues (`ListQueues`), reads it (`GetTicket`: waiting,
or matched with its `MatchFound`) and cancels it (`CancelTicket`). HTTP, plus two pushes:

| Push | Data |
|---|---|
| `match.found` | `MatchFound` (`ticket`, `queue`, `players`, the rules' `data`), to each matched player |
| `match.expired` | `TicketExpired` (`ticket`, `queue`): the ticket ran out unmatched |

```rust
use net_backend_protocol::matchmaking::{CreateTicket, GetTicket};
use net_backend_protocol::HttpCall;

let ticket = CreateTicket::new("duel").with_attributes(serde_json::json!({"rating": 1520}));
assert!(ticket.validate().is_ok());
assert_eq!(GetTicket::new().path().as_deref(), Some("/v1/matchmaking/ticket"));
```

## Ids, timestamps, pages

- Ids are `i64` newtypes (`UserId`, `RoomId`, `MessageId`, `NotificationId`, `GroupId`, `LobbyId`,
  `TicketId`, `FileId`) and travel as plain JSON numbers. Ids from outside (a SteamID64) travel as strings.
- Timestamps are `UnixMillis`: `i64` milliseconds since 1970 UTC, a plain JSON number
  (`UnixMillis::now()`, `from_system_time`, `to_system_time`).
- Lists use **cursor** pagination: `PageRequest { cursor, limit }` (default 50, at most 100) →
  `Page<T> { items, next_cursor }`; the last page has no `next_cursor`. Cursors are opaque (at
  most 512 bytes, `MAX_CURSOR_BYTES`): pass them back unchanged.

## Versioning and compatibility

- **Routes:** `/v1` is stable: within `/v1` routes and kinds keep their names and stay.
- **Protocol version:** `PROTOCOL_VERSION` (1) identifies the set of messages within `/v1`. A
  client names the version it speaks in the `x-net-backend-protocol` header (`PROTOCOL_HEADER`,
  on HTTP requests and the WebSocket handshake) or in the `protocol` field of the first-message
  `auth`; none means 1. The server answers with its own version in the same header and in
  `auth.ok`, lists the accepted range at `GET /v1/info`, and refuses an unsupported version with
  `unsupported_protocol`: HTTP 400 on plain HTTP routes; on `/v1/ws` NEVER 400 (the client would
  retry it forever) but upgrade and close 4010 (`version::WS_REFUSAL_CLOSE`, no reconnect), or 403
  before the upgrade.
- **Forward compatibility:** unknown fields are ignored when decoding (no type uses
  `deny_unknown_fields`), so extra fields are fine; optional fields may be absent; an
  unknown enum value (a room kind, a write rule) decodes as `Unknown`; unknown error codes and
  provider names are kept as text; a push kind a client does not know is still a push.

## Optional integration with bevy_net_backend

With the feature `bevy_net_backend`, this crate's WebSocket messages implement that client's
`WsRequest` / `WsPushMessage` (same kinds, same answer types: `JoinRoom`, `LeaveRoom`, `SendMessage`,
`ChatHistory`, `ListMembers`, `EditMessage`, `MarkRead`, `ListReceipts`, `UnreadQuery`, `SetTyping`,
`NotificationQuery`, `CountNotifications`, `MarkNotifications`, `DeleteNotification`; pushes
`ChatMessage`, `MessageDeleted`, `Presence`, `MessageEdited`, `ReadReceipt`, `TypingUpdate`,
`RoomUpdate`, `Notification`, `FriendPresence`, `LobbyMemberUpdate`, `LobbyUpdate`, `MatchFound`,
`TicketExpired`) and `AccessToken` implements its
`Credentials` (a `Bearer` header on every request and WebSocket handshake). The client's default
envelope is the one above, so nothing else is needed:

```rust,ignore
use net_backend_protocol::chat::{ChatMessage, JoinRoom, SendMessage};

app.add_ws_request::<JoinRoom>()
    .add_ws_request::<SendMessage>()
    .add_ws_push::<ChatMessage>();

// In a system: `ws: Res<WsClient>`, `credentials: ResMut<BackendCredentials>`.
credentials.set(session.tokens.access_token.clone());
ws.request("main", &JoinRoom::new("world"));
// Answers arrive as `WsResponse<RoomInfo>`, pushes as `WsPush<ChatMessage>`; a refused request is
// `BackendError::Rejected`, whose payload decodes as `ApiError` (`rejection.json::<ApiError>()`).
```

Every HTTP route is a typed call there as well (module `net_backend_protocol::bevy`):
`request(&call)` builds the client's `OutgoingRequest` exactly as `net_backend_client` sends the
call (`C::ROUTE.method` on `call.path()`, the payload as the JSON body or the query string,
`accept: application/json` and the `x-net-backend-protocol` header; the game's credentials only on
routes that need a token), `HttpClientCalls::call(&call)` sends it with the answer decoded as
`C::Response`, and `api_error(&error)` reads the protocol's `ApiError` out of a refused answer. A
path parameter that is missing or would need escaping is answered `InvalidRequest` and never sent.

```rust,ignore
use net_backend_protocol::bevy::{api_error, request, HttpClientCalls};
use net_backend_protocol::leaderboards::{GetLeaderboard, LeaderboardPage, TopQuery};

app.add_json_response::<LeaderboardPage>();

// In a system: `http: Res<HttpClient>`.
http.call(&GetLeaderboard::new("highscore").with_query(TopQuery::new().with_limit(10)));
// Or adjust the request first (a timeout, a header):
http.send_json::<LeaderboardPage>(request(&GetLeaderboard::new("highscore")).with_timeout(Duration::from_secs(5)));

// Answers arrive as `JsonResponse<LeaderboardPage>`:
match &answer.result {
    Ok(page) => show(page),
    Err(error) => match api_error(error) {
        Some(api) => warn!("refused: {} ({})", api.message, api.code),
        None => warn!("no answer: {error}"),
    },
}
```

Add the Bevy client itself to your game with `cargo add bevy_net_backend`.

## API design rules

- Types a client or server only **reads** (answers, pushes, errors) are `#[non_exhaustive]` with
  public fields, so adding a field is not a breaking change.
- Types you **build** (requests) are `#[non_exhaustive]` too and have constructors plus `with_*`
  builders: `SendMessage::new(room, text)`, `PutObject::new(value).if_version(v)`. Optional
  fields are set through builders, so adding one is not a breaking change.
- Nothing that may hold a float (any JSON `Value`) implements `Eq`.
- Every serde attribute is explicit; JSON field names are `snake_case`.
- Secrets (`Password`, `AccessToken`, `RefreshToken`, `Secret`) print `<redacted>` in `Debug`,
  have no `Display` and no `PartialEq` (compare secrets in constant time on the server). When one
  is dropped, its whole allocation is overwritten with zeros first (the `zeroize` crate); each
  clone is wiped on its own. Not wiped: the `String` that `into_inner` returns, copies made with
  `expose().to_string()` or by serializing it, and the input it was decoded from.
- `validate()` methods carry the shape rules both sides share; the server is still the authority.

## Testing

```sh
cargo test -p net_backend_protocol                              # unit, golden JSON, round trips, redaction, wiped secrets, forward compatibility, HttpCall table
cargo test -p net_backend_protocol --features bevy_net_backend  # plus the frames through the client's own JsonEnvelope and every HTTP call as its request
cargo run -p net_backend_protocol --example print_frames        # prints the JSON of every frame
```

The golden tests pin the exact JSON text of every envelope frame. With the feature on, the tests
run every frame through `bevy_net_backend`'s real encoder and decoder in both directions,
check that both decoders classify the same frames the same way, and build every route's typed
call as that client's request, compared with what `net_backend_client` sends. CI runs fmt, clippy,
tests and docs for every feature combination on Rust 1.96.0.

## FAQ

**Do I need this crate to talk to the server?** No. The JSON is the contract; any HTTP / WebSocket
client in any language works. This crate saves Rust code from re-typing the messages.

**Why `i64` ids and millisecond timestamps instead of UUIDs and RFC 3339 strings?** They map to
one column type on every database the server supports, sort naturally and cost nothing to parse.

**Why cursors and not page numbers?** Lists such as chat history grow while you page through
them; an offset would skip or repeat messages, a cursor does not.

**Why does `chat.send` not resend after a reconnect?** It could post a message twice. Chat
membership also ends with the connection, so a game joins again after reconnecting.

## License

Dual-licensed under MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0
([LICENSE-APACHE](LICENSE-APACHE)), at your option.

## Contributing

This crate lives in the [`net_backend`](https://github.com/warmar94/net_backend) repository
together with the server and the client. Issues and pull requests are welcome. Please run
`cargo fmt --all`, `cargo clippy -p net_backend_protocol --all-targets --all-features -- -D warnings`
and `cargo test -p net_backend_protocol --all-features` before opening a pull request.
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.

Website: [net-backend.com](https://net-backend.com) · Contact: [info@net-backend.com](mailto:info@net-backend.com)
