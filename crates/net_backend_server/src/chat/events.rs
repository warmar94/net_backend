//! What games can hook into in the chat module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeChatJoin`] | before | a connection is about to join a room; refuse (a private event room, a muted player) |
//! | [`BeforeChatSend`] | before | a message is about to be stored and pushed, or a message's text is about to change (`edit`): filter / rewrite `text`, or refuse (moderation, mutes, spam rules) |
//! | [`AfterChatSend`] | after | a message was stored and pushed (logging, achievements, a bot answering) |
//! | [`BeforeDirectOpen`] | before | a user opens a direct-message room with another; refuse (blocks, privacy settings) |
//! | [`AfterChatDelete`] | after | a message was deleted (by its sender, a moderator or server code) |
//! | [`AfterChatEdit`] | after | a message's text was changed (by its sender, a moderator or server code) |
//! | [`AfterChatRead`] | after | a user's read marker moved forward |
//! | [`BeforeChatTyping`] | before | a typing indicator is about to be pushed; refuse (mutes) |
//! | [`BeforeRoomCreate`] | before | a player creates a room: change the name / visibility, or refuse |
//! | [`BeforeRoomUpdate`] | before | a player room is renamed or its visibility changes: change or refuse |
//! | [`BeforeRoomInvite`] | before | a player is invited into a player room; refuse (blocks) |
//! | [`AfterRoomChange`] | after | a player room changed (created, updated, deleted, members, roles, owner) |
//!
//! Joining a player room (the membership, not only a connection) also runs [`BeforeChatJoin`].
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

use net_backend_protocol::chat::{ChatMessage, RoomChange, RoomKind, RoomRole, RoomVisibility};
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

/// A message is about to be stored and pushed, or (with `edit`) an existing message's text is
/// about to change: one hook filters both. Hooks may change `text` (checked again against the
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
    /// The sender's nonce, if any (never on an edit).
    pub nonce: Option<String>,
    /// An edit of this message (`sender` is the message's sender; the editor may be a moderator);
    /// `None`: a new message.
    pub edit: Option<MessageId>,
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

/// A message's text was changed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterChatEdit {
    /// The room.
    pub room: RoomId,
    /// The message.
    pub message: MessageId,
    /// Its sender.
    pub sender: UserId,
    /// Who edited it (`None`: server code).
    pub edited_by: Option<UserId>,
    /// The new text (after the hooks).
    pub text: String,
}

impl Event for AfterChatEdit {
    const NAME: &'static str = "chat.after_edit";
}

/// A user's read marker of a room moved forward.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterChatRead {
    /// The room.
    pub room: RoomId,
    /// Its kind.
    pub kind: RoomKind,
    /// Who read.
    pub user_id: UserId,
    /// The newest message read.
    pub message: MessageId,
}

impl Event for AfterChatRead {
    const NAME: &'static str = "chat.after_read";
}

/// A typing indicator is about to be pushed (after the throttle). Refuse to keep it quiet (the
/// caller gets the refusal).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeChatTyping {
    /// The room.
    pub room: RoomId,
    /// Its kind.
    pub kind: RoomKind,
    /// Who types.
    pub user_id: UserId,
    /// Typing or stopped.
    pub typing: bool,
}

impl Event for BeforeChatTyping {
    const NAME: &'static str = "chat.before_typing";
}

/// A player creates a room. Hooks may change `name` (checked again) and `visibility`, or refuse.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeRoomCreate {
    /// Who creates it (its owner).
    pub user_id: UserId,
    /// The name.
    pub name: String,
    /// Public or private.
    pub visibility: RoomVisibility,
}

impl Event for BeforeRoomCreate {
    const NAME: &'static str = "chat.before_room_create";
}

/// A player room is about to be renamed or change its visibility. Hooks may change the new values
/// (checked again) or refuse.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeRoomUpdate {
    /// The room.
    pub room: RoomId,
    /// Who changes it.
    pub user_id: UserId,
    /// The new name, if it changes.
    pub name: Option<String>,
    /// The new visibility, if it changes.
    pub visibility: Option<RoomVisibility>,
}

impl Event for BeforeRoomUpdate {
    const NAME: &'static str = "chat.before_room_update";
}

/// A player is about to be invited into a player room. Refuse to keep them out (e.g. a block).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeRoomInvite {
    /// The room.
    pub room: RoomId,
    /// Who invites.
    pub user_id: UserId,
    /// The invited player.
    pub invitee: UserId,
}

impl Event for BeforeRoomInvite {
    const NAME: &'static str = "chat.before_room_invite";
}

/// A player room changed (the same changes as the `chat.room` push, plus `created`).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterRoomChange {
    /// The room.
    pub room: RoomId,
    /// What changed (`created` is reported as [`RoomChange::Joined`] of the owner with
    /// `created = true`).
    pub change: RoomChange,
    /// The room was just created.
    pub created: bool,
    /// The player the change is about, if any.
    pub user: Option<UserId>,
    /// Who made it (`None`: server code, e.g. the upkeep naming a new owner).
    pub actor: Option<UserId>,
    /// The new role, for `role` and `owner`.
    pub role: Option<RoomRole>,
}

impl Event for AfterRoomChange {
    const NAME: &'static str = "chat.after_room_change";
}
