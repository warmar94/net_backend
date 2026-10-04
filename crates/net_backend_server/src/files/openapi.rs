//! OpenAPI schemas of the protocol's files types (mirror structs: the protocol crate has no OpenAPI
//! dependency). A test serializes the real types and compares the field names with these schemas.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::ToSchema;

/// A stored file (without its bytes).
#[derive(Serialize, ToSchema)]
pub(crate) struct FileInfo {
    /// The file.
    id: i64,
    /// Its owner.
    owner: i64,
    /// Its name.
    name: String,
    /// Its content type.
    content_type: String,
    /// Its size in bytes.
    size: u64,
    /// The SHA-256 of its bytes (lower-case hex; also the download's ETag).
    sha256: String,
    /// `private`, `public`, `friends` or `shared`.
    visibility: String,
    /// The accounts it is shared with (to the owner, in the file's own answers).
    shared_with: Vec<i64>,
    /// The game's own data.
    #[schema(value_type = Option<Object>)]
    metadata: Option<serde_json::Value>,
    /// When it was uploaded (unix ms).
    created_at: i64,
    /// When it or its settings last changed (unix ms).
    updated_at: i64,
}

/// A page of files, newest first.
#[derive(Serialize, ToSchema)]
pub(crate) struct FilePage {
    /// The files.
    items: Vec<FileInfo>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// The `meta` part of an upload (every field optional).
#[derive(Serialize, ToSchema)]
pub(crate) struct FileMeta {
    /// The name (default: the file part's filename).
    name: Option<String>,
    /// The content type (default: the file part's Content-Type).
    content_type: Option<String>,
    /// `private` (default), `public`, `friends` or `shared`.
    visibility: Option<String>,
    /// The accounts it is shared with (for `shared`).
    shared_with: Option<Vec<i64>>,
    /// The game's own data.
    #[schema(value_type = Option<Object>)]
    metadata: Option<serde_json::Value>,
    /// The SHA-256 the bytes must have (lower-case hex).
    sha256: Option<String>,
}

/// The upload's form: `meta` (JSON, optional, first) and `file` (the bytes).
#[derive(ToSchema)]
pub(crate) struct UploadForm {
    /// The settings (`application/json`).
    meta: Option<FileMeta>,
    /// The bytes.
    #[schema(value_type = String, format = Binary)]
    file: Vec<u8>,
}

/// `PATCH /v1/files/{file}` body (absent fields stay; `"metadata":null` clears).
#[derive(Serialize, ToSchema)]
pub(crate) struct UpdateFile {
    /// A new name.
    name: Option<String>,
    /// A new visibility.
    visibility: Option<String>,
    /// The accounts it is shared with (replaces the list).
    shared_with: Option<Vec<i64>>,
    /// New metadata (`null` clears it).
    #[schema(value_type = Option<Object>)]
    metadata: Option<serde_json::Value>,
}

/// What the caller's files use, and the limits.
#[derive(Serialize, ToSchema)]
pub(crate) struct FileUsage {
    /// Files.
    files: u64,
    /// Bytes.
    bytes: u64,
    /// The most files per player.
    max_files: u64,
    /// The most bytes per player.
    max_bytes: u64,
    /// The largest file.
    max_file_bytes: u64,
}

/// An empty answer.
#[derive(Serialize, ToSchema)]
pub(crate) struct Ack {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::files as p;
    use net_backend_protocol::{FileId, UnixMillis, UserId};
    use serde_json::{json, Value};
    use utoipa::openapi::schema::Schema;
    use utoipa::openapi::RefOr;
    use utoipa::PartialSchema;

    use super::*;

    fn properties<T: PartialSchema>() -> BTreeSet<String> {
        match T::schema() {
            RefOr::T(Schema::Object(object)) => object.properties.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    fn keys<T: serde::Serialize>(value: T) -> BTreeSet<String> {
        match serde_json::to_value(value).unwrap_or(Value::Null) {
            Value::Object(map) => map.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    #[test]
    fn mirrors_match_the_protocol() {
        let mut info = p::FileInfo::new(FileId(1), UserId(2), "a", "x/y", 3, "s", UnixMillis(4));
        info.shared_with = vec![UserId(5)];
        info.metadata = Some(json!({}));
        assert_eq!(properties::<FileInfo>(), keys(&info));
        let meta = p::FileMeta::new().with_name("a").with_content_type("x/y").shared_with(vec![UserId(1)]).with_metadata(json!(1)).with_sha256("s");
        assert_eq!(properties::<FileMeta>(), keys(meta));
        let update = p::UpdateFile::new().with_name("a").shared_with(vec![UserId(1)]).with_metadata(json!(1));
        assert_eq!(properties::<UpdateFile>(), keys(update));
        assert_eq!(properties::<FileUsage>(), keys(p::FileUsage::new(1, 2, 3, 4, 5)));
        assert_eq!(properties::<UploadForm>(), [p::UPLOAD_META_PART.to_string(), p::UPLOAD_FILE_PART.to_string()].into_iter().collect());
    }
}
