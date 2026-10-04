//! `/v1/admin/users/{user}/storage/*`: a user's objects for operators. Every route needs the
//! `admin` role; every access (reads too) is written to the audit log (`admin.storage_list`,
//! `admin.storage_read`, `admin.storage_write`, `admin.storage_delete`; writes in the same
//! transaction as the change).

use axum::extract::State;
use http::header::{HeaderMap, ETAG};
use net_backend_protocol::admin::{GetUserObject, ListUserObjects, RemoveUserObject, WriteUserObject};
use net_backend_protocol::Ack;
use serde_json::json;

use super::events::Writer;
use super::openapi as doc;
use super::routes::{condition, etag};
use super::service::{StorageService, WriteRequest};
use crate::auth::audit::{self, AuditRecord};
use crate::auth::RequireAdmin;
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::{ClientIp, Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;

fn record(action: &str, admin: &RequireAdmin, user: net_backend_protocol::UserId, ip: &ClientIp, request_id: &RequestId) -> AuditRecord {
    AuditRecord::new(format!("admin.{action}")).actor(Some(admin.0.user_id)).target_user(user).ip(ip.ip()).request_id(Some(request_id))
}

/// List one collection of a user's storage (no values).
#[utoipa::path(get, path = "/v1/admin/users/{user}/storage/{collection}", tag = "admin", operation_id = "admin_storage_list", security(("bearer" = [])),
    params(
        ("user" = i64, Path, description = "The user id"),
        ("collection" = String, Path, description = "The collection"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses((status = 200, description = "A page of objects without values", body = doc::StorageObjectInfoPage), (status = 403, description = "`forbidden`: not an admin", body = ErrorBody)))]
pub(crate) async fn list(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    admin: RequireAdmin,
    ip: ClientIp,
    request_id: RequestId,
    Call(call): Call<ListUserObjects>,
) -> CallResult<ListUserObjects> {
    let page = service.list(&state, call.user, &call.collection, &call.page).await?;
    let entry = record("storage_list", &admin, call.user, &ip, &request_id).data(json!({ "collection": call.collection }));
    audit::record(state.db(), state.now(), &entry).await?;
    Ok(Reply::new(page))
}

/// Read one of a user's objects.
#[utoipa::path(get, path = "/v1/admin/users/{user}/storage/{collection}/{key}", tag = "admin", operation_id = "admin_storage_get", security(("bearer" = [])),
    params(("user" = i64, Path, description = "The user id"), ("collection" = String, Path, description = "The collection"), ("key" = String, Path, description = "The key")),
    responses((status = 200, description = "The object", body = doc::StorageObject), (status = 404, description = "`not_found`", body = ErrorBody)))]
pub(crate) async fn get(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    admin: RequireAdmin,
    ip: ClientIp,
    request_id: RequestId,
    Call(call): Call<GetUserObject>,
) -> CallResult<GetUserObject> {
    let object = service.get(&state, call.user, &call.collection, &call.key).await?;
    let entry =
        record("storage_read", &admin, call.user, &ip, &request_id).data(json!({ "collection": call.collection, "key": call.key, "found": object.is_some() }));
    audit::record(state.db(), state.now(), &entry).await?;
    let object = object.ok_or_else(|| AppError::not_found("no such object"))?;
    let tag = etag(object.version);
    let mut reply = Reply::new(object);
    if let Some(tag) = tag {
        reply = reply.with_header(ETAG, tag);
    }
    Ok(reply)
}

/// Write one of a user's objects (as the server: ignores the owner's write lock; `write` sets it).
#[utoipa::path(put, path = "/v1/admin/users/{user}/storage/{collection}/{key}", tag = "admin", operation_id = "admin_storage_put", request_body = doc::AdminPutObject, security(("bearer" = [])),
    params(("user" = i64, Path, description = "The user id"), ("collection" = String, Path, description = "The collection"), ("key" = String, Path, description = "The key")),
    responses(
        (status = 200, description = "Written", body = doc::ObjectAck),
        (status = 404, description = "`not_found`: no such account", body = ErrorBody),
        (status = 409, description = "`version_conflict`", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the value is too large", body = ErrorBody),
    ))]
pub(crate) async fn put(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    admin: RequireAdmin,
    ip: ClientIp,
    request_id: RequestId,
    headers: HeaderMap,
    Call(call): Call<WriteUserObject>,
) -> CallResult<WriteUserObject> {
    let if_version = condition(&headers, call.put.if_version, true)?;
    let entry = record("storage_write", &admin, call.user, &ip, &request_id);
    let request = WriteRequest {
        user: call.user,
        collection: call.collection,
        key: call.key,
        value: call.put.value,
        if_version,
        write: call.put.write,
        visibility: call.put.visibility,
        writer: Writer::Admin(admin.0.user_id),
    };
    let ack = service.write(&state, &HookCtx::new(state.clone(), Some(request_id)), request, Some(entry)).await?;
    let tag = etag(ack.version);
    let mut reply = Reply::new(ack);
    if let Some(tag) = tag {
        reply = reply.with_header(ETAG, tag);
    }
    Ok(reply)
}

/// Delete one of a user's objects (also a server-locked one).
#[utoipa::path(delete, path = "/v1/admin/users/{user}/storage/{collection}/{key}", tag = "admin", operation_id = "admin_storage_delete", security(("bearer" = [])),
    params(
        ("user" = i64, Path, description = "The user id"),
        ("collection" = String, Path, description = "The collection"),
        ("key" = String, Path, description = "The key"),
        ("if_version" = Option<i64>, Query, description = "Only if the stored version is this one"),
    ),
    responses((status = 200, description = "Deleted (or not there)", body = doc::Ack), (status = 409, description = "`version_conflict`", body = ErrorBody)))]
pub(crate) async fn delete(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    admin: RequireAdmin,
    ip: ClientIp,
    request_id: RequestId,
    headers: HeaderMap,
    Call(call): Call<RemoveUserObject>,
) -> CallResult<RemoveUserObject> {
    let if_version = condition(&headers, call.delete.if_version, false)?;
    let entry = record("storage_delete", &admin, call.user, &ip, &request_id);
    let ctx = HookCtx::new(state.clone(), Some(request_id));
    service.remove(&state, &ctx, call.user, &call.collection, &call.key, if_version, Writer::Admin(admin.0.user_id), Some(entry)).await?;
    Ok(Reply::new(Ack::new()))
}
