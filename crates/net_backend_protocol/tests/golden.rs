//! Golden JSON: the exact text of every envelope frame and of the main bodies. These strings are
//! the wire contract; a change here is a protocol change.

use net_backend_protocol::auth::{AccessToken, Account, AuthSession, LoginRequest, RefreshToken, TokenPair};
use net_backend_protocol::chat::{ChatHistory, ChatMessage, JoinRoom, LeaveRoom, MessageDeleted, RoomInfo, RoomKind, SendAck, SendMessage};
use net_backend_protocol::storage::{ObjectVersion, PutObject};
use net_backend_protocol::{
    codes, Ack, ApiError, ErrorBody, MessageId, Page, RoomId, UnixMillis, UserId, WsAuth, WsAuthOk, WsClientFrame, WsPushFrame, WsRequestFrame,
    WsResponseFrame, WsServerFrame,
};
use serde::Serialize;
use serde_json::{json, Value};

const NOW: UnixMillis = UnixMillis(1_790_000_000_000);

fn json_of(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| panic!("serialize: {e}"))
}

fn message() -> ChatMessage {
    ChatMessage::new(MessageId(981), RoomId(12), UserId(42), "hello", NOW).with_sender_name("Ada")
}

#[test]
fn request_frames() {
    assert_eq!(json_of(&WsRequestFrame::call(7, SendMessage::new(RoomId(12), "hello"))), r#"{"id":7,"type":"chat.send","data":{"room":12,"text":"hello"}}"#);
    assert_eq!(json_of(&WsRequestFrame::call(8, JoinRoom::new("world"))), r#"{"id":8,"type":"chat.join","data":{"room":"world"}}"#);
    assert_eq!(json_of(&WsRequestFrame::call(9, LeaveRoom::new(RoomId(12)))), r#"{"id":9,"type":"chat.leave","data":{"room":12}}"#);
    assert_eq!(json_of(&WsRequestFrame::call(10, ChatHistory::new(RoomId(12)))), r#"{"id":10,"type":"chat.history","data":{"room":12}}"#);
    assert_eq!(json_of(&WsRequestFrame::new(u64::MAX, "game.ping", Value::Null)), r#"{"id":18446744073709551615,"type":"game.ping","data":null}"#);
}

#[test]
fn request_frames_decode() {
    let frame = WsClientFrame::parse(r#"{"id":7,"type":"chat.send","data":{"room":12,"text":"hello"}}"#).unwrap_or_else(|e| panic!("{e}"));
    let WsClientFrame::Request(request) = frame else { panic!("not a request") };
    assert_eq!((request.id, request.kind.as_str()), (7, "chat.send"));
    assert_eq!(request.data_as::<SendMessage>().ok(), Some(SendMessage::new(RoomId(12), "hello")));
    // Typed decoding, key order irrelevant, missing data = null.
    let typed: WsRequestFrame<SendMessage> =
        serde_json::from_str(r#"{"data":{"text":"hello","room":12},"type":"chat.send","id":7}"#).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(typed, WsRequestFrame::call(7, SendMessage::new(RoomId(12), "hello")));
    let no_data: WsRequestFrame = serde_json::from_str(r#"{"id":1,"type":"game.ping"}"#).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(no_data.data, Value::Null);
    let big = WsClientFrame::parse(r#"{"id":18446744073709551615,"type":"x","data":1}"#).ok();
    assert!(matches!(big, Some(WsClientFrame::Request(r)) if r.id == u64::MAX));
    // Not answerable: no id, a negative / fractional / string id, no type.
    for bad in
        [r#"{"type":"chat.send","data":{}}"#, r#"{"id":-1,"type":"x"}"#, r#"{"id":1.5,"type":"x"}"#, r#"{"id":"1","type":"x"}"#, r#"{"id":1}"#, "[]", "plain"]
    {
        assert!(WsClientFrame::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn response_frames() {
    assert_eq!(json_of(&WsResponseFrame::ok(7, SendAck::new(MessageId(981), NOW))), r#"{"id":7,"ok":true,"data":{"message_id":981,"sent_at":1790000000000}}"#);
    assert_eq!(json_of(&WsResponseFrame::ok(9, Ack::new())), r#"{"id":9,"ok":true,"data":{}}"#);
    assert_eq!(
        json_of(&WsResponseFrame::ok(8, RoomInfo::new(RoomId(12), RoomKind::Room).with_key("world"))),
        r#"{"id":8,"ok":true,"data":{"id":12,"kind":"room","key":"world"}}"#
    );
    assert_eq!(
        json_of(&WsResponseFrame::<Value>::error(7, ApiError::new(codes::NOT_A_MEMBER, "join the room first"))),
        r#"{"id":7,"ok":false,"error":{"code":"not_a_member","message":"join the room first"}}"#
    );
    assert_eq!(
        json_of(&WsResponseFrame::<Value>::error(u64::MAX, ApiError::new(codes::RATE_LIMITED, "slow down").with_details(json!({"retry_after_ms": 1500})))),
        r#"{"id":18446744073709551615,"ok":false,"error":{"code":"rate_limited","message":"slow down","details":{"retry_after_ms":1500}}}"#
    );
    assert_eq!(
        json_of(&WsResponseFrame::ok(10, Page::new(vec![message()], None))),
        r#"{"id":10,"ok":true,"data":{"items":[{"id":981,"room":12,"sender":42,"sender_name":"Ada","text":"hello","sent_at":1790000000000}]}}"#
    );
}

#[test]
fn response_frames_decode() {
    let ok: WsResponseFrame<SendAck> =
        serde_json::from_str(r#"{"id":7,"ok":true,"data":{"message_id":981,"sent_at":1790000000000}}"#).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(ok, WsResponseFrame::ok(7, SendAck::new(MessageId(981), NOW)));
    // `ok` defaults to true when the frame has no `type` (the client's rule).
    let implicit: WsResponseFrame<Value> = serde_json::from_str(r#"{"id":7,"data":3}"#).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(implicit, WsResponseFrame::ok(7, json!(3)));
    let missing_data: WsResponseFrame<Ack> = serde_json::from_str(r#"{"id":7,"ok":true}"#).unwrap_or_else(|e| panic!("{e}"));
    assert!(missing_data.is_ok());
    let error: WsResponseFrame<SendAck> =
        serde_json::from_str(r#"{"id":7,"ok":false,"error":{"code":"room_full","message":"full"}}"#).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(error.result, Err(ApiError::new(codes::ROOM_FULL, "full")));
    // A push is not an answer.
    assert!(serde_json::from_str::<WsResponseFrame>(r#"{"type":"chat.message","id":3,"data":1}"#).is_err());
}

#[test]
fn push_frames() {
    assert_eq!(
        json_of(&WsPushFrame::push(message())),
        r#"{"type":"chat.message","data":{"id":981,"room":12,"sender":42,"sender_name":"Ada","text":"hello","sent_at":1790000000000}}"#
    );
    assert_eq!(json_of(&WsPushFrame::push(MessageDeleted::new(MessageId(981), RoomId(12)))), r#"{"type":"chat.deleted","data":{"id":981,"room":12}}"#);
    let push: WsPushFrame<ChatMessage> =
        serde_json::from_str(r#"{"type":"chat.message","data":{"id":981,"room":12,"sender":42,"sender_name":"Ada","text":"hello","sent_at":1790000000000}}"#)
            .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(push, WsPushFrame::push(message()));
    // A push may carry its own id as long as it has no `ok` (the client's rule).
    assert!(serde_json::from_str::<WsPushFrame>(r#"{"type":"tick","id":5,"data":1}"#).is_ok());
    for not_push in [r#"{"type":"tick","id":5,"ok":true}"#, r#"{"type":"auth.ok"}"#, r#"{"data":1}"#] {
        assert!(serde_json::from_str::<WsPushFrame>(not_push).is_err(), "{not_push}");
    }
}

#[test]
fn auth_frames() {
    assert_eq!(WsAuth::new("tok-example").to_message(), r#"{"type":"auth","data":{"token":"tok-example","protocol":1}}"#);
    assert_eq!(json_of(&WsServerFrame::AuthOk(Some(WsAuthOk::new(UserId(42))))), r#"{"type":"auth.ok","data":{"user_id":42,"protocol":1}}"#);
    assert_eq!(json_of(&WsServerFrame::AuthOk(None)), r#"{"type":"auth.ok"}"#);
    assert_eq!(
        json_of(&WsServerFrame::AuthFailed(ApiError::new(codes::UNAUTHORIZED, "the token expired"))),
        r#"{"type":"auth.failed","error":{"code":"unauthorized","message":"the token expired"}}"#
    );
    let auth = WsClientFrame::parse(r#"{"type":"auth","data":{"token":"tok-example"}}"#).ok();
    assert!(matches!(&auth, Some(WsClientFrame::Auth(a)) if a.token.expose() == "tok-example" && a.protocol.is_none()));
    assert_eq!(json_of(&WsClientFrame::Auth(WsAuth::new("t"))), r#"{"type":"auth","data":{"token":"t","protocol":1}}"#);
    // A request whose kind is "auth" (it has an id) is a request.
    assert!(matches!(WsClientFrame::parse(r#"{"id":1,"type":"auth","data":{}}"#), Ok(WsClientFrame::Request(_))));
}

/// Every server frame decodes back to itself through `WsServerFrame` (the client's rules).
#[test]
fn server_frames_round_trip() {
    let frames = [
        WsServerFrame::Response(WsResponseFrame::ok(1, json!({"a": 1}))),
        WsServerFrame::Response(WsResponseFrame::error(2, ApiError::new(codes::INTERNAL, "oops"))),
        WsServerFrame::Push(WsPushFrame::new("chat.message", json!({"text": "x"}))),
        WsServerFrame::Push(WsPushFrame::new("game.tick", Value::Null)),
        WsServerFrame::AuthOk(Some(WsAuthOk::new(UserId(1)))),
        WsServerFrame::AuthOk(None),
        WsServerFrame::AuthFailed(ApiError::new(codes::BANNED, "banned")),
    ];
    for frame in frames {
        let text = frame.to_json().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(WsServerFrame::parse(&text).ok(), Some(frame), "{text}");
    }
    for ignored in ["plain", "[1]", r#"{"id":"x"}"#, r#"{"data":1}"#] {
        assert!(WsServerFrame::parse(ignored).is_err(), "{ignored}");
    }
}

#[test]
fn http_bodies() {
    assert_eq!(json_of(&LoginRequest::new("ada@example.com", "pw-example")), r#"{"email":"ada@example.com","password":"pw-example"}"#);
    let tokens = TokenPair::new(AccessToken::new("a1"), UnixMillis(10), RefreshToken::new("r1"), UnixMillis(20));
    assert_eq!(
        json_of(&AuthSession::new(Account::new(UserId(42), UnixMillis(5)).with_email("ada@example.com", true).with_display_name("Ada"), tokens)),
        concat!(
            r#"{"account":{"id":42,"email":"ada@example.com","email_verified":true,"display_name":"Ada","roles":[],"identities":[],"created_at":5},"#,
            r#""tokens":{"token_type":"Bearer","access_token":"a1","access_expires_at":10,"refresh_token":"r1","refresh_expires_at":20}}"#
        )
    );
    assert_eq!(json_of(&PutObject::new(json!({"level": 3})).if_version(ObjectVersion(2))), r#"{"value":{"level":3},"if_version":2}"#);
    assert_eq!(json_of(&ErrorBody::new(ApiError::new(codes::NOT_FOUND, "no such object"))), r#"{"error":{"code":"not_found","message":"no such object"}}"#);
}

/// S2: a malformed request that HAS an id keeps it, so the server can answer `bad_request`.
#[test]
fn malformed_requests_keep_their_id() {
    for (frame, id) in [
        (r#"{"id":5,"type":7}"#, Some(5)),
        (r#"{"id":5,"data":{}}"#, Some(5)),
        (r#"{"id":5,"type":null,"data":1}"#, Some(5)),
        (r#"{"id":18446744073709551615,"type":["x"]}"#, Some(u64::MAX)),
        (r#"{"id":-5,"type":"x"}"#, None),
        (r#"{"id":"5","type":"x"}"#, None),
        (r#"{"type":"chat.send"}"#, None),
        ("not json", None),
        ("[5]", None),
    ] {
        let error = WsClientFrame::parse(frame).err().unwrap_or_else(|| panic!("{frame} parsed"));
        assert_eq!(error.id, id, "{frame}: {error}");
        assert_eq!(WsClientFrame::request_id(frame), id, "{frame}");
    }
    let answer = WsClientFrame::parse(r#"{"id":5,"type":7}"#).err().and_then(|e| e.answer()).unwrap_or_else(|| panic!("no answer"));
    assert_eq!(json_of(&answer), r#"{"id":5,"ok":false,"error":{"code":"bad_request","message":"the request is malformed"}}"#);
    // A well-formed envelope whose data does not fit the kind: still a request with its id.
    let Ok(WsClientFrame::Request(request)) = WsClientFrame::parse(r#"{"id":6,"type":"chat.send","data":{"room":"x"}}"#) else {
        panic!("not a request");
    };
    assert!(request.data_as::<SendMessage>().is_err());
    assert_eq!(request.id, 6);
}

/// N1: the auth kinds cannot be built as pushes through the checked constructor.
#[test]
fn reserved_kinds_are_not_pushes() {
    for kind in ["auth", "auth.ok", "auth.failed", ""] {
        assert!(WsPushFrame::checked(kind, Value::Null).is_none(), "{kind}");
    }
    assert!(WsPushFrame::checked("game.tick", Value::Null).is_some());
}

/// S7: the details of a batch version conflict name the failing item.
#[test]
fn version_conflict_details() {
    use net_backend_protocol::storage::VersionConflict;
    let body = ErrorBody::new(VersionConflict::new(Some(ObjectVersion(3))).at_index(2).into_error());
    // `details` is a JSON value (key order not part of the contract): compare as values.
    let expected: Value = serde_json::from_str(
        r#"{"error":{"code":"version_conflict","message":"the object's version is not the expected one","details":{"index":2,"current_version":3}}}"#,
    )
    .unwrap_or_default();
    assert_eq!(serde_json::to_value(&body).ok(), Some(expected));
    assert_eq!(json_of(&VersionConflict::new(Some(ObjectVersion(3))).at_index(2)), r#"{"index":2,"current_version":3}"#);
}
