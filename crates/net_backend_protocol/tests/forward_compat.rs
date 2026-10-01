//! Forward compatibility: what an older client does with JSON from a newer server (and the other
//! way round). The rule: unknown fields are IGNORED (no type uses `deny_unknown_fields`), optional
//! fields may be absent, unknown enum values decode as `Unknown`, unknown error codes and provider
//! names are kept as text.

use net_backend_protocol::auth::{Account, LinkedIdentity, TokenPair};
use net_backend_protocol::chat::{ChatMessage, RoomInfo, RoomKind, SendAck, SendMessage};
use net_backend_protocol::storage::{StorageObject, WriteAccess};
use net_backend_protocol::{ApiError, Page, RoomId, ServerInfo, UserId, WsAuthOk, WsRequestFrame, WsResponseFrame, WsServerFrame};
use serde::de::DeserializeOwned;

fn decode<T: DeserializeOwned>(json: &str) -> T {
    serde_json::from_str(json).unwrap_or_else(|e| panic!("{json}: {e}"))
}

#[test]
fn unknown_fields_are_ignored() {
    let message: ChatMessage = decode(r#"{"id":1,"room":2,"sender":3,"text":"hi","sent_at":4,"reactions":[],"edited":true}"#);
    assert_eq!(message.text, "hi");
    let ack: SendAck = decode(r#"{"message_id":1,"sent_at":2,"slow_mode":true}"#);
    assert_eq!(ack.message_id.get(), 1);
    let error: ApiError = decode(r#"{"code":"x","message":"m","trace_id":"abc"}"#);
    assert!(error.is("x"));
    let tokens: TokenPair =
        decode(r#"{"token_type":"Bearer","access_token":"a","access_expires_at":1,"refresh_token":"r","refresh_expires_at":2,"scope":"all"}"#);
    assert_eq!(tokens.token_type, "Bearer");
    let info: ServerInfo = decode(r#"{"protocol":3,"min_protocol":1,"modules":[],"motd":"hello"}"#);
    assert!(info.supports(2));
    // Unknown envelope fields too (on both directions).
    let request: WsRequestFrame<SendMessage> = decode(r#"{"id":1,"type":"chat.send","data":{"room":1,"text":"x","reply_to":5},"trace":"t"}"#);
    assert_eq!(request.data.text, "x");
    let answer: WsResponseFrame<SendAck> = decode(r#"{"id":1,"ok":true,"data":{"message_id":1,"sent_at":2},"server_time":3}"#);
    assert!(answer.is_ok());
    assert!(matches!(WsServerFrame::parse(r#"{"type":"auth.ok","data":{"user_id":1,"protocol":2,"session":"s"}}"#), Ok(WsServerFrame::AuthOk(Some(_)))));
}

#[test]
fn admin_types_ignore_unknown_and_absent_fields() {
    use net_backend_protocol::admin::{AdminUser, AuditEntry, BanRequest};
    let user: AdminUser = decode(r#"{"account":{"id":1,"created_at":0},"risk":"low"}"#);
    assert!(user.ban.is_none() && user.active_sessions == 0);
    let entry: AuditEntry = decode(r#"{"id":1,"action":"x","created_at":0,"severity":"info"}"#);
    assert!(entry.actor.is_none());
    let ban: BanRequest = decode("{}");
    assert!(ban.reason.is_none() && ban.until.is_none());
}

#[test]
fn optional_fields_may_be_absent() {
    let account: Account = decode(r#"{"id":5,"created_at":0}"#);
    assert_eq!(account, Account::new(UserId(5), net_backend_protocol::UnixMillis(0)));
    let error: ApiError = decode(r#"{"code":"x"}"#);
    assert!(error.message.is_empty() && error.details.is_none());
    let room: RoomInfo = decode(r#"{"id":1,"kind":"dm"}"#);
    assert_eq!(room, RoomInfo::new(RoomId(1), RoomKind::Dm));
    let page: Page<u8> = decode(r#"{"items":[1]}"#);
    assert!(page.is_last());
    let object: StorageObject = decode(r#"{"collection":"c","key":"k","owner":1,"value":null,"version":1,"updated_at":0}"#);
    assert_eq!(object.write, WriteAccess::Owner);
    // auth.ok with unexpected data still means "accepted" (the client only reads the type).
    assert!(matches!(WsServerFrame::parse(r#"{"type":"auth.ok","data":"yes"}"#), Ok(WsServerFrame::AuthOk(None))));
    let ok: WsAuthOk = decode(r#"{"user_id":1,"protocol":1}"#);
    assert_eq!(ok.user_id, UserId(1));
}

#[test]
fn unknown_values_are_kept_or_mapped() {
    let room: RoomInfo = decode(r#"{"id":1,"kind":"guild_hall"}"#);
    assert_eq!(room.kind, RoomKind::Unknown);
    let object: StorageObject = decode(r#"{"collection":"c","key":"k","owner":1,"value":1,"version":1,"read":"friends","write":"moderators","updated_at":0}"#);
    assert_eq!(object.write, WriteAccess::Unknown);
    let error: ApiError = decode(r#"{"code":"guild_full","message":"m"}"#);
    assert_eq!(error.code, "guild_full");
    let identity: LinkedIdentity = decode(r#"{"provider":"discord","subject":"1"}"#);
    assert_eq!(identity.provider, "discord");
    // A push kind this version does not know is still a push.
    assert!(matches!(WsServerFrame::parse(r#"{"type":"lobby.updated","data":{}}"#), Ok(WsServerFrame::Push(p)) if p.kind == "lobby.updated"));
}

#[test]
fn wrong_types_are_errors() {
    // Ignoring unknown fields never means accepting wrong types for known ones.
    assert!(serde_json::from_str::<ChatMessage>(r#"{"id":"1","room":2,"sender":3,"text":"hi","sent_at":4}"#).is_err());
    assert!(serde_json::from_str::<SendAck>(r#"{"message_id":1}"#).is_err());
    assert!(serde_json::from_str::<WsResponseFrame<SendAck>>(r#"{"id":1,"ok":true,"data":{"nope":1}}"#).is_err());
}
