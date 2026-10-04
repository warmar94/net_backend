//! OpenAPI / AsyncAPI schemas of the protocol's notification types (mirror structs: the protocol
//! crate has no OpenAPI dependency). A test serializes the real types and compares the field
//! names with these schemas, so the documents cannot drift from the wire format.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::ToSchema;

/// A stored notification (also the `notify.new` push).
#[derive(Serialize, ToSchema)]
pub(crate) struct Notification {
    /// The id (newer notifications have larger ids).
    id: i64,
    /// What it is about (the game's own kinds).
    kind: String,
    /// A text to show.
    text: Option<String>,
    /// The game's data.
    #[schema(value_type = Option<Value>)]
    data: Option<serde_json::Value>,
    /// The account that caused it.
    sender: Option<i64>,
    /// When it was created (unix ms).
    created_at: i64,
    /// Whether the player marked it as read.
    read: bool,
}

/// A page of notifications, newest first.
#[derive(Serialize, ToSchema)]
pub(crate) struct NotificationPage {
    /// The notifications.
    items: Vec<Notification>,
    /// Pass as `cursor` for the next page; absent on the last page.
    next_cursor: Option<String>,
}

/// `notify.list` request (the query of `GET /v1/notifications`).
#[derive(Serialize, ToSchema)]
pub(crate) struct NotificationQuery {
    /// The previous page's next_cursor.
    cursor: Option<String>,
    /// 1-100, default 50.
    limit: Option<u32>,
    /// Only the unread ones.
    unread_only: Option<bool>,
}

/// `notify.count` request: `{}`.
#[derive(Serialize, ToSchema)]
pub(crate) struct CountNotifications {}

/// The caller's counts.
#[derive(Serialize, ToSchema)]
pub(crate) struct NotificationCount {
    /// Unread notifications.
    unread: u64,
    /// All notifications.
    total: u64,
}

/// `notify.mark` request / `POST /v1/notifications/mark` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct MarkNotifications {
    /// 1 to 100 distinct ids (unless `all`).
    ids: Vec<i64>,
    /// Every notification of the caller.
    all: bool,
    /// Read (true) or unread (false).
    read: bool,
}

/// The answer to a mark.
#[derive(Serialize, ToSchema)]
pub(crate) struct MarkAck {
    /// How many changed state.
    changed: u64,
    /// The caller's unread notifications now.
    unread: u64,
}

/// `notify.delete` request.
#[derive(Serialize, ToSchema)]
pub(crate) struct DeleteNotification {
    /// The notification id.
    id: i64,
}

/// An empty success answer: `{}`.
#[derive(Serialize, ToSchema)]
pub(crate) struct Ack {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::notifications as p;
    use net_backend_protocol::{Cursor, NotificationId, Page, UnixMillis, UserId};
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
        let n = p::Notification::new(NotificationId(1), "gift", UnixMillis(1)).with_text("t").with_data(json!(1)).with_sender(UserId(2));
        assert_eq!(properties::<Notification>(), keys(&n));
        assert_eq!(properties::<NotificationPage>(), keys(Page::new(vec![n], Some(Cursor::new("1")))));
        assert_eq!(properties::<NotificationQuery>(), keys(p::NotificationQuery::new().after(Cursor::new("1")).with_limit(1).unread_only()));
        assert_eq!(properties::<CountNotifications>(), keys(p::CountNotifications::new()));
        assert_eq!(properties::<NotificationCount>(), keys(p::NotificationCount::new(1, 2)));
        let mut mark = p::MarkNotifications::all_read();
        mark.ids.push(NotificationId(1));
        assert_eq!(properties::<MarkNotifications>(), keys(&mark));
        assert_eq!(properties::<MarkAck>(), keys(p::MarkAck::new(1, 2)));
        assert_eq!(properties::<DeleteNotification>(), keys(p::DeleteNotification::new(NotificationId(1))));
    }
}
