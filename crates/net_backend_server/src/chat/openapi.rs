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
    /// `room` (public), `dm`, `group` or `player` (created by a player).
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
    /// For a player room: `public` or `private`.
    visibility: Option<String>,
    /// For a player room: its owner.
    owner: Option<i64>,
    /// For a player room: the caller's role (`owner`, `moderator`, `member`, `invited`, `banned`).
    role: Option<String>,
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
    /// When the text was last edited (unix ms); absent: never.
    edited_at: Option<i64>,
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

/// `PATCH /v1/chat/rooms/{room}/messages/{message}` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct MessageEdit {
    /// The new text (the rules of a new message).
    text: String,
}

/// `chat.edit` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct EditMessage {
    /// The room id.
    room: i64,
    /// The message id.
    message: i64,
    /// The new text.
    text: String,
}

/// The `chat.edited` push.
#[derive(Serialize, ToSchema)]
pub(crate) struct MessageEdited {
    /// The message id.
    id: i64,
    /// The room id.
    room: i64,
    /// The new text.
    text: String,
    /// When (unix ms).
    edited_at: i64,
    /// Who edited it (absent: server code).
    edited_by: Option<i64>,
}

/// `PUT /v1/chat/rooms/{room}/read` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct ReadUpTo {
    /// The newest message read (of the room).
    message: i64,
}

/// `chat.mark_read` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct MarkRead {
    /// The room id.
    room: i64,
    /// The newest message read (of the room).
    message: i64,
}

/// A read marker (the `chat.read` push and the receipts).
#[derive(Serialize, ToSchema)]
pub(crate) struct ReadReceipt {
    /// The room id.
    room: i64,
    /// Who read.
    user: i64,
    /// The newest message read.
    message: i64,
    /// When the marker was stored (unix ms).
    read_at: i64,
}

/// `chat.receipts` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct ListReceipts {
    /// The room id.
    room: i64,
}

/// The read markers of a room.
#[derive(Serialize, ToSchema)]
pub(crate) struct ReadReceipts {
    /// The room id.
    room: i64,
    /// Up to 200 markers, the newest first.
    receipts: Vec<ReadReceipt>,
}

/// `POST /v1/chat/unread` body and `chat.unread` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct UnreadQuery {
    /// 1 to 100 room ids.
    rooms: Vec<i64>,
}

/// One room's unread count.
#[derive(Serialize, ToSchema)]
pub(crate) struct UnreadCount {
    /// The room id.
    room: i64,
    /// Unread messages, at most 1000 ("this many or more").
    unread: u32,
    /// The caller's marker.
    last_read: Option<i64>,
}

/// The unread counts.
#[derive(Serialize, ToSchema)]
pub(crate) struct UnreadCounts {
    /// The rooms the caller can read, in the order asked.
    rooms: Vec<UnreadCount>,
}

/// `chat.set_typing` data.
#[derive(Serialize, ToSchema)]
pub(crate) struct SetTyping {
    /// The room id (joined on this connection, or a DM room).
    room: i64,
    /// Typing or stopped.
    typing: bool,
}

/// The `chat.typing` push.
#[derive(Serialize, ToSchema)]
pub(crate) struct TypingUpdate {
    /// The room id.
    room: i64,
    /// Who types.
    user: i64,
    /// Typing or stopped.
    typing: bool,
    /// How long a client shows the indicator without a new push (0 when stopped).
    expires_in_ms: u32,
}

/// `POST /v1/chat/rooms` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct CreateRoom {
    /// The name (1-64 characters).
    name: String,
    /// `public` or `private` (default).
    visibility: Option<String>,
}

/// `PATCH /v1/chat/rooms/{room}` body (absent fields stay).
#[derive(Serialize, ToSchema)]
pub(crate) struct UpdateRoom {
    /// A new name (owner, moderators).
    name: Option<String>,
    /// `public` or `private` (owner).
    visibility: Option<String>,
}

/// A player: the invite and hand-on body.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoomUser {
    /// The user id.
    user: i64,
}

/// `PUT /v1/chat/rooms/{room}/members/{user}/role` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoomRoleChange {
    /// `moderator` or `member`.
    role: String,
}

/// One row of a player room.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoomMembership {
    /// The user id.
    user: i64,
    /// The display name.
    name: Option<String>,
    /// `owner`, `moderator`, `member`, `invited` or `banned`.
    role: String,
    /// Since when (unix ms).
    since: i64,
}

/// A page of a player room's rows.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoomMembershipPage {
    /// The rows, oldest first.
    items: Vec<RoomMembership>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// The `chat.room` push.
#[derive(Serialize, ToSchema)]
pub(crate) struct RoomUpdate {
    /// The room id.
    room: i64,
    /// `updated`, `deleted`, `invited`, `joined`, `left`, `kicked`, `role` or `owner`.
    change: String,
    /// The player the change is about.
    user: Option<i64>,
    /// Who made it (absent: server code).
    by: Option<i64>,
    /// The new role (`role`, `owner`).
    role: Option<String>,
    /// The room after the change (`updated`).
    info: Option<RoomInfo>,
}

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
        let room = p::RoomInfo::new(RoomId(1), p::RoomKind::Dm)
            .with_key("k")
            .with_name("n")
            .with_members(Some(1), Some(2))
            .with_peer(UserId(3))
            .with_visibility(p::RoomVisibility::Public)
            .with_owner(UserId(3))
            .with_role(p::RoomRole::Owner);
        assert_eq!(properties::<RoomInfo>(), keys(&room));
        assert_eq!(properties::<RoomInfoPage>(), keys(Page::new(vec![room.clone()], Some(Cursor::new("c")))));
        let message = p::ChatMessage::new(MessageId(1), RoomId(1), UserId(1), "x", t).with_sender_name("A").with_nonce("n").with_edited_at(t);
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
        assert_eq!(properties::<MessageEdit>(), keys(p::MessageEdit::new("x")));
        assert_eq!(properties::<EditMessage>(), keys(p::EditMessage::new(RoomId(1), MessageId(2), "x")));
        assert_eq!(properties::<MessageEdited>(), keys(p::MessageEdited::new(MessageId(1), RoomId(1), "x", t).by(UserId(1))));
        assert_eq!(properties::<ReadUpTo>(), keys(p::ReadUpTo::new(MessageId(1))));
        assert_eq!(properties::<MarkRead>(), keys(p::MarkRead::new(RoomId(1), MessageId(1))));
        let receipt = p::ReadReceipt::new(RoomId(1), UserId(1), MessageId(1), t);
        assert_eq!(properties::<ReadReceipt>(), keys(receipt));
        assert_eq!(properties::<ListReceipts>(), keys(p::ListReceipts::new(RoomId(1))));
        assert_eq!(properties::<ReadReceipts>(), keys(p::ReadReceipts::new(RoomId(1), vec![receipt])));
        assert_eq!(properties::<UnreadQuery>(), keys(p::UnreadQuery::new(vec![RoomId(1)])));
        let count = p::UnreadCount::new(RoomId(1), 3, Some(MessageId(1)));
        assert_eq!(properties::<UnreadCount>(), keys(count));
        assert_eq!(properties::<UnreadCounts>(), keys(p::UnreadCounts::new(vec![count])));
        assert_eq!(properties::<SetTyping>(), keys(p::SetTyping::started(RoomId(1))));
        assert_eq!(properties::<TypingUpdate>(), keys(p::TypingUpdate::new(RoomId(1), UserId(1), true, 6000)));
        assert_eq!(properties::<CreateRoom>(), keys(p::CreateRoom::new("Den").public()));
        assert_eq!(properties::<UpdateRoom>(), keys(p::UpdateRoom::new().with_name("Den").with_visibility(p::RoomVisibility::Private)));
        assert_eq!(properties::<RoomUser>(), keys(p::RoomUser::new(UserId(1))));
        assert_eq!(properties::<RoomRoleChange>(), keys(p::RoomRoleChange::new(p::RoomRole::Moderator)));
        let row = p::RoomMembership::new(UserId(1), p::RoomRole::Member, t).with_name("A");
        assert_eq!(properties::<RoomMembership>(), keys(&row));
        assert_eq!(properties::<RoomMembershipPage>(), keys(Page::new(vec![row], Some(Cursor::new("c")))));
        let update = p::RoomUpdate::new(RoomId(1), p::RoomChange::Updated).about(UserId(1)).by(UserId(2)).with_role(p::RoomRole::Member).with_info(room);
        assert_eq!(properties::<RoomUpdate>(), keys(update));
    }
}
