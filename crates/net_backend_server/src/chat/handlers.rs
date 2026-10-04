//! The chat module's WebSocket kinds (`chat.join`, `chat.leave`, `chat.send`, `chat.history`,
//! `chat.members`, `chat.edit`, `chat.mark_read`, `chat.receipts`, `chat.unread`,
//! `chat.set_typing`; pushes `chat.message`, `chat.deleted`, `chat.presence`, `chat.edited`,
//! `chat.read`, `chat.typing`, `chat.room`) and HTTP routes (`/v1/chat/*`, exactly as the protocol
//! defines them).

use std::sync::Arc;

use axum::extract::State;
use net_backend_protocol::chat::{
    ChatHistory, ChatMessage, CreateRoom, DeleteMessage, DeleteRoom, EditMessage, EditRoom, GetRoom, InviteToRoom, JoinChatRoom, JoinRoom, KickFromRoom,
    LeaveChatRoom, LeaveRoom, ListDirects, ListMembers, ListMessages, ListReceipts, ListRoomMembers, ListRooms, MarkRead, MessageDeleted, MessageEdited,
    MyRooms, OpenDirect, Presence, PublicRooms, ReadReceipt, ReadReceipts, RoomInfo, RoomMembers, RoomUpdate, SendAck, SendMessage, SetRoomRole, SetTyping,
    TransferRoom, TypingUpdate, UnreadCounts, UnreadQuery,
};
use net_backend_protocol::{Ack, Page};

use super::openapi as doc;
use super::service::ChatService;
use crate::auth::AuthContext;
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::{Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;
use crate::ws::{WsCtx, WsHandlers};

fn service(state: &AppState) -> Result<Arc<ChatService>, AppError> {
    state.get::<ChatService>().ok_or_else(|| AppError::internal(std::io::Error::other("the chat module is not set up")))
}

fn ws_ctx(ctx: &WsCtx) -> HookCtx {
    HookCtx::new(ctx.state.clone(), None)
}

// ---- WebSocket ----------------------------------------------------------------------------------

async fn join(ctx: WsCtx, request: JoinRoom) -> Result<RoomInfo, AppError> {
    request.validate()?;
    let staff = ChatService::may_moderate(&ctx.state, &ctx.auth);
    service(&ctx.state)?.join(&ctx.state, &ws_ctx(&ctx), ctx.connection, ctx.auth.user_id, staff, &request.room).await
}

async fn leave(ctx: WsCtx, request: LeaveRoom) -> Result<Ack, AppError> {
    service(&ctx.state)?.leave(ctx.hub(), ctx.connection, ctx.auth.user_id, request.room);
    Ok(Ack::new())
}

async fn send(ctx: WsCtx, request: SendMessage) -> Result<SendAck, AppError> {
    service(&ctx.state)?.send(&ctx.state, &ws_ctx(&ctx), ctx.connection, ctx.auth.user_id, request).await
}

async fn history(ctx: WsCtx, request: ChatHistory) -> Result<Page<ChatMessage>, AppError> {
    let staff = ChatService::may_moderate(&ctx.state, &ctx.auth);
    service(&ctx.state)?.history_as(&ctx.state, ctx.auth.user_id, staff, request.room, &request.page).await
}

async fn members(ctx: WsCtx, request: ListMembers) -> Result<RoomMembers, AppError> {
    service(&ctx.state)?.members(&ctx.state, Some(ctx.connection), ctx.auth.user_id, request.room).await
}

/// The chat kinds, documented for the AsyncAPI document.
pub(crate) fn register(handlers: &mut WsHandlers) {
    handlers
        .call::<JoinRoom, _, _>(join)
        .summary("Join a room")
        .description("By id or a public room's key. Membership lasts as long as the connection (rejoin after a reconnect); group rooms need membership; a player room makes the caller a member first (public, or accepting an invitation; a holder of `chat.moderate` joins any player room; stored); DM rooms need no join (answered with their info). Errors: `not_found`, `not_a_member`, `forbidden` (banned from a player room), `room_full` (409), `quota_exceeded` (too many rooms on this connection, or a full player room).")
        .schemas::<doc::JoinRoom, doc::RoomInfo>();
    handlers
        .call::<LeaveRoom, _, _>(leave)
        .summary("Leave a room")
        .description("Leaving a room not joined is not an error.")
        .schemas::<doc::LeaveRoom, doc::Ack>();
    handlers
        .call::<SendMessage, _, _>(send)
        .summary("Send a message")
        .description("To a room joined on this connection, or a DM room (no join). The answer comes BEFORE the sender's own `chat.message` echo (dedupe by `message_id`, or match the `nonce`; in public and group rooms every member's push carries the nonce, in DMs only the sender's). Rate: a burst of `rate_messages`, then one per `rate_window_secs / rate_messages` (default 5, then one every 2 s). Errors: `validation_failed` (text rules), `rate_limited` (details: retry_after_ms), `not_a_member`, or a hook's refusal.")
        .schemas::<doc::SendMessage, doc::SendAck>();
    handlers
        .call::<ChatHistory, _, _>(history)
        .summary("A page of a room's history")
        .description("Newest first; `cursor` from the previous page. Public rooms: anyone; group, player and DM rooms: their members (player rooms also holders of `chat.moderate`).")
        .schemas::<doc::ChatHistory, doc::ChatMessagePage>();
    handlers
        .call::<ListMembers, _, _>(members)
        .summary("Who is online in a room")
        .description("For a room joined on this connection; users on this server instance. A DM room lists only the caller (a DM never reveals whether the peer is online).")
        .schemas::<doc::ListMembers, doc::RoomMembers>();
    handlers.push::<ChatMessage>().summary("A new message in a joined room (or a DM)").schema::<doc::ChatMessage>();
    handlers.push::<MessageDeleted>().summary("A message was deleted (moderation)").schema::<doc::MessageDeleted>();
    handlers
        .push::<Presence>()
        .summary("A user came online in a room or left it")
        .description("Best effort: not in rooms over the presence cap, not beyond the per-room rate; `chat.members` is the full list.")
        .schema::<doc::Presence>();
    register_extras(handlers);
}

// ---- HTTP ---------------------------------------------------------------------------------------

/// The public rooms.
#[utoipa::path(get, path = "/v1/chat/rooms", tag = "chat", operation_id = "chat_rooms", security(("bearer" = [])),
    params(("cursor" = Option<String>, Query, description = "The previous page's next_cursor"), ("limit" = Option<u32>, Query, description = "1-100, default 50")),
    responses((status = 200, description = "A page of public rooms", body = doc::RoomInfoPage), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn rooms(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    _who: AuthContext,
    Call(call): Call<ListRooms>,
) -> CallResult<ListRooms> {
    service.list_rooms(&state, &call.page).await.map(Reply::new)
}

/// A page of a room's history, newest first.
#[utoipa::path(get, path = "/v1/chat/rooms/{room}/messages", tag = "chat", operation_id = "chat_history", security(("bearer" = [])),
    params(
        ("room" = i64, Path, description = "The room id"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses(
        (status = 200, description = "A page of messages", body = doc::ChatMessagePage),
        (status = 403, description = "`not_a_member` (group, player and DM rooms; player rooms are open to holders of `chat.moderate`)", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn messages(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    Call(call): Call<ListMessages>,
) -> CallResult<ListMessages> {
    let staff = ChatService::may_moderate(&state, &who);
    service.history_as(&state, who.user_id, staff, call.room, &call.page).await.map(Reply::new)
}

/// Delete a message (its sender, or a moderator: audited); the room gets `chat.deleted`.
#[utoipa::path(delete, path = "/v1/chat/rooms/{room}/messages/{message}", tag = "chat", operation_id = "chat_delete_message", security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id"), ("message" = i64, Path, description = "The message id")),
    responses(
        (status = 200, description = "Deleted", body = doc::Ack),
        (status = 403, description = "`forbidden`: neither its sender nor a moderator", body = ErrorBody),
        (status = 404, description = "`not_found`: no such message (or deleted already)", body = ErrorBody),
    ))]
pub(crate) async fn delete_message(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<DeleteMessage>,
) -> CallResult<DeleteMessage> {
    service.delete(&state, &HookCtx::new(state.clone(), Some(request_id)), Some(&who), call.room, call.message).await?;
    Ok(Reply::new(Ack::new()))
}

/// Open (or find) the direct-message room with another user.
#[utoipa::path(post, path = "/v1/chat/dm", tag = "chat", operation_id = "chat_open_dm", request_body = doc::OpenDirect, security(("bearer" = [])),
    responses(
        (status = 200, description = "The room (with `peer`)", body = doc::RoomInfo),
        (status = 400, description = "`bad_request`: yourself", body = ErrorBody),
        (status = 403, description = "refused by a hook (e.g. a block)", body = ErrorBody),
        (status = 404, description = "`not_found`: no such account", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many DM opens", body = ErrorBody),
    ))]
pub(crate) async fn open_dm(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<OpenDirect>,
) -> CallResult<OpenDirect> {
    service.check_dm_rate(who.user_id)?;
    service.open_direct(&state, &HookCtx::new(state.clone(), Some(request_id)), who.user_id, call.user).await.map(Reply::new)
}

/// The caller's direct-message rooms, newest activity first.
#[utoipa::path(get, path = "/v1/chat/dms", tag = "chat", operation_id = "chat_dms", security(("bearer" = [])),
    params(("cursor" = Option<String>, Query, description = "The previous page's next_cursor"), ("limit" = Option<u32>, Query, description = "1-100, default 50")),
    responses((status = 200, description = "A page of DM rooms (with `peer`)", body = doc::RoomInfoPage)))]
pub(crate) async fn dms(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    Call(call): Call<ListDirects>,
) -> CallResult<ListDirects> {
    service.list_directs(&state, who.user_id, &call.page).await.map(Reply::new)
}

// ---- chat extras: editing, read markers, unread counts, typing (WebSocket) ----------------------

async fn edit(ctx: WsCtx, request: EditMessage) -> Result<ChatMessage, AppError> {
    service(&ctx.state)?.edit(&ctx.state, &ws_ctx(&ctx), Some(&ctx.auth), request).await
}

async fn mark_read(ctx: WsCtx, request: MarkRead) -> Result<Ack, AppError> {
    service(&ctx.state)?.mark_read(&ctx.state, &ws_ctx(&ctx), ctx.auth.user_id, request).await?;
    Ok(Ack::new())
}

async fn receipts(ctx: WsCtx, request: ListReceipts) -> Result<ReadReceipts, AppError> {
    service(&ctx.state)?.receipts(&ctx.state, ctx.auth.user_id, request.room).await
}

async fn unread(ctx: WsCtx, request: UnreadQuery) -> Result<UnreadCounts, AppError> {
    service(&ctx.state)?.unread(&ctx.state, ctx.auth.user_id, &request).await
}

async fn set_typing(ctx: WsCtx, request: SetTyping) -> Result<Ack, AppError> {
    service(&ctx.state)?.set_typing(&ctx.state, &ws_ctx(&ctx), ctx.connection, ctx.auth.user_id, request).await?;
    Ok(Ack::new())
}

/// The chat extras' kinds, documented for the AsyncAPI document.
fn register_extras(handlers: &mut WsHandlers) {
    handlers
        .call::<EditMessage, _, _>(edit)
        .summary("Edit a message")
        .description("Its sender within the server's edit window (default 900 s; counted on the send rate), or a holder of the `chat.moderate` permission (any message, audited). The text rules and the server's send hooks apply. Answered with the edited message; the room gets `chat.edited`. Errors: `not_found`, `forbidden` (not the sender, the window is over, editing turned off), `validation_failed`, `rate_limited`, `not_a_member`.")
        .schemas::<doc::EditMessage, doc::ChatMessage>();
    handlers
        .call::<MarkRead, _, _>(mark_read)
        .summary("Store the caller's read marker")
        .description("\"Read up to this message\" (a message of the room); the marker only moves forward. DM, group and player rooms push `chat.read` (at most one per user, room and the server's interval; the newest marker wins). Rate: `read_rate` per `read_window_secs` (default 30 per 60 s). Errors: `not_a_member`, `not_found`, `rate_limited`.")
        .schemas::<doc::MarkRead, doc::Ack>();
    handlers
        .call::<ListReceipts, _, _>(receipts)
        .summary("The read markers of a room")
        .description("DM, group and player rooms (members only), the newest 200 first. Errors: `bad_request` (a public room), `not_a_member`, `not_found`.")
        .schemas::<doc::ListReceipts, doc::ReadReceipts>();
    handlers
        .call::<UnreadQuery, _, _>(unread)
        .summary("The caller's unread counts")
        .description("1 to 100 rooms; a count stops at 1000. Messages newer than the caller's marker, not its own, not deleted, inside the retention. Rooms the caller cannot read are left out.")
        .schemas::<doc::UnreadQuery, doc::UnreadCounts>();
    handlers
        .call::<SetTyping, _, _>(set_typing)
        .summary("The caller types (or stopped)")
        .description("Nothing is stored. A room joined on this connection, or a DM room. Throttled: at most one `chat.typing` push per user, room and `typing_interval_ms`; none in rooms with more online users than `typing_max_members`. Errors: `not_a_member`, or a hook's refusal.")
        .schemas::<doc::SetTyping, doc::Ack>();
    handlers.push::<MessageEdited>().summary("A message's text was changed").schema::<doc::MessageEdited>();
    handlers
        .push::<ReadReceipt>()
        .summary("A member's read marker moved")
        .description("DM, group and player rooms; coalesced per user and room.")
        .schema::<doc::ReadReceipt>();
    handlers
        .push::<TypingUpdate>()
        .summary("A user types in a room (or stopped)")
        .description("Best effort; a client drops the indicator after `expires_in_ms` or at the user's next `chat.message`.")
        .schema::<doc::TypingUpdate>();
    handlers
        .push::<RoomUpdate>()
        .summary("A player room changed")
        .description("To its members and invited players (and the player an invitation or a kick is about): `updated`, `deleted`, `invited`, `joined`, `left`, `kicked`, `role`, `owner`.")
        .schema::<doc::RoomUpdate>();
}

// ---- chat extras (HTTP) -------------------------------------------------------------------------

fn hook_ctx(state: &AppState, request_id: RequestId) -> HookCtx {
    HookCtx::new(state.clone(), Some(request_id))
}

/// Edit a message (its sender within the edit window, or `chat.moderate`); the room gets `chat.edited`.
#[utoipa::path(patch, path = "/v1/chat/rooms/{room}/messages/{message}", tag = "chat", operation_id = "chat_edit_message", request_body = doc::MessageEdit, security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id"), ("message" = i64, Path, description = "The message id")),
    responses(
        (status = 200, description = "The edited message", body = doc::ChatMessage),
        (status = 403, description = "`forbidden`: not its sender, the edit window is over, editing is off, or a hook refused; `not_a_member`", body = ErrorBody),
        (status = 404, description = "`not_found`: no such message (or deleted)", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the text rules", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): the send rate", body = ErrorBody),
    ))]
pub(crate) async fn edit_message(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<EditMessage>,
) -> CallResult<EditMessage> {
    service.edit(&state, &hook_ctx(&state, request_id), Some(&who), call).await.map(Reply::new)
}

/// Store the caller's read marker of a room (forward only).
#[utoipa::path(put, path = "/v1/chat/rooms/{room}/read", tag = "chat", operation_id = "chat_mark_read", request_body = doc::ReadUpTo, security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id")),
    responses(
        (status = 200, description = "Stored (or the marker was there or beyond)", body = doc::Ack),
        (status = 403, description = "`not_a_member`", body = ErrorBody),
        (status = 404, description = "`not_found`: no such room, or the message is not in it", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms)", body = ErrorBody),
    ))]
pub(crate) async fn mark_read_http(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<MarkRead>,
) -> CallResult<MarkRead> {
    service.mark_read(&state, &hook_ctx(&state, request_id), who.user_id, call).await?;
    Ok(Reply::new(Ack::new()))
}

/// The read markers of a DM, group or player room (members only).
#[utoipa::path(get, path = "/v1/chat/rooms/{room}/receipts", tag = "chat", operation_id = "chat_receipts", security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id")),
    responses(
        (status = 200, description = "The newest 200 markers", body = doc::ReadReceipts),
        (status = 400, description = "`bad_request`: a public room", body = ErrorBody),
        (status = 403, description = "`not_a_member`", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn receipts_http(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    Call(call): Call<ListReceipts>,
) -> CallResult<ListReceipts> {
    service.receipts(&state, who.user_id, call.room).await.map(Reply::new)
}

/// The caller's unread counts of 1 to 100 rooms.
#[utoipa::path(post, path = "/v1/chat/unread", tag = "chat", operation_id = "chat_unread", request_body = doc::UnreadQuery, security(("bearer" = [])),
    responses(
        (status = 200, description = "The counts (rooms the caller cannot read are left out)", body = doc::UnreadCounts),
        (status = 422, description = "`validation_failed`: no rooms or more than 100", body = ErrorBody),
    ))]
pub(crate) async fn unread_http(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    Call(call): Call<UnreadQuery>,
) -> CallResult<UnreadQuery> {
    service.unread(&state, who.user_id, &call).await.map(Reply::new)
}

/// Create a player room; the caller owns it.
#[utoipa::path(post, path = "/v1/chat/rooms", tag = "chat", operation_id = "chat_create_room", request_body = doc::CreateRoom, security(("bearer" = [])),
    responses(
        (status = 200, description = "The room (with `owner` and `role`)", body = doc::RoomInfo),
        (status = 403, description = "`quota_exceeded`: the caller owns too many rooms; `forbidden`: player rooms are off, or a hook refused", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the name or the visibility", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms)", body = ErrorBody),
    ))]
pub(crate) async fn create_room(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<CreateRoom>,
) -> CallResult<CreateRoom> {
    service.create_player_room(&state, &hook_ctx(&state, request_id), who.user_id, call).await.map(Reply::new)
}

/// The caller's player rooms and invitations (with `role`), oldest membership first.
#[utoipa::path(get, path = "/v1/chat/rooms/mine", tag = "chat", operation_id = "chat_my_rooms", security(("bearer" = [])),
    params(("cursor" = Option<String>, Query, description = "The previous page's next_cursor"), ("limit" = Option<u32>, Query, description = "1-100, default 50")),
    responses((status = 200, description = "A page of player rooms", body = doc::RoomInfoPage)))]
pub(crate) async fn my_rooms(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    Call(call): Call<MyRooms>,
) -> CallResult<MyRooms> {
    service.my_rooms(&state, who.user_id, &call.page).await.map(Reply::new)
}

/// The public player rooms, oldest first.
#[utoipa::path(get, path = "/v1/chat/rooms/public", tag = "chat", operation_id = "chat_public_rooms", security(("bearer" = [])),
    params(("cursor" = Option<String>, Query, description = "The previous page's next_cursor"), ("limit" = Option<u32>, Query, description = "1-100, default 50")),
    responses((status = 200, description = "A page of public player rooms", body = doc::RoomInfoPage)))]
pub(crate) async fn public_rooms(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    Call(call): Call<PublicRooms>,
) -> CallResult<PublicRooms> {
    service.public_player_rooms(&state, who.user_id, &call.page).await.map(Reply::new)
}

/// One room as the caller sees it.
#[utoipa::path(get, path = "/v1/chat/rooms/{room}", tag = "chat", operation_id = "chat_get_room", security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id")),
    responses(
        (status = 200, description = "The room", body = doc::RoomInfo),
        (status = 403, description = "`not_a_member` (player rooms are open to holders of `chat.moderate`)", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn get_room(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    Call(call): Call<GetRoom>,
) -> CallResult<GetRoom> {
    let staff = ChatService::may_moderate(&state, &who);
    service.get_room_as(&state, who.user_id, staff, call.room).await.map(Reply::new)
}

/// Rename a player room (owner, moderators) or change its visibility (owner).
#[utoipa::path(patch, path = "/v1/chat/rooms/{room}", tag = "chat", operation_id = "chat_edit_room", request_body = doc::UpdateRoom, security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id")),
    responses(
        (status = 200, description = "The changed room", body = doc::RoomInfo),
        (status = 400, description = "`bad_request`: not a player room", body = ErrorBody),
        (status = 403, description = "`not_a_member`, or `forbidden`: not allowed, or a hook refused", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
        (status = 422, description = "`validation_failed`", body = ErrorBody),
    ))]
pub(crate) async fn edit_room(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<EditRoom>,
) -> CallResult<EditRoom> {
    service.update_player_room(&state, &hook_ctx(&state, request_id), &who, call.room, call.update).await.map(Reply::new)
}

/// Delete a player room (its owner, or `chat.moderate`: audited).
#[utoipa::path(delete, path = "/v1/chat/rooms/{room}", tag = "chat", operation_id = "chat_delete_room", security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id")),
    responses(
        (status = 200, description = "Deleted", body = doc::Ack),
        (status = 400, description = "`bad_request`: not a player room", body = ErrorBody),
        (status = 403, description = "`not_a_member` / `forbidden`: not the owner", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn delete_room(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<DeleteRoom>,
) -> CallResult<DeleteRoom> {
    service.delete_player_room(&state, &hook_ctx(&state, request_id), Some(&who), call.room).await?;
    Ok(Reply::new(Ack::new()))
}

/// Become a member of a player room (a public room, or accept an invitation).
#[utoipa::path(post, path = "/v1/chat/rooms/{room}/join", tag = "chat", operation_id = "chat_join_room", security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id")),
    responses(
        (status = 200, description = "The room (with `role`)", body = doc::RoomInfo),
        (status = 400, description = "`bad_request`: not a player room", body = ErrorBody),
        (status = 403, description = "`not_a_member`: private and not invited (holders of `chat.moderate` join any player room); `forbidden`: banned, or a hook refused; `quota_exceeded`: the room is full", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn join_room(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<JoinChatRoom>,
) -> CallResult<JoinChatRoom> {
    let staff = ChatService::may_moderate(&state, &who);
    service.join_player_room(&state, &hook_ctx(&state, request_id), who.user_id, staff, call.room).await.map(Reply::new)
}

/// Stop being a member of a player room (or decline an invitation).
#[utoipa::path(post, path = "/v1/chat/rooms/{room}/leave", tag = "chat", operation_id = "chat_leave_room", security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id")),
    responses(
        (status = 200, description = "Left (also when not a member)", body = doc::Ack),
        (status = 400, description = "`bad_request`: not a player room", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn leave_room(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<LeaveChatRoom>,
) -> CallResult<LeaveChatRoom> {
    service.leave_player_room(&state, &hook_ctx(&state, request_id), who.user_id, call.room).await?;
    Ok(Reply::new(Ack::new()))
}

/// The members of a player room with their roles.
#[utoipa::path(get, path = "/v1/chat/rooms/{room}/members", tag = "chat", operation_id = "chat_room_members", security(("bearer" = [])),
    params(
        ("room" = i64, Path, description = "The room id"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses(
        (status = 200, description = "A page of members, oldest first (bans: owner and moderators only)", body = doc::RoomMembershipPage),
        (status = 400, description = "`bad_request`: not a player room", body = ErrorBody),
        (status = 403, description = "`not_a_member`", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn room_members(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    Call(call): Call<ListRoomMembers>,
) -> CallResult<ListRoomMembers> {
    service.room_members(&state, &who, call.room, &call.page).await.map(Reply::new)
}

/// Invite a player (owner, moderators; lifts a ban).
#[utoipa::path(post, path = "/v1/chat/rooms/{room}/invites", tag = "chat", operation_id = "chat_invite", request_body = doc::RoomUser, security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id")),
    responses(
        (status = 200, description = "Invited (or a member / invited already)", body = doc::Ack),
        (status = 400, description = "`bad_request`: not a player room, or yourself", body = ErrorBody),
        (status = 403, description = "`not_a_member` / `forbidden`: not the owner or a moderator, the player blocked the caller, or a hook refused; `quota_exceeded`: the room is full", body = ErrorBody),
        (status = 404, description = "`not_found`: no such room or account", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many invitations", body = ErrorBody),
    ))]
pub(crate) async fn invite(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<InviteToRoom>,
) -> CallResult<InviteToRoom> {
    service.check_invite_rate(who.user_id)?;
    service.invite_to_room(&state, &hook_ctx(&state, request_id), &who, call.room, call.invitee.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// Kick a player (banned until invited again) or withdraw an invitation.
#[utoipa::path(delete, path = "/v1/chat/rooms/{room}/members/{user}", tag = "chat", operation_id = "chat_kick", security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id"), ("user" = i64, Path, description = "The player")),
    responses(
        (status = 200, description = "Kicked (or banned already)", body = doc::Ack),
        (status = 400, description = "`bad_request`: not a player room, or yourself", body = ErrorBody),
        (status = 403, description = "`not_a_member` / `forbidden`: not allowed", body = ErrorBody),
        (status = 404, description = "`not_found`: no such room, or the player is not in it", body = ErrorBody),
    ))]
pub(crate) async fn kick(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<KickFromRoom>,
) -> CallResult<KickFromRoom> {
    service.kick_from_room(&state, &hook_ctx(&state, request_id), &who, call.room, call.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// Make a member a moderator or a member again (the owner).
#[utoipa::path(put, path = "/v1/chat/rooms/{room}/members/{user}/role", tag = "chat", operation_id = "chat_set_role", request_body = doc::RoomRoleChange, security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id"), ("user" = i64, Path, description = "The member")),
    responses(
        (status = 200, description = "Changed (or it was that role)", body = doc::Ack),
        (status = 400, description = "`bad_request`: not a player room, or the owner", body = ErrorBody),
        (status = 403, description = "`not_a_member` / `forbidden`: not the owner", body = ErrorBody),
        (status = 404, description = "`not_found`: no such room or member", body = ErrorBody),
        (status = 422, description = "`validation_failed`: not moderator or member", body = ErrorBody),
    ))]
pub(crate) async fn set_role(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<SetRoomRole>,
) -> CallResult<SetRoomRole> {
    service.set_room_role(&state, &hook_ctx(&state, request_id), &who, call.room, call.user, call.change.role).await?;
    Ok(Reply::new(Ack::new()))
}

/// Hand the room to a member (the owner); the old owner becomes a moderator.
#[utoipa::path(post, path = "/v1/chat/rooms/{room}/owner", tag = "chat", operation_id = "chat_transfer_room", request_body = doc::RoomUser, security(("bearer" = [])),
    params(("room" = i64, Path, description = "The room id")),
    responses(
        (status = 200, description = "Handed on", body = doc::Ack),
        (status = 400, description = "`bad_request`: not a player room, or yourself", body = ErrorBody),
        (status = 403, description = "`not_a_member` / `forbidden`: not the owner; `quota_exceeded`: the new owner owns too many rooms", body = ErrorBody),
        (status = 404, description = "`not_found`: no such room, account or member", body = ErrorBody),
    ))]
pub(crate) async fn transfer(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<TransferRoom>,
) -> CallResult<TransferRoom> {
    service.transfer_room(&state, &hook_ctx(&state, request_id), &who, call.room, call.to.user).await?;
    Ok(Reply::new(Ack::new()))
}
