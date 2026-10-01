//! OpenAPI / AsyncAPI schemas of the protocol's chat types (mirror structs: the protocol crate has
//! no OpenAPI dependency). A test serializes the real types and compares the field names with
//! these schemas, so the documents cannot drift from the wire format.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::ToSchema;

/// A room.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoomInfo {
    /// The id.
    id: i64,
    /// `room` (public), `dm` or `group`.
    kind: String,
    /// The key of a public room.
    key: Option<String>,
    /// A display name.
    name: Option<String>,
    /// Users online in the room now (this server instance).
    member_count: Option<u32>,
    /// The member cap (connections).
    max_members: Option<u32>,
    /// For a direct-message room: the other user.
    peer: Option<i64>,
}

/// A page of rooms.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoomInfoPage {
    /// The rooms.
    items: Vec<RoomInfo>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// A chat message (the `chat.message` push and the history items).
#[derive(Serialize, ToSchema)]
pub(crate) struct ChatMessage {
    /// The message id (grows over time).
    id: i64,
    /// The room.
    room: i64,
    /// The sender's user id.
    sender: i64,
    /// The sender's display name at sending time.
    sender_name: Option<String>,
    /// The text (after the server's hooks).
    text: String,
    /// When it was stored (unix ms).
    sent_at: i64,
    /// The sender's nonce, echoed.
    nonce: Option<String>,
}

/// A page of history, newest first.
#[derive(Serialize, ToSchema)]
pub(crate) struct ChatMessagePage {
    /// The messages.
    items: Vec<ChatMessage>,
    /// Pass as `cursor` for older messages; absent on the last page.
    next_cursor: Option<String>,
}

/// `POST /v1/chat/dm` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct OpenDirect {
    /// The other user.
    user: i64,
}

/// `chat.join` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct JoinRoom {
    /// A room id (number) or a public room's key (string).
    #[schema(value_type = Object)]
    room: serde_json::Value,
}

/// `chat.leave` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct LeaveRoom {
    /// The room id.
    room: i64,
}

/// `chat.send` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct SendMessage {
    /// The room id (joined, or a DM room of the sender).
    room: i64,
    /// The text (not blank, at most `max_text_chars`).
    text: String,
    /// A client tag (1-64 visible ASCII characters), echoed in the `chat.message` push.
    nonce: Option<String>,
}

/// The `chat.send` answer (sent before the sender's own `chat.message`).
#[derive(Serialize, ToSchema)]
pub(crate) struct SendAck {
    /// The stored message's id.
    message_id: i64,
    /// When it was stored (unix ms).
    sent_at: i64,
}

/// `chat.history` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct ChatHistory {
    /// The room id.
    room: i64,
    /// The previous page's next_cursor.
    cursor: Option<String>,
    /// 1-100, default 50.
    limit: Option<u32>,
}

/// `chat.members` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct ListMembers {
    /// The room id.
    room: i64,
}

/// One online member.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoomMember {
    /// The user id.
    user: i64,
    /// The display name.
    name: Option<String>,
}

/// The `chat.members` answer.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoomMembers {
    /// The room id.
    room: i64,
    /// Up to 200 online users.
    members: Vec<RoomMember>,
    /// How many users are online.
    count: u32,
    /// Whether `members` is cut.
    truncated: Option<bool>,
}

/// The `chat.deleted` push.
#[derive(Serialize, ToSchema)]
pub(crate) struct MessageDeleted {
    /// The message id.
    id: i64,
    /// The room id.
    room: i64,
}

/// The `chat.presence` push.
#[derive(Serialize, ToSchema)]
pub(crate) struct Presence {
    /// The room id.
    room: i64,
    /// The user id.
    user: i64,
    /// `joined` or `left`.
    event: String,
    /// The user's display name.
    name: Option<String>,
    /// Users online in the room now.
    count: Option<u32>,
}

/// An empty success answer: `{}`.
#[derive(Serialize, ToSchema)]
pub(crate) struct Ack {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::chat as p;
    use net_backend_protocol::{Cursor, MessageId, Page, PageRequest, RoomId, UnixMillis, UserId};
    use serde_json::Value;
    use utoipa::openapi::schema::Schema;
    use utoipa::openapi::RefOr;
    use utoipa::PartialSchema;

    use super::*;

    fn properties<T: PartialSchema>() -> BTreeSet<String> {
        match T::schema() {
            RefOr::T(Schema::Object(object)) => object.properties.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    fn keys(value: impl serde::Serialize) -> BTreeSet<String> {
        match serde_json::to_value(value) {
            Ok(Value::Object(map)) => map.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    /// Every mirror has exactly the fields of the real type with all optional fields set.
    #[test]
    fn mirrors_match_the_protocol() {
        let t = UnixMillis(1);
        let room = p::RoomInfo::new(RoomId(1), p::RoomKind::Dm).with_key("k").with_name("n").with_members(Some(1), Some(2)).with_peer(UserId(3));
        assert_eq!(properties::<RoomInfo>(), keys(&room));
        assert_eq!(properties::<RoomInfoPage>(), keys(Page::new(vec![room], Some(Cursor::new("c")))));
        let message = p::ChatMessage::new(MessageId(1), RoomId(1), UserId(1), "x", t).with_sender_name("A").with_nonce("n");
        assert_eq!(properties::<ChatMessage>(), keys(&message));
        assert_eq!(properties::<ChatMessagePage>(), keys(Page::new(vec![message], Some(Cursor::new("c")))));
        assert_eq!(properties::<OpenDirect>(), keys(p::OpenDirect::new(UserId(1))));
        assert_eq!(properties::<JoinRoom>(), keys(p::JoinRoom::new("world")));
        assert_eq!(properties::<LeaveRoom>(), keys(p::LeaveRoom::new(RoomId(1))));
        assert_eq!(properties::<SendMessage>(), keys(p::SendMessage::new(RoomId(1), "x").with_nonce("n")));
        assert_eq!(properties::<SendAck>(), keys(p::SendAck::new(MessageId(1), t)));
        assert_eq!(properties::<ChatHistory>(), keys(p::ChatHistory::new(RoomId(1)).with_page(PageRequest::after(Cursor::new("c")).with_limit(5))));
        assert_eq!(properties::<ListMembers>(), keys(p::ListMembers::new(RoomId(1))));
        assert_eq!(properties::<RoomMember>(), keys(p::RoomMember::new(UserId(1)).with_name("A")));
        assert_eq!(properties::<RoomMembers>(), keys(p::RoomMembers::new(RoomId(1), vec![], 1).truncated()));
        assert_eq!(properties::<MessageDeleted>(), keys(p::MessageDeleted::new(MessageId(1), RoomId(1))));
        assert_eq!(properties::<Presence>(), keys(p::Presence::new(RoomId(1), UserId(1), p::PresenceEvent::Joined).with_name("A").with_count(1)));
    }
}
