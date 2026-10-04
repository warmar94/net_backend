//! `/v1/groups/*`: groups, members, roles and invitations, exactly as the protocol defines them.

use axum::extract::State;
use net_backend_protocol::groups::{
    AcceptGroupInvite, CreateGroup, DeclineGroupInvite, DeleteGroup, EditGroup, GetGroup, InviteToGroup, JoinGroup, KickMember, LeaveGroup, ListGroupInvites,
    ListGroupMembers, ListGroups, MyGroups, RevokeGroupInvite, SetMemberRole, TransferGroup,
};
use net_backend_protocol::Ack;

use super::openapi as doc;
use super::service::GroupService;
use crate::auth::AuthContext;
use crate::hooks::HookCtx;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::{Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;

fn ctx(state: &AppState, request_id: RequestId) -> HookCtx {
    HookCtx::new(state.clone(), Some(request_id))
}

/// The groups by name, optionally those whose name starts with `query`.
#[utoipa::path(get, path = "/v1/groups", tag = "groups", operation_id = "groups_list", security(("bearer" = [])),
    params(
        ("query" = Option<String>, Query, description = "The start of the name (at most 32 characters, case does not matter)"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses(
        (status = 200, description = "A page of groups by name (with the caller's role where a member)", body = doc::GroupPage),
        (status = 422, description = "`validation_failed`: the query is too long", body = ErrorBody),
    ))]
pub(crate) async fn list(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    Call(call): Call<ListGroups>,
) -> CallResult<ListGroups> {
    service.search(&state, who.user_id, &call.query).await.map(Reply::new)
}

/// Create a group; the caller owns it.
#[utoipa::path(post, path = "/v1/groups", tag = "groups", operation_id = "groups_create", request_body = doc::CreateGroup, security(("bearer" = [])),
    responses(
        (status = 200, description = "The new group (the caller's role: owner)", body = doc::GroupInfo),
        (status = 403, description = "`quota_exceeded`: the caller is in too many groups; `forbidden`: refused by a hook", body = ErrorBody),
        (status = 409, description = "`conflict`: a group with this name exists", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the name, description or metadata", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many new groups", body = ErrorBody),
    ))]
pub(crate) async fn create(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(create): Call<CreateGroup>,
) -> CallResult<CreateGroup> {
    create.validate()?;
    service.check_rate(who.user_id)?;
    service.create(&state, &ctx(&state, request_id), who.user_id, create).await.map(Reply::new)
}

/// The caller's groups with its role.
#[utoipa::path(get, path = "/v1/groups/mine", tag = "groups", operation_id = "groups_mine", security(("bearer" = [])),
    responses((status = 200, description = "The caller's groups, oldest membership first", body = doc::GroupList), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn mine(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    Call(_call): Call<MyGroups>,
) -> CallResult<MyGroups> {
    service.mine(&state, who.user_id).await.map(Reply::new)
}

/// The caller's invitations, newest first.
#[utoipa::path(get, path = "/v1/groups/invites", tag = "groups", operation_id = "groups_invites", security(("bearer" = [])),
    params(
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses((status = 200, description = "A page of invitations", body = doc::InvitePage), (status = 400, description = "`bad_request`: an invalid cursor", body = ErrorBody)))]
pub(crate) async fn invites(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    Call(call): Call<ListGroupInvites>,
) -> CallResult<ListGroupInvites> {
    service.invites(&state, who.user_id, &call.page).await.map(Reply::new)
}

/// One group, with the caller's role when a member.
#[utoipa::path(get, path = "/v1/groups/{group}", tag = "groups", operation_id = "groups_get", security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group")),
    responses((status = 200, description = "The group", body = doc::GroupInfo), (status = 404, description = "`not_found`: no such group", body = ErrorBody)))]
pub(crate) async fn get(State(state): State<AppState>, Ext(service): Ext<GroupService>, who: AuthContext, Call(call): Call<GetGroup>) -> CallResult<GetGroup> {
    service.get(&state, who.user_id, call.group).await.map(Reply::new)
}

/// Change a group (the owner or an admin).
#[utoipa::path(patch, path = "/v1/groups/{group}", tag = "groups", operation_id = "groups_update", request_body = doc::UpdateGroup, security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group")),
    responses(
        (status = 200, description = "The changed group", body = doc::GroupInfo),
        (status = 403, description = "`not_a_member`, or `forbidden`: not the owner or an admin, or refused by a hook", body = ErrorBody),
        (status = 404, description = "`not_found`: no such group", body = ErrorBody),
        (status = 409, description = "`conflict`: a group with this name exists", body = ErrorBody),
        (status = 422, description = "`validation_failed`", body = ErrorBody),
    ))]
pub(crate) async fn update(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<EditGroup>,
) -> CallResult<EditGroup> {
    service.update(&state, &ctx(&state, request_id), who.user_id, call.group, call.update).await.map(Reply::new)
}

/// Delete a group (its owner).
#[utoipa::path(delete, path = "/v1/groups/{group}", tag = "groups", operation_id = "groups_delete", security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group")),
    responses(
        (status = 200, description = "Deleted", body = doc::Ack),
        (status = 403, description = "`not_a_member`, or `forbidden`: not the owner", body = ErrorBody),
        (status = 404, description = "`not_found`: no such group", body = ErrorBody),
    ))]
pub(crate) async fn delete(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<DeleteGroup>,
) -> CallResult<DeleteGroup> {
    service.delete(&state, &ctx(&state, request_id), who.user_id, call.group).await?;
    Ok(Reply::new(Ack::new()))
}

/// A group's members, in the order they joined.
#[utoipa::path(get, path = "/v1/groups/{group}/members", tag = "groups", operation_id = "groups_members", security(("bearer" = [])),
    params(
        ("group" = i64, Path, description = "The group"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses((status = 200, description = "A page of members", body = doc::MemberPage), (status = 404, description = "`not_found`: no such group", body = ErrorBody)))]
pub(crate) async fn members(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    _who: AuthContext,
    Call(call): Call<ListGroupMembers>,
) -> CallResult<ListGroupMembers> {
    service.members(&state, call.group, &call.page).await.map(Reply::new)
}

/// Join an open group, or one that invited the caller.
#[utoipa::path(post, path = "/v1/groups/{group}/join", tag = "groups", operation_id = "groups_join", security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group")),
    responses(
        (status = 200, description = "The group (also when the caller was a member already)", body = doc::GroupInfo),
        (status = 403, description = "`forbidden`: invitation only, or refused by a hook; `quota_exceeded`: the group is full or the caller is in too many groups", body = ErrorBody),
        (status = 404, description = "`not_found`: no such group", body = ErrorBody),
    ))]
pub(crate) async fn join(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<JoinGroup>,
) -> CallResult<JoinGroup> {
    service.join(&state, &ctx(&state, request_id), who.user_id, call.group, false).await.map(Reply::new)
}

/// Leave a group (the owner only as the last member: the group is deleted then).
#[utoipa::path(post, path = "/v1/groups/{group}/leave", tag = "groups", operation_id = "groups_leave", security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group")),
    responses(
        (status = 200, description = "Left (also when the caller was no member)", body = doc::Ack),
        (status = 404, description = "`not_found`: no such group", body = ErrorBody),
        (status = 409, description = "`conflict`: the owner of a group with other members (transfer first)", body = ErrorBody),
    ))]
pub(crate) async fn leave(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<LeaveGroup>,
) -> CallResult<LeaveGroup> {
    service.leave(&state, &ctx(&state, request_id), who.user_id, call.group).await?;
    Ok(Reply::new(Ack::new()))
}

/// Invite a player (the owner or an admin).
#[utoipa::path(post, path = "/v1/groups/{group}/invites", tag = "groups", operation_id = "groups_invite", request_body = doc::Invitee, security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group")),
    responses(
        (status = 200, description = "Invited (also when the player was invited already)", body = doc::Ack),
        (status = 403, description = "`not_a_member` / `forbidden`: not the owner or an admin, the player blocked the caller, or a hook refused; `quota_exceeded`: too many open invitations", body = ErrorBody),
        (status = 404, description = "`not_found`: no such group or account", body = ErrorBody),
        (status = 409, description = "`conflict`: the player is a member already", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the caller's own id", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many invitations", body = ErrorBody),
    ))]
pub(crate) async fn invite(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<InviteToGroup>,
) -> CallResult<InviteToGroup> {
    service.check_invite_rate(who.user_id)?;
    service.invite(&state, &ctx(&state, request_id), who.user_id, call.group, call.invitee.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// Accept an invitation: join the group.
#[utoipa::path(post, path = "/v1/groups/{group}/invites/accept", tag = "groups", operation_id = "groups_accept", security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group")),
    responses(
        (status = 200, description = "The group, the caller a member", body = doc::GroupInfo),
        (status = 403, description = "`quota_exceeded`: the group is full or the caller is in too many groups; `forbidden`: refused by a hook", body = ErrorBody),
        (status = 404, description = "`not_found`: no such group, or no invitation", body = ErrorBody),
    ))]
pub(crate) async fn accept(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<AcceptGroupInvite>,
) -> CallResult<AcceptGroupInvite> {
    service.join(&state, &ctx(&state, request_id), who.user_id, call.group, true).await.map(Reply::new)
}

/// Decline an invitation (answered `{}` also when there was none).
#[utoipa::path(post, path = "/v1/groups/{group}/invites/decline", tag = "groups", operation_id = "groups_decline", security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group")),
    responses((status = 200, description = "Declined (or there was none)", body = doc::Ack), (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody)))]
pub(crate) async fn decline(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<DeclineGroupInvite>,
) -> CallResult<DeclineGroupInvite> {
    service.decline_invite(&state, &ctx(&state, request_id), who.user_id, call.group).await?;
    Ok(Reply::new(Ack::new()))
}

/// Withdraw an invitation (the owner or an admin; answered `{}` also when there was none).
#[utoipa::path(delete, path = "/v1/groups/{group}/invites/{user}", tag = "groups", operation_id = "groups_revoke", security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group"), ("user" = i64, Path, description = "The invited player")),
    responses(
        (status = 200, description = "Withdrawn (or there was none)", body = doc::Ack),
        (status = 403, description = "`not_a_member` / `forbidden`: not the owner or an admin", body = ErrorBody),
        (status = 404, description = "`not_found`: no such group", body = ErrorBody),
    ))]
pub(crate) async fn revoke(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<RevokeGroupInvite>,
) -> CallResult<RevokeGroupInvite> {
    service.revoke_invite(&state, &ctx(&state, request_id), who.user_id, call.group, call.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// Remove a member (the owner: anyone; admins: members).
#[utoipa::path(delete, path = "/v1/groups/{group}/members/{user}", tag = "groups", operation_id = "groups_kick", security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group"), ("user" = i64, Path, description = "The member")),
    responses(
        (status = 200, description = "Removed (or the player was no member)", body = doc::Ack),
        (status = 403, description = "`not_a_member` / `forbidden`: not allowed to remove this member", body = ErrorBody),
        (status = 404, description = "`not_found`: no such group", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the caller's own id (leave instead)", body = ErrorBody),
    ))]
pub(crate) async fn kick(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<KickMember>,
) -> CallResult<KickMember> {
    service.kick(&state, &ctx(&state, request_id), who.user_id, call.group, call.user).await?;
    Ok(Reply::new(Ack::new()))
}

/// Give a member the role `admin` or `member` (the owner).
#[utoipa::path(put, path = "/v1/groups/{group}/members/{user}/role", tag = "groups", operation_id = "groups_role", request_body = doc::RoleChange, security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group"), ("user" = i64, Path, description = "The member")),
    responses(
        (status = 200, description = "Changed (or it was that role)", body = doc::Ack),
        (status = 403, description = "`not_a_member` / `forbidden`: not the owner", body = ErrorBody),
        (status = 404, description = "`not_found`: no such group or member", body = ErrorBody),
        (status = 422, description = "`validation_failed`: not admin or member, or the owner's own id", body = ErrorBody),
    ))]
pub(crate) async fn role(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<SetMemberRole>,
) -> CallResult<SetMemberRole> {
    service.set_role(&state, &ctx(&state, request_id), who.user_id, call.group, call.user, call.change.role).await?;
    Ok(Reply::new(Ack::new()))
}

/// Hand the group to a member (the owner); the old owner becomes an admin.
#[utoipa::path(post, path = "/v1/groups/{group}/transfer", tag = "groups", operation_id = "groups_transfer", request_body = doc::Invitee, security(("bearer" = [])),
    params(("group" = i64, Path, description = "The group")),
    responses(
        (status = 200, description = "The member owns the group now", body = doc::Ack),
        (status = 403, description = "`not_a_member` / `forbidden`: not the owner", body = ErrorBody),
        (status = 404, description = "`not_found`: no such group or member", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the owner's own id", body = ErrorBody),
    ))]
pub(crate) async fn transfer(
    State(state): State<AppState>,
    Ext(service): Ext<GroupService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<TransferGroup>,
) -> CallResult<TransferGroup> {
    service.transfer(&state, &ctx(&state, request_id), who.user_id, call.group, call.to.user).await?;
    Ok(Reply::new(Ack::new()))
}
