//! `/v1/friends/*`: friends, requests, blocks, the friend code, the heartbeat, the Steam ID lookup
//! and the settings, exactly as the protocol defines them.

use axum::extract::State;
use net_backend_protocol::friends::{
    AcceptFriend, AddFriend, BlockUser, CancelFriendRequest, DeclineFriend, FriendState, FriendsHeartbeat, GetFriendCode, GetFriendSettings, ListBlocks,
    ListFriendRequests, ListFriends, RemoveFriend, RequestDirection, ResetFriendCode, SteamMatch, SteamMatchResult, UnblockUser, UpdateFriendSettings,
};
use net_backend_protocol::Ack;
use net_backend_protocol::ValidationDetails;

use super::openapi as doc;
use super::service::FriendService;
use crate::auth::{AuthContext, AuthService};
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::{Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;

fn ctx(state: &AppState, request_id: RequestId) -> HookCtx {
    HookCtx::new(state.clone(), Some(request_id))
}

/// The caller's friends, newest first, with their online state.
#[utoipa::path(get, path = "/v1/friends", tag = "friends", operation_id = "friends_list", security(("bearer" = [])),
    params(
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses(
        (status = 200, description = "A page of friends (state `friend`, with `online` and `last_seen`)", body = doc::FriendPage),
        (status = 400, description = "`bad_request`: an invalid cursor", body = ErrorBody),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
    ))]
pub(crate) async fn friends(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    Call(call): Call<ListFriends>,
) -> CallResult<ListFriends> {
    service.list(&state, who.user_id, FriendState::Friend, &call.page).await.map(Reply::new)
}

/// End a friendship (answered `{}` also when there was none).
#[utoipa::path(delete, path = "/v1/friends/{user}", tag = "friends", operation_id = "friends_remove", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The friend")),
    responses(
        (status = 200, description = "Ended (or there was none)", body = doc::Ack),
        (status = 404, description = "`not_found`: no such account", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the caller's own id", body = ErrorBody),
    ))]
pub(crate) async fn remove(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<RemoveFriend>,
) -> CallResult<RemoveFriend> {
    service.remove(&state, &ctx(&state, request_id), who.user_id, call.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// The caller's open friend requests, received (default) or sent, newest first.
#[utoipa::path(get, path = "/v1/friends/requests", tag = "friends", operation_id = "friends_requests", security(("bearer" = [])),
    params(
        ("direction" = Option<String>, Query, description = "`received` (default) or `sent`"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses(
        (status = 200, description = "A page of requests (state `received` or `sent`)", body = doc::FriendPage),
        (status = 400, description = "`bad_request`: an invalid cursor or direction", body = ErrorBody),
    ))]
pub(crate) async fn requests(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    Call(call): Call<ListFriendRequests>,
) -> CallResult<ListFriendRequests> {
    let which = match call.query.direction.unwrap_or_default() {
        RequestDirection::Received => FriendState::Received,
        RequestDirection::Sent => FriendState::Sent,
        _ => return Err(AppError::bad_request("direction must be received or sent")),
    };
    service.list(&state, who.user_id, which, &call.query.page()).await.map(Reply::new)
}

/// Send a friend request by account id, display name or friend code.
#[utoipa::path(post, path = "/v1/friends/requests", tag = "friends", operation_id = "friends_add", request_body = doc::AddFriend, security(("bearer" = [])),
    responses(
        (status = 200, description = "The caller's entry: `sent`, or `friend` when that player had asked first (also for a request already sent)", body = doc::FriendEntry),
        (status = 403, description = "`forbidden`: that player blocked the caller, or refused by a hook; `quota_exceeded`: too many open requests (the caller's sent or the other's received)", body = ErrorBody),
        (status = 404, description = "`not_found`: no such account, name or code", body = ErrorBody),
        (status = 409, description = "`conflict`: several players have that name, or the caller blocked that player", body = ErrorBody),
        (status = 422, description = "`validation_failed`: not exactly one of user, name and code, or the caller's own account", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many requests", body = ErrorBody),
    ))]
pub(crate) async fn add(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    request_id: RequestId,
    Call(add): Call<AddFriend>,
) -> CallResult<AddFriend> {
    add.validate()?;
    service.check_rate(who.user_id)?;
    service.add(&state, &ctx(&state, request_id), who.user_id, &add).await.map(Reply::new)
}

/// Withdraw a friend request the caller sent (answered `{}` also when there was none).
#[utoipa::path(delete, path = "/v1/friends/requests/{user}", tag = "friends", operation_id = "friends_cancel", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The player the request went to")),
    responses((status = 200, description = "Withdrawn (or there was none)", body = doc::Ack), (status = 404, description = "`not_found`: no such account", body = ErrorBody)))]
pub(crate) async fn cancel(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<CancelFriendRequest>,
) -> CallResult<CancelFriendRequest> {
    service.cancel(&state, &ctx(&state, request_id), who.user_id, call.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// Accept a friend request the caller received.
#[utoipa::path(post, path = "/v1/friends/requests/{user}/accept", tag = "friends", operation_id = "friends_accept", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The player who sent the request")),
    responses(
        (status = 200, description = "Friends now (also when they were already)", body = doc::FriendEntry),
        (status = 404, description = "`not_found`: no request from this player", body = ErrorBody),
        (status = 403, description = "`quota_exceeded`: one of the two has reached the friend limit", body = ErrorBody),
    ))]
pub(crate) async fn accept(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<AcceptFriend>,
) -> CallResult<AcceptFriend> {
    service.accept(&state, &ctx(&state, request_id), who.user_id, call.user).await.map(Reply::new)
}

/// Decline a friend request the caller received (answered `{}` also when there was none).
#[utoipa::path(post, path = "/v1/friends/requests/{user}/decline", tag = "friends", operation_id = "friends_decline", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The player who sent the request")),
    responses((status = 200, description = "Declined (or there was none)", body = doc::Ack), (status = 404, description = "`not_found`: no such account", body = ErrorBody)))]
pub(crate) async fn decline(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<DeclineFriend>,
) -> CallResult<DeclineFriend> {
    service.decline(&state, &ctx(&state, request_id), who.user_id, call.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// The players the caller blocked, newest first.
#[utoipa::path(get, path = "/v1/friends/blocks", tag = "friends", operation_id = "friends_blocks", security(("bearer" = [])),
    params(
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses((status = 200, description = "A page of blocked players (state `blocked`)", body = doc::FriendPage), (status = 400, description = "`bad_request`: an invalid cursor", body = ErrorBody)))]
pub(crate) async fn blocks(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    Call(call): Call<ListBlocks>,
) -> CallResult<ListBlocks> {
    service.list(&state, who.user_id, FriendState::Blocked, &call.page).await.map(Reply::new)
}

/// Block a player: ends a friendship and the open requests between the two.
#[utoipa::path(put, path = "/v1/friends/blocks/{user}", tag = "friends", operation_id = "friends_block", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The player to block")),
    responses(
        (status = 200, description = "Blocked (also when it was already)", body = doc::Ack),
        (status = 404, description = "`not_found`: no such account", body = ErrorBody),
        (status = 403, description = "`quota_exceeded`: too many blocked players", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the caller's own id", body = ErrorBody),
    ))]
pub(crate) async fn block(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<BlockUser>,
) -> CallResult<BlockUser> {
    service.block(&state, &ctx(&state, request_id), who.user_id, call.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// Lift a block (answered `{}` also when there was none).
#[utoipa::path(delete, path = "/v1/friends/blocks/{user}", tag = "friends", operation_id = "friends_unblock", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The blocked player")),
    responses((status = 200, description = "Unblocked (or there was no block)", body = doc::Ack), (status = 404, description = "`not_found`: no such account", body = ErrorBody)))]
pub(crate) async fn unblock(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<UnblockUser>,
) -> CallResult<UnblockUser> {
    service.unblock(&state, &ctx(&state, request_id), who.user_id, call.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// The caller's friend code (made on first use).
#[utoipa::path(get, path = "/v1/friends/code", tag = "friends", operation_id = "friends_code", security(("bearer" = [])),
    responses((status = 200, description = "The code", body = doc::FriendCode), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn code(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    Call(_call): Call<GetFriendCode>,
) -> CallResult<GetFriendCode> {
    service.code(&state, who.user_id).await.map(Reply::new)
}

/// A new friend code for the caller; the old one stops working.
#[utoipa::path(post, path = "/v1/friends/code", tag = "friends", operation_id = "friends_code_reset", security(("bearer" = [])),
    responses(
        (status = 200, description = "The new code", body = doc::FriendCode),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many heartbeats, code resets and settings changes", body = ErrorBody),
    ))]
pub(crate) async fn reset_code(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    Call(_call): Call<ResetFriendCode>,
) -> CallResult<ResetFriendCode> {
    service.check_update_rate(who.user_id)?;
    service.reset_code(&state, who.user_id).await.map(Reply::new)
}

/// The caller is online for the online window (clients without a WebSocket connection).
#[utoipa::path(post, path = "/v1/friends/presence", tag = "friends", operation_id = "friends_heartbeat", security(("bearer" = [])),
    responses(
        (status = 200, description = "Online for the server's online window (90 s by default)", body = doc::Ack),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many heartbeats, code resets and settings changes", body = ErrorBody),
    ))]
pub(crate) async fn heartbeat(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    Call(_call): Call<FriendsHeartbeat>,
) -> CallResult<FriendsHeartbeat> {
    service.check_update_rate(who.user_id)?;
    service.heartbeat(&state, who.user_id).await?;
    Ok(Reply::new(Ack::new()))
}

/// Which of these Steam IDs belong to accounts here (the caller has a Steam account linked).
#[utoipa::path(post, path = "/v1/friends/steam", tag = "friends", operation_id = "friends_steam", request_body = doc::SteamMatch, security(("bearer" = [])),
    responses(
        (status = 200, description = "The accounts found, in the order of the request: Steam-linked, not the caller, not banned, findable, no block between them and the caller", body = doc::SteamMatchResult),
        (status = 403, description = "`forbidden`: the caller has no Steam account linked, or refused by a hook", body = ErrorBody),
        (status = 404, description = "`not_found`: Steam login is not enabled on this server", body = ErrorBody),
        (status = 422, description = "`validation_failed`: an entry is not the decimal SteamID64 of an individual Steam account, or more Steam IDs than the server's cap (500 by default)", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many lookups (3, then one every 5 minutes, by default)", body = ErrorBody),
    ))]
pub(crate) async fn steam(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    request_id: RequestId,
    Call(lookup): Call<SteamMatch>,
) -> CallResult<SteamMatch> {
    if !state.get::<AuthService>().is_some_and(|auth| auth.steam_enabled()) {
        return Err(AppError::not_found("Steam login is not enabled on this server"));
    }
    lookup.validate()?;
    let cap = service.config().steam_max_ids;
    if lookup.steam_ids.len() > cap as usize {
        let mut details = ValidationDetails::new();
        details.add("steam_ids", format!("at most {cap} Steam IDs"));
        return Err(AppError::validation(details));
    }
    service.check_steam_rate(who.user_id)?;
    let players = service.steam_match(&state, &ctx(&state, request_id), who.user_id, lookup.ids()).await?;
    Ok(Reply::new(SteamMatchResult::new(players)))
}

/// The caller's friends settings.
#[utoipa::path(get, path = "/v1/friends/settings", tag = "friends", operation_id = "friends_settings", security(("bearer" = [])),
    responses((status = 200, description = "The settings (`steam_findable` is true until the player turns it off)", body = doc::FriendSettings), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn settings(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    Call(_call): Call<GetFriendSettings>,
) -> CallResult<GetFriendSettings> {
    service.settings(&state, who.user_id).await.map(Reply::new)
}

/// Change the caller's friends settings (fields left out keep their value).
#[utoipa::path(put, path = "/v1/friends/settings", tag = "friends", operation_id = "friends_settings_update", request_body = doc::UpdateFriendSettings, security(("bearer" = [])),
    responses(
        (status = 200, description = "The settings after the change", body = doc::FriendSettings),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many heartbeats, code resets and settings changes", body = ErrorBody),
    ))]
pub(crate) async fn update_settings(
    State(state): State<AppState>,
    Ext(service): Ext<FriendService>,
    who: AuthContext,
    Call(update): Call<UpdateFriendSettings>,
) -> CallResult<UpdateFriendSettings> {
    service.check_update_rate(who.user_id)?;
    service.update_settings(&state, who.user_id, &update).await.map(Reply::new)
}
