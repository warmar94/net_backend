//! `/v1/leaderboards/*`: the boards, the top, submitting, the caller's rank and the ranks around
//! the caller, exactly as the protocol defines them.

use axum::extract::State;
use net_backend_protocol::leaderboards::{GetAroundMe, GetLeaderboard, GetMyRank, ListBoards, PostScore};

use super::events::Submitter;
use super::openapi as doc;
use super::service::LeaderboardService;
use crate::auth::AuthContext;
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::{Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;

/// Every board with its current period.
#[utoipa::path(get, path = "/v1/leaderboards", tag = "leaderboards", operation_id = "leaderboards_list", security(("bearer" = [])),
    responses(
        (status = 200, description = "Every board, ordered by key", body = doc::Boards),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
    ))]
pub(crate) async fn boards(
    State(state): State<AppState>,
    Ext(service): Ext<LeaderboardService>,
    _who: AuthContext,
    Call(_call): Call<ListBoards>,
) -> CallResult<ListBoards> {
    Ok(Reply::new(service.boards(&state)))
}

/// A page of a board, best first.
#[utoipa::path(get, path = "/v1/leaderboards/{board}", tag = "leaderboards", operation_id = "leaderboards_top", security(("bearer" = [])),
    params(
        ("board" = String, Path, description = "The board's key"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
        ("at" = Option<i64>, Query, description = "A time inside the period to show (unix ms); default: now"),
    ),
    responses(
        (status = 200, description = "A page of entries, best first", body = doc::LeaderboardPage),
        (status = 400, description = "`bad_request`: an invalid key or cursor", body = ErrorBody),
        (status = 404, description = "`not_found`: no such board", body = ErrorBody),
    ))]
pub(crate) async fn top(
    State(state): State<AppState>,
    Ext(service): Ext<LeaderboardService>,
    _who: AuthContext,
    Call(call): Call<GetLeaderboard>,
) -> CallResult<GetLeaderboard> {
    service.top(&state, &call.board, &call.query).await.map(Reply::new)
}

/// Submit a score for the caller.
#[utoipa::path(post, path = "/v1/leaderboards/{board}/scores", tag = "leaderboards", operation_id = "leaderboards_submit", request_body = doc::SubmitScore, security(("bearer" = [])),
    params(("board" = String, Path, description = "The board's key")),
    responses(
        (status = 200, description = "Stored (or a better score kept: `changed` false)", body = doc::ScoreAck),
        (status = 403, description = "`forbidden`: the board takes scores from the server only, or refused by a hook", body = ErrorBody),
        (status = 404, description = "`not_found`: no such board", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the score is i64::MIN or the metadata is too large", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many submissions", body = ErrorBody),
    ))]
pub(crate) async fn submit(
    State(state): State<AppState>,
    Ext(service): Ext<LeaderboardService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<PostScore>,
) -> CallResult<PostScore> {
    // An unknown board is 404 before the rate counts it.
    if service.config().board(&call.board).is_none() {
        return Err(AppError::not_found("no such leaderboard"));
    }
    service.check_rate(who.user_id)?;
    let ctx = HookCtx::new(state.clone(), Some(request_id));
    service.submit_as(&state, &ctx, who.user_id, &call.board, call.submit, Submitter::Player).await.map(Reply::new)
}

/// The caller's rank and the number of players in the period.
#[utoipa::path(get, path = "/v1/leaderboards/{board}/me", tag = "leaderboards", operation_id = "leaderboards_me", security(("bearer" = [])),
    params(
        ("board" = String, Path, description = "The board's key"),
        ("at" = Option<i64>, Query, description = "A time inside the period (unix ms); default: now"),
    ),
    responses(
        (status = 200, description = "The caller's entry (absent without a score) and the total", body = doc::MyRank),
        (status = 404, description = "`not_found`: no such board", body = ErrorBody),
    ))]
pub(crate) async fn me(
    State(state): State<AppState>,
    Ext(service): Ext<LeaderboardService>,
    who: AuthContext,
    Call(call): Call<GetMyRank>,
) -> CallResult<GetMyRank> {
    service.rank(&state, who.user_id, &call.board, call.query.at).await.map(Reply::new)
}

/// The entries around the caller, the caller included, best first.
#[utoipa::path(get, path = "/v1/leaderboards/{board}/around", tag = "leaderboards", operation_id = "leaderboards_around", security(("bearer" = [])),
    params(
        ("board" = String, Path, description = "The board's key"),
        ("above" = Option<u32>, Query, description = "Entries above the caller: 0-50, default 5"),
        ("below" = Option<u32>, Query, description = "Entries below the caller: 0-50, default 5"),
        ("at" = Option<i64>, Query, description = "A time inside the period (unix ms); default: now"),
    ),
    responses(
        (status = 200, description = "The entries (empty when the caller has no score in the period)", body = doc::LeaderboardPage),
        (status = 404, description = "`not_found`: no such board", body = ErrorBody),
    ))]
pub(crate) async fn around(
    State(state): State<AppState>,
    Ext(service): Ext<LeaderboardService>,
    who: AuthContext,
    Call(call): Call<GetAroundMe>,
) -> CallResult<GetAroundMe> {
    service.around(&state, who.user_id, &call.board, &call.query).await.map(Reply::new)
}
