//! Compatibility with the published client `bevy_net_backend` 0.1.x (feature `bevy_net_backend`):
//! frames this crate builds go through the client's own `JsonEnvelope` (its real encoder and
//! decoder) and come out as this crate's types, in both directions.

use bevy_net_backend::{Credentials, JsonEnvelope, OutgoingRequest, Rejection, WsFrame, WsIncoming, WsProtocol, WsPushMessage, WsRequest};
use net_backend_protocol::chat::{ChatHistory, ChatMessage, JoinRoom, LeaveRoom, MessageDeleted, RoomInfo, RoomKind, SendAck, SendMessage};
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
