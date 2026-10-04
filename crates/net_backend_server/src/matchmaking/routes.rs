//! `/v1/matchmaking/*`: queues and the caller's ticket, exactly as the protocol defines them.

use axum::extract::State;
use net_backend_protocol::matchmaking::{CancelTicket, CreateTicket, GetTicket, ListQueues};
use net_backend_protocol::Ack;

use super::openapi as doc;
use super::service::MatchmakingService;
use crate::auth::AuthContext;
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::{Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;

/// The server's queues with how many tickets wait.
#[utoipa::path(get, path = "/v1/matchmaking/queues", tag = "matchmaking", operation_id = "matchmaking_queues", security(("bearer" = [])),
    responses((status = 200, description = "The queues", body = doc::Queues), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn queues(Ext(service): Ext<MatchmakingService>, _who: AuthContext, Call(_call): Call<ListQueues>) -> CallResult<ListQueues> {
    Ok(Reply::new(service.queues()))
}

/// Put a ticket into a queue (one ticket per player).
#[utoipa::path(post, path = "/v1/matchmaking/ticket", tag = "matchmaking", operation_id = "matchmaking_create", request_body = doc::CreateTicket, security(("bearer" = [])),
    responses(
        (status = 200, description = "The waiting ticket", body = doc::MatchTicket),
        (status = 403, description = "`forbidden`: refused by a hook", body = ErrorBody),
        (status = 404, description = "`not_found`: no such queue", body = ErrorBody),
        (status = 409, description = "`conflict`: a ticket of the caller waits already", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the queue key or the attributes' size", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many tickets", body = ErrorBody),
        (status = 503, description = "`unavailable`: the server holds too many tickets", body = ErrorBody),
    ))]
pub(crate) async fn create(
    State(state): State<AppState>,
    Ext(service): Ext<MatchmakingService>,
    who: AuthContext,
    request_id: RequestId,
    Call(create): Call<CreateTicket>,
) -> CallResult<CreateTicket> {
    create.validate()?;
    service.check_rate(who.user_id)?;
    service.create(&state, &HookCtx::new(state.clone(), Some(request_id)), who.user_id, create).await.map(Reply::new)
}

/// The caller's ticket: waiting, or matched with its match.
#[utoipa::path(get, path = "/v1/matchmaking/ticket", tag = "matchmaking", operation_id = "matchmaking_ticket", security(("bearer" = [])),
    responses(
        (status = 200, description = "The ticket", body = doc::MatchTicket),
        (status = 404, description = "`not_found`: no ticket (none made, cancelled, run out, or matched longer ago than the server keeps it)", body = ErrorBody),
    ))]
pub(crate) async fn ticket(
    State(state): State<AppState>,
    Ext(service): Ext<MatchmakingService>,
    who: AuthContext,
    Call(_call): Call<GetTicket>,
) -> CallResult<GetTicket> {
    service.ticket_of(&state, who.user_id).map(Reply::new).ok_or_else(|| AppError::not_found("no ticket"))
}

/// Take the caller's ticket out of its queue.
#[utoipa::path(delete, path = "/v1/matchmaking/ticket", tag = "matchmaking", operation_id = "matchmaking_cancel", security(("bearer" = [])),
    responses((status = 200, description = "Cancelled (or there was none)", body = doc::Ack), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn cancel(Ext(service): Ext<MatchmakingService>, who: AuthContext, Call(_call): Call<CancelTicket>) -> CallResult<CancelTicket> {
    service.cancel(who.user_id);
    Ok(Reply::new(Ack::new()))
}
