//! Compatibility with the client `bevy_net_backend` (feature `bevy_net_backend`): frames this crate
//! builds go through the client's own `JsonEnvelope` (its real encoder and decoder) and come out as
//! this crate's types, in both directions; every typed HTTP call becomes the client's request
//! (`net_backend_protocol::bevy`) exactly as `net_backend_client` sends it.

use bevy_net_backend::{Credentials, JsonEnvelope, OutgoingRequest, Rejection, WsFrame, WsIncoming, WsProtocol, WsPushMessage, WsRequest};
use net_backend_protocol::chat::{
    ChatHistory, ChatMessage, JoinRoom, LeaveRoom, ListMembers, MessageDeleted, Presence, PresenceEvent, RoomInfo, RoomKind, RoomMember, RoomMembers, SendAck,
    SendMessage,
};
use net_backend_protocol::{
    codes, AccessToken, Ack, ApiError, CloseCode, MessageId, Page, RoomId, UnixMillis, UserId, WsAuth, WsAuthOk, WsCall, WsClientFrame, WsPushFrame,
    WsRequestFrame, WsResponseFrame, WsServerFrame,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

const NOW: UnixMillis = UnixMillis(1_790_000_000_000);

fn text(frame: &impl Serialize) -> String {
    serde_json::to_string(frame).unwrap_or_else(|e| panic!("{e}"))
}

/// What `WsClient::request::<R>` puts on the wire: `serde_json::to_vec(request)` handed to the
/// protocol's `encode_request` with `R::KIND`.
fn client_encode<R: WsRequest>(wire_id: u64, request: &R) -> String {
    let payload = serde_json::to_vec(request).unwrap_or_else(|e| panic!("{e}"));
    match JsonEnvelope.encode_request(wire_id, R::KIND, &payload) {
        Ok(WsFrame::Text(text)) => text,
        other => panic!("not a text frame: {other:?}"),
    }
}

/// The server decodes what the client sends into the same request.
fn server_sees<R: WsRequest + WsCall + PartialEq + std::fmt::Debug + Clone>(wire_id: u64, request: R) {
    let wire = client_encode(wire_id, &request);
    let Ok(WsClientFrame::Request(frame)) = WsClientFrame::parse(&wire) else { panic!("server could not parse {wire}") };
    assert_eq!((frame.id, frame.kind.as_str()), (wire_id, <R as WsCall>::KIND));
    assert_eq!(frame.data_as::<R>().ok(), Some(request.clone()), "{wire}");
    // The same JSON as this crate's own request frame (key order aside).
    let ours: Value = serde_json::from_str(&text(&WsRequestFrame::call(wire_id, request))).unwrap_or_default();
    let theirs: Value = serde_json::from_str(&wire).unwrap_or_default();
    assert_eq!(ours, theirs);
}

#[test]
fn requests_from_the_client_parse_on_the_server() {
    server_sees(1, JoinRoom::new("world"));
    server_sees(2, JoinRoom::new(RoomId(12)));
    server_sees(3, LeaveRoom::new(RoomId(12)));
    server_sees(4, SendMessage::new(RoomId(12), "hello \u{1F600} \"quoted\""));
    server_sees(u64::MAX, ChatHistory::new(RoomId(12)));
    server_sees(5, ListMembers::new(RoomId(12)));
    server_sees(6, net_backend_protocol::chat::EditMessage::new(RoomId(12), MessageId(981), "hello again"));
    server_sees(7, net_backend_protocol::chat::MarkRead::new(RoomId(12), MessageId(981)));
    server_sees(8, net_backend_protocol::chat::ListReceipts::new(RoomId(12)));
    server_sees(9, net_backend_protocol::chat::UnreadQuery::new(vec![RoomId(12), RoomId(13)]));
    server_sees(10, net_backend_protocol::chat::SetTyping::started(RoomId(12)));
}

/// A push this crate builds comes out of the client's decoder as the same value, under its kind.
fn client_decodes_push<P>(push: P)
where
    P: net_backend_protocol::ServerPush + WsPushMessage + Serialize + DeserializeOwned + PartialEq + std::fmt::Debug + Clone,
{
    let wire = text(&WsPushFrame::push(push.clone()));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Push { kind, data } => {
            assert_eq!(kind, <P as WsPushMessage>::KIND);
            assert_eq!(serde_json::from_slice::<P>(&data).ok(), Some(push), "{wire}");
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
}

#[test]
fn chat_extras_pushes_decode_in_the_client() {
    use net_backend_protocol::chat::{MessageEdited, ReadReceipt, RoomChange, RoomRole, RoomUpdate, RoomVisibility, TypingUpdate};
    client_decodes_push(MessageEdited::new(MessageId(981), RoomId(12), "hello again", NOW).by(UserId(42)));
    client_decodes_push(ReadReceipt::new(RoomId(12), UserId(42), MessageId(981), NOW));
    client_decodes_push(TypingUpdate::new(RoomId(12), UserId(42), true, 6000));
    let info = RoomInfo::new(RoomId(12), RoomKind::Player).with_name("Night Owls").with_visibility(RoomVisibility::Public).with_owner(UserId(42));
    client_decodes_push(RoomUpdate::new(RoomId(12), RoomChange::Updated).by(UserId(42)).with_info(info));
    client_decodes_push(RoomUpdate::new(RoomId(12), RoomChange::Role).about(UserId(7)).with_role(RoomRole::Moderator));
}

/// The client turns a server answer into `T` exactly as its typed route does (`serde_json::from_slice`
/// on the `data` bytes, an all-whitespace payload read as `null`).
fn client_decodes_answer<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(wire_id: u64, data: T) {
    let wire = text(&WsResponseFrame::ok(wire_id, &data));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Response { wire_id: id, result: Ok(bytes) } => {
            assert_eq!(id, wire_id);
            assert_eq!(serde_json::from_slice::<T>(&bytes).ok(), Some(data), "{wire}");
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
}

#[test]
fn answers_from_the_server_decode_in_the_client() {
    client_decodes_answer(1, RoomInfo::new(RoomId(12), RoomKind::Room).with_key("world"));
    client_decodes_answer(2, Ack::new());
    client_decodes_answer(3, SendAck::new(MessageId(981), NOW));
    client_decodes_answer(4, RoomMembers::new(RoomId(12), vec![RoomMember::new(UserId(42)).with_name("Ada")], 1));
    client_decodes_answer(u64::MAX, Page::new(vec![ChatMessage::new(MessageId(1), RoomId(2), UserId(3), "x", NOW)], None));
}

#[test]
fn errors_from_the_server_become_rejections_with_an_api_error() {
    let error = ApiError::new(codes::NOT_A_MEMBER, "join the room first").with_details(serde_json::json!({"room": 12}));
    let wire = text(&WsResponseFrame::<Value>::error(7, error.clone()));
    let WsIncoming::Response { wire_id: 7, result: Err(payload) } = JsonEnvelope.decode(&WsFrame::Text(wire.clone())) else {
        panic!("{wire} is not an error answer for the client");
    };
    // The client answers `BackendError::Rejected(Rejection)` with these bytes; a game reads them
    // with `Rejection::json`.
    assert_eq!(Rejection::new(payload).json::<ApiError>().ok(), Some(error));
}

#[test]
fn pushes_from_the_server_decode_in_the_client() {
    let message = ChatMessage::new(MessageId(981), RoomId(12), UserId(42), "hello", NOW).with_sender_name("Ada");
    let wire = text(&WsPushFrame::push(message.clone()));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Push { kind, data } => {
            assert_eq!(kind, <ChatMessage as WsPushMessage>::KIND);
            assert_eq!(serde_json::from_slice::<ChatMessage>(&data).ok(), Some(message));
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
    let deleted = MessageDeleted::new(MessageId(1), RoomId(2));
    let wire = text(&WsPushFrame::push(deleted));
    assert!(matches!(JsonEnvelope.decode(&WsFrame::Text(wire)), WsIncoming::Push { kind, .. } if kind == "chat.deleted"));
    let presence = Presence::new(RoomId(12), UserId(42), PresenceEvent::Joined).with_count(2);
    let wire = text(&WsPushFrame::push(presence.clone()));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Push { kind, data } => {
            assert_eq!(kind, <Presence as WsPushMessage>::KIND);
            assert_eq!(serde_json::from_slice::<Presence>(&data).ok(), Some(presence));
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
    use net_backend_protocol::notifications::Notification;
    let notification = Notification::new(net_backend_protocol::NotificationId(31), "reward", NOW).with_data(serde_json::json!({"gold": 50}));
    let wire = text(&WsPushFrame::push(notification.clone()));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Push { kind, data } => {
            assert_eq!(kind, <Notification as WsPushMessage>::KIND);
            assert_eq!(serde_json::from_slice::<Notification>(&data).ok(), Some(notification));
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
    use net_backend_protocol::friends::FriendPresence;
    let presence = FriendPresence::new(UserId(7), false).with_last_seen(NOW);
    let wire = text(&WsPushFrame::push(presence.clone()));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Push { kind, data } => {
            assert_eq!(kind, <FriendPresence as WsPushMessage>::KIND);
            assert_eq!(serde_json::from_slice::<FriendPresence>(&data).ok(), Some(presence));
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
    use net_backend_protocol::lobbies::{
        LobbyChange, LobbyCode, LobbyInfo, LobbyMember, LobbyMemberUpdate, LobbyState, LobbyUpdate, LobbyVisibility, MemberChange,
    };
    use net_backend_protocol::matchmaking::{MatchFound, TicketExpired};
    use net_backend_protocol::{LobbyId, TicketId};
    let member = LobbyMemberUpdate::new(LobbyId(7), MemberChange::Ready, LobbyMember::new(UserId(42), NOW).with_ready(true));
    let wire = text(&WsPushFrame::push(member.clone()));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Push { kind, data } => {
            assert_eq!(kind, <LobbyMemberUpdate as WsPushMessage>::KIND);
            assert_eq!(serde_json::from_slice::<LobbyMemberUpdate>(&data).ok(), Some(member));
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
    let info = LobbyInfo::new(LobbyId(7), LobbyVisibility::Public, LobbyState::Closed, 4, 2, NOW).with_code(LobbyCode::from_u64(99).expect("code"));
    let update = LobbyUpdate::new(vec![LobbyChange::State], info);
    let wire = text(&WsPushFrame::push(update.clone()));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Push { kind, data } => {
            assert_eq!(kind, <LobbyUpdate as WsPushMessage>::KIND);
            assert_eq!(serde_json::from_slice::<LobbyUpdate>(&data).ok(), Some(update));
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
    let found = MatchFound::new(TicketId(9), "duel", vec![UserId(42), UserId(7)]).with_data(serde_json::json!({"lobby": 12}));
    let wire = text(&WsPushFrame::push(found.clone()));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Push { kind, data } => {
            assert_eq!(kind, <MatchFound as WsPushMessage>::KIND);
            assert_eq!(serde_json::from_slice::<MatchFound>(&data).ok(), Some(found));
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
    let expired = TicketExpired::new(TicketId(9), "duel");
    let wire = text(&WsPushFrame::push(expired.clone()));
    match JsonEnvelope.decode(&WsFrame::Text(wire.clone())) {
        WsIncoming::Push { kind, data } => {
            assert_eq!(kind, <TicketExpired as WsPushMessage>::KIND);
            assert_eq!(serde_json::from_slice::<TicketExpired>(&data).ok(), Some(expired));
        }
        other => panic!("{wire} decoded as {other:?}"),
    }
}

#[test]
fn auth_frames_match_the_client() {
    let ok = text(&WsServerFrame::AuthOk(Some(WsAuthOk::new(UserId(42)))));
    assert_eq!(JsonEnvelope.decode(&WsFrame::Text(ok)), WsIncoming::AuthOk);
    assert_eq!(JsonEnvelope.decode(&WsFrame::Text(text(&WsServerFrame::AuthOk(None)))), WsIncoming::AuthOk);
    let error = ApiError::new(codes::UNAUTHORIZED, "the token expired");
    let failed = text(&WsServerFrame::AuthFailed(error.clone()));
    let WsIncoming::AuthFailed(reason) = JsonEnvelope.decode(&WsFrame::Text(failed)) else { panic!("not auth.failed") };
    assert_eq!(serde_json::from_str::<ApiError>(&reason).ok(), Some(error));
    // The first message a game returns from `Credentials::ws_auth_message` parses on the server.
    let first = WsAuth::new("tok-example").to_message();
    assert!(matches!(WsClientFrame::parse(&first), Ok(WsClientFrame::Auth(a)) if a.token.expose() == "tok-example"));
}

/// Every frame the server can send is classified by the client exactly as `WsServerFrame` classifies it.
#[test]
fn both_decoders_agree() {
    let frames = [
        r#"{"id":1,"ok":true,"data":{"a":1}}"#,
        r#"{"id":1,"ok":false,"error":{"code":"x","message":"y"}}"#,
        r#"{"id":1,"data":2}"#,
        r#"{"id":1}"#,
        r#"{"type":"chat.message","data":{"x":1}}"#,
        r#"{"type":"chat.message","id":5,"data":{"x":1}}"#,
        r#"{"type":"tick"}"#,
        r#"{"type":"auth.ok"}"#,
        r#"{"type":"auth.failed","error":{"code":"unauthorized","message":""}}"#,
        r#"{"data":1}"#,
        r#"{"id":"1","ok":true}"#,
        "[1,2]",
        "not json",
    ];
    for frame in frames {
        let theirs = JsonEnvelope.decode(&WsFrame::Text(frame.into()));
        let ours = WsServerFrame::parse(frame).ok();
        assert!(same_class(&theirs, ours.as_ref()), "{frame}: client {theirs:?}, protocol {ours:?}");
    }
}

/// Whether the client's decoding and `WsServerFrame` put a frame into the same class (same id,
/// same success / failure, same push kind).
fn same_class(theirs: &WsIncoming, ours: Option<&WsServerFrame>) -> bool {
    match (theirs, ours) {
        (WsIncoming::Response { wire_id, result }, Some(WsServerFrame::Response(r))) => *wire_id == r.id && result.is_ok() == r.is_ok(),
        (WsIncoming::Push { kind, .. }, Some(WsServerFrame::Push(p))) => *kind == p.kind,
        (WsIncoming::AuthOk, Some(WsServerFrame::AuthOk(_))) => true,
        (WsIncoming::AuthFailed(_), Some(WsServerFrame::AuthFailed(_))) => true,
        (WsIncoming::Ignore, None) => true,
        _ => false,
    }
}

#[test]
fn close_codes_match_the_client_retry_rule() {
    for code in CloseCode::ALL {
        assert_eq!(JsonEnvelope.retry_after_close(code.get()), !code.is_permanent(), "{code}");
    }
    for raw in 1000..=4999u16 {
        assert_eq!(JsonEnvelope.retry_after_close(raw), !CloseCode(raw).is_permanent(), "{raw}");
    }
}

#[test]
fn trait_impls_match_the_protocol() {
    fn same_response<R>()
    where
        R: WsCall + WsRequest<Response = <R as WsCall>::Response>,
    {
        assert_eq!(<R as WsRequest>::KIND, <R as WsCall>::KIND);
    }
    same_response::<JoinRoom>();
    same_response::<LeaveRoom>();
    same_response::<SendMessage>();
    same_response::<ChatHistory>();
    same_response::<ListMembers>();
    same_response::<net_backend_protocol::chat::EditMessage>();
    same_response::<net_backend_protocol::chat::MarkRead>();
    same_response::<net_backend_protocol::chat::ListReceipts>();
    same_response::<net_backend_protocol::chat::UnreadQuery>();
    same_response::<net_backend_protocol::chat::SetTyping>();
    assert_eq!(<net_backend_protocol::chat::MessageEdited as WsPushMessage>::KIND, "chat.edited");
    assert_eq!(<net_backend_protocol::chat::ReadReceipt as WsPushMessage>::KIND, "chat.read");
    assert_eq!(<net_backend_protocol::chat::TypingUpdate as WsPushMessage>::KIND, "chat.typing");
    assert_eq!(<net_backend_protocol::chat::RoomUpdate as WsPushMessage>::KIND, "chat.room");
    assert!(!net_backend_protocol::chat::EditMessage::new(RoomId(1), MessageId(1), "x").resend_on_reconnect());
    same_response::<net_backend_protocol::notifications::NotificationQuery>();
    same_response::<net_backend_protocol::notifications::CountNotifications>();
    same_response::<net_backend_protocol::notifications::MarkNotifications>();
    same_response::<net_backend_protocol::notifications::DeleteNotification>();
    assert_eq!(<net_backend_protocol::notifications::Notification as WsPushMessage>::KIND, "notify.new");
    assert_eq!(<net_backend_protocol::friends::FriendPresence as WsPushMessage>::KIND, "friends.presence");
    assert_eq!(<net_backend_protocol::lobbies::LobbyMemberUpdate as WsPushMessage>::KIND, "lobby.member");
    assert_eq!(<net_backend_protocol::lobbies::LobbyUpdate as WsPushMessage>::KIND, "lobby.changed");
    assert_eq!(<net_backend_protocol::matchmaking::MatchFound as WsPushMessage>::KIND, "match.found");
    assert_eq!(<net_backend_protocol::matchmaking::TicketExpired as WsPushMessage>::KIND, "match.expired");
    assert!(!net_backend_protocol::notifications::MarkNotifications::all_read().resend_on_reconnect());
    assert_eq!(<Presence as WsPushMessage>::KIND, "chat.presence");
    assert_eq!(<ChatMessage as WsPushMessage>::KIND, "chat.message");
    assert_eq!(<MessageDeleted as WsPushMessage>::KIND, "chat.deleted");
    assert!(!SendMessage::new(RoomId(1), "x").resend_on_reconnect());
}

#[test]
fn access_token_is_bearer_credentials() {
    let mut request = OutgoingRequest::get("/v1/account");
    AccessToken::new("tok-example").apply(&mut request);
    let header = request.headers().get("authorization").unwrap_or_else(|| panic!("no Authorization header"));
    assert_eq!(header.to_str().ok(), Some("Bearer tok-example"));
    assert!(header.is_sensitive());
    assert_eq!(AccessToken::new("t").ws_auth_message(), None);
}

/// N12: the review's adversarial frames, classified identically by the client and by `WsServerFrame`.
#[test]
fn both_decoders_agree_on_adversarial_frames() {
    let frames = [
        r#"{"id":1,"type":5}"#,
        r#"{"id":1,"ok":"false","error":{}}"#,
        r#"{"id":1,"type":null}"#,
        r#"{"id":1,"type":"auth.ok"}"#,
        r#"{"id":1,"type":"auth.failed","error":{"code":"x"}}"#,
        r#"{"type":"auth.failed"}"#,
        r#"{"id":1,"ok":null,"type":"x"}"#,
        r#"{"id":0}"#,
        r#"{"id":1e2}"#,
        r#"{"id":18446744073709551616}"#,
        r#"{"type":"x","data":null}"#,
        r#"{"type":""}"#,
        r#"{"id":1,"ok":true,"data":1,"type":"chat.message"}"#,
        r#"{"type":"auth.ok","id":2,"ok":true}"#,
        "  {\"id\":1}  ",
        r#"{"id":1,"error":{"code":"x"}}"#,
        r#"{"type":"auth.ok","data":{"user_id":"x"}}"#,
        r#"{"id":1,"id":"x","type":"t"}"#,
        r#"{"type":"t","type":5}"#,
        r#"{"id":1,"ok":false,"data":{"a":1}}"#,
        r#"{"id":-0,"ok":true}"#,
        r#"{"id":1.0,"ok":true}"#,
        r#"{"id":1,"ok":true,"data":{"a":1}}trailing"#,
    ];
    for frame in frames {
        let theirs = JsonEnvelope.decode(&WsFrame::Text(frame.into()));
        let ours = WsServerFrame::parse(frame).ok();
        assert!(same_class(&theirs, ours.as_ref()), "{frame}: client {theirs:?}, protocol {ours:?}");
    }
}

/// S2: the `bad_request` answer to a malformed request reaches the client as a rejection of that id.
#[test]
fn malformed_request_answers_reach_the_client() {
    let error = WsClientFrame::parse(r#"{"id":41,"type":7}"#).err().unwrap_or_else(|| panic!("parsed"));
    let wire = text(&error.answer().unwrap_or_else(|| panic!("no id")));
    let WsIncoming::Response { wire_id: 41, result: Err(payload) } = JsonEnvelope.decode(&WsFrame::Text(wire.clone())) else {
        panic!("{wire} is not an error answer for request 41");
    };
    assert!(Rejection::new(payload).json::<ApiError>().is_ok_and(|e| e.is(codes::BAD_REQUEST)));
}

/// Typed HTTP calls through `net_backend_protocol::bevy`: every route of `routes::ALL` becomes the
/// `bevy_net_backend` request `net_backend_client` sends for it (method, path, the payload's exact
/// JSON bytes or query pairs, the protocol header, credentials only where the route needs a token),
/// and the protocol's error comes back out of a refused answer.
mod http {
    use bevy_net_backend::http::StatusCode;
    use bevy_net_backend::{BackendError, RawResponse};
    use net_backend_protocol::admin::{
        AdminPutObject, AuditQuery, BanRequest, BanUser, GetUser, GetUserObject, GrantRole, ListUserObjects, RemoveUserObject, RevokeRole, RevokeSessions,
        UnbanUser, UnlinkUserIdentity, UserListQuery, WriteUserObject,
    };
    use net_backend_protocol::auth::{
        ChangePasswordRequest, ForgotPasswordRequest, GetAccount, LoginRequest, LogoutRequest, RefreshRequest, RegisterRequest, ResendVerification,
        ResetPasswordRequest, SteamLoginRequest, UnlinkIdentity, UpdateAccountRequest, VerifyEmailRequest,
    };
    use net_backend_protocol::bevy::{api_error, request};
    use net_backend_protocol::chat::{
        CreateRoom, DeleteMessage, DeleteRoom, EditMessage, EditRoom, GetRoom, InviteToRoom, JoinChatRoom, KickFromRoom, LeaveChatRoom, ListDirects,
        ListMessages, ListReceipts, ListRoomMembers, ListRooms, MarkRead, MyRooms, OpenDirect, PublicRooms, RoomRole, RoomUser, RoomVisibility, SetRoomRole,
        TransferRoom, UnreadQuery, UpdateRoom,
    };
    use net_backend_protocol::files::{DeleteFile, EditFile, FileQuery, FileVisibility, GetFile, GetFileUsage, ListFiles, UpdateFile};
    use net_backend_protocol::friends::{
        AcceptFriend, AddFriend, BlockUser, CancelFriendRequest, DeclineFriend, FriendsHeartbeat, GetFriendCode, GetFriendSettings, ListBlocks,
        ListFriendRequests, ListFriends, RemoveFriend, RequestQuery, ResetFriendCode, SteamMatch, UnblockUser, UpdateFriendSettings,
    };
    use net_backend_protocol::groups::{
        AcceptGroupInvite, CreateGroup, DeclineGroupInvite, DeleteGroup, EditGroup, GetGroup, GroupQuery, GroupRole, InviteToGroup, Invitee, JoinGroup,
        KickMember, LeaveGroup, ListGroupInvites, ListGroupMembers, ListGroups, MyGroups, RevokeGroupInvite, SetMemberRole, TransferGroup, UpdateGroup,
    };
    use net_backend_protocol::http_call::query_pairs;
    use net_backend_protocol::leaderboards::{AroundQuery, GetAroundMe, GetLeaderboard, GetMyRank, ListBoards, PostScore, RankQuery, SubmitScore, TopQuery};
    use net_backend_protocol::lobbies::{
        CreateLobby, EditLobby, GetLobby, JoinLobby, JoinLobbyByCode, KickFromLobby, LeaveLobby, LobbyPlayer, LobbySearch, LobbyState, LobbyVisibility,
        MyLobbies, NewLobbyCode, SetLobbyReady, SetReady, TransferLobby, UpdateLobby,
    };
    use net_backend_protocol::matchmaking::{CancelTicket, CreateTicket, GetTicket, ListQueues};
    use net_backend_protocol::notifications::{CountNotifications, DeleteNotification, MarkNotifications, NotificationQuery};
    use net_backend_protocol::oauth::{OAuthLogin, OAuthToken};
    use net_backend_protocol::routes::{self, Route};
    use net_backend_protocol::storage::{
        BatchGet, BatchPut, GetObject, GetPlayerObject, ListObjects, ListPlayerObjects, ObjectRef, ObjectVersion, PutObject, RemoveObject, WriteAccess,
        WriteObject,
    };
    use net_backend_protocol::version::GetServerInfo;
    use net_backend_protocol::{
        codes, Cursor, FileId, GroupId, HttpCall, LobbyId, MessageId, NotificationId, PageRequest, PayloadKind, RoomId, UnixMillis, UserId,
    };
    use net_backend_protocol::{ApiError, ErrorBody, PROTOCOL_HEADER, PROTOCOL_VERSION};
    use serde_json::json;

    /// `request(&call)` against what `net_backend_client` sends for the same call (its
    /// `Outgoing::for_call`: the method, `call.path()`, `serde_json::to_writer` of the payload or
    /// `query_pairs`, `accept` + the protocol header, the Bearer token only when `ROUTE.auth`, and
    /// for logout, which `net_backend_client`'s `logout` sends with the access token).
    fn facts<C: HttpCall>(call: C) -> Route {
        let built = request(&call);
        let route = C::ROUTE.path;
        assert!(built.error().is_none(), "{route}: {:?}", built.error());
        assert_eq!(built.method().as_str(), C::ROUTE.method.as_str(), "{route}");
        assert_eq!(Some(built.path()), call.path().as_deref(), "{route}");
        let header = |name: &str| built.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        assert_eq!(header("accept").as_deref(), Some("application/json"), "{route}");
        assert_eq!(header(PROTOCOL_HEADER), Some(PROTOCOL_VERSION.to_string()), "{route}");
        // Logout takes a Bearer token or the refresh token in the body: the game's credentials go along.
        assert_eq!(built.uses_credentials(), C::ROUTE.auth || route == routes::auth::LOGOUT, "{route}");
        match C::PAYLOAD {
            PayloadKind::Json => {
                let bytes = serde_json::to_vec(call.payload()).unwrap_or_else(|e| panic!("{route}: {e}"));
                assert_eq!(built.body(), Some(bytes.as_slice()), "{route}");
                assert_eq!(header("content-type").as_deref(), Some("application/json"), "{route}");
                assert!(built.query().is_empty(), "{route}");
            }
            PayloadKind::Query => {
                let pairs = query_pairs(call.payload()).unwrap_or_else(|e| panic!("{route}: {e:?}"));
                assert_eq!(built.query(), pairs.as_slice(), "{route}");
                assert!(built.body().is_none() && header("content-type").is_none(), "{route}");
            }
            _ => assert!(built.body().is_none() && built.query().is_empty() && header("content-type").is_none(), "{route}"),
        }
        C::ROUTE
    }

    /// Logout works with the access token alone (no refresh token in the body): the game's
    /// credentials go along; login and refresh never carry them.
    #[test]
    fn logout_keeps_the_credentials() {
        assert!(request(&LogoutRequest::everywhere()).uses_credentials());
        assert!(request(&LogoutRequest::this_session()).uses_credentials());
        assert!(!request(&LoginRequest::new("a@example.com", "correct horse battery")).uses_credentials());
        assert!(!request(&RefreshRequest::new("nbsr_x")).uses_credentials());
    }

    fn every_call() -> Vec<Route> {
        let user = UserId(42);
        vec![
            facts(GetServerInfo::new()),
            facts(RegisterRequest::new("a@example.com", "correct horse battery")),
            facts(LoginRequest::new("a@example.com", "correct horse battery")),
            facts(SteamLoginRequest::new("0a0b", "my-game")),
            facts(OAuthLogin::new("google", OAuthToken::new("aGVhZGVy.cGF5bG9hZA.c2ln").with_nonce("n-1"))),
            facts(GetPlayerObject::new(user, "profile", "public")),
            facts(ListPlayerObjects::new(user, "levels").with_page(PageRequest::first().with_limit(5))),
            facts(ListFiles::new().with_query(FileQuery::of(user).with_limit(10))),
            facts(GetFileUsage::new()),
            facts(GetFile::new(FileId(12))),
            facts(EditFile::new(FileId(12), UpdateFile::new().with_visibility(FileVisibility::Public).with_metadata(serde_json::Value::Null))),
            facts(DeleteFile::new(FileId(12))),
            facts(RefreshRequest::new("nbsr_x")),
            facts(LogoutRequest::everywhere()),
            facts(VerifyEmailRequest::new("nbse_x")),
            facts(ResendVerification::new()),
            facts(ForgotPasswordRequest::new("a@example.com")),
            facts(ResetPasswordRequest::new("nbse_x", "a new long password")),
            facts(GetAccount::new()),
            facts(UpdateAccountRequest::new().with_display_name("Ada")),
            facts(ChangePasswordRequest::new("old password!", "new password!")),
            facts(UnlinkIdentity::new("steam")),
            facts(ListObjects::new("saves").with_page(PageRequest::after(Cursor::new("slot-1")).with_limit(10))),
            facts(GetObject::new("saves", "slot-1")),
            facts(WriteObject::new("saves", "slot-1", PutObject::new(json!({"level": 3})).if_version(ObjectVersion(2)))),
            facts(RemoveObject::new("saves", "slot-1").if_version(ObjectVersion(3))),
            facts(BatchGet::new(vec![ObjectRef::new("saves", "a")])),
            facts(BatchPut::new(vec![])),
            facts(ListRooms::new()),
            facts(ListMessages::new(RoomId(12)).with_page(PageRequest::first().with_limit(20))),
            facts(DeleteMessage::new(RoomId(12), MessageId(981))),
            facts(OpenDirect::new(UserId(7))),
            facts(ListDirects::new()),
            facts(EditMessage::new(RoomId(12), MessageId(981), "hello again")),
            facts(MarkRead::new(RoomId(12), MessageId(981))),
            facts(ListReceipts::new(RoomId(12))),
            facts(UnreadQuery::new(vec![RoomId(12), RoomId(13)])),
            facts(CreateRoom::new("Night Owls").public()),
            facts(MyRooms::new()),
            facts(PublicRooms::new().with_page(PageRequest::first().with_limit(10))),
            facts(GetRoom::new(RoomId(12))),
            facts(EditRoom::new(RoomId(12), UpdateRoom::new().with_name("Early Birds").with_visibility(RoomVisibility::Private))),
            facts(DeleteRoom::new(RoomId(12))),
            facts(JoinChatRoom::new(RoomId(12))),
            facts(LeaveChatRoom::new(RoomId(12))),
            facts(ListRoomMembers::new(RoomId(12)).with_page(PageRequest::first().with_limit(20))),
            facts(InviteToRoom::new(RoomId(12), RoomUser::new(UserId(7)))),
            facts(KickFromRoom::new(RoomId(12), UserId(7))),
            facts(SetRoomRole::new(RoomId(12), UserId(7), RoomRole::Moderator)),
            facts(TransferRoom::new(RoomId(12), RoomUser::new(UserId(7)))),
            facts(ListBoards::new()),
            facts(GetLeaderboard::new("highscore").with_query(TopQuery::new().after(Cursor::new("-5.9.1")).with_limit(10).at(UnixMillis(7)))),
            facts(PostScore::new("highscore", SubmitScore::new(1200).with_metadata(json!({"car": "red"})))),
            facts(GetMyRank::new("highscore").with_query(RankQuery::new().at(UnixMillis(7)))),
            facts(GetAroundMe::new("highscore").with_query(AroundQuery::new().with_counts(2, 3))),
            facts(NotificationQuery::new().after(Cursor::new("90")).with_limit(20).unread_only()),
            facts(CountNotifications::new()),
            facts(MarkNotifications::read(vec![NotificationId(31), NotificationId(32)])),
            facts(DeleteNotification::new(NotificationId(31))),
            facts(ListFriends::new().with_page(PageRequest::first().with_limit(20))),
            facts(RemoveFriend::new(UserId(7))),
            facts(ListFriendRequests::sent().with_query(RequestQuery::sent().after(Cursor::new("12")).with_limit(5))),
            facts(AddFriend::by_code("K7M2Q9XD")),
            facts(CancelFriendRequest::new(UserId(7))),
            facts(AcceptFriend::new(UserId(7))),
            facts(DeclineFriend::new(UserId(7))),
            facts(ListBlocks::new()),
            facts(BlockUser::new(UserId(7))),
            facts(UnblockUser::new(UserId(7))),
            facts(GetFriendCode::new()),
            facts(ResetFriendCode::new()),
            facts(FriendsHeartbeat::new()),
            facts(SteamMatch::new([76_561_201_960_265_729, 76_561_201_960_265_730])),
            facts(GetFriendSettings::new()),
            facts(UpdateFriendSettings::new().steam_findable(false)),
            facts(ListGroups::new().with_query(GroupQuery::starting_with("night").with_limit(10))),
            facts(CreateGroup::new("Night Owls").open()),
            facts(MyGroups::new()),
            facts(ListGroupInvites::new()),
            facts(GetGroup::new(GroupId(5))),
            facts(EditGroup::new(GroupId(5), UpdateGroup::new().with_open(true))),
            facts(DeleteGroup::new(GroupId(5))),
            facts(ListGroupMembers::new(GroupId(5)).with_page(PageRequest::first().with_limit(20))),
            facts(JoinGroup::new(GroupId(5))),
            facts(LeaveGroup::new(GroupId(5))),
            facts(InviteToGroup::new(GroupId(5), Invitee::new(UserId(7)))),
            facts(AcceptGroupInvite::new(GroupId(5))),
            facts(DeclineGroupInvite::new(GroupId(5))),
            facts(RevokeGroupInvite::new(GroupId(5), UserId(7))),
            facts(KickMember::new(GroupId(5), UserId(7))),
            facts(SetMemberRole::new(GroupId(5), UserId(7), GroupRole::Admin)),
            facts(TransferGroup::new(GroupId(5), Invitee::new(UserId(7)))),
            facts(CreateLobby::new(4).with_visibility(LobbyVisibility::Private).with_meta("mode", "ranked")),
            facts(MyLobbies::new()),
            facts(LobbySearch::new().with_filter("mode", "ranked").with_limit(10)),
            facts(JoinLobbyByCode::new("K7M2-Q9XD")),
            facts(GetLobby::new(LobbyId(7))),
            facts(EditLobby::new(LobbyId(7), UpdateLobby::new().with_state(LobbyState::InGame).remove_meta("map"))),
            facts(JoinLobby::new(LobbyId(7))),
            facts(LeaveLobby::new(LobbyId(7))),
            facts(SetLobbyReady::new(LobbyId(7), SetReady::new(true))),
            facts(NewLobbyCode::new(LobbyId(7))),
            facts(TransferLobby::new(LobbyId(7), LobbyPlayer::new(UserId(9)))),
            facts(KickFromLobby::new(LobbyId(7), UserId(9))),
            facts(ListQueues::new()),
            facts(CreateTicket::new("duel").with_attributes(json!({"rating": 1520}))),
            facts(GetTicket::new()),
            facts(CancelTicket::new()),
            facts(UserListQuery::new().with_search("ada")),
            facts(GetUser::new(user)),
            facts(BanUser::new(user, BanRequest::new().with_reason("spam"))),
            facts(UnbanUser::new(user)),
            facts(RevokeSessions::new(user)),
            facts(UnlinkUserIdentity::new(user, "steam")),
            facts(GrantRole::new(user, "moderator")),
            facts(RevokeRole::new(user, "moderator")),
            facts(AuditQuery::new().with_user(user)),
            facts(ListUserObjects::new(user, "saves")),
            facts(GetUserObject::new(user, "saves", "slot-1")),
            facts(WriteUserObject::new(user, "saves", "slot-1", AdminPutObject::new(json!(1)).with_write(WriteAccess::Server))),
            facts(RemoveUserObject::new(user, "saves", "slot-1")),
        ]
    }

    #[test]
    fn every_route_is_the_request_net_backend_client_sends() {
        let calls = every_call();
        assert_eq!(calls.len(), routes::ALL.len());
        for route in routes::ALL {
            assert!(calls.contains(route), "no call checked for {} {}", route.method, route.path);
        }
        // A query value that needs encoding stays one value (the client percent-encodes it).
        let search = request(&UserListQuery::new().with_search("ada & bo=1"));
        assert!(search.query().iter().any(|(name, value)| name == "q" && value == "ada & bo=1"), "{:?}", search.query());
    }

    #[test]
    fn calls_that_cannot_be_sent_are_answered_invalid_and_never_sent() {
        for built in [request(&GetObject::new("saves", "../etc")), request(&GetPlayerObject::new(UserId(1), "a b", "public"))] {
            let error = built.error().cloned();
            assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("escaping")), "{error:?}");
            assert_eq!(error.and_then(|e| e.was_sent()), Some(false));
        }
    }

    #[test]
    fn the_protocol_error_comes_out_of_a_refused_answer() {
        let refusal = ErrorBody::new(ApiError::new(codes::CONFLICT, "the object changed"));
        let body = serde_json::to_vec(&refusal).unwrap_or_else(|e| panic!("{e}"));
        let status = BackendError::Status(Box::new(RawResponse::new(StatusCode::CONFLICT, body)));
        let error = api_error(&status).unwrap_or_else(|| panic!("no ApiError in {status:?}"));
        assert!(error.is(codes::CONFLICT) && error.message == "the object changed");
        // Anything else is not a protocol error: a proxy's page, a timeout, no answer at all.
        let page = BackendError::Status(Box::new(RawResponse::new(StatusCode::BAD_GATEWAY, "<html>bad gateway</html>")));
        for other in [page, BackendError::Timeout("global limit".into()), BackendError::Network("refused".into())] {
            assert_eq!(api_error(&other), None, "{other:?}");
        }
    }
}
