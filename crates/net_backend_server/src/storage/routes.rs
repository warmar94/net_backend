//! `/v1/storage/*`: the caller's own objects, exactly as the protocol defines them.

use axum::extract::State;
use http::header::{HeaderMap, HeaderValue, ETAG, IF_MATCH, IF_NONE_MATCH};
use net_backend_protocol::storage::{BatchGet, BatchPut, GetObject, ListObjects, ObjectVersion, RemoveObject, WriteObject};
use net_backend_protocol::Ack;

use super::events::Writer;
use super::openapi as doc;
use super::service::{StorageService, WriteRequest};
use crate::auth::AuthContext;
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::{Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;

/// The `ETag` of a version (`"3"`).
pub(crate) fn etag(version: ObjectVersion) -> Option<HeaderValue> {
    HeaderValue::from_str(&version.etag()).ok()
}

fn with_etag<C: net_backend_protocol::HttpCall>(reply: Reply<C>, version: ObjectVersion) -> Reply<C> {
    match etag(version) {
        Some(value) => reply.with_header(ETAG, value),
        None => reply,
    }
}

/// The condition of a write / delete from `If-Match: "N"` / `If-None-Match: *` and the body's
/// `if_version` (400 if they disagree or a header has another form).
pub(crate) fn condition(headers: &HeaderMap, body: Option<ObjectVersion>, allow_none_match: bool) -> Result<Option<ObjectVersion>, AppError> {
    let read = |name| -> Result<Option<String>, AppError> {
        let mut values = headers.get_all(name).iter();
        let value = values.next().map(|v| v.to_str().map(|s| s.trim().to_string()).map_err(|_| AppError::bad_request("a condition header is not text")));
        if values.next().is_some() {
            return Err(AppError::bad_request("give a condition header once"));
        }
        value.transpose()
    };
    let from_header = match (read(IF_MATCH)?, read(IF_NONE_MATCH)?) {
        (Some(_), Some(_)) => return Err(AppError::bad_request("give If-Match or If-None-Match, not both")),
        (Some(tag), None) => {
            let version = tag.strip_prefix('"').and_then(|t| t.strip_suffix('"')).and_then(|n| n.parse::<i64>().ok()).filter(|n| *n >= 1);
            Some(ObjectVersion(version.ok_or_else(|| AppError::bad_request("If-Match must be one version in quotes, e.g. \"3\""))?))
        }
        (None, Some(tag)) if tag == "*" && allow_none_match => Some(ObjectVersion::ABSENT),
        (None, Some(_)) => return Err(AppError::bad_request("If-None-Match must be * (only if the object does not exist)")),
        (None, None) => None,
    };
    match (from_header, body) {
        (Some(header), Some(body)) if header != body => Err(AppError::bad_request("the condition header and if_version disagree")),
        (Some(header), _) => Ok(Some(header)),
        (None, body) => Ok(body),
    }
}

/// List the caller's objects in a collection (no values), ordered by key.
#[utoipa::path(get, path = "/v1/storage/{collection}", tag = "storage", operation_id = "storage_list", security(("bearer" = [])),
    params(
        ("collection" = String, Path, description = "1-128 bytes of [A-Za-z0-9_.-], starting with a letter or digit"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses(
        (status = 200, description = "A page of objects without values", body = doc::StorageObjectInfoPage),
        (status = 400, description = "`bad_request`: an invalid name or cursor", body = ErrorBody),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
    ))]
pub(crate) async fn list(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    who: AuthContext,
    Call(call): Call<ListObjects>,
) -> CallResult<ListObjects> {
    service.list(&state, who.user_id, &call.collection, &call.page).await.map(Reply::new)
}

/// Read one of the caller's objects (`ETag`: its version).
#[utoipa::path(get, path = "/v1/storage/{collection}/{key}", tag = "storage", operation_id = "storage_get", security(("bearer" = [])),
    params(("collection" = String, Path, description = "The collection"), ("key" = String, Path, description = "The key")),
    responses(
        (status = 200, description = "The object", body = doc::StorageObject, headers(("ETag" = String, description = "The version in quotes"))),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn get(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    who: AuthContext,
    Call(call): Call<GetObject>,
) -> CallResult<GetObject> {
    let object = service.get(&state, who.user_id, &call.collection, &call.key).await?.ok_or_else(|| AppError::not_found("no such object"))?;
    let version = object.version;
    Ok(with_etag(Reply::new(object), version))
}

/// Write one of the caller's objects. Without a condition the last write wins; `if_version` (or
/// `If-Match: "N"`, `If-None-Match: *`) makes it conditional.
#[utoipa::path(put, path = "/v1/storage/{collection}/{key}", tag = "storage", operation_id = "storage_put", request_body = doc::PutObject, security(("bearer" = [])),
    params(
        ("collection" = String, Path, description = "The collection"),
        ("key" = String, Path, description = "The key"),
        ("If-Match" = Option<String>, Header, description = "\"N\": only if the stored version is N"),
        ("If-None-Match" = Option<String>, Header, description = "*: only if the object does not exist"),
    ),
    responses(
        (status = 200, description = "Written", body = doc::ObjectAck, headers(("ETag" = String, description = "The new version in quotes"))),
        (status = 400, description = "`bad_request`: an invalid name, condition header, or one that disagrees with if_version", body = ErrorBody),
        (status = 403, description = "`forbidden`: the object (or its collection) is written by the server only; `quota_exceeded`: too many objects or bytes; or refused by a hook", body = ErrorBody),
        (status = 409, description = "`version_conflict` (details: VersionConflict)", body = ErrorBody),
        (status = 413, description = "`payload_too_large`", body = ErrorBody),
        (status = 422, description = "`validation_failed`: the value is too large", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many writes", body = ErrorBody),
    ))]
pub(crate) async fn put(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    who: AuthContext,
    request_id: RequestId,
    headers: HeaderMap,
    Call(call): Call<WriteObject>,
) -> CallResult<WriteObject> {
    service.check_rate(who.user_id)?;
    call.put.validate(service.config().max_object_bytes)?;
    let if_version = condition(&headers, call.put.if_version, true)?;
    let request =
        WriteRequest { user: who.user_id, collection: call.collection, key: call.key, value: call.put.value, if_version, write: None, writer: Writer::Owner };
    let ack = service.write(&state, &HookCtx::new(state.clone(), Some(request_id)), request, None).await?;
    let version = ack.version;
    Ok(with_etag(Reply::new(ack), version))
}

/// Delete one of the caller's objects (absent: answered all the same, unless a version is named).
#[utoipa::path(delete, path = "/v1/storage/{collection}/{key}", tag = "storage", operation_id = "storage_delete", security(("bearer" = [])),
    params(
        ("collection" = String, Path, description = "The collection"),
        ("key" = String, Path, description = "The key"),
        ("if_version" = Option<i64>, Query, description = "Only if the stored version is this one"),
        ("If-Match" = Option<String>, Header, description = "\"N\": only if the stored version is N"),
    ),
    responses(
        (status = 200, description = "Deleted (or not there)", body = doc::Ack),
        (status = 403, description = "`forbidden`: the object (or its collection) is written by the server only; or refused by a hook", body = ErrorBody),
        (status = 409, description = "`version_conflict`", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many writes", body = ErrorBody),
    ))]
pub(crate) async fn delete(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    who: AuthContext,
    request_id: RequestId,
    headers: HeaderMap,
    Call(call): Call<RemoveObject>,
) -> CallResult<RemoveObject> {
    service.check_rate(who.user_id)?;
    let if_version = condition(&headers, call.delete.if_version, false)?;
    let ctx = HookCtx::new(state.clone(), Some(request_id));
    service.remove(&state, &ctx, who.user_id, &call.collection, &call.key, if_version, Writer::Owner, None).await?;
    Ok(Reply::new(Ack::new()))
}

/// Read several of the caller's objects (missing ones are absent from the answer).
#[utoipa::path(post, path = "/v1/storage/_batch/get", tag = "storage", operation_id = "storage_batch_get", request_body = doc::BatchGet, security(("bearer" = [])),
    responses(
        (status = 200, description = "The objects found", body = doc::BatchObjects),
        (status = 413, description = "`payload_too_large`: more than 4 MiB of values; read fewer per batch", body = ErrorBody),
        (status = 422, description = "`validation_failed`: 1-16 distinct objects with valid names", body = ErrorBody),
    ))]
pub(crate) async fn batch_get(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    who: AuthContext,
    Call(batch): Call<BatchGet>,
) -> CallResult<BatchGet> {
    service.get_many(&state, who.user_id, &batch).await.map(Reply::new)
}

/// Write several of the caller's objects in one transaction (all or nothing).
#[utoipa::path(post, path = "/v1/storage/_batch/put", tag = "storage", operation_id = "storage_batch_put", request_body = doc::BatchPut, security(("bearer" = [])),
    responses(
        (status = 200, description = "Every write, in request order", body = doc::BatchAcks),
        (status = 403, description = "`forbidden` (server-locked object or collection), `quota_exceeded`, or refused by a hook; details: the index of the failing item", body = ErrorBody),
        (status = 409, description = "`version_conflict` (details: index + current_version of the first failing item)", body = ErrorBody),
        (status = 422, description = "`validation_failed`: 1-16 distinct objects, sizes", body = ErrorBody),
        (status = 429, description = "`rate_limited` (details: retry_after_ms): too many writes (a batch counts once)", body = ErrorBody),
    ))]
pub(crate) async fn batch_put(
    State(state): State<AppState>,
    Ext(service): Ext<StorageService>,
    who: AuthContext,
    request_id: RequestId,
    Call(batch): Call<BatchPut>,
) -> CallResult<BatchPut> {
    service.check_rate(who.user_id)?;
    let ctx = HookCtx::new(state.clone(), Some(request_id));
    service.write_batch(&state, &ctx, who.user_id, batch).await.map(Reply::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            if let Ok(value) = HeaderValue::from_str(value) {
                map.append(*name, value);
            }
        }
        map
    }

    #[test]
    fn conditions() {
        let v = |n| Some(ObjectVersion(n));
        assert_eq!(condition(&headers(&[]), None, true).ok(), Some(None));
        assert_eq!(condition(&headers(&[]), v(2), true).ok(), Some(v(2)));
        assert_eq!(condition(&headers(&[("if-match", "\"3\"")]), None, true).ok(), Some(v(3)));
        assert_eq!(condition(&headers(&[("if-match", "\"3\"")]), v(3), true).ok(), Some(v(3)));
        assert!(condition(&headers(&[("if-match", "\"3\"")]), v(2), true).is_err());
        assert_eq!(condition(&headers(&[("if-none-match", "*")]), None, true).ok(), Some(v(0)));
        assert!(condition(&headers(&[("if-none-match", "*")]), None, false).is_err());
        for bad in ["*", "3", "W/\"3\"", "\"0\"", "\"-1\"", "\"x\""] {
            assert!(condition(&headers(&[("if-match", bad)]), None, true).is_err(), "{bad}");
        }
        assert!(condition(&headers(&[("if-match", "\"1\""), ("if-match", "\"1\"")]), None, true).is_err());
        assert!(condition(&headers(&[("if-match", "\"1\""), ("if-none-match", "*")]), None, true).is_err());
        assert_eq!(etag(ObjectVersion(7)).as_ref().and_then(|v| v.to_str().ok()), Some("\"7\""));
    }
}
