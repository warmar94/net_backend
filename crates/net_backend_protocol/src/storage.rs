//! Storage: save slots and other per-user key-value objects. Routes: [`routes::storage`](crate::routes::storage).
//!
//! An object lives at `(owner, collection, key)` and holds one JSON value (a save game, settings,
//! an inventory snapshot). Every route addresses the CALLER's own objects; another user's objects
//! are reached only through the admin routes.
//!
//! **Versions.** Every write bumps the object's [`ObjectVersion`] (1 for a new object). The default
//! is **last write wins**. A write that names the version it expects ([`PutObject::if_version`])
//! is refused with 409 [`codes::VERSION_CONFLICT`](crate::codes::VERSION_CONFLICT) and a
//! [`VersionConflict`] in `details` if the stored version differs, and nothing changes: only
//! then two devices cannot silently overwrite each other's save. [`ObjectVersion::ABSENT`] means
//! "only if it does not exist yet". The body field is the primary form; the server also honours
//! `If-Match: "N"` and `If-None-Match: *` on single-object PUT / DELETE (400 if header and body
//! disagree) and always sends the version as an `ETag` header (`"3"`).
//!
//! **Delete** is idempotent: deleting an object that does not exist answers `Ack`, unless
//! `if_version` names a version (then 409 `version_conflict`).
//!
//! **Write access.** [`WriteAccess::Server`] objects (set only by server-side game code, never by a
//! client request) refuse client writes with 403.
//!
//! **Body budget.** `bevy_net_backend` refuses answers over 10 MiB by default. Single objects are
//! at most [`DEFAULT_MAX_OBJECT_BYTES`] (256 KiB); a batch carries at most [`MAX_BATCH`] objects and
//! [`MAX_BATCH_BYTES`] (4 MiB) of values, so a batch answer stays near 4 MiB; collection listings
//! return [`StorageObjectInfo`] (no values: ~200 bytes each × 100 = ~20 KB). Request body limits the
//! server sets: [`PUT_BODY_LIMIT_BYTES`], [`BATCH_BODY_LIMIT_BYTES`], everything else
//! [`routes::DEFAULT_BODY_LIMIT_BYTES`](crate::routes::DEFAULT_BODY_LIMIT_BYTES).
//!
//! Binary data: the value is JSON; put bytes in a string (e.g. base64, +33 %: it counts against the
//! size limit) yourself.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ApiError, ValidationDetails};
use crate::ids::UserId;
use crate::time::UnixMillis;

/// The longest collection name or key, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// The default largest value, in bytes of its JSON (256 KiB; the server may configure less, or
/// more as long as a batch read stays within [`MAX_BATCH_BYTES`]).
pub const DEFAULT_MAX_OBJECT_BYTES: usize = 256 * 1024;
/// The default number of objects one user may own (the server may configure another).
pub const DEFAULT_MAX_OBJECTS_PER_USER: u32 = 1000;
/// The most objects in one batch request.
pub const MAX_BATCH: usize = 16;
/// The most value bytes (sum of the values' JSON) in one batch, written or read. A batch read whose
/// objects exceed it is refused with 413 `payload_too_large` (read fewer objects per batch).
pub const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;
/// The request body limit for a single-object PUT: the value plus room for the envelope.
pub const PUT_BODY_LIMIT_BYTES: usize = DEFAULT_MAX_OBJECT_BYTES + 16 * 1024;
/// The request body limit for a batch put: the values plus room for the item envelopes.
pub const BATCH_BODY_LIMIT_BYTES: usize = MAX_BATCH_BYTES + 64 * 1024;

// The budget, checked at compile time: a full batch of default-size objects fits MAX_BATCH_BYTES,
// and the largest request / answer stays below half of the client's default 10 MiB body limit.
const _: () = assert!(MAX_BATCH * DEFAULT_MAX_OBJECT_BYTES <= MAX_BATCH_BYTES);
const _: () = assert!(BATCH_BODY_LIMIT_BYTES < 5 * 1024 * 1024);

/// Whether `name` is a valid collection name or key: 1 to [`MAX_NAME_BYTES`] bytes of ASCII
/// letters, digits, `_`, `-` and `.`, starting with a letter or digit. Valid names are safe in a
/// URL path without escaping (and cannot be `.`, `..` or `_batch`).
pub fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() <= MAX_NAME_BYTES
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// The size of a value: the length of its JSON.
pub fn value_bytes(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |json| json.len())
}

/// An object's version: 1 after the first write, +1 on every write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectVersion(pub i64);

impl ObjectVersion {
    /// "The object does not exist": as [`PutObject::if_version`] a create-only write.
    pub const ABSENT: ObjectVersion = ObjectVersion(0);

    /// The version with this value.
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// The value.
    pub const fn get(self) -> i64 {
        self.0
    }

    /// The `ETag` header value: the number in double quotes (`"3"`).
    pub fn etag(self) -> String {
        format!("\"{}\"", self.0)
    }
}

/// Who may write an object (read-only for clients: they see it, server-side game code sets it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WriteAccess {
    /// The owner (and the server).
    #[default]
    Owner,
    /// Only the server (game logic in hooks or the game's own routes); client writes get 403.
    Server,
    /// A value from a newer server this version does not know (never sent by a server; treat as
    /// `Server`).
    #[serde(other)]
    Unknown,
}

/// The `details` of a 409 [`codes::VERSION_CONFLICT`](crate::codes::VERSION_CONFLICT).
///
/// JSON: `{"current_version":3}`; for a batch also `"index":2` (the first failing item, in request
/// order); `current_version` is absent when the object does not exist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VersionConflict {
    /// The batch item that failed (absent for single-object routes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    /// The stored version (absent: the object does not exist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<ObjectVersion>,
}

impl VersionConflict {
    /// A conflict with this stored version (`None`: no object).
    pub fn new(current_version: Option<ObjectVersion>) -> Self {
        Self { index: None, current_version }
    }

    /// The same conflict for batch item `index`.
    pub fn at_index(mut self, index: u32) -> Self {
        self.index = Some(index);
        self
    }

    /// The 409 error carrying these details.
    pub fn into_error(self) -> ApiError {
        let details = serde_json::to_value(self).unwrap_or(Value::Null);
        ApiError::new(crate::codes::VERSION_CONFLICT, "the object's version is not the expected one").with_details(details)
    }
}

/// Write one object: `PUT /v1/storage/{collection}/{key}` → [`ObjectAck`].
///
/// JSON: `{"value":{…},"if_version":3}` (`if_version` optional; without it the last write wins).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PutObject {
    /// The value.
    pub value: Value,
    /// Only write if the stored version is this one ([`ObjectVersion::ABSENT`]: only if new).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_version: Option<ObjectVersion>,
}

impl PutObject {
    /// An unconditional write of `value`.
    pub fn new(value: Value) -> Self {
        Self { value, if_version: None }
    }

    /// Serialize `value` to JSON first.
    pub fn from_serialize<T: Serialize + ?Sized>(value: &T) -> Result<Self, serde_json::Error> {
        Ok(Self::new(serde_json::to_value(value)?))
    }

    /// Only write if the stored version is `version`.
    pub fn if_version(mut self, version: ObjectVersion) -> Self {
        self.if_version = Some(version);
        self
    }

    /// Only write if the object does not exist yet.
    pub fn if_absent(self) -> Self {
        self.if_version(ObjectVersion::ABSENT)
    }

    /// The size rule: the value's JSON is at most `max_bytes`
    /// ([`DEFAULT_MAX_OBJECT_BYTES`] unless the server configures another).
    pub fn validate(&self, max_bytes: usize) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        check_value(&self.value, max_bytes, "value", &mut details);
        details.into_result()
    }
}

fn check_value(value: &Value, max_bytes: usize, field: &str, details: &mut ValidationDetails) -> usize {
    let size = value_bytes(value);
    if size > max_bytes {
        details.add(field, format!("is larger than {max_bytes} bytes"));
    }
    size
}

fn check_names(collection: &str, key: &str, prefix: &str, details: &mut ValidationDetails) {
    if !is_valid_name(collection) {
        details.add(format!("{prefix}collection"), "is not a valid storage name");
    }
    if !is_valid_name(key) {
        details.add(format!("{prefix}key"), "is not a valid storage name");
    }
}

/// Delete one object: `DELETE /v1/storage/{collection}/{key}?if_version=3` → [`Ack`](crate::Ack).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeleteObject {
    /// Only delete if the stored version is this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_version: Option<ObjectVersion>,
}

impl DeleteObject {
    /// An unconditional delete.
    pub fn new() -> Self {
        Self::default()
    }

    /// Only delete if the stored version is `version`.
    pub fn if_version(mut self, version: ObjectVersion) -> Self {
        self.if_version = Some(version);
        self
    }
}

/// A stored object with its value: `GET /v1/storage/{collection}/{key}` and batch reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StorageObject {
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
    /// The owner.
    pub owner: UserId,
    /// The value.
    pub value: Value,
    /// The version.
    pub version: ObjectVersion,
    /// Who may write it.
    #[serde(default)]
    pub write: WriteAccess,
    /// When it was last written.
    pub updated_at: UnixMillis,
}

impl StorageObject {
    /// An object (owner write access).
    pub fn new(collection: impl Into<String>, key: impl Into<String>, owner: UserId, value: Value, version: ObjectVersion, updated_at: UnixMillis) -> Self {
        Self { collection: collection.into(), key: key.into(), owner, value, version, write: WriteAccess::Owner, updated_at }
    }

    /// The same object with this write access.
    pub fn with_write(mut self, write: WriteAccess) -> Self {
        self.write = write;
        self
    }

    /// The value decoded as `T`.
    pub fn value_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(&self.value)
    }

    /// The listing entry for this object (no value).
    pub fn info(&self) -> StorageObjectInfo {
        StorageObjectInfo {
            collection: self.collection.clone(),
            key: self.key.clone(),
            version: self.version,
            write: self.write,
            size_bytes: u64::try_from(value_bytes(&self.value)).unwrap_or(u64::MAX),
            updated_at: self.updated_at,
        }
    }
}

/// An object without its value: the items of `GET /v1/storage/{collection}` (a listing stays small
/// whatever the values weigh; read the values with a GET or a batch get).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StorageObjectInfo {
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
    /// The version.
    pub version: ObjectVersion,
    /// Who may write it.
    #[serde(default)]
    pub write: WriteAccess,
    /// The size of the value's JSON, in bytes.
    pub size_bytes: u64,
    /// When it was last written.
    pub updated_at: UnixMillis,
}

impl StorageObjectInfo {
    /// A listing entry.
    pub fn new(collection: impl Into<String>, key: impl Into<String>, version: ObjectVersion, size_bytes: u64, updated_at: UnixMillis) -> Self {
        Self { collection: collection.into(), key: key.into(), version, write: WriteAccess::Owner, size_bytes, updated_at }
    }
}

/// The answer to a write: where it went and its new version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ObjectAck {
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
    /// The new version.
    pub version: ObjectVersion,
    /// When it was written.
    pub updated_at: UnixMillis,
}

impl ObjectAck {
    /// An acknowledgement.
    pub fn new(collection: impl Into<String>, key: impl Into<String>, version: ObjectVersion, updated_at: UnixMillis) -> Self {
        Self { collection: collection.into(), key: key.into(), version, updated_at }
    }
}

/// One object to read in a [`BatchGet`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ObjectRef {
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
}

impl ObjectRef {
    /// A reference.
    pub fn new(collection: impl Into<String>, key: impl Into<String>) -> Self {
        Self { collection: collection.into(), key: key.into() }
    }
}

fn check_batch_len(len: usize, details: &mut ValidationDetails) {
    if len == 0 {
        details.add("objects", "is empty");
    }
    if len > MAX_BATCH {
        details.add("objects", format!("has more than {MAX_BATCH} entries"));
    }
}

/// Adds a message for every `(collection, key)` that appeared earlier in the batch.
fn check_duplicates<'a>(names: impl Iterator<Item = (&'a str, &'a str)>, details: &mut ValidationDetails) {
    let mut seen = HashSet::new();
    for (i, name) in names.enumerate() {
        if !seen.insert(name) {
            details.add(format!("objects.{i}"), "names the same object as an earlier entry");
        }
    }
}

/// Read several of the caller's objects: `POST /v1/storage/_batch/get` → [`BatchObjects`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BatchGet {
    /// The objects (1 to [`MAX_BATCH`], no duplicates).
    pub objects: Vec<ObjectRef>,
}

impl BatchGet {
    /// A batch read.
    pub fn new(objects: Vec<ObjectRef>) -> Self {
        Self { objects }
    }

    /// The rules: 1 to [`MAX_BATCH`] distinct objects with valid names.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        check_batch_len(self.objects.len(), &mut details);
        for (i, object) in self.objects.iter().enumerate() {
            check_names(&object.collection, &object.key, &format!("objects.{i}."), &mut details);
        }
        check_duplicates(self.objects.iter().map(|o| (o.collection.as_str(), o.key.as_str())), &mut details);
        details.into_result()
    }
}

/// The answer to a [`BatchGet`]: the objects that exist (missing ones are simply absent), at most
/// [`MAX_BATCH_BYTES`] of values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BatchObjects {
    /// The objects found.
    pub objects: Vec<StorageObject>,
}

impl BatchObjects {
    /// An answer.
    pub fn new(objects: Vec<StorageObject>) -> Self {
        Self { objects }
    }
}

/// One write in a [`BatchPut`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BatchPutItem {
    /// The collection.
    pub collection: String,
    /// The key.
    pub key: String,
    /// The write.
    #[serde(flatten)]
    pub put: PutObject,
}

impl BatchPutItem {
    /// A write.
    pub fn new(collection: impl Into<String>, key: impl Into<String>, put: PutObject) -> Self {
        Self { collection: collection.into(), key: key.into(), put }
    }
}

/// Write several objects in one transaction: `POST /v1/storage/_batch/put` → [`BatchAcks`].
/// All or nothing: one failed condition or rule fails the whole batch (a version conflict names
/// the first failing item: [`VersionConflict::index`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BatchPut {
    /// The writes (1 to [`MAX_BATCH`], no two for the same object).
    pub objects: Vec<BatchPutItem>,
}

impl BatchPut {
    /// A batch write.
    pub fn new(objects: Vec<BatchPutItem>) -> Self {
        Self { objects }
    }

    /// The rules: 1 to [`MAX_BATCH`] writes to distinct objects with valid names, each value within
    /// `max_object_bytes`, all values together within [`MAX_BATCH_BYTES`].
    pub fn validate(&self, max_object_bytes: usize) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        check_batch_len(self.objects.len(), &mut details);
        let mut total = 0usize;
        for (i, item) in self.objects.iter().enumerate() {
            let prefix = format!("objects.{i}.");
            check_names(&item.collection, &item.key, &prefix, &mut details);
            total = total.saturating_add(check_value(&item.put.value, max_object_bytes, &format!("{prefix}value"), &mut details));
        }
        if total > MAX_BATCH_BYTES {
            details.add("objects", format!("carry more than {MAX_BATCH_BYTES} bytes of values"));
        }
        check_duplicates(self.objects.iter().map(|o| (o.collection.as_str(), o.key.as_str())), &mut details);
        details.into_result()
    }
}

/// The answer to a [`BatchPut`]: one acknowledgement per write, in request order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BatchAcks {
    /// The acknowledgements.
    pub objects: Vec<ObjectAck>,
}

impl BatchAcks {
    /// An answer.
    pub fn new(objects: Vec<ObjectAck>) -> Self {
        Self { objects }
    }
}

// ---- typed HTTP calls (see `http_call`) ---------------------------------------------------------

/// The typed HTTP calls of this module (in their own scope: their imports stay out of the
/// module's doc-link scope).
mod calls {
    use super::*;

    use crate::envelope::Ack;
    use crate::http_call::{payload_call, HttpCall, NoPayload, PathParams, PayloadKind, NO_PAYLOAD};
    use crate::page::{Page, PageRequest};
    use crate::routes::{self, HttpMethod, Route};

    payload_call!(BatchGet, Post, routes::storage::BATCH_GET, true, Json, BatchObjects);
    payload_call!(BatchPut, Post, routes::storage::BATCH_PUT, true, Json, BatchAcks);

    const NOT_A_NAME: &str = "is not a valid storage name";

    fn names(params: &PathParams) -> Result<(String, String), ApiError> {
        Ok((params.checked("collection", is_valid_name, NOT_A_NAME)?, params.checked("key", is_valid_name, NOT_A_NAME)?))
    }

    /// List the caller's objects in a collection: `GET /v1/storage/{collection}?cursor=…&limit=…` →
    /// [`Page`]`<`[`StorageObjectInfo`]`>` (no values; ordered by key).
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListObjects {
        /// The collection.
        pub collection: String,
        /// Which page.
        pub page: PageRequest,
    }

    impl ListObjects {
        /// The first page of `collection`.
        pub fn new(collection: impl Into<String>) -> Self {
            Self { collection: collection.into(), page: PageRequest::first() }
        }

        /// The same call for this page.
        pub fn with_page(mut self, page: PageRequest) -> Self {
            self.page = page;
            self
        }
    }

    impl HttpCall for ListObjects {
        type Payload = PageRequest;
        type Response = Page<StorageObjectInfo>;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::storage::COLLECTION, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &PageRequest {
            &self.page
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("collection", &self.collection)
        }

        fn from_parts(params: &PathParams, page: PageRequest) -> Result<Self, ApiError> {
            Ok(Self::new(params.checked("collection", is_valid_name, NOT_A_NAME)?).with_page(page))
        }
    }

    /// Read one of the caller's objects: `GET /v1/storage/{collection}/{key}` → [`StorageObject`]
    /// (the `ETag` header carries its version).
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct GetObject {
        /// The collection.
        pub collection: String,
        /// The key.
        pub key: String,
    }

    impl GetObject {
        /// Read `collection` / `key`.
        pub fn new(collection: impl Into<String>, key: impl Into<String>) -> Self {
            Self { collection: collection.into(), key: key.into() }
        }
    }

    impl HttpCall for GetObject {
        type Payload = NoPayload;
        type Response = StorageObject;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::storage::OBJECT, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("collection", &self.collection).with("key", &self.key)
        }

        fn from_parts(params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            let (collection, key) = names(params)?;
            Ok(Self::new(collection, key))
        }
    }

    /// Write one of the caller's objects: `PUT /v1/storage/{collection}/{key}` with a [`PutObject`]
    /// → [`ObjectAck`].
    #[derive(Clone, Debug, PartialEq)]
    #[non_exhaustive]
    pub struct WriteObject {
        /// The collection.
        pub collection: String,
        /// The key.
        pub key: String,
        /// The write.
        pub put: PutObject,
    }

    impl WriteObject {
        /// Write `put` to `collection` / `key`.
        pub fn new(collection: impl Into<String>, key: impl Into<String>, put: PutObject) -> Self {
            Self { collection: collection.into(), key: key.into(), put }
        }
    }

    impl HttpCall for WriteObject {
        type Payload = PutObject;
        type Response = ObjectAck;
        const ROUTE: Route = Route::new(HttpMethod::Put, routes::storage::OBJECT, true);
        const PAYLOAD: PayloadKind = PayloadKind::Json;

        fn payload(&self) -> &PutObject {
            &self.put
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("collection", &self.collection).with("key", &self.key)
        }

        fn from_parts(params: &PathParams, put: PutObject) -> Result<Self, ApiError> {
            let (collection, key) = names(params)?;
            Ok(Self::new(collection, key, put))
        }
    }

    /// Delete one of the caller's objects: `DELETE /v1/storage/{collection}/{key}?if_version=…` →
    /// [`Ack`] (idempotent without `if_version`).
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct RemoveObject {
        /// The collection.
        pub collection: String,
        /// The key.
        pub key: String,
        /// The condition.
        pub delete: DeleteObject,
    }

    impl RemoveObject {
        /// Delete `collection` / `key`.
        pub fn new(collection: impl Into<String>, key: impl Into<String>) -> Self {
            Self { collection: collection.into(), key: key.into(), delete: DeleteObject::new() }
        }

        /// Only delete if the stored version is `version`.
        pub fn if_version(mut self, version: ObjectVersion) -> Self {
            self.delete = self.delete.if_version(version);
            self
        }
    }

    impl HttpCall for RemoveObject {
        type Payload = DeleteObject;
        type Response = Ack;
        const ROUTE: Route = Route::new(HttpMethod::Delete, routes::storage::OBJECT, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &DeleteObject {
            &self.delete
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("collection", &self.collection).with("key", &self.key)
        }

        fn from_parts(params: &PathParams, delete: DeleteObject) -> Result<Self, ApiError> {
            let (collection, key) = names(params)?;
            Ok(Self { collection, key, delete })
        }
    }
}

pub use calls::{GetObject, ListObjects, RemoveObject, WriteObject};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for good in ["saves", "slot-1", "a.b_c", "0", &"x".repeat(MAX_NAME_BYTES)] {
            assert!(is_valid_name(good), "{good}");
        }
        for bad in ["", ".", "..", "_batch", "-x", "a/b", "a b", "ä", "a%2F", "a\0", "a\u{200B}", &"x".repeat(MAX_NAME_BYTES + 1)] {
            assert!(!is_valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn versions_and_rules() {
        assert_eq!(ObjectVersion(3).etag(), "\"3\"");
        assert_eq!(PutObject::new(Value::Null).if_absent().if_version, Some(ObjectVersion::ABSENT));
        assert!(PutObject::new(Value::String("x".repeat(100))).validate(50).is_err());
        assert!(PutObject::new(Value::Bool(true)).validate(DEFAULT_MAX_OBJECT_BYTES).is_ok());
        assert!(BatchGet::new(vec![]).validate().is_err());
        assert!(BatchGet::new(vec![ObjectRef::new("saves", "a")]).validate().is_ok());
        assert!(BatchGet::new(vec![ObjectRef::new("saves", "../a")]).validate().is_err());
        assert!(BatchGet::new(vec![ObjectRef::new("s", "k"); 2]).validate().is_err());
        let many: Vec<ObjectRef> = (0..=MAX_BATCH).map(|i| ObjectRef::new("s", format!("k{i}"))).collect();
        assert!(BatchGet::new(many).validate().is_err());
        let put = BatchPut::new(vec![BatchPutItem::new("saves", "a", PutObject::new(Value::from("y".repeat(100))))]);
        assert!(put.validate(DEFAULT_MAX_OBJECT_BYTES).is_ok());
        assert!(put.validate(10).is_err());
    }

    #[test]
    fn batch_duplicates_and_budget() {
        let item = |key: &str, size: usize| BatchPutItem::new("saves", key, PutObject::new(Value::from("x".repeat(size))));
        let duplicate = BatchPut::new(vec![item("a", 1), item("b", 1), item("a", 1)]);
        let error = duplicate.validate(DEFAULT_MAX_OBJECT_BYTES).err().and_then(|e| e.details_as::<ValidationDetails>()).unwrap_or_default();
        assert!(error.fields.contains_key("objects.2"), "{error:?}");
        // 16 values of 300 KiB pass a raised per-object limit but break the batch budget.
        let heavy: Vec<BatchPutItem> = (0..MAX_BATCH).map(|i| item(&format!("k{i}"), 300 * 1024)).collect();
        assert!(BatchPut::new(heavy).validate(512 * 1024).is_err());
    }

    #[test]
    fn conflict_details_and_info() {
        let error = VersionConflict::new(Some(ObjectVersion(3))).at_index(2).into_error();
        assert_eq!(error.details, Some(serde_json::json!({"index": 2, "current_version": 3})));
        assert_eq!(error.http_status(), 409);
        assert_eq!(serde_json::to_string(&VersionConflict::new(None)).ok().as_deref(), Some("{}"));
        let object = StorageObject::new("s", "k", UserId(1), serde_json::json!({"level": 3}), ObjectVersion(1), UnixMillis(5));
        assert_eq!(object.info().size_bytes, 11);
        #[derive(Deserialize)]
        struct Save {
            level: u32,
        }
        assert_eq!(object.value_as::<Save>().map(|s| s.level).ok(), Some(3));
        assert_eq!(serde_json::from_str::<WriteAccess>("\"moderators\"").ok(), Some(WriteAccess::Unknown));
    }
}
