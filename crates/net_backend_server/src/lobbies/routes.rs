//! `/v1/lobbies/*`: lobbies, members, ready flags, metadata and join codes, exactly as the
//! protocol defines them.

use axum::extract::State;
use net_backend_protocol::lobbies::{
    CreateLobby, EditLobby, GetLobby, JoinLobby, JoinLobbyByCode, KickFromLobby, LeaveLobby, LobbySearch, MyLobbies, NewLobbyCode, SetLobbyReady, TransferLobby,
};
use net_backend_protocol::Ack;

use super::openapi as doc;
use super::service::{LobbyActor, LobbyService};
use crate::auth::AuthContext;
use crate::hooks::HookCtx;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::{Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;

fn ctx(state: &AppState, request_id: RequestId) -> HookCtx {
    HookCtx::new(state.clone(), Some(request_id))
}

/// The caller acting on a lobby: a manager (with the `lobbies.manage` permission) acts on any.
fn actor(state: &AppState, who: &AuthContext) -> LobbyActor {
    if super::may_manage(state, who) {
        LobbyActor::Manager(who.user_id)
    } else {
        LobbyActor::Player(who.user_id)
    }
}

/// Create a lobby; the caller hosts it.
#[utoipa::path(post, path = "/v1/lobbies", tag = "lobbies", operation_id = "lobbies_create", request_body = doc::CreateLobby, security(("bearer" = [])),
    responses(
        (status = 200, description = "The new lobby with its join code (the caller hosts it)", body = doc::LobbyInfo),
        (status = 403, description = "`quota_exceeded`: the caller is in too many lobbies; `forbidden`: refused by a hook", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the size, visibility or metadata", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many new lobbies", body = ErrorBody),
    ))]
pub(crate) async fn create(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    request_id: RequestId,
    Call(create): Call<CreateLobby>,
) -> CallResult<CreateLobby> {
    create.validate()?;
    service.check_create_rate(who.user_id)?;
    service.create(&state, &ctx(&state, request_id), who.user_id, create).await.map(Reply::new)
}

/// The caller's lobbies with their members.
#[utoipa::path(get, path = "/v1/lobbies/mine", tag = "lobbies", operation_id = "lobbies_mine", security(("bearer" = [])),
    responses((status = 200, description = "The caller's lobbies, oldest membership first", body = doc::LobbyList), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn mine(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    Call(_call): Call<MyLobbies>,
) -> CallResult<MyLobbies> {
    service.mine(&state, who.user_id).await.map(Reply::new)
}

/// Open lobbies by metadata filters, newest first.
#[utoipa::path(post, path = "/v1/lobbies/search", tag = "lobbies", operation_id = "lobbies_search", request_body = doc::LobbySearch, security(("bearer" = [])),
    responses(
        (status = 200, description = "A page of open lobbies (public ones, or the caller's friends' with `friends`), without their join codes", body = doc::LobbyPage),
        (status = 400, description = "`bad_request`: an invalid cursor", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the filters, or `friends` without the friends module", body = ErrorBody),
    ))]
pub(crate) async fn search(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    Call(search): Call<LobbySearch>,
) -> CallResult<LobbySearch> {
    service.search(&state, who.user_id, &search).await.map(Reply::new)
}

/// Join a lobby with its join code.
#[utoipa::path(post, path = "/v1/lobbies/join", tag = "lobbies", operation_id = "lobbies_join_code", request_body = doc::JoinLobbyByCode, security(("bearer" = [])),
    responses(
        (status = 200, description = "The lobby, the caller a member (also when it was one already)", body = doc::LobbyInfo),
        (status = 403, description = "`quota_exceeded`: the lobby is full or the caller is in too many lobbies; `forbidden`: refused (the host's block, a hook)", body = ErrorBody),
        (status = 404, description = "`not_found`: no lobby has this code", body = ErrorBody),
        (status = 409, description = "`conflict`: the lobby is not open", body = ErrorBody),
        (status = 422, description = "`validation_failed`: not a join code", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many join attempts, or too many codes that matched no lobby", body = ErrorBody),
    ))]
pub(crate) async fn join_code(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    request_id: RequestId,
    Call(join): Call<JoinLobbyByCode>,
) -> CallResult<JoinLobbyByCode> {
    join.validate()?;
    service.check_join_rate(who.user_id)?;
    service.join_by_code(&state, &ctx(&state, request_id), who.user_id, &join.code).await.map(Reply::new)
}

/// One lobby with its members (the join code and the chat room for members).
#[utoipa::path(get, path = "/v1/lobbies/{lobby}", tag = "lobbies", operation_id = "lobbies_get", security(("bearer" = [])),
    params(("lobby" = i64, Path, description = "The lobby")),
    responses((status = 200, description = "The lobby", body = doc::LobbyInfo), (status = 404, description = "`not_found`: no such lobby (or one the caller may not see)", body = ErrorBody)))]
pub(crate) async fn get(State(state): State<AppState>, Ext(service): Ext<LobbyService>, who: AuthContext, Call(call): Call<GetLobby>) -> CallResult<GetLobby> {
    service.get(&state, who.user_id, call.lobby).await.map(Reply::new)
}

/// Change a lobby (the host): metadata, size, visibility, state.
#[utoipa::path(patch, path = "/v1/lobbies/{lobby}", tag = "lobbies", operation_id = "lobbies_update", request_body = doc::UpdateLobby, security(("bearer" = [])),
    params(("lobby" = i64, Path, description = "The lobby")),
    responses(
        (status = 200, description = "The lobby as it is now (after `closed`: as it was, with state `closed`)", body = doc::LobbyInfo),
        (status = 403, description = "`forbidden`: not the host, or refused by a hook", body = ErrorBody),
        (status = 404, description = "`not_found`: no such lobby (also for a caller who is not a member)", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the size (below the member count, above the server's), the visibility or the metadata (also more than two changes per allowed key)", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many lobby changes", body = ErrorBody),
    ))]
pub(crate) async fn update(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<EditLobby>,
) -> CallResult<EditLobby> {
    service.check_update_rate(who.user_id)?;
    service.update(&state, &ctx(&state, request_id), actor(&state, &who), call.lobby, call.update).await.map(Reply::new)
}

/// Join a lobby by id: a public one, or a friends-only one of a friend.
#[utoipa::path(post, path = "/v1/lobbies/{lobby}/join", tag = "lobbies", operation_id = "lobbies_join", security(("bearer" = [])),
    params(("lobby" = i64, Path, description = "The lobby")),
    responses(
        (status = 200, description = "The lobby, the caller a member (also when it was one already)", body = doc::LobbyInfo),
        (status = 403, description = "`quota_exceeded`: the lobby is full or the caller is in too many lobbies; `forbidden`: refused (the host's block, a hook)", body = ErrorBody),
        (status = 404, description = "`not_found`: no such lobby (or one joined with its code only)", body = ErrorBody),
        (status = 409, description = "`conflict`: the lobby is not open", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many join attempts", body = ErrorBody),
    ))]
pub(crate) async fn join(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<JoinLobby>,
) -> CallResult<JoinLobby> {
    service.check_join_rate(who.user_id)?;
    service.join(&state, &ctx(&state, request_id), who.user_id, call.lobby).await.map(Reply::new)
}

/// Leave a lobby (the host passes to the member who joined first; the last member's leaving
/// removes the lobby).
#[utoipa::path(post, path = "/v1/lobbies/{lobby}/leave", tag = "lobbies", operation_id = "lobbies_leave", security(("bearer" = [])),
    params(("lobby" = i64, Path, description = "The lobby")),
    responses(
        (status = 200, description = "Left (also when the caller was no member)", body = doc::Ack),
        (status = 404, description = "`not_found`: no such lobby", body = ErrorBody),
    ))]
pub(crate) async fn leave(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<LeaveLobby>,
) -> CallResult<LeaveLobby> {
    service.leave(&state, &ctx(&state, request_id), who.user_id, call.lobby).await?;
    Ok(Reply::new(Ack::new()))
}

/// Set the caller's ready flag.
#[utoipa::path(put, path = "/v1/lobbies/{lobby}/ready", tag = "lobbies", operation_id = "lobbies_ready", request_body = doc::SetReady, security(("bearer" = [])),
    params(("lobby" = i64, Path, description = "The lobby")),
    responses(
        (status = 200, description = "Set (also when it was that already)", body = doc::Ack),
        (status = 403, description = "`not_a_member`: the caller is not in the lobby", body = ErrorBody),
        (status = 404, description = "`not_found`: no such lobby", body = ErrorBody),
    ))]
pub(crate) async fn ready(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<SetLobbyReady>,
) -> CallResult<SetLobbyReady> {
    service.set_ready(&state, &ctx(&state, request_id), who.user_id, call.lobby, call.ready.ready).await?;
    Ok(Reply::new(Ack::new()))
}

/// A new join code (the host); the old one stops working.
#[utoipa::path(post, path = "/v1/lobbies/{lobby}/code", tag = "lobbies", operation_id = "lobbies_code", security(("bearer" = [])),
    params(("lobby" = i64, Path, description = "The lobby")),
    responses(
        (status = 200, description = "The lobby with its new code", body = doc::LobbyInfo),
        (status = 403, description = "`forbidden`: not the host", body = ErrorBody),
        (status = 404, description = "`not_found`: no such lobby (also for a caller who is not a member)", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many lobby changes", body = ErrorBody),
    ))]
pub(crate) async fn code(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<NewLobbyCode>,
) -> CallResult<NewLobbyCode> {
    service.check_update_rate(who.user_id)?;
    service.new_code(&state, &ctx(&state, request_id), actor(&state, &who), call.lobby).await.map(Reply::new)
}

/// Hand the lobby to another member (the host).
#[utoipa::path(post, path = "/v1/lobbies/{lobby}/host", tag = "lobbies", operation_id = "lobbies_host", request_body = doc::LobbyPlayer, security(("bearer" = [])),
    params(("lobby" = i64, Path, description = "The lobby")),
    responses(
        (status = 200, description = "The member hosts the lobby now", body = doc::Ack),
        (status = 403, description = "`forbidden`: not the host", body = ErrorBody),
        (status = 404, description = "`not_found`: no such lobby or member (also for a caller who is not a member)", body = ErrorBody),
    ))]
pub(crate) async fn host(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<TransferLobby>,
) -> CallResult<TransferLobby> {
    service.transfer(&state, &ctx(&state, request_id), actor(&state, &who), call.lobby, call.to.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// Remove a member (the host).
#[utoipa::path(delete, path = "/v1/lobbies/{lobby}/members/{user}", tag = "lobbies", operation_id = "lobbies_kick", security(("bearer" = [])),
    params(("lobby" = i64, Path, description = "The lobby"), ("user" = i64, Path, description = "The member")),
    responses(
        (status = 200, description = "Removed (or the player was no member)", body = doc::Ack),
        (status = 403, description = "`forbidden`: not the host", body = ErrorBody),
        (status = 404, description = "`not_found`: no such lobby (also for a caller who is not a member)", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the caller's own id (leave instead)", body = ErrorBody),
    ))]
pub(crate) async fn kick(
    State(state): State<AppState>,
    Ext(service): Ext<LobbyService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<KickFromLobby>,
) -> CallResult<KickFromLobby> {
    service.kick(&state, &ctx(&state, request_id), actor(&state, &who), call.lobby, call.user).await?;
    Ok(Reply::new(Ack::new()))
}
