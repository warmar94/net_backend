//! Every serializable type survives JSON → value → JSON unchanged (and equals itself where it has
//! `PartialEq`).

use std::fmt::Debug;

use net_backend_protocol::admin::*;
use net_backend_protocol::auth::*;
use net_backend_protocol::chat::*;
use net_backend_protocol::storage::*;
use net_backend_protocol::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

/// JSON → value → JSON gives the same text.
fn same_json<T: Serialize + DeserializeOwned>(value: &T) -> T {
    let first = serde_json::to_string(value).unwrap_or_else(|e| panic!("serialize: {e}"));
    let back: T = serde_json::from_str(&first).unwrap_or_else(|e| panic!("deserialize {first}: {e}"));
    let second = serde_json::to_string(&back).unwrap_or_else(|e| panic!("serialize again: {e}"));
    assert_eq!(first, second);
    back
}

/// Also equal as values.
fn same<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: T) {
    assert_eq!(same_json(&value), value);
}

const T0: UnixMillis = UnixMillis(1_790_000_000_000);

#[test]
fn ids_time_pages() {
    same(UserId(1));
    same(RoomId(-5));
    same(MessageId(i64::MAX));
    same(UnixMillis(i64::MIN));
    same(Cursor::new("opaque=="));
    same(PageRequest::first());
    same(PageRequest::after(Cursor::new("c")).with_limit(20));
    same(Page::new(vec![UserId(1), UserId(2)], Some(Cursor::new("next"))));
    same(Page::<UserId>::new(vec![], None));
    same(ServerInfo::new(vec!["auth".into(), "chat".into()]).with_versions(1, 2));
}

#[test]
fn errors() {
    same(ApiError::new(codes::NOT_FOUND, "missing"));
    same(ApiError::new(codes::RATE_LIMITED, "").with_details(json!({"retry_after_ms": 10})));
    same(ErrorBody::new(ApiError::new("game_specific", "custom")));
    let mut details = ValidationDetails::new();
    details.add("email", "is not an email address");
    details.add("email", "is too long");
    same(details.clone());
    same(ApiError::validation(details));
}

#[test]
fn auth_types() {
    same_json(&RegisterRequest::new("a@example.com", "long enough pw").with_display_name("Ada"));
    same_json(&RegisterRequest::new("a@example.com", "long enough pw"));
    same_json(&LoginRequest::new("a@example.com", "pw"));
    same_json(&SteamLoginRequest::new("0a0b", "my-game"));
    same_json(&RefreshRequest::new("r"));
    same_json(&LogoutRequest::this_session());
    same_json(&LogoutRequest::everywhere().with_refresh_token("r"));
    same_json(&TokenPair::new(AccessToken::new("a"), T0, RefreshToken::new("r"), T0));
    same(LinkedIdentity::new(auth::provider::STEAM, "76500000000000001"));
    let account = Account::new(UserId(7), T0)
        .with_email("a@example.com", false)
        .with_display_name("Ada")
        .with_roles(vec!["admin".into()])
        .with_identities(vec![LinkedIdentity::new("steam", "76500000000000001")]);
    same(account.clone());
    same(Account::new(UserId(8), T0));
    same_json(&AuthSession::new(account, TokenPair::new(AccessToken::new("a"), T0, RefreshToken::new("r"), T0)));
    same(UpdateAccountRequest::new().with_display_name("Bea"));
    same(UpdateAccountRequest::new());
    same_json(&ChangePasswordRequest::new("old", "new password here"));
    same_json(&VerifyEmailRequest::new("token"));
    same(ForgotPasswordRequest::new("a@example.com"));
    same_json(&ResetPasswordRequest::new("token", "new password here"));
    same_json(&Password::new("p"));
    same_json(&Secret::new("s"));
}

#[test]
fn admin_types() {
    let account = Account::new(UserId(7), T0).with_email("a@example.com", true).with_roles(vec![ADMIN_ROLE.into()]);
    same(BanInfo::new(T0));
    same(BanInfo::new(T0).with_until(UnixMillis(T0.get() + 1)).with_reason("cheating"));
    same(AdminUser::new(account.clone()));
    same(AdminUser::new(account).with_ban(BanInfo::new(T0)).with_last_seen_at(T0).with_active_sessions(2));
    same(UserListQuery::new());
    same(UserListQuery::new().with_search("ada").with_cursor(Cursor::new("c")).with_limit(10));
    same(BanRequest::new());
    same(BanRequest::new().with_reason("x").with_until(T0));
    same(AuditEntry::new(1, "admin.ban", T0));
    same(
        AuditEntry::new(2, "auth.login", T0)
            .with_actor(UserId(3))
            .with_target("user", "3")
            .with_ip("203.0.113.9")
            .with_request_id("r-1")
            .with_data(json!({"method": "password"})),
    );
    same(AuditQuery::new().with_user(UserId(3)).with_action("admin.").with_cursor(Cursor::new("c")).with_limit(5));
    // Query strings (what an HTTP client sends) are the same fields.
    assert_eq!(serde_json::to_value(UserListQuery::new().with_search("ada").with_limit(2)).ok(), Some(json!({"q": "ada", "limit": 2})));
    let page = Page::new(vec![AuditEntry::new(1, "x", T0)], Some(Cursor::new("n")));
    same(page);
}

#[test]
fn storage_types() {
    let object = StorageObject::new("saves", "slot-1", UserId(3), json!({"hp": 10, "pos": [1.5, 2.0]}), ObjectVersion(4), T0).with_write(WriteAccess::Server);
    same(object.clone());
    same(PutObject::new(json!({"a": [1, 2]})));
    same(PutObject::new(Value::Null).if_absent());
    same(object.info());
    same(StorageObjectInfo::new("saves", "slot-2", ObjectVersion(1), 12, T0));
    same(VersionConflict::new(Some(ObjectVersion(3))).at_index(1));
    same(VersionConflict::new(None));
    same(DeleteObject::new());
    same(DeleteObject::new().if_version(ObjectVersion(2)));
    same(ObjectAck::new("saves", "slot-1", ObjectVersion(5), T0));
    same(ObjectRef::new("saves", "slot-1"));
    same(BatchGet::new(vec![ObjectRef::new("saves", "a"), ObjectRef::new("saves", "b")]));
    same(BatchObjects::new(vec![object]));
    same(BatchPutItem::new("saves", "a", PutObject::new(json!(1)).if_version(ObjectVersion(1))));
    same(BatchPut::new(vec![BatchPutItem::new("saves", "a", PutObject::new(json!("x")))]));
    same(BatchAcks::new(vec![ObjectAck::new("saves", "a", ObjectVersion(1), T0)]));
    same(ObjectVersion::ABSENT);
    same(AdminPutObject::new(json!({"gold": 5})).if_version(ObjectVersion(2)).with_write(WriteAccess::Server));
    same(AdminPutObject::new(json!(null)));
    same(NoPayload::new());
    for access in [WriteAccess::Owner, WriteAccess::Server] {
        same(access);
    }
}

#[test]
fn chat_types() {
    same(RoomRef::from(RoomId(3)));
    same(RoomRef::from("world"));
    for kind in [RoomKind::Room, RoomKind::Dm, RoomKind::Group] {
        same(kind);
    }
    same(RoomInfo::new(RoomId(1), RoomKind::Dm).with_peer(UserId(8)));
    same(RoomInfo::new(RoomId(1), RoomKind::Room).with_key("world").with_name("World").with_members(Some(10), Some(200)));
    same(JoinRoom::new("world"));
    same(JoinRoom::new(RoomId(4)));
    same(LeaveRoom::new(RoomId(4)));
    same(SendMessage::new(RoomId(4), "hi \"there\" \u{1F600}"));
    same(SendMessage::new(RoomId(4), "x").with_nonce("n-1"));
    same(ChatMessage::new(MessageId(1), RoomId(4), UserId(2), "x", T0).with_nonce("n-1"));
    same(SendAck::new(MessageId(1), T0));
    same(ChatHistory::new(RoomId(4)).with_page(PageRequest::after(Cursor::new("c")).with_limit(5)));
    same(ChatMessage::new(MessageId(1), RoomId(4), UserId(2), "hi", T0));
    same(ChatMessage::new(MessageId(1), RoomId(4), UserId(2), "hi", T0).with_sender_name("Ada"));
    same(MessageDeleted::new(MessageId(1), RoomId(4)));
    same(OpenDirect::new(UserId(9)));
    same(ListMembers::new(RoomId(4)));
    same(RoomMember::new(UserId(2)).with_name("Ada"));
    same(RoomMembers::new(RoomId(4), vec![RoomMember::new(UserId(2))], 1));
    same(RoomMembers::new(RoomId(4), vec![], 500).truncated());
    for event in [PresenceEvent::Joined, PresenceEvent::Left] {
        same(Presence::new(RoomId(4), UserId(2), event).with_name("Ada").with_count(3));
    }
    same(Presence::new(RoomId(4), UserId(2), PresenceEvent::Left));
}

#[test]
fn envelope_types() {
    same(Ack::new());
    same_json(&WsAuth::new("tok"));
    same(WsAuthOk::new(UserId(1)));
    same(WsRequestFrame::new(1, "x", json!({"k": [1, 2.5, null]})));
    same(WsRequestFrame::call(2, SendMessage::new(RoomId(1), "hi")));
    same(WsResponseFrame::ok(3, json!(null)));
    same(WsResponseFrame::ok(3, SendAck::new(MessageId(1), T0)));
    same(WsResponseFrame::<SendAck>::error(4, ApiError::new(codes::INTERNAL, "x")));
    same(WsPushFrame::new("game.tick", json!(1)));
    same(WsPushFrame::push(MessageDeleted::new(MessageId(1), RoomId(1))));
    same(WsServerFrame::AuthOk(None));
    same_json(&WsClientFrame::Request(WsRequestFrame::new(5, "chat.leave", json!({"room": 1}))));
    same_json(&WsClientFrame::Auth(WsAuth::new("tok")));
}

#[test]
fn typed_and_untyped_frames_agree() {
    let typed = WsRequestFrame::call(9, SendMessage::new(RoomId(1), "hi"));
    let untyped: WsRequestFrame = serde_json::from_str(&serde_json::to_string(&typed).unwrap_or_default()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(untyped.data_as::<SendMessage>().ok(), Some(typed.data));
    let answer = WsResponseFrame::ok_serialize(9, &SendAck::new(MessageId(3), T0)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(answer.decode::<SendAck>().ok(), Some(Ok(SendAck::new(MessageId(3), T0))));
    let push = WsPushFrame::new("chat.deleted", json!({"id": 1, "room": 2}));
    assert_eq!(push.data_as::<MessageDeleted>().ok(), Some(MessageDeleted::new(MessageId(1), RoomId(2))));
}
