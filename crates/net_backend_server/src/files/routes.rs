//! `/v1/files/*`: uploads (multipart), downloads, listings, settings and deletes.

use std::io;

use axum::body::Body;
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::stream::StreamExt as _;
use http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use http::{HeaderMap, HeaderValue, StatusCode};
use net_backend_protocol::files::{DeleteFile, EditFile, FileInfo, FileMeta, GetFile, GetFileUsage, ListFiles, UPLOAD_FILE_PART, UPLOAD_META_PART};
use net_backend_protocol::{Ack, FileId};

use super::openapi as doc;
use super::service::FileService;
use crate::auth::AuthContext;
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::http::call::{Call, CallResult, Reply};
use crate::http::{Ext, RequestId};
use crate::openapi::ErrorBody;
use crate::state::AppState;

/// The largest meta part.
const MAX_META_BYTES: usize = 64 * 1024;

fn ctx(state: &AppState, request_id: RequestId) -> HookCtx {
    HookCtx::new(state.clone(), Some(request_id))
}

fn multipart_error(error: axum::extract::multipart::MultipartError) -> AppError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        AppError::payload_too_large("the upload is larger than the server accepts")
    } else {
        AppError::bad_request("the multipart body is malformed")
    }
}

/// Upload a file: a `multipart/form-data` body with an optional `meta` part (JSON settings, first)
/// and the `file` part (the bytes; its `filename` and `Content-Type` are the defaults).
#[utoipa::path(post, path = "/v1/files", tag = "files", operation_id = "files_upload", security(("bearer" = [])),
    request_body(content = doc::UploadForm, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Stored", body = doc::FileInfo),
        (status = 400, description = "`bad_request`: not multipart, an unknown or repeated part, a meta part after the file, a body that ended early", body = ErrorBody),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
        (status = 403, description = "`quota_exceeded`: too many files or bytes; or refused by a hook", body = ErrorBody),
        (status = 413, description = "`payload_too_large`: larger than `max_file_bytes`", body = ErrorBody),
        (status = 422, description = "`validation_failed`: no file part, a bad name / content type / metadata / visibility / share list, a SHA-256 that does not match", body = ErrorBody),
        (status = 429, description = "`rate_limited`", body = ErrorBody),
    ))]
pub(crate) async fn upload(
    State(state): State<AppState>,
    Ext(service): Ext<FileService>,
    who: AuthContext,
    request_id: RequestId,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Json<FileInfo>, AppError> {
    service.check_rate(who.user_id)?;
    let mut multipart = multipart.map_err(|_| AppError::bad_request("the body must be multipart/form-data"))?;
    let hook_ctx = ctx(&state, request_id);
    let mut meta: Option<FileMeta> = None;
    let mut stored: Option<FileInfo> = None;
    let result = read_parts(&state, &service, &who, &hook_ctx, &mut multipart, &mut meta, &mut stored).await;
    match (result, stored) {
        (Ok(()), Some(info)) => Ok(Json(info)),
        (Ok(()), None) => {
            let mut details = net_backend_protocol::ValidationDetails::new();
            details.add("file", "the upload has no file part");
            Err(AppError::validation(details))
        }
        (Err(error), stored) => {
            // The file part was stored, but the rest of the body is refused: the upload as a whole
            // is, so the stored file goes again.
            if let Some(info) = stored {
                if let Err(cleanup) = service.delete(&state, &hook_ctx, who.user_id, info.id).await {
                    tracing::warn!(%cleanup, "files: could not remove a file whose upload was refused");
                }
            }
            Err(error)
        }
    }
}

/// Read the upload's parts: `meta` (optional, first), then `file`; nothing after it.
async fn read_parts(
    state: &AppState,
    service: &FileService,
    who: &AuthContext,
    hook_ctx: &HookCtx,
    multipart: &mut Multipart,
    meta: &mut Option<FileMeta>,
    stored: &mut Option<FileInfo>,
) -> Result<(), AppError> {
    while let Some(mut field) = multipart.next_field().await.map_err(multipart_error)? {
        match field.name() {
            Some(UPLOAD_META_PART) if meta.is_none() && stored.is_none() => {
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                    if bytes.len() + chunk.len() > MAX_META_BYTES {
                        return Err(AppError::payload_too_large("the meta part is larger than 64 KiB"));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let parsed: FileMeta =
                    serde_json::from_slice(&bytes).map_err(|_| AppError::bad_request("the meta part is not a JSON object of file settings"))?;
                *meta = Some(parsed);
            }
            Some(UPLOAD_FILE_PART) if stored.is_none() => {
                let name = field.file_name().map(str::to_string);
                let content_type = field.content_type().map(str::to_string);
                let data = futures_util::stream::unfold(&mut field, |field| async move {
                    match field.chunk().await {
                        Ok(Some(chunk)) => Some((Ok(chunk), field)),
                        Ok(None) => None,
                        Err(error) => Some((Err(io::Error::other(error)), field)),
                    }
                })
                .boxed();
                let info =
                    service.upload(state, hook_ctx, who.user_id, meta.take().unwrap_or_default(), name.as_deref(), content_type.as_deref(), data).await?;
                *stored = Some(info);
            }
            Some(UPLOAD_META_PART) => return Err(AppError::bad_request("one meta part, before the file part")),
            Some(UPLOAD_FILE_PART) => return Err(AppError::bad_request("one file part per upload")),
            _ => return Err(AppError::bad_request("the parts are `meta` (optional, first) and `file`")),
        }
    }
    Ok(())
}

/// A `Content-Disposition` for a download: an ASCII fallback name and the UTF-8 name (RFC 6266).
fn disposition(name: &str) -> HeaderValue {
    let ascii: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') { c } else { '_' }).collect();
    let mut encoded = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    HeaderValue::from_str(&format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// Download a file's bytes (the owner, or a player who may read it).
#[utoipa::path(get, path = "/v1/files/{file}/content", tag = "files", operation_id = "files_content", security(("bearer" = [])),
    params(("file" = i64, Path, description = "The file"), ("If-None-Match" = Option<String>, Header, description = "The ETag from before: 304 when unchanged")),
    responses(
        (status = 200, description = "The bytes (as an attachment; `Content-Type` as stored, `ETag` = the SHA-256 in quotes)", content_type = "application/octet-stream"),
        (status = 304, description = "Not modified (the `If-None-Match` ETag matches)"),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
        (status = 404, description = "`not_found`: no such file, or the caller may not read it", body = ErrorBody),
    ))]
pub(crate) async fn content(
    State(state): State<AppState>,
    Ext(service): Ext<FileService>,
    who: AuthContext,
    Path(file): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (info, bytes) = service.open(&state, who.user_id, FileId(file)).await?;
    let etag = format!("\"{}\"", info.sha256);
    let mut response = if headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == etag)) {
        drop(bytes);
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut response = Response::new(Body::from_stream(bytes));
        let content_type = HeaderValue::from_str(&info.content_type).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, content_type);
        headers.insert(CONTENT_LENGTH, HeaderValue::from(info.size));
        headers.insert(CONTENT_DISPOSITION, disposition(&info.name));
        response
    };
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(ETAG, value);
    }
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; sandbox"));
    Ok(response)
}

/// The caller's files, or (with `owner`) another player's files the caller may read; newest first.
#[utoipa::path(get, path = "/v1/files", tag = "files", operation_id = "files_list", security(("bearer" = [])),
    params(
        ("owner" = Option<i64>, Query, description = "Another player (default: the caller)"),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor"),
        ("limit" = Option<u32>, Query, description = "1-100, default 50"),
    ),
    responses(
        (status = 200, description = "A page of files", body = doc::FilePage),
        (status = 400, description = "`bad_request`: an invalid cursor", body = ErrorBody),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
    ))]
pub(crate) async fn list(
    State(state): State<AppState>,
    Ext(service): Ext<FileService>,
    who: AuthContext,
    Call(call): Call<ListFiles>,
) -> CallResult<ListFiles> {
    service.list(&state, who.user_id, &call.query).await.map(Reply::new)
}

/// What the caller's files use, and the limits.
#[utoipa::path(get, path = "/v1/files/usage", tag = "files", operation_id = "files_usage", security(("bearer" = [])),
    responses(
        (status = 200, description = "The usage", body = doc::FileUsage),
        (status = 401, description = "`unauthorized` / `token_expired`", body = ErrorBody),
    ))]
pub(crate) async fn usage(
    State(state): State<AppState>,
    Ext(service): Ext<FileService>,
    who: AuthContext,
    Call(_call): Call<GetFileUsage>,
) -> CallResult<GetFileUsage> {
    service.usage(&state, who.user_id).await.map(Reply::new)
}

/// One file's settings (the owner also sees `shared_with`).
#[utoipa::path(get, path = "/v1/files/{file}", tag = "files", operation_id = "files_get", security(("bearer" = [])),
    params(("file" = i64, Path, description = "The file")),
    responses(
        (status = 200, description = "The file", body = doc::FileInfo),
        (status = 404, description = "`not_found`: no such file, or the caller may not read it", body = ErrorBody),
    ))]
pub(crate) async fn get(State(state): State<AppState>, Ext(service): Ext<FileService>, who: AuthContext, Call(call): Call<GetFile>) -> CallResult<GetFile> {
    service.read_as(&state, who.user_id, call.file).await?.map(Reply::new).ok_or_else(|| AppError::not_found("no such file"))
}

/// Change a file's settings (its owner): name, visibility, the share list, metadata.
#[utoipa::path(patch, path = "/v1/files/{file}", tag = "files", operation_id = "files_update", request_body = doc::UpdateFile, security(("bearer" = [])),
    params(("file" = i64, Path, description = "The file")),
    responses(
        (status = 200, description = "Changed", body = doc::FileInfo),
        (status = 403, description = "`forbidden`: not the owner, or refused by a hook", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
        (status = 422, description = "`validation_failed`: a bad name, visibility, share list or metadata", body = ErrorBody),
    ))]
pub(crate) async fn edit(
    State(state): State<AppState>,
    Ext(service): Ext<FileService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<EditFile>,
) -> CallResult<EditFile> {
    service.update(&state, &ctx(&state, request_id), who.user_id, call.file, call.update).await.map(Reply::new)
}

/// Delete a file (its owner).
#[utoipa::path(delete, path = "/v1/files/{file}", tag = "files", operation_id = "files_delete", security(("bearer" = [])),
    params(("file" = i64, Path, description = "The file")),
    responses(
        (status = 200, description = "Deleted", body = doc::Ack),
        (status = 403, description = "`forbidden`: not the owner", body = ErrorBody),
        (status = 404, description = "`not_found`", body = ErrorBody),
    ))]
pub(crate) async fn delete(
    State(state): State<AppState>,
    Ext(service): Ext<FileService>,
    who: AuthContext,
    request_id: RequestId,
    Call(call): Call<DeleteFile>,
) -> CallResult<DeleteFile> {
    service.delete(&state, &ctx(&state, request_id), who.user_id, call.file).await?;
    Ok(Reply::new(Ack::new()))
}
