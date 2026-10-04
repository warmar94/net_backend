//! Feature `bevy_net_backend`: this crate's WebSocket messages implement that client's
//! `WsRequest` / `WsPushMessage`, and an [`AccessToken`] is a ready-made `Credentials`.
//! Nothing public lives here: these are trait impls only.

use bevy_net_backend::{BearerToken, Credentials, OutgoingRequest, WsPushMessage, WsRequest};

use crate::auth::AccessToken;
use crate::chat::{
    ChatHistory, ChatMessage, EditMessage, JoinRoom, LeaveRoom, ListMembers, ListReceipts, MarkRead, MessageDeleted, MessageEdited, Presence, ReadReceipt,
    RoomUpdate, SendMessage, SetTyping, TypingUpdate, UnreadQuery,
};
use crate::envelope::{ServerPush, WsCall};
use crate::friends::FriendPresence;
use crate::lobbies::{LobbyMemberUpdate, LobbyUpdate};
use crate::matchmaking::{MatchFound, TicketExpired};
use crate::notifications::{CountNotifications, DeleteNotification, MarkNotifications, Notification, NotificationQuery};

/// `WsRequest` from this crate's [`WsCall`]: the same kind and answer type. No request asks to be
/// resent after a reconnect (the client's default): chat membership ends with the connection, a
/// resent `chat.send` could post a message twice, and a resent `notify.*` request is answered by
/// asking again.
macro_rules! ws_requests {
    ($($request:ty),* $(,)?) => {
        $(
            impl WsRequest for $request {
                type Response = <$request as WsCall>::Response;
                const KIND: &'static str = <$request as WsCall>::KIND;
            }
        )*
    };
}

macro_rules! ws_pushes {
    ($($push:ty),* $(,)?) => {
        $(
            impl WsPushMessage for $push {
                const KIND: &'static str = <$push as ServerPush>::KIND;
            }
        )*
    };
}

ws_requests!(
    JoinRoom,
    LeaveRoom,
    SendMessage,
    ChatHistory,
    ListMembers,
    EditMessage,
    MarkRead,
    ListReceipts,
    UnreadQuery,
    SetTyping,
    NotificationQuery,
    CountNotifications,
    MarkNotifications,
    DeleteNotification
);
ws_pushes!(
    ChatMessage,
    MessageDeleted,
    Presence,
    MessageEdited,
    ReadReceipt,
    TypingUpdate,
    RoomUpdate,
    Notification,
    FriendPresence,
    LobbyMemberUpdate,
    LobbyUpdate,
    MatchFound,
    TicketExpired
);

/// `Authorization: Bearer <token>` on every HTTP request and WebSocket handshake (the client's
/// `BearerToken` does the work, including the sensitive-header flag and the refusal of a token
/// that is not a valid header value). The handshake header authenticates the WebSocket, so no
/// first-message `auth` is sent.
impl Credentials for AccessToken {
    fn apply(&self, request: &mut OutgoingRequest) {
        BearerToken::new(self.expose()).apply(request);
    }
}
