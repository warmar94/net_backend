//! OpenAPI schemas of the protocol's storage types (mirror structs: the protocol crate has no
//! OpenAPI dependency). A test serializes the real types and compares the field names with these
//! schemas, so the document cannot drift from the wire format.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::ToSchema;

/// A stored object with its value.
#[derive(Serialize, ToSchema)]
pub(crate) struct StorageObject {
    /// The collection.
    collection: String,
    /// The key.
    key: String,
    /// The owner's user id.
    owner: i64,
    /// The value (any JSON).
    #[schema(value_type = Value)]
    value: serde_json::Value,
    /// The version: 1 after the first write, +1 on every write (also the `ETag`).
    version: i64,
    /// Who may write it: `owner` or `server` (server-locked: client writes get 403).
    write: String,
    /// Who may read it: `private` (the owner), `public` (every player) or `friends` (the owner's friends).
    visibility: String,
    /// When it was last written (unix ms).
    updated_at: i64,
}

/// An object without its value (listings).
#[derive(Serialize, ToSchema)]
pub(crate) struct StorageObjectInfo {
    /// The collection.
    collection: String,
    /// The key.
    key: String,
    /// The version.
    version: i64,
    /// `owner` or `server`.
    write: String,
    /// `private`, `public` or `friends`.
    visibility: String,
    /// The size of the value's JSON in bytes.
    size_bytes: u64,
    /// When it was last written (unix ms).
    updated_at: i64,
}

/// A page of listings, ordered by key.
#[derive(Serialize, ToSchema)]
pub(crate) struct StorageObjectInfoPage {
    /// The objects (no values).
    items: Vec<StorageObjectInfo>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// The answer to a write.
#[derive(Serialize, ToSchema)]
pub(crate) struct ObjectAck {
    /// The collection.
    collection: String,
    /// The key.
    key: String,
    /// The new version.
    version: i64,
    /// When it was written (unix ms).
    updated_at: i64,
}

/// `PUT /v1/storage/{collection}/{key}` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct PutObject {
    /// The value (any JSON, at most `max_object_bytes`).
    #[schema(value_type = Value)]
    value: serde_json::Value,
    /// Only write if the stored version is this one (0: only if new); 409 `version_conflict`
    /// otherwise. Without it the last write wins.
    if_version: Option<i64>,
    /// Who may read it from now on: `private`, `public` or `friends` (the friends module); absent:
    /// unchanged (a new object is private).
    visibility: Option<String>,
}

/// One object to read.
#[derive(Serialize, ToSchema)]
pub(crate) struct ObjectRef {
    /// The collection.
    collection: String,
    /// The key.
    key: String,
}

/// `POST /v1/storage/_batch/get` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct BatchGet {
    /// 1 to 16 distinct objects.
    objects: Vec<ObjectRef>,
}

/// The objects found (missing ones are absent), at most 4 MiB of values.
#[derive(Serialize, ToSchema)]
pub(crate) struct BatchObjects {
    /// The objects.
    objects: Vec<StorageObject>,
}

/// One write of a batch.
#[derive(Serialize, ToSchema)]
pub(crate) struct BatchPutItem {
    /// The collection.
    collection: String,
    /// The key.
    key: String,
    /// The value.
    #[schema(value_type = Value)]
    value: serde_json::Value,
    /// Only write if the stored version is this one.
    if_version: Option<i64>,
}

/// `POST /v1/storage/_batch/put` body: one transaction, all or nothing.
#[derive(Serialize, ToSchema)]
pub(crate) struct BatchPut {
    /// 1 to 16 writes to distinct objects, at most 4 MiB of values together.
    objects: Vec<BatchPutItem>,
}

/// One acknowledgement per write, in request order.
#[derive(Serialize, ToSchema)]
pub(crate) struct BatchAcks {
    /// The acknowledgements.
    objects: Vec<ObjectAck>,
}

/// `PUT /v1/admin/users/{user}/storage/{collection}/{key}` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct AdminPutObject {
    /// The value.
    #[schema(value_type = Value)]
    value: serde_json::Value,
    /// Only write if the stored version is this one.
    if_version: Option<i64>,
    /// `owner` or `server` (locks the object against the owner); absent: unchanged (new: `owner`).
    write: Option<String>,
    /// `private`, `public` or `friends`; absent: unchanged (new: `private`).
    visibility: Option<String>,
}

/// The `details` of a 409 `version_conflict`.
#[derive(Serialize, ToSchema)]
pub(crate) struct VersionConflict {
    /// The failing batch item (batches only).
    index: Option<u32>,
    /// The stored version (absent: the object does not exist).
    current_version: Option<i64>,
}

/// An empty success answer: `{}`.
#[derive(Serialize, ToSchema)]
pub(crate) struct Ack {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::admin as p_admin;
    use net_backend_protocol::storage as p;
    use net_backend_protocol::{Cursor, Page, UnixMillis, UserId};
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

    fn keys(value: impl serde::Serialize) -> BTreeSet<String> {
        match serde_json::to_value(value) {
            Ok(Value::Object(map)) => map.keys().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    /// Every mirror has exactly the fields of the real type with all optional fields set.
    #[test]
    fn mirrors_match_the_protocol() {
        let t = UnixMillis(1);
        let object = p::StorageObject::new("s", "k", UserId(1), json!({}), p::ObjectVersion(1), t);
        assert_eq!(properties::<StorageObject>(), keys(&object));
        assert_eq!(properties::<StorageObjectInfo>(), keys(object.info()));
        assert_eq!(properties::<StorageObjectInfoPage>(), keys(Page::new(vec![object.info()], Some(Cursor::new("k")))));
        assert_eq!(properties::<ObjectAck>(), keys(p::ObjectAck::new("s", "k", p::ObjectVersion(1), t)));
        assert_eq!(properties::<PutObject>(), keys(p::PutObject::new(json!(1)).if_absent().with_visibility(p::ObjectVisibility::Public)));
        assert_eq!(properties::<ObjectRef>(), keys(p::ObjectRef::new("s", "k")));
        assert_eq!(properties::<BatchGet>(), keys(p::BatchGet::new(vec![])));
        assert_eq!(properties::<BatchObjects>(), keys(p::BatchObjects::new(vec![])));
        assert_eq!(properties::<BatchPutItem>(), keys(p::BatchPutItem::new("s", "k", p::PutObject::new(json!(1)).if_absent())));
        assert_eq!(properties::<BatchPut>(), keys(p::BatchPut::new(vec![])));
        assert_eq!(properties::<BatchAcks>(), keys(p::BatchAcks::new(vec![])));
        assert_eq!(
            properties::<AdminPutObject>(),
            keys(
                p_admin::AdminPutObject::new(json!(1))
                    .if_version(p::ObjectVersion(1))
                    .with_write(p::WriteAccess::Server)
                    .with_visibility(p::ObjectVisibility::Public)
            )
        );
        assert_eq!(properties::<VersionConflict>(), keys(p::VersionConflict::new(Some(p::ObjectVersion(1))).at_index(0)));
    }
}
