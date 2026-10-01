//! What games can hook into in the chat module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeChatJoin`] | before | a connection is about to join a room; refuse (a private event room, a muted player) |
//! | [`BeforeChatSend`] | before | a message is about to be stored and pushed: filter / rewrite `text`, or refuse (moderation, mutes, spam rules) |
//! | [`AfterChatSend`] | after | a message was stored and pushed (logging, achievements, a bot answering) |
//! | [`BeforeDirectOpen`] | before | a user opens a direct-message room with another; refuse (blocks, privacy settings) |
//! | [`AfterChatDelete`] | after | a message was deleted (by its sender, a moderator or server code) |
//!
//! ```
//! use net_backend_server::chat::events::BeforeChatSend;
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeChatSend, _, _>(|_ctx, mut message| async move {
//!     if message.text.contains("buy gold") {
//!         return Ok(Decision::Reject(AppError::forbidden("no advertising")));
//!     }
//!     message.text = message.text.replace("darn", "d**n");
//!     Ok(Decision::Continue(message))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::chat::{ChatMessage, RoomKind};
use net_backend_protocol::{MessageId, RoomId, UserId};

use crate::hooks::Event;

/// A connection is about to join a room. Refuse to keep the user out.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeChatJoin {
    /// The room.
    pub room: RoomId,
    /// Its kind.
    pub kind: RoomKind,
    /// Its key (public rooms).
    pub key: Option<String>,
    /// Who joins.
    pub user_id: UserId,
}

impl Event for BeforeChatJoin {
    const NAME: &'static str = "chat.before_join";
}

/// A message is about to be stored and pushed. Hooks may change `text` (checked again against the
/// text rules) or refuse; the other fields are for reading.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeChatSend {
    /// The room.
    pub room: RoomId,
    /// Its kind.
    pub kind: RoomKind,
    /// The sender.
    pub sender: UserId,
    /// The text (hooks may change it).
    pub text: String,
    /// The sender's nonce, if any.
    pub nonce: Option<String>,
}

impl Event for BeforeChatSend {
    const NAME: &'static str = "chat.before_send";
}

/// A message was stored and pushed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterChatSend {
    /// The message as pushed.
    pub message: ChatMessage,
    /// The room's kind.
    pub kind: RoomKind,
}

impl Event for AfterChatSend {
    const NAME: &'static str = "chat.after_send";
}

/// A user opens (or finds) the direct-message room with another. Refuse to keep them apart.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeDirectOpen {
    /// Who opens it.
    pub user_id: UserId,
    /// The other user.
    pub peer: UserId,
}

impl Event for BeforeDirectOpen {
    const NAME: &'static str = "chat.before_direct_open";
}

/// A message was deleted.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterChatDelete {
    /// The room.
    pub room: RoomId,
    /// The message.
    pub message: MessageId,
    /// Its sender.
    pub sender: UserId,
    /// Who deleted it (`None`: server code).
    pub deleted_by: Option<UserId>,
}

impl Event for AfterChatDelete {
    const NAME: &'static str = "chat.after_delete";
}
