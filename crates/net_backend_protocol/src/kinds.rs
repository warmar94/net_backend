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

/// Whether `kind` is reserved for authentication (`auth`, `auth.ok`, `auth.failed`): never a push
/// or a game's own request kind.
pub fn is_reserved(kind: &str) -> bool {
    matches!(kind, AUTH | AUTH_OK | AUTH_FAILED)
}

/// Every kind this crate defines (for tests and for a server's routing table).
pub const ALL: &[&str] = &[AUTH, AUTH_OK, AUTH_FAILED, CHAT_JOIN, CHAT_LEAVE, CHAT_SEND, CHAT_HISTORY, CHAT_MESSAGE, CHAT_DELETED];

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
