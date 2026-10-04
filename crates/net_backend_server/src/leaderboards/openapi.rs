//! OpenAPI schemas of the protocol's leaderboard types (mirror structs: the protocol crate has no
//! OpenAPI dependency). A test serializes the real types and compares the field names with these
//! schemas, so the document cannot drift from the wire format.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::ToSchema;

/// A board.
#[derive(Serialize, ToSchema)]
pub(crate) struct BoardInfo {
    /// The key.
    key: String,
    /// A display name.
    name: Option<String>,
    /// What a new score does: `best`, `latest` or `sum`.
    mode: String,
    /// Which scores rank higher: `desc` (higher is better) or `asc` (lower is better).
    order: String,
    /// When the board starts over: `all_time`, `daily` or `weekly` (00:00 UTC; weeks start on Monday).
    period: String,
    /// The start of the current period (unix ms; absent for all-time boards).
    period_start: Option<i64>,
    /// The next reset (unix ms; absent for all-time boards).
    period_end: Option<i64>,
    /// Whether players may submit scores themselves.
    client_submit: bool,
}

/// Every board, by key.
#[derive(Serialize, ToSchema)]
pub(crate) struct Boards {
    /// The boards.
    boards: Vec<BoardInfo>,
}

/// `POST /v1/leaderboards/{board}/scores` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct SubmitScore {
    /// The score (any 64-bit integer except the smallest).
    score: i64,
    /// A small JSON value stored with the score (at most `max_metadata_bytes`).
    #[schema(value_type = Option<Value>)]
    metadata: Option<serde_json::Value>,
}

/// The answer to a submitted score.
#[derive(Serialize, ToSchema)]
pub(crate) struct ScoreAck {
    /// The board.
    board: String,
    /// The period the score went into (unix ms; absent for all-time boards).
    period_start: Option<i64>,
    /// The player's stored score now.
    score: i64,
    /// The score that was submitted (after the server's hooks).
    submitted: i64,
    /// Whether the stored score changed.
    changed: bool,
    /// The player's rank now (1 = first).
    rank: u64,
}

/// One row of a board.
#[derive(Serialize, ToSchema)]
pub(crate) struct LeaderboardEntry {
    /// The rank (1 = first; equal scores: who reached it first).
    rank: u64,
    /// The player's user id.
    user: i64,
    /// The player's display name.
    name: Option<String>,
    /// The stored score.
    score: i64,
    /// The metadata stored with the score.
    #[schema(value_type = Option<Value>)]
    metadata: Option<serde_json::Value>,
    /// When the player reached this score (unix ms).
    achieved_at: i64,
}

/// A page of a board, best first.
#[derive(Serialize, ToSchema)]
pub(crate) struct LeaderboardPage {
    /// The board.
    board: String,
    /// The start of the period shown (unix ms; absent for all-time boards).
    period_start: Option<i64>,
    /// The end of the period shown (unix ms; absent for all-time boards).
    period_end: Option<i64>,
    /// The entries, best first.
    items: Vec<LeaderboardEntry>,
    /// Pass as `cursor` for the next page; absent on the last page (and around the caller).
    next_cursor: Option<String>,
}

/// The caller's place on a board.
#[derive(Serialize, ToSchema)]
pub(crate) struct MyRank {
    /// The board.
    board: String,
    /// The start of the period (unix ms; absent for all-time boards).
    period_start: Option<i64>,
    /// The end of the period (unix ms; absent for all-time boards).
    period_end: Option<i64>,
    /// The caller's entry (absent: no score in this period).
    entry: Option<LeaderboardEntry>,
    /// How many players have a score in the period.
    total: u64,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::leaderboards as p;
    use net_backend_protocol::{Cursor, UnixMillis, UserId};
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
        let t = Some(UnixMillis(1));
        let info = p::BoardInfo::new("hs", p::ScoreMode::Best, p::ScoreOrder::Desc, p::Period::Daily).with_name("High").with_period_bounds(t, t);
        assert_eq!(properties::<BoardInfo>(), keys(&info));
        assert_eq!(properties::<Boards>(), keys(p::Boards::new(vec![info])));
        assert_eq!(properties::<SubmitScore>(), keys(p::SubmitScore::new(1).with_metadata(json!({}))));
        assert_eq!(properties::<ScoreAck>(), keys(p::ScoreAck::new("hs", t, 1, 1, true, 1)));
        let entry = p::LeaderboardEntry::new(1, UserId(1), 5, UnixMillis(1)).with_name("Ada").with_metadata(json!(1));
        assert_eq!(properties::<LeaderboardEntry>(), keys(&entry));
        assert_eq!(properties::<LeaderboardPage>(), keys(p::LeaderboardPage::new("hs", t, t, vec![entry.clone()]).with_next_cursor(Cursor::new("c"))));
        assert_eq!(properties::<MyRank>(), keys(p::MyRank::new("hs", t, t, Some(entry), 1)));
    }
}
