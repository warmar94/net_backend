//! OpenAPI schemas of the protocol's matchmaking types (mirror structs: the protocol crate has no
//! OpenAPI dependency). A test serializes the real types and compares the field names with these
//! schemas, so the document cannot drift from the wire format.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::ToSchema;

/// One queue.
#[derive(Serialize, ToSchema)]
pub(crate) struct QueueInfo {
    /// The key tickets name.
    key: String,
    /// Players per match of the server's default rule.
    players: u32,
    /// How many tickets wait now.
    waiting: u32,
}

/// The server's queues.
#[derive(Serialize, ToSchema)]
pub(crate) struct Queues {
    /// The queues.
    queues: Vec<QueueInfo>,
}

/// `POST /v1/matchmaking/ticket` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct CreateTicket {
    /// The queue.
    queue: String,
    /// What the game's rules read (at most the server's limit, 1 KiB by default).
    #[schema(value_type = Option<Value>)]
    attributes: Option<serde_json::Value>,
}

/// A match (the `match.found` push, and a matched ticket's `found`).
#[derive(Serialize, ToSchema)]
pub(crate) struct MatchFound {
    /// The receiving player's ticket.
    ticket: i64,
    /// The queue.
    queue: String,
    /// Every player of the match.
    players: Vec<i64>,
    /// What the game's rules attached.
    #[schema(value_type = Option<Value>)]
    data: Option<serde_json::Value>,
}

/// The caller's ticket.
#[derive(Serialize, ToSchema)]
pub(crate) struct MatchTicket {
    /// The ticket.
    id: i64,
    /// Its queue.
    queue: String,
    /// `waiting` or `matched`.
    status: String,
    /// When it was made (unix ms).
    created_at: i64,
    /// When it runs out unmatched (waiting), or when the server forgets it (matched) (unix ms).
    expires_at: i64,
    /// The match, once matched.
    found: Option<MatchFound>,
}

/// The `match.expired` push.
#[derive(Serialize, ToSchema)]
pub(crate) struct TicketExpired {
    /// The ticket.
    ticket: i64,
    /// Its queue.
    queue: String,
}

/// An empty success answer: `{}`.
#[derive(Serialize, ToSchema)]
pub(crate) struct Ack {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::matchmaking as p;
    use net_backend_protocol::{TicketId, UnixMillis, UserId};
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
        assert_eq!(properties::<QueueInfo>(), keys(p::QueueInfo::new("duel", 2, 0)));
        assert_eq!(properties::<Queues>(), keys(p::Queues::new(vec![])));
        assert_eq!(properties::<CreateTicket>(), keys(p::CreateTicket::new("duel").with_attributes(json!(1))));
        let found = p::MatchFound::new(TicketId(1), "duel", vec![UserId(1)]).with_data(json!(1));
        assert_eq!(properties::<MatchFound>(), keys(&found));
        assert_eq!(properties::<MatchTicket>(), keys(p::MatchTicket::new(TicketId(1), "duel", UnixMillis(1), UnixMillis(2)).with_match(found)));
        assert_eq!(properties::<TicketExpired>(), keys(p::TicketExpired::new(TicketId(1), "duel")));
    }
}
