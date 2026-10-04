//! Prints the JSON of every WebSocket frame and a few HTTP bodies, as they go over the wire.
//!
//! ```text
//! cargo run -p net_backend_protocol --example print_frames
//! ```

use net_backend_protocol::auth::{Account, AuthSession, LoginRequest, TokenPair};
use net_backend_protocol::chat::{ChatMessage, JoinRoom, RoomInfo, RoomKind, SendAck, SendMessage};
use net_backend_protocol::storage::{ObjectVersion, PutObject};
use net_backend_protocol::{
    codes, AccessToken, ApiError, CloseCode, ErrorBody, MessageId, RefreshToken, RoomId, UnixMillis, UserId, WsAuth, WsAuthOk, WsPushFrame, WsRequestFrame,
    WsResponseFrame, WsServerFrame,
};
use serde::Serialize;

fn show(label: &str, value: &impl Serialize) {
    match serde_json::to_string(value) {
        Ok(json) => println!("{label:<22} {json}"),
        Err(error) => println!("{label:<22} <not serializable: {error}>"),
    }
}

fn main() {
    let now = UnixMillis(1_790_000_000_000);
    println!("WebSocket");
    println!("{:<22} {}", "auth (client)", WsAuth::new("example-access-token").to_message());
    show("auth.ok", &WsServerFrame::AuthOk(Some(WsAuthOk::new(UserId(42)))));
    show("auth.failed", &WsServerFrame::AuthFailed(ApiError::new(codes::TOKEN_EXPIRED, "the token expired")));
    show("request chat.join", &WsRequestFrame::call(1, JoinRoom::new("world")));
    show("answer", &WsResponseFrame::ok(1, RoomInfo::new(RoomId(12), RoomKind::Room).with_key("world")));
    show("request chat.send", &WsRequestFrame::call(2, SendMessage::new(RoomId(12), "hello")));
    show("answer", &WsResponseFrame::ok(2, SendAck::new(MessageId(981), now)));
    show("refused", &WsResponseFrame::<()>::error(3, ApiError::new(codes::NOT_A_MEMBER, "join the room first")));
    show("push chat.message", &WsPushFrame::push(ChatMessage::new(MessageId(981), RoomId(12), UserId(42), "hello", now).with_sender_name("Ada")));
    let codes: Vec<String> = CloseCode::ALL.iter().map(|c| format!("{c}{}", if c.is_permanent() { " (permanent)" } else { "" })).collect();
    println!("{:<22} {}", "close codes", codes.join(", "));

    println!();
    println!("HTTP");
    show("POST login", &LoginRequest::new("ada@example.com", "example password"));
    let tokens = TokenPair::new(
        AccessToken::new("example-access"),
        now.saturating_add_millis(3_600_000),
        RefreshToken::new("example-refresh"),
        now.saturating_add_millis(30 * 86_400_000),
    );
    show("  → 200", &AuthSession::new(Account::new(UserId(42), now).with_email("ada@example.com", true), tokens));
    show("PUT storage object", &PutObject::new(serde_json::json!({"level": 3})).if_version(ObjectVersion(2)));
    show("  → 409", &ErrorBody::new(ApiError::new(codes::VERSION_CONFLICT, "the object changed").with_details(serde_json::json!({"current_version": 3}))));
}
