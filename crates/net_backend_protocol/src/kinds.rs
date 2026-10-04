//! The WebSocket message kinds: the `type` field of every frame. Public API from 0.1.0: never
//! renamed; new kinds may be added. A module's kinds start with the module name and a dot.

/// Client → server, first-message authentication: `{"type":"auth","data":{"token":…}}` (no `id`).
pub const AUTH: &str = "auth";
/// Server → client: first-message authentication accepted.
pub const AUTH_OK: &str = "auth.ok";
/// Server → client: first-message authentication refused (the server then closes with 4001).
pub const AUTH_FAILED: &str = "auth.failed";

/// Request: join a chat room ([`JoinRoom`](crate::chat::JoinRoom)).
pub const CHAT_JOIN: &str = "chat.join";
/// Request: leave a chat room ([`LeaveRoom`](crate::chat::LeaveRoom)).
pub const CHAT_LEAVE: &str = "chat.leave";
/// Request: send a chat message ([`SendMessage`](crate::chat::SendMessage)).
pub const CHAT_SEND: &str = "chat.send";
/// Request: a page of a room's history ([`ChatHistory`](crate::chat::ChatHistory)).
pub const CHAT_HISTORY: &str = "chat.history";
/// Push: a new chat message in a joined room ([`ChatMessage`](crate::chat::ChatMessage)).
pub const CHAT_MESSAGE: &str = "chat.message";
/// Push: a chat message was deleted (moderation) ([`MessageDeleted`](crate::chat::MessageDeleted)).
pub const CHAT_DELETED: &str = "chat.deleted";
/// Request: who is online in a room ([`ListMembers`](crate::chat::ListMembers)).
pub const CHAT_MEMBERS: &str = "chat.members";
/// Push: a user came online in a room or left it ([`Presence`](crate::chat::Presence)).
pub const CHAT_PRESENCE: &str = "chat.presence";
/// Request: change a message's text ([`EditMessage`](crate::chat::EditMessage)).
pub const CHAT_EDIT: &str = "chat.edit";
/// Push: a message's text was changed ([`MessageEdited`](crate::chat::MessageEdited)).
pub const CHAT_EDITED: &str = "chat.edited";
/// Request: store the caller's read marker of a room ([`MarkRead`](crate::chat::MarkRead)).
pub const CHAT_MARK_READ: &str = "chat.mark_read";
/// Push: a member's read marker moved ([`ReadReceipt`](crate::chat::ReadReceipt)).
pub const CHAT_READ: &str = "chat.read";
/// Request: the read markers of a room ([`ListReceipts`](crate::chat::ListReceipts)).
pub const CHAT_RECEIPTS: &str = "chat.receipts";
/// Request: the caller's unread counts ([`UnreadQuery`](crate::chat::UnreadQuery)).
pub const CHAT_UNREAD: &str = "chat.unread";
/// Request: the caller types in a room or stopped ([`SetTyping`](crate::chat::SetTyping)).
pub const CHAT_SET_TYPING: &str = "chat.set_typing";
/// Push: a user types in a room or stopped ([`TypingUpdate`](crate::chat::TypingUpdate)).
pub const CHAT_TYPING: &str = "chat.typing";
/// Push: a player room changed: name, members, roles, deletion ([`RoomUpdate`](crate::chat::RoomUpdate)).
pub const CHAT_ROOM: &str = "chat.room";

/// Request: a page of the caller's notifications ([`NotificationQuery`](crate::notifications::NotificationQuery)).
pub const NOTIFY_LIST: &str = "notify.list";
/// Request: how many notifications the caller has ([`CountNotifications`](crate::notifications::CountNotifications)).
pub const NOTIFY_COUNT: &str = "notify.count";
/// Request: mark notifications read or unread ([`MarkNotifications`](crate::notifications::MarkNotifications)).
pub const NOTIFY_MARK: &str = "notify.mark";
/// Request: delete a notification ([`DeleteNotification`](crate::notifications::DeleteNotification)).
pub const NOTIFY_DELETE: &str = "notify.delete";
/// Push: a new notification for the player ([`Notification`](crate::notifications::Notification)).
pub const NOTIFY_NEW: &str = "notify.new";

/// Push: a friend came online or went offline ([`FriendPresence`](crate::friends::FriendPresence)).
pub const FRIENDS_PRESENCE: &str = "friends.presence";

/// Push: a lobby member joined, left, was kicked or changed its ready flag ([`LobbyMemberUpdate`](crate::lobbies::LobbyMemberUpdate)).
pub const LOBBY_MEMBER: &str = "lobby.member";
/// Push: a lobby's host, metadata, settings, state or join code changed ([`LobbyUpdate`](crate::lobbies::LobbyUpdate)).
pub const LOBBY_CHANGED: &str = "lobby.changed";

/// Push: the player's matchmaking ticket was matched ([`MatchFound`](crate::matchmaking::MatchFound)).
pub const MATCH_FOUND: &str = "match.found";
/// Push: the player's matchmaking ticket ran out unmatched ([`TicketExpired`](crate::matchmaking::TicketExpired)).
pub const MATCH_EXPIRED: &str = "match.expired";

/// Whether `kind` is reserved for authentication (`auth`, `auth.ok`, `auth.failed`): never a push
/// or a game's own request kind.
pub fn is_reserved(kind: &str) -> bool {
    matches!(kind, AUTH | AUTH_OK | AUTH_FAILED)
}

/// Every kind this crate defines (for tests and for a server's routing table).
pub const ALL: &[&str] = &[
    AUTH,
    AUTH_OK,
    AUTH_FAILED,
    CHAT_JOIN,
    CHAT_LEAVE,
    CHAT_SEND,
    CHAT_HISTORY,
    CHAT_MESSAGE,
    CHAT_DELETED,
    CHAT_MEMBERS,
    CHAT_PRESENCE,
    CHAT_EDIT,
    CHAT_EDITED,
    CHAT_MARK_READ,
    CHAT_READ,
    CHAT_RECEIPTS,
    CHAT_UNREAD,
    CHAT_SET_TYPING,
    CHAT_TYPING,
    CHAT_ROOM,
    NOTIFY_LIST,
    NOTIFY_COUNT,
    NOTIFY_MARK,
    NOTIFY_DELETE,
    NOTIFY_NEW,
    FRIENDS_PRESENCE,
    LOBBY_MEMBER,
    LOBBY_CHANGED,
    MATCH_FOUND,
    MATCH_EXPIRED,
];

#[cfg(test)]
mod tests {
    #[test]
    fn kinds_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for kind in super::ALL {
            assert!(seen.insert(*kind), "duplicate {kind}");
            assert!(kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'_'), "{kind}");
        }
        assert!(super::is_reserved("auth.ok") && !super::is_reserved("chat.message"));
    }
}
