//! `/v1/admin/*`: account administration and the audit log. Every route needs the `admin` role.

use axum::extract::State;
use net_backend_protocol::admin::{AuditQuery, BanUser, GetUser, GrantRole, RevokeRole, RevokeSessions, UnbanUser, UnlinkUserIdentity, UserListQuery};
use net_backend_protocol::Ack;

use super::openapi as doc;
use super::service::{Actor, AuthService, ReqInfo};
use super::RequireAdmin;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::Ext;
use crate::openapi::ErrorBody;
use crate::state::AppState;

fn actor(admin: &RequireAdmin, info: ReqInfo) -> Actor {
    Actor { user: Some(admin.0.user_id), info, cli: false }
}

/// List accounts, newest first (`q` searches email, display name and id).
#[utoipa::path(get, path = "/v1/admin/users", tag = "admin", operation_id = "admin_list_users", security(("bearer" = [])),
    params(
        ("q" = Option<String>, Query, description = "Search text: part of the email (case-insensitive) or display name, or an id"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses((status = 200, description = "A page of accounts", body = doc::AdminUserPage), (status = 403, description = "`forbidden`: not an admin", body = ErrorBody)))]
pub(crate) async fn list_users(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    _admin: RequireAdmin,
    Call(query): Call<UserListQuery>,
) -> CallResult<UserListQuery> {
    service.list_users(&state, &query).await.map(Reply::new)
}

/// One account.
#[utoipa::path(get, path = "/v1/admin/users/{user}", tag = "admin", operation_id = "admin_get_user", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The user id")),
    responses((status = 200, description = "The account", body = doc::AdminUser), (status = 404, description = "`not_found`", body = ErrorBody)))]
pub(crate) async fn get_user(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    _admin: RequireAdmin,
    Call(call): Call<GetUser>,
) -> CallResult<GetUser> {
    service.admin_user(&state, call.user).await.map(Reply::new)
}

/// Ban an account: its sessions are revoked, logins answer `banned` while the ban lasts.
#[utoipa::path(post, path = "/v1/admin/users/{user}/ban", tag = "admin", operation_id = "admin_ban", request_body = doc::BanRequest, security(("bearer" = [])),
    params(("user" = i64, Path, description = "The user id")),
    responses((status = 200, description = "Banned", body = doc::Ack), (status = 404, description = "`not_found`", body = ErrorBody), (status = 409, description = "`conflict`: banning yourself", body = ErrorBody), (status = 422, description = "`validation_failed`", body = ErrorBody)))]
pub(crate) async fn ban(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    admin: RequireAdmin,
    info: ReqInfo,
    Call(call): Call<BanUser>,
) -> CallResult<BanUser> {
    service.ban(&state, &actor(&admin, info), call.user, call.ban).await.map(|()| Reply::new(Ack::new()))
}

/// Lift a ban.
#[utoipa::path(post, path = "/v1/admin/users/{user}/unban", tag = "admin", operation_id = "admin_unban", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The user id")),
    responses((status = 200, description = "Unbanned", body = doc::Ack), (status = 404, description = "`not_found`", body = ErrorBody)))]
pub(crate) async fn unban(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    admin: RequireAdmin,
    info: ReqInfo,
    Call(call): Call<UnbanUser>,
) -> CallResult<UnbanUser> {
    service.unban(&state, &actor(&admin, info), call.user).await.map(|()| Reply::new(Ack::new()))
}

/// Revoke every session of an account.
#[utoipa::path(delete, path = "/v1/admin/users/{user}/sessions", tag = "admin", operation_id = "admin_revoke_sessions", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The user id")),
    responses((status = 200, description = "Revoked", body = doc::Ack), (status = 404, description = "`not_found`", body = ErrorBody)))]
pub(crate) async fn revoke_sessions(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    admin: RequireAdmin,
    info: ReqInfo,
    Call(call): Call<RevokeSessions>,
) -> CallResult<RevokeSessions> {
    service.admin_revoke(&state, &actor(&admin, info), call.user).await.map(|_| Reply::new(Ack::new()))
}

/// Grant a role (takes effect with the account's next request).
#[utoipa::path(put, path = "/v1/admin/users/{user}/roles/{role}", tag = "admin", operation_id = "admin_grant_role", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The user id"), ("role" = String, Path, description = "[a-z][a-z0-9_.-]*, at most 64 bytes")),
    responses((status = 200, description = "Granted (or already held)", body = doc::Ack), (status = 404, description = "`not_found`", body = ErrorBody)))]
pub(crate) async fn grant_role(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    admin: RequireAdmin,
    info: ReqInfo,
    Call(call): Call<GrantRole>,
) -> CallResult<GrantRole> {
    service.set_role(&state, &actor(&admin, info), call.user, &call.role, true).await.map(|()| Reply::new(Ack::new()))
}

/// Revoke a role.
#[utoipa::path(delete, path = "/v1/admin/users/{user}/roles/{role}", tag = "admin", operation_id = "admin_revoke_role", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The user id"), ("role" = String, Path, description = "The role")),
    responses((status = 200, description = "Revoked (or not held)", body = doc::Ack), (status = 409, description = "`conflict`: removing your own admin role", body = ErrorBody)))]
pub(crate) async fn revoke_role(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    admin: RequireAdmin,
    info: ReqInfo,
    Call(call): Call<RevokeRole>,
) -> CallResult<RevokeRole> {
    service.set_role(&state, &actor(&admin, info), call.user, &call.role, false).await.map(|()| Reply::new(Ack::new()))
}

/// The audit log, newest first.
#[utoipa::path(get, path = "/v1/admin/audit", tag = "admin", operation_id = "admin_audit", security(("bearer" = [])),
    params(
        ("user" = Option<i64>, Query, description = "Entries where this user acted or was the target"),
        ("action" = Option<String>, Query, description = "One action, or a prefix ending in `.` (`admin.`)"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses((status = 200, description = "A page of entries", body = doc::AuditPage), (status = 403, description = "`forbidden`: not an admin", body = ErrorBody)))]
pub(crate) async fn audit(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    _admin: RequireAdmin,
    Call(query): Call<AuditQuery>,
) -> CallResult<AuditQuery> {
    service.audit_log(&state, &query).await.map(Reply::new)
}

/// Unlink a login provider from an account.
#[utoipa::path(delete, path = "/v1/admin/users/{user}/identities/{provider}", tag = "admin", operation_id = "admin_unlink_identity", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The user id"), ("provider" = String, Path, description = "The provider, e.g. `steam`")),
    responses((status = 200, description = "Unlinked", body = doc::Ack), (status = 404, description = "`not_found`", body = ErrorBody)))]
pub(crate) async fn unlink_identity(
    State(state): State<AppState>,
    Ext(service): Ext<AuthService>,
    admin: RequireAdmin,
    info: ReqInfo,
    Call(call): Call<UnlinkUserIdentity>,
) -> CallResult<UnlinkUserIdentity> {
    let actor = actor(&admin, info.clone());
    service.unlink_identity(&state, &info, call.user, &call.provider, Some(&actor)).await.map(|()| Reply::new(Ack::new()))
}
