//! Matchmaking: a player puts a ticket into one of the server's queues; the server groups waiting
//! tickets into matches by the game's rules and tells each matched player (`match.found`); a
//! ticket nobody matched runs out (`match.expired`). The game decides who plays with whom; the
//! server only coordinates (the players then meet in a lobby or on the game's own server, as the
//! match's `data` says).
//!
//! | Route / kind | Request → answer |
//! |---|---|
//! | `GET /v1/matchmaking/queues` | [`ListQueues`] → [`Queues`] (with how many tickets wait) |
//! | `POST /v1/matchmaking/ticket` | [`CreateTicket`] → [`MatchTicket`] (one ticket per player) |
//! | `GET /v1/matchmaking/ticket` | [`GetTicket`] → [`MatchTicket`] (waiting, or matched with its [`MatchFound`]) |
//! | `DELETE /v1/matchmaking/ticket` | [`CancelTicket`] → [`Ack`] (also when there is none) |
//! | push `match.found` | [`MatchFound`], to each matched player |
//! | push `match.expired` | [`TicketExpired`], when a ticket ran out unmatched |
//!
//! A ticket carries `attributes` (a small JSON value: a skill rating, a region, a party size) the
//! game's rules read. A client without a WebSocket asks [`GetTicket`] instead of waiting for the
//! pushes: a matched ticket is answered with its match for a while after the match.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::{Ack, ServerPush};
use crate::error::{ApiError, ValidationDetails};
use crate::ids::{TicketId, UserId};
use crate::kinds;
use crate::time::UnixMillis;

/// The longest queue key, in bytes.
pub const MAX_QUEUE_KEY_BYTES: usize = 64;
/// The default largest ticket `attributes`, in bytes of their JSON (a server may configure another).
pub const DEFAULT_MAX_ATTRIBUTES_BYTES: usize = 1024;

/// Whether `key` is a valid queue key: 1 to [`MAX_QUEUE_KEY_BYTES`] bytes of ASCII lower-case
/// letters, digits, `_`, `-` and `.`, starting with a letter or digit (`"duel"`, `"ranked.eu-2v2"`).
pub fn is_valid_queue_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() <= MAX_QUEUE_KEY_BYTES
        && bytes.first().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'))
}

/// One queue of the server.
///
/// JSON: `{"key":"duel","players":2,"waiting":5}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QueueInfo {
    /// The key tickets name.
    pub key: String,
    /// How many players one match of the server's default rule has (the game's rules may differ).
    pub players: u32,
    /// How many tickets wait now.
    pub waiting: u32,
}

impl QueueInfo {
    /// A queue.
    pub fn new(key: impl Into<String>, players: u32, waiting: u32) -> Self {
        Self { key: key.into(), players, waiting }
    }
}

/// The server's queues ([`ListQueues`]).
///
/// JSON: `{"queues":[QueueInfo]}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Queues {
    /// The queues, by key.
    pub queues: Vec<QueueInfo>,
}

impl Queues {
    /// A list.
    pub fn new(queues: Vec<QueueInfo>) -> Self {
        Self { queues }
    }
}

/// Put a ticket into a queue: `POST /v1/matchmaking/ticket` → [`MatchTicket`]. A player has one
/// ticket at a time (409 `conflict` while one waits).
///
/// JSON: `{"queue":"duel","attributes":{"rating":1520,"region":"eu"}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CreateTicket {
    /// The queue ([`is_valid_queue_key`]).
    pub queue: String,
    /// What the game's rules read (at most the server's limit, [`DEFAULT_MAX_ATTRIBUTES_BYTES`]
    /// by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Value>,
}

impl CreateTicket {
    /// A ticket for `queue` without attributes.
    pub fn new(queue: impl Into<String>) -> Self {
        Self { queue: queue.into(), attributes: None }
    }

    /// The same ticket with attributes.
    pub fn with_attributes(mut self, attributes: Value) -> Self {
        self.attributes = Some(attributes);
        self
    }

    /// The shape rule: a valid queue key (the attributes' size is the server's).
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if !is_valid_queue_key(&self.queue) {
            details.add("queue", "is not a valid queue key");
        }
        details.into_result()
    }
}

/// Where a ticket is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TicketStatus {
    /// It waits in its queue.
    Waiting,
    /// It was matched ([`MatchTicket::found`]).
    Matched,
    /// A status from a newer server this version does not know.
    #[serde(other)]
    Unknown,
}

/// The caller's ticket.
///
/// JSON: `{"id":881234,"queue":"duel","status":"waiting","created_at":1790000000000,"expires_at":1790000120000}`
/// (+ `"found":MatchFound` once matched).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MatchTicket {
    /// The ticket.
    pub id: TicketId,
    /// Its queue.
    pub queue: String,
    /// Waiting or matched.
    pub status: TicketStatus,
    /// When it was made.
    pub created_at: UnixMillis,
    /// When it runs out unmatched (waiting), or when the server forgets it (matched).
    pub expires_at: UnixMillis,
    /// The match, once matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found: Option<MatchFound>,
}

impl MatchTicket {
    /// A waiting ticket.
    pub fn new(id: TicketId, queue: impl Into<String>, created_at: UnixMillis, expires_at: UnixMillis) -> Self {
        Self { id, queue: queue.into(), status: TicketStatus::Waiting, created_at, expires_at, found: None }
    }

    /// The same ticket, matched.
    pub fn with_match(mut self, found: MatchFound) -> Self {
        self.status = TicketStatus::Matched;
        self.found = Some(found);
        self
    }
}

/// Push `match.found` (and the `found` of a matched [`MatchTicket`]): the players of one match.
///
/// JSON: `{"ticket":881234,"queue":"duel","players":[42,7],"data":{"lobby":12}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MatchFound {
    /// The receiving player's ticket.
    pub ticket: TicketId,
    /// The queue.
    pub queue: String,
    /// Every player of the match, in the order the game's rules put them.
    pub players: Vec<UserId>,
    /// What the game's rules attached (a lobby, a server address, the teams), if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl MatchFound {
    /// A match without data.
    pub fn new(ticket: TicketId, queue: impl Into<String>, players: Vec<UserId>) -> Self {
        Self { ticket, queue: queue.into(), players, data: None }
    }

    /// The same match with data.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl ServerPush for MatchFound {
    const KIND: &'static str = kinds::MATCH_FOUND;
}

/// Push `match.expired`: the caller's ticket ran out unmatched (and is gone).
///
/// JSON: `{"ticket":881234,"queue":"duel"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TicketExpired {
    /// The ticket.
    pub ticket: TicketId,
    /// Its queue.
    pub queue: String,
}

impl TicketExpired {
    /// An expiry.
    pub fn new(ticket: TicketId, queue: impl Into<String>) -> Self {
        Self { ticket, queue: queue.into() }
    }
}

impl ServerPush for TicketExpired {
    const KIND: &'static str = kinds::MATCH_EXPIRED;
}

// ---- typed HTTP calls (see `http_call`) ---------------------------------------------------------

/// The typed HTTP calls of this module (in their own scope: their imports stay out of the
/// module's doc-link scope).
mod calls {
    use super::*;

    use crate::http_call::{payload_call, HttpCall, NoPayload, PathParams, PayloadKind, NO_PAYLOAD};
    use crate::routes::{self, HttpMethod, Route};

    payload_call!(CreateTicket, Post, routes::matchmaking::TICKET, true, Json, MatchTicket);

    /// A call without a payload.
    macro_rules! plain_call {
        ($(#[$meta:meta])* $name:ident, $method:ident, $path:expr, $response:ty) => {
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
            #[non_exhaustive]
            pub struct $name {}

            impl $name {
                /// The call.
                pub fn new() -> Self {
                    Self {}
                }
            }

            impl HttpCall for $name {
                type Payload = NoPayload;
                type Response = $response;
                const ROUTE: Route = Route::new(HttpMethod::$method, $path, true);
                const PAYLOAD: PayloadKind = PayloadKind::Empty;

                fn payload(&self) -> &NoPayload {
                    &NO_PAYLOAD
                }

                fn from_parts(_params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
                    Ok(Self::new())
                }
            }
        };
    }

    plain_call!(
        /// The server's queues: `GET /v1/matchmaking/queues` → [`Queues`].
        ListQueues,
        Get,
        routes::matchmaking::QUEUES,
        Queues
    );
    plain_call!(
        /// The caller's ticket: `GET /v1/matchmaking/ticket` → [`MatchTicket`] (404 without one).
        GetTicket,
        Get,
        routes::matchmaking::TICKET,
        MatchTicket
    );
    plain_call!(
        /// Take the caller's ticket out of its queue: `DELETE /v1/matchmaking/ticket` → [`Ack`] (also
        /// when there is none).
        CancelTicket,
        Delete,
        routes::matchmaking::TICKET,
        Ack
    );
}

pub use calls::{CancelTicket, GetTicket, ListQueues};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_json_and_rules() {
        for good in ["duel", "ranked.eu-2v2", "1v1", &"q".repeat(MAX_QUEUE_KEY_BYTES)] {
            assert!(is_valid_queue_key(good), "{good}");
        }
        for bad in ["", "Duel", "-x", "a b", "a/b", &"q".repeat(MAX_QUEUE_KEY_BYTES + 1)] {
            assert!(!is_valid_queue_key(bad), "{bad}");
        }
        assert!(CreateTicket::new("duel").validate().is_ok() && CreateTicket::new("Duel").validate().is_err());
        let create = CreateTicket::new("duel").with_attributes(serde_json::json!({"rating": 1520}));
        assert_eq!(serde_json::to_string(&create).ok().as_deref(), Some(r#"{"queue":"duel","attributes":{"rating":1520}}"#));
        let found = MatchFound::new(TicketId(9), "duel", vec![UserId(42), UserId(7)]).with_data(serde_json::json!({"lobby": 12}));
        assert_eq!(serde_json::to_string(&found).ok().as_deref(), Some(r#"{"ticket":9,"queue":"duel","players":[42,7],"data":{"lobby":12}}"#));
        let ticket = MatchTicket::new(TicketId(9), "duel", UnixMillis(1), UnixMillis(2));
        assert_eq!(serde_json::to_string(&ticket).ok().as_deref(), Some(r#"{"id":9,"queue":"duel","status":"waiting","created_at":1,"expires_at":2}"#));
        assert_eq!(ticket.with_match(found).status, TicketStatus::Matched);
        assert_eq!(serde_json::from_str::<TicketStatus>(r#""cancelled""#).ok(), Some(TicketStatus::Unknown));
        assert_eq!(serde_json::to_string(&TicketExpired::new(TicketId(9), "duel")).ok().as_deref(), Some(r#"{"ticket":9,"queue":"duel"}"#));
    }
}
