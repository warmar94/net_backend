//! The chat module's WebSocket kinds (`chat.join`, `chat.leave`, `chat.send`, `chat.history`,
//! `chat.members`; pushes `chat.message`, `chat.deleted`, `chat.presence`) and HTTP routes
//! (`/v1/chat/*`, exactly as the protocol defines them).

use std::sync::Arc;

use axum::extract::State;
use net_backend_protocol::chat::{
    ChatHistory, ChatMessage, DeleteMessage, JoinRoom, LeaveRoom, ListDirects, ListMembers, ListMessages, ListRooms, MessageDeleted, OpenDirect, Presence,
    RoomInfo, RoomMembers, SendAck, SendMessage,
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
    service(&ctx.state)?.join(&ctx.state, &ws_ctx(&ctx), ctx.connection, ctx.auth.user_id, &request.room).await
}

async fn leave(ctx: WsCtx, request: LeaveRoom) -> Result<Ack, AppError> {
    service(&ctx.state)?.leave(ctx.hub(), ctx.connection, ctx.auth.user_id, request.room);
    Ok(Ack::new())
}

async fn send(ctx: WsCtx, request: SendMessage) -> Result<SendAck, AppError> {
    service(&ctx.state)?.send(&ctx.state, &ws_ctx(&ctx), ctx.connection, ctx.auth.user_id, request).await
}

async fn history(ctx: WsCtx, request: ChatHistory) -> Result<Page<ChatMessage>, AppError> {
    service(&ctx.state)?.history(&ctx.state, ctx.auth.user_id, request.room, &request.page).await
}

async fn members(ctx: WsCtx, request: ListMembers) -> Result<RoomMembers, AppError> {
    service(&ctx.state)?.members(&ctx.state, Some(ctx.connection), ctx.auth.user_id, request.room).await
}

/// The chat kinds, documented for the AsyncAPI document.
pub(crate) fn register(handlers: &mut WsHandlers) {
    handlers
        .call::<JoinRoom, _, _>(join)
        .summary("Join a room")
        .description("By id or a public room's key. Membership lasts as long as the connection (rejoin after a reconnect); group rooms need membership; DM rooms need no join (answered with their info). Errors: `not_found`, `not_a_member`, `room_full` (409), `quota_exceeded` (too many rooms on this connection).")
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
        .description("Newest first; `cursor` from the previous page. Public rooms: anyone; group and DM rooms: their members.")
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
        (status = 403, description = "`not_a_member` (group and DM rooms)", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn messages(
    State(state): State<AppState>,
    Ext(service): Ext<ChatService>,
    who: AuthContext,
    Call(call): Call<ListMessages>,
) -> CallResult<ListMessages> {
    service.history(&state, who.user_id, call.room, &call.page).await.map(Reply::new)
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
