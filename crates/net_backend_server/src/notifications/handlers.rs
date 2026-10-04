//! The notifications module's WebSocket kinds (`notify.list`, `notify.count`, `notify.mark`,
//! `notify.delete`; push `notify.new`) and HTTP routes (`/v1/notifications*`, exactly as the
//! protocol defines them). Both answer the same.

use std::sync::Arc;

use axum::extract::State;
use net_backend_protocol::notifications::{
    CountNotifications, DeleteNotification, MarkAck, MarkNotifications, Notification, NotificationCount, NotificationQuery,
};
use net_backend_protocol::{Ack, Page};

use super::openapi as doc;
use super::service::NotificationService;
use crate::auth::AuthContext;
use crate::error::AppError;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::Ext;
use crate::openapi::ErrorBody;
use crate::state::AppState;
use crate::ws::{WsCtx, WsHandlers};

fn service(state: &AppState) -> Result<Arc<NotificationService>, AppError> {
    state.get::<NotificationService>().ok_or_else(|| AppError::internal(std::io::Error::other("the notifications module is not set up")))
}

// ---- WebSocket ----------------------------------------------------------------------------------

async fn ws_list(ctx: WsCtx, query: NotificationQuery) -> Result<Page<Notification>, AppError> {
    service(&ctx.state)?.list(&ctx.state, ctx.auth.user_id, &query).await
}

async fn ws_count(ctx: WsCtx, _request: CountNotifications) -> Result<NotificationCount, AppError> {
    service(&ctx.state)?.count(&ctx.state, ctx.auth.user_id).await
}

async fn ws_mark(ctx: WsCtx, mark: MarkNotifications) -> Result<MarkAck, AppError> {
    service(&ctx.state)?.mark(&ctx.state, ctx.auth.user_id, &mark).await
}

async fn ws_delete(ctx: WsCtx, request: DeleteNotification) -> Result<Ack, AppError> {
    service(&ctx.state)?.delete(&ctx.state, ctx.auth.user_id, request.id).await?;
    Ok(Ack::new())
}

/// The notification kinds, documented for the AsyncAPI document.
pub(crate) fn register(handlers: &mut WsHandlers) {
    handlers
        .call::<NotificationQuery, _, _>(ws_list)
        .summary("A page of the caller's notifications")
        .description("Newest first; `cursor` from the previous page; `unread_only` for the unread ones. The same as `GET /v1/notifications`.")
        .schemas::<doc::NotificationQuery, doc::NotificationPage>();
    handlers
        .call::<CountNotifications, _, _>(ws_count)
        .summary("How many notifications the caller has")
        .description("Unread and total. The same as `GET /v1/notifications/count`.")
        .schemas::<doc::CountNotifications, doc::NotificationCount>();
    handlers
        .call::<MarkNotifications, _, _>(ws_mark)
        .summary("Mark notifications read or unread")
        .description("`ids` (1-100 distinct) or `all: true`; ids of other players or already in that state are skipped. Answers how many changed and the unread count. Errors: `validation_failed`.")
        .schemas::<doc::MarkNotifications, doc::MarkAck>();
    handlers
        .call::<DeleteNotification, _, _>(ws_delete)
        .summary("Delete a notification")
        .description("One of the caller's; answered `{}` also when it does not exist.")
        .schemas::<doc::DeleteNotification, doc::Ack>();
    handlers
        .push::<Notification>()
        .summary("A new notification")
        .description("To every open connection of its player, when the server stores it. A client that was offline reads the missed ones with `notify.list` (`unread_only`).")
        .schema::<doc::Notification>();
}

// ---- HTTP ---------------------------------------------------------------------------------------

/// A page of the caller's notifications, newest first.
#[utoipa::path(get, path = "/v1/notifications", tag = "notifications", operation_id = "notifications_list", security(("bearer" = [])),
    params(
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
        ("unread_only" = Option<bool>, Query, description = "Only the unread ones"),
    ),
    responses(
        (status = 200, description = "A page of notifications, newest first", body = doc::NotificationPage),
        (status = 400, description = "`bad_request`: an invalid cursor", body = ErrorBody),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
    ))]
pub(crate) async fn list(
    State(state): State<AppState>,
    Ext(service): Ext<NotificationService>,
    who: AuthContext,
    Call(query): Call<NotificationQuery>,
) -> CallResult<NotificationQuery> {
    service.list(&state, who.user_id, &query).await.map(Reply::new)
}

/// How many notifications the caller has (unread and total).
#[utoipa::path(get, path = "/v1/notifications/count", tag = "notifications", operation_id = "notifications_count", security(("bearer" = [])),
    responses((status = 200, description = "The counts", body = doc::NotificationCount), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn count(
    State(state): State<AppState>,
    Ext(service): Ext<NotificationService>,
    who: AuthContext,
    Call(_request): Call<CountNotifications>,
) -> CallResult<CountNotifications> {
    service.count(&state, who.user_id).await.map(Reply::new)
}

/// Mark some or all of the caller's notifications read or unread.
#[utoipa::path(post, path = "/v1/notifications/mark", tag = "notifications", operation_id = "notifications_mark", request_body = doc::MarkNotifications, security(("bearer" = [])),
    responses(
        (status = 200, description = "How many changed, and the unread count now", body = doc::MarkAck),
        (status = 422, description = "`validation_failed`: 1-100 distinct ids, or all", body = ErrorBody),
    ))]
pub(crate) async fn mark(
    State(state): State<AppState>,
    Ext(service): Ext<NotificationService>,
    who: AuthContext,
    Call(mark): Call<MarkNotifications>,
) -> CallResult<MarkNotifications> {
    service.mark(&state, who.user_id, &mark).await.map(Reply::new)
}

/// Delete one of the caller's notifications (answered `{}` also when it does not exist).
#[utoipa::path(delete, path = "/v1/notifications/{id}", tag = "notifications", operation_id = "notifications_delete", security(("bearer" = [])),
    params(("id" = i64, Path, description = "The notification id")),
    responses((status = 200, description = "Deleted (or not there)", body = doc::Ack), (status = 400, description = "`bad_request`: the id is not a number", body = ErrorBody)))]
pub(crate) async fn delete(
    State(state): State<AppState>,
    Ext(service): Ext<NotificationService>,
    who: AuthContext,
    Call(call): Call<DeleteNotification>,
) -> CallResult<DeleteNotification> {
    service.delete(&state, who.user_id, call.id).await?;
    Ok(Reply::new(Ack::new()))
}
