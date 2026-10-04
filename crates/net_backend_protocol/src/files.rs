//! Files: binary uploads (screenshots, replays, mods, levels) owned by a player, with a content type,
//! a SHA-256 checksum, per-player quotas and a visibility (private, public, the owner's friends, or
//! shared with chosen accounts).
//!
//! | Route | Request → answer |
//! |---|---|
//! | `POST /v1/files` | a `multipart/form-data` body: an optional [`UPLOAD_META_PART`] (JSON [`FileMeta`]) and the [`UPLOAD_FILE_PART`] (the bytes) → [`FileInfo`] |
//! | `GET /v1/files` | [`ListFiles`] ([`FileQuery`]: the caller's files, or with `owner` the files of another player the caller may read) → [`Page`]`<`[`FileInfo`]`>` |
//! | `GET /v1/files/usage` | [`GetFileUsage`] → [`FileUsage`] |
//! | `GET /v1/files/{file}` | [`GetFile`] → [`FileInfo`] |
//! | `PATCH /v1/files/{file}` | [`EditFile`] ([`UpdateFile`]) → [`FileInfo`] (the owner) |
//! | `DELETE /v1/files/{file}` | [`DeleteFile`] → [`Ack`] (the owner) |
//! | `GET /v1/files/{file}/content` | → the bytes (`Content-Type`, `Content-Length`, `ETag` = the SHA-256) |
//!
//! The upload and the download are not JSON calls: they are in
//! [`routes::BINARY`](crate::routes::BINARY), not in [`routes::ALL`](crate::routes::ALL), and
//! clients send them themselves (`net_backend_client` has `upload_file` / `download_file`).
//!
//! **Who reads a file:** its owner always; anyone logged in when it is [`FileVisibility::Public`];
//! the owner's friends when it is [`FileVisibility::Friends`] (servers with the friends module);
//! the accounts in `shared_with` when it is [`FileVisibility::Shared`]. Everyone else gets 404 (a
//! private file's existence is not revealed). Only the owner changes or deletes a file.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::Ack;
use crate::error::{ApiError, ValidationDetails};
use crate::ids::{FileId, UserId};
use crate::page::{Cursor, Page};
use crate::time::UnixMillis;

/// The name of the upload's JSON part ([`FileMeta`], optional, before the file part).
pub const UPLOAD_META_PART: &str = "meta";
/// The name of the upload's file part (the bytes; its `filename` and `Content-Type` are used when
/// the meta part names none).
pub const UPLOAD_FILE_PART: &str = "file";
/// The longest file name (bytes).
pub const MAX_NAME_BYTES: usize = 255;
/// The longest content type (bytes).
pub const MAX_CONTENT_TYPE_BYTES: usize = 127;
/// The most accounts a file is shared with in one request.
pub const MAX_SHARED_WITH: usize = 100;
/// The default largest metadata (bytes of its JSON); a server may set less.
pub const DEFAULT_MAX_METADATA_BYTES: usize = 4096;

/// Who may read a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FileVisibility {
    /// The owner only (the default).
    #[default]
    Private,
    /// Every logged-in player.
    Public,
    /// The owner's friends (on servers with the friends module).
    Friends,
    /// The accounts in `shared_with`.
    Shared,
    /// A visibility this crate does not know (a newer server).
    #[serde(other)]
    Unknown,
}

impl FileVisibility {
    /// The name on the wire (`"private"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            FileVisibility::Private => "private",
            FileVisibility::Public => "public",
            FileVisibility::Friends => "friends",
            FileVisibility::Shared => "shared",
            FileVisibility::Unknown => "unknown",
        }
    }

    /// The visibility named `text`, if it is one this crate knows.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "private" => Some(FileVisibility::Private),
            "public" => Some(FileVisibility::Public),
            "friends" => Some(FileVisibility::Friends),
            "shared" => Some(FileVisibility::Shared),
            _ => None,
        }
    }
}

/// A stored file (without its bytes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FileInfo {
    /// The file.
    pub id: FileId,
    /// Its owner.
    pub owner: UserId,
    /// Its name (as uploaded or changed; not unique).
    pub name: String,
    /// Its content type (`image/png`; `application/octet-stream` when none was given).
    pub content_type: String,
    /// Its size in bytes.
    pub size: u64,
    /// The SHA-256 of its bytes, lower-case hex (also the download's `ETag`).
    pub sha256: String,
    /// Who may read it.
    #[serde(default)]
    pub visibility: FileVisibility,
    /// The accounts it is shared with (shown to the owner only; empty otherwise).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<UserId>,
    /// The game's own data about it (e.g. a level's title), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// When it was uploaded.
    pub created_at: UnixMillis,
    /// When it or its settings last changed.
    pub updated_at: UnixMillis,
}

impl FileInfo {
    /// A file (servers build these).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: FileId,
        owner: UserId,
        name: impl Into<String>,
        content_type: impl Into<String>,
        size: u64,
        sha256: impl Into<String>,
        at: UnixMillis,
    ) -> Self {
        Self {
            id,
            owner,
            name: name.into(),
            content_type: content_type.into(),
            size,
            sha256: sha256.into(),
            visibility: FileVisibility::Private,
            shared_with: Vec::new(),
            metadata: None,
            created_at: at,
            updated_at: at,
        }
    }
}

/// What a name must not be: `None` if fine. 1 to [`MAX_NAME_BYTES`] bytes after trimming, no
/// control, invisible or direction-changing characters, no `/` or `\`.
pub fn name_problem(name: &str) -> Option<&'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Some("is empty");
    }
    if trimmed.len() > MAX_NAME_BYTES {
        return Some("is longer than 255 bytes");
    }
    if trimmed.contains(['/', '\\']) {
        return Some("contains a slash");
    }
    crate::text::name_problem(trimmed)
}

/// Whether `content_type` is a plain `type/subtype` (RFC 6838 characters, at most
/// [`MAX_CONTENT_TYPE_BYTES`] bytes, no parameters).
pub fn is_valid_content_type(content_type: &str) -> bool {
    let token = |part: &str| {
        !part.is_empty()
            && part.len() <= 64
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'!' | b'#' | b'$' | b'&' | b'-' | b'^' | b'_' | b'.' | b'+'))
    };
    content_type.len() <= MAX_CONTENT_TYPE_BYTES && content_type.split_once('/').is_some_and(|(kind, sub)| token(kind) && token(sub))
}

/// Whether `text` is a lower-case hex SHA-256 (64 characters).
pub fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn check_shares(details: &mut ValidationDetails, visibility: Option<FileVisibility>, shared_with: Option<&Vec<UserId>>) {
    if let Some(list) = shared_with {
        if list.len() > MAX_SHARED_WITH {
            details.add("shared_with", format!("names more than {MAX_SHARED_WITH} accounts"));
        }
    }
    if visibility == Some(FileVisibility::Unknown) {
        details.add("visibility", "is not private, public, friends or shared");
    }
}

/// The upload's JSON part: settings of the new file (every field optional).
///
/// JSON: `{"name":"level-3.map","visibility":"shared","shared_with":[42],"metadata":{"title":"Caves"},"sha256":"…"}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FileMeta {
    /// The name (default: the file part's `filename`, else `file`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The content type (default: the file part's `Content-Type`, else
    /// `application/octet-stream`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Who may read it (default private).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<FileVisibility>,
    /// The accounts it is shared with (for `shared`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_with: Option<Vec<UserId>>,
    /// The game's own data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// The SHA-256 the bytes must have (lower-case hex): the server refuses the upload (422) when
    /// what arrived differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl FileMeta {
    /// No settings (add fields).
    pub fn new() -> Self {
        Self::default()
    }

    /// The same settings with a name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same settings with a content type.
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// The same settings with a visibility.
    pub fn with_visibility(mut self, visibility: FileVisibility) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Shared with these accounts (sets the visibility to `shared`).
    pub fn shared_with(mut self, users: Vec<UserId>) -> Self {
        self.visibility = Some(FileVisibility::Shared);
        self.shared_with = Some(users);
        self
    }

    /// The same settings with the game's data.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// The same settings with the expected SHA-256 (lower-case hex).
    pub fn with_sha256(mut self, sha256: impl Into<String>) -> Self {
        self.sha256 = Some(sha256.into());
        self
    }

    /// The shape rules (a server checks them again, with its own metadata limit).
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let Some(problem) = self.name.as_deref().and_then(name_problem) {
            details.add("name", problem);
        }
        if self.content_type.as_deref().is_some_and(|c| !is_valid_content_type(c)) {
            details.add("content_type", "is not a plain type/subtype");
        }
        if self.sha256.as_deref().is_some_and(|s| !is_sha256_hex(s)) {
            details.add("sha256", "is not 64 lower-case hex characters");
        }
        check_shares(&mut details, self.visibility, self.shared_with.as_ref());
        details.into_result()
    }
}

/// A change of a file's settings, the body of `PATCH /v1/files/{file}` (absent fields stay).
///
/// JSON: `{"visibility":"public"}`, `{"metadata":null}` (clears the metadata).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UpdateFile {
    /// A new name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A new visibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<FileVisibility>,
    /// The accounts it is shared with (replaces the list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_with: Option<Vec<UserId>>,
    /// New metadata (a present `null` clears it).
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// A field that is present decodes as `Some`, also when it is `null`.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

impl UpdateFile {
    /// A change of nothing (add fields).
    pub fn new() -> Self {
        Self::default()
    }

    /// The same change with a new name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same change with a new visibility.
    pub fn with_visibility(mut self, visibility: FileVisibility) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Shared with exactly these accounts (sets the visibility to `shared`).
    pub fn shared_with(mut self, users: Vec<UserId>) -> Self {
        self.visibility = Some(FileVisibility::Shared);
        self.shared_with = Some(users);
        self
    }

    /// The same change with new metadata (`Value::Null` clears it).
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// The shape rules.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let Some(problem) = self.name.as_deref().and_then(name_problem) {
            details.add("name", problem);
        }
        check_shares(&mut details, self.visibility, self.shared_with.as_ref());
        details.into_result()
    }
}

/// Which files: the caller's own (no `owner`, or the caller's id), or another player's that the
/// caller may read; newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FileQuery {
    /// Whose files (default: the caller's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<UserId>,
    /// Where to continue (`next_cursor` of the previous page).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// At most this many (default 50, at most 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl FileQuery {
    /// The caller's files.
    pub fn mine() -> Self {
        Self::default()
    }

    /// The files of `owner` the caller may read.
    pub fn of(owner: UserId) -> Self {
        Self { owner: Some(owner), ..Self::default() }
    }

    /// The page after `cursor`.
    pub fn after(mut self, cursor: Cursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// The same query with this limit.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// What the caller's files use, and the server's limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FileUsage {
    /// How many files the caller has.
    pub files: u64,
    /// Their bytes.
    pub bytes: u64,
    /// The most files per player.
    pub max_files: u64,
    /// The most bytes per player.
    pub max_bytes: u64,
    /// The largest file.
    pub max_file_bytes: u64,
}

impl FileUsage {
    /// A usage (servers build these).
    pub fn new(files: u64, bytes: u64, max_files: u64, max_bytes: u64, max_file_bytes: u64) -> Self {
        Self { files, bytes, max_files, max_bytes, max_file_bytes }
    }
}

// ---- typed HTTP calls (see `http_call`) ---------------------------------------------------------

mod calls {
    use super::*;

    use crate::http_call::{HttpCall, NoPayload, PathParams, PayloadKind, NO_PAYLOAD};
    use crate::routes::{self, HttpMethod, Route};

    /// The caller's files, or another player's readable ones: `GET /v1/files?owner=…` →
    /// [`Page`]`<`[`FileInfo`]`>` (newest first).
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListFiles {
        /// Whose files, and the page.
        pub query: FileQuery,
    }

    impl ListFiles {
        /// The caller's files, first page.
        pub fn new() -> Self {
            Self::default()
        }

        /// The same call with this query.
        pub fn with_query(mut self, query: FileQuery) -> Self {
            self.query = query;
            self
        }
    }

    impl HttpCall for ListFiles {
        type Payload = FileQuery;
        type Response = Page<FileInfo>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::files::LIST, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &FileQuery {
            &self.query
        }

        fn from_parts(_params: &PathParams, query: FileQuery) -> Result<Self, ApiError> {
            Ok(Self::new().with_query(query))
        }
    }

    /// What the caller's files use: `GET /v1/files/usage` → [`FileUsage`].
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct GetFileUsage {}

    impl GetFileUsage {
        /// The call.
        pub fn new() -> Self {
            Self {}
        }
    }

    impl HttpCall for GetFileUsage {
        type Payload = NoPayload;
        type Response = FileUsage;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::files::USAGE, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn from_parts(_params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            Ok(Self::new())
        }
    }

    /// A call naming one file in the path, without a payload.
    macro_rules! file_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {
                /// The file.
                pub file: FileId,
            }

            impl $name {
                /// The call for `file`.
                pub fn new(file: FileId) -> Self {
                    Self { file }
                }
            }

            impl HttpCall for $name {
                type Payload = NoPayload;
                type Response = $response;
                const ROUTE: Route = Route::new(HttpMethod::$method, routes::files::ONE, true);
                const PAYLOAD: PayloadKind = PayloadKind::Empty;

                fn payload(&self) -> &NoPayload {
                    &NO_PAYLOAD
                }

                fn path_params(&self) -> PathParams {
                    PathParams::new().with("file", self.file)
                }

                fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
                    Ok(Self::new(params.id("file")?))
                }
            }
        };
    }

    file_call!(
        /// One file's settings: `GET /v1/files/{file}` → [`FileInfo`] (the owner, or a player who may
        /// read it; 404 otherwise).
        GetFile,
        Get,
        FileInfo
    );
    file_call!(
        /// Delete a file (its owner): `DELETE /v1/files/{file}` → [`Ack`].
        DeleteFile,
        Delete,
        Ack
    );

    /// Change a file's settings (its owner): `PATCH /v1/files/{file}` with an [`UpdateFile`] →
    /// [`FileInfo`].
    #[derive(Clone, Debug, PartialEq)]
    #[non_exhaustive]
    pub struct EditFile {
        /// The file.
        pub file: FileId,
        /// The change.
        pub update: UpdateFile,
    }

    impl EditFile {
        /// Change `file`.
        pub fn new(file: FileId, update: UpdateFile) -> Self {
            Self { file, update }
        }
    }

    impl HttpCall for EditFile {
        type Payload = UpdateFile;
        type Response = FileInfo;
        const ROUTE: Route = Route::new(HttpMethod::Patch, routes::files::ONE, true);
        const PAYLOAD: PayloadKind = PayloadKind::Json;

        fn payload(&self) -> &UpdateFile {
            &self.update
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("file", self.file)
        }

        fn from_parts(params: &PathParams, update: UpdateFile) -> Result<Self, ApiError> {
            Ok(Self::new(params.id("file")?, update))
        }
    }
}

pub use calls::{DeleteFile, EditFile, GetFile, GetFileUsage, ListFiles};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert!(name_problem("level-3.map").is_none());
        assert!(name_problem("  ").is_some() && name_problem("a/b").is_some() && name_problem("a\\b").is_some());
        assert!(name_problem(&"x".repeat(256)).is_some() && name_problem("a\u{202e}b").is_some());
        assert!(is_valid_content_type("image/png") && is_valid_content_type("application/vnd.game+json"));
        assert!(!is_valid_content_type("image") && !is_valid_content_type("text/html; charset=utf-8") && !is_valid_content_type("a/b c"));
        assert!(is_sha256_hex(&"a".repeat(64)) && !is_sha256_hex(&"A".repeat(64)) && !is_sha256_hex("abc"));
        assert!(FileMeta::new().with_sha256("nope").validate().is_err());
        assert!(FileMeta::new().shared_with((0..101).map(UserId).collect()).validate().is_err());
        assert!(FileMeta::new().with_name("a.png").with_content_type("image/png").validate().is_ok());
        assert!(UpdateFile::new().with_name("").validate().is_err());
        assert_eq!(FileVisibility::parse("friends"), Some(FileVisibility::Friends));
        assert_eq!(FileVisibility::parse("nope"), None);
    }

    #[test]
    fn json() {
        let meta = FileMeta::new().with_name("a.png").shared_with(vec![UserId(4)]);
        assert_eq!(serde_json::to_value(&meta).unwrap_or_default(), serde_json::json!({"name": "a.png", "visibility": "shared", "shared_with": [4]}));
        let update: UpdateFile = serde_json::from_str(r#"{"metadata":null}"#).unwrap_or_default();
        assert_eq!(update.metadata, Some(Value::Null));
        let keep: UpdateFile = serde_json::from_str("{}").unwrap_or_default();
        assert_eq!(keep.metadata, None);
        let info: FileInfo = serde_json::from_value(serde_json::json!({
            "id": 1, "owner": 2, "name": "a", "content_type": "x/y", "size": 3, "sha256": "s", "visibility": "galaxy",
            "created_at": 5, "updated_at": 6
        }))
        .unwrap_or_else(|_| FileInfo::new(FileId(0), UserId(0), "", "", 0, "", UnixMillis(0)));
        assert_eq!((info.id, info.visibility, info.shared_with.len()), (FileId(1), FileVisibility::Unknown, 0));
    }
}
