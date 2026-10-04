//! Leaderboards: boards the server configures, scores the players (or the game's server code)
//! submit, the top of a board, the caller's own rank and the ranks around it. Routes:
//! [`routes::leaderboards`](crate::routes::leaderboards).
//!
//! | Method | Path | Call → answer |
//! |---|---|---|
//! | GET | `/v1/leaderboards` | [`ListBoards`] → [`Boards`] |
//! | GET | `/v1/leaderboards/{board}` | [`GetLeaderboard`] (query [`TopQuery`]) → [`LeaderboardPage`] (best first) |
//! | POST | `/v1/leaderboards/{board}/scores` | [`PostScore`] (body [`SubmitScore`]) → [`ScoreAck`] |
//! | GET | `/v1/leaderboards/{board}/me` | [`GetMyRank`] (query [`RankQuery`]) → [`MyRank`] |
//! | GET | `/v1/leaderboards/{board}/around` | [`GetAroundMe`] (query [`AroundQuery`]) → [`LeaderboardPage`] |
//!
//! **A board** has a key (`"weekly-race"`), a [`ScoreMode`] (what a new score does to the stored
//! one: keep the best, keep the latest, add up), a [`ScoreOrder`] (higher or lower is better) and a
//! [`Period`] (all-time, or a daily / weekly reset at 00:00 UTC, weeks starting on Monday). Every
//! period is its own table of scores; the old ones stay readable (`at`, any time inside that
//! period) for as long as the server keeps them.
//!
//! **Ranks** are 1-based and unique: equal scores are ordered by who reached the score first,
//! then by the lower account id. They are computed when read.
//!
//! **Submitting** is the server's decision: it may refuse client submissions for a board (403)
//! and accept scores only from its own code, and its hooks may check or change a score (anti-cheat
//! is the game's). A score is any `i64` except `i64::MIN`; a sum saturates at `i64::MAX` /
//! `-i64::MAX`.
//!
//! **Metadata:** a score may carry a small JSON value (a character, a replay id), stored with the
//! score it belongs to and shown with it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ApiError, ValidationDetails};
use crate::ids::UserId;
use crate::page::Cursor;
use crate::time::UnixMillis;

/// The longest board key, in bytes.
pub const MAX_BOARD_KEY_BYTES: usize = 64;
/// The default largest score metadata, in bytes of its JSON (a server may configure another).
pub const DEFAULT_MAX_METADATA_BYTES: usize = 1024;
/// The default number of entries above and below the caller in [`AroundQuery`].
pub const DEFAULT_AROUND: u32 = 5;
/// The most entries above (and below) the caller one [`AroundQuery`] may ask for.
pub const MAX_AROUND: u32 = 50;

const DAY_MILLIS: i64 = 86_400_000;

/// Whether `key` is a valid board key: 1 to [`MAX_BOARD_KEY_BYTES`] bytes of ASCII lower-case
/// letters, digits, `_`, `-` and `.`, starting with a letter or digit (`"highscore"`,
/// `"race.track-2"`). Valid keys are safe in a URL path without escaping.
pub fn is_valid_board_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() <= MAX_BOARD_KEY_BYTES
        && bytes.first().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'))
}

/// What a new score does to the player's stored score on a board.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ScoreMode {
    /// Keep the better of the two (by the board's [`ScoreOrder`]).
    #[default]
    Best,
    /// The new score replaces the stored one.
    Latest,
    /// The new score is added to the stored one (saturating).
    Sum,
    /// A mode from a newer server this version does not know (never sent by a server).
    #[serde(other)]
    Unknown,
}

/// Which scores rank higher on a board.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ScoreOrder {
    /// Higher is better (points).
    #[default]
    Desc,
    /// Lower is better (race times, moves).
    Asc,
    /// An order from a newer server this version does not know (never sent by a server).
    #[serde(other)]
    Unknown,
}

/// When a board starts over. Resets happen at 00:00 UTC; a week starts on Monday.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Period {
    /// Never: one table of scores for all time.
    #[default]
    AllTime,
    /// Every day at 00:00 UTC.
    Daily,
    /// Every Monday at 00:00 UTC.
    Weekly,
    /// A period from a newer server this version does not know (never sent by a server).
    #[serde(other)]
    Unknown,
}

impl Period {
    /// The start of the period containing `at` (`None` for [`Period::AllTime`] and unknown periods).
    ///
    /// ```
    /// use net_backend_protocol::leaderboards::Period;
    /// use net_backend_protocol::UnixMillis;
    ///
    /// // 2026-10-02 (a Friday) 15:00 UTC.
    /// let at = UnixMillis(1_790_953_200_000);
    /// assert_eq!(Period::Daily.start_of(at), Some(UnixMillis(1_790_899_200_000))); // 2026-10-02 00:00
    /// assert_eq!(Period::Weekly.start_of(at), Some(UnixMillis(1_790_553_600_000))); // Monday 2026-09-28
    /// assert_eq!(Period::AllTime.start_of(at), None);
    /// ```
    pub fn start_of(self, at: UnixMillis) -> Option<UnixMillis> {
        let day = at.get().div_euclid(DAY_MILLIS);
        match self {
            Period::Daily => Some(UnixMillis(day.saturating_mul(DAY_MILLIS))),
            // Day 0 (1970-01-01) was a Thursday: day -3 was a Monday.
            Period::Weekly => Some(UnixMillis((day + 3).div_euclid(7).saturating_mul(7).saturating_sub(3).saturating_mul(DAY_MILLIS))),
            _ => None,
        }
    }

    /// The end of the period containing `at` (the next reset; `None` for [`Period::AllTime`]).
    pub fn end_of(self, at: UnixMillis) -> Option<UnixMillis> {
        let length = match self {
            Period::Daily => DAY_MILLIS,
            Period::Weekly => 7 * DAY_MILLIS,
            _ => return None,
        };
        self.start_of(at).map(|start| start.saturating_add_millis(length))
    }
}

/// A board, as the server describes it.
///
/// JSON: `{"key":"weekly-race","name":"Weekly race","mode":"best","order":"asc","period":"weekly","period_start":1790553600000,"period_end":1791158400000}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BoardInfo {
    /// The key.
    pub key: String,
    /// A display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What a new score does.
    pub mode: ScoreMode,
    /// Which scores rank higher.
    pub order: ScoreOrder,
    /// When the board starts over.
    pub period: Period,
    /// The start of the current period (absent for all-time boards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_start: Option<UnixMillis>,
    /// The next reset (absent for all-time boards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_end: Option<UnixMillis>,
    /// Whether players may submit scores themselves (`false`: only the game's server code does).
    #[serde(default = "yes")]
    pub client_submit: bool,
}

fn yes() -> bool {
    true
}

impl BoardInfo {
    /// A board.
    pub fn new(key: impl Into<String>, mode: ScoreMode, order: ScoreOrder, period: Period) -> Self {
        Self { key: key.into(), name: None, mode, order, period, period_start: None, period_end: None, client_submit: true }
    }

    /// The same board with a display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same board with the current period's bounds.
    pub fn with_period_bounds(mut self, start: Option<UnixMillis>, end: Option<UnixMillis>) -> Self {
        self.period_start = start;
        self.period_end = end;
        self
    }

    /// The same board with client submissions on or off.
    pub fn with_client_submit(mut self, allowed: bool) -> Self {
        self.client_submit = allowed;
        self
    }
}

/// The answer to [`ListBoards`]: every board of the server, by key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Boards {
    /// The boards, ordered by key.
    pub boards: Vec<BoardInfo>,
}

impl Boards {
    /// The answer.
    pub fn new(boards: Vec<BoardInfo>) -> Self {
        Self { boards }
    }
}

/// A score to submit: the body of `POST /v1/leaderboards/{board}/scores` ([`PostScore`]).
///
/// JSON: `{"score":1200}` (+ `"metadata":{…}` when set).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubmitScore {
    /// The score (any `i64` except `i64::MIN`).
    pub score: i64,
    /// A small JSON value stored with the score (at most the server's limit,
    /// [`DEFAULT_MAX_METADATA_BYTES`] by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl SubmitScore {
    /// A score without metadata.
    pub fn new(score: i64) -> Self {
        Self { score, metadata: None }
    }

    /// The same score with metadata.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// The shape rules: the score is not `i64::MIN`; the metadata's JSON is at most
    /// `max_metadata_bytes`.
    pub fn validate(&self, max_metadata_bytes: usize) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if self.score == i64::MIN {
            details.add("score", "must be greater than -9223372036854775808");
        }
        if let Some(metadata) = &self.metadata {
            if crate::storage::value_bytes(metadata) > max_metadata_bytes {
                details.add("metadata", format!("is larger than {max_metadata_bytes} bytes"));
            }
        }
        details.into_result()
    }
}

/// The answer to a submitted score.
///
/// JSON: `{"board":"highscore","score":1500,"submitted":1200,"changed":false,"rank":3}` (+
/// `"period_start"` for boards with resets).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScoreAck {
    /// The board.
    pub board: String,
    /// The period the score went into (absent for all-time boards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_start: Option<UnixMillis>,
    /// The player's stored score now (the best, the latest or the sum).
    pub score: i64,
    /// The score that was submitted (after the server's hooks).
    pub submitted: i64,
    /// Whether the stored score changed (`false`: a `best` board kept a better score).
    pub changed: bool,
    /// The player's rank now (1 = first).
    pub rank: u64,
}

impl ScoreAck {
    /// An acknowledgement.
    pub fn new(board: impl Into<String>, period_start: Option<UnixMillis>, score: i64, submitted: i64, changed: bool, rank: u64) -> Self {
        Self { board: board.into(), period_start, score, submitted, changed, rank }
    }
}

/// One row of a board.
///
/// JSON: `{"rank":1,"user":42,"name":"Ada","score":1500,"achieved_at":1790000000000}` (+
/// `"metadata"` when the score has some).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LeaderboardEntry {
    /// The rank (1 = first).
    pub rank: u64,
    /// The player.
    pub user: UserId,
    /// The player's display name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The stored score.
    pub score: i64,
    /// The metadata stored with the score, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// When the player reached this score (the tie-breaker: earlier ranks higher).
    pub achieved_at: UnixMillis,
}

impl LeaderboardEntry {
    /// An entry.
    pub fn new(rank: u64, user: UserId, score: i64, achieved_at: UnixMillis) -> Self {
        Self { rank, user, name: None, score, metadata: None, achieved_at }
    }

    /// The same entry with the player's name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same entry with metadata.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A page of a board, best first (also the answer around the caller).
///
/// JSON: `{"board":"highscore","items":[…],"next_cursor":"…"}` (+ `"period_start"` /
/// `"period_end"` for boards with resets).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LeaderboardPage {
    /// The board.
    pub board: String,
    /// The start of the period shown (absent for all-time boards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_start: Option<UnixMillis>,
    /// The end of the period shown (absent for all-time boards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_end: Option<UnixMillis>,
    /// The entries, best first.
    pub items: Vec<LeaderboardEntry>,
    /// The cursor of the next page (top pages only); absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

impl LeaderboardPage {
    /// A page.
    pub fn new(board: impl Into<String>, period_start: Option<UnixMillis>, period_end: Option<UnixMillis>, items: Vec<LeaderboardEntry>) -> Self {
        Self { board: board.into(), period_start, period_end, items, next_cursor: None }
    }

    /// The same page with the next page's cursor.
    pub fn with_next_cursor(mut self, cursor: Cursor) -> Self {
        self.next_cursor = Some(cursor);
        self
    }
}

/// The caller's own place on a board: the answer to [`GetMyRank`].
///
/// JSON: `{"board":"highscore","entry":{…},"total":812}` (`entry` absent when the caller has no
/// score in that period).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MyRank {
    /// The board.
    pub board: String,
    /// The start of the period (absent for all-time boards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_start: Option<UnixMillis>,
    /// The end of the period (absent for all-time boards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_end: Option<UnixMillis>,
    /// The caller's entry, if the caller has a score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<LeaderboardEntry>,
    /// How many players have a score in the period.
    pub total: u64,
}

impl MyRank {
    /// An answer.
    pub fn new(
        board: impl Into<String>,
        period_start: Option<UnixMillis>,
        period_end: Option<UnixMillis>,
        entry: Option<LeaderboardEntry>,
        total: u64,
    ) -> Self {
        Self { board: board.into(), period_start, period_end, entry, total }
    }
}

/// Which page of a board: the query of `GET /v1/leaderboards/{board}`
/// (`?cursor=…&limit=…&at=…`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TopQuery {
    /// Where to continue (`next_cursor` of the previous page); absent for the top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// At most this many entries (default 50, at most 100: [`PageRequest`](crate::PageRequest)'s limits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// A time inside the period to show (absent: the current period; ignored by all-time boards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<UnixMillis>,
}

impl TopQuery {
    /// The top of the current period.
    pub fn new() -> Self {
        Self::default()
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

    /// The same query for the period containing `at`.
    pub fn at(mut self, at: UnixMillis) -> Self {
        self.at = Some(at);
        self
    }

    /// The limit to apply (like [`PageRequest::limit_or_default`](crate::PageRequest::limit_or_default)).
    pub fn limit_or_default(&self) -> u32 {
        self.limit.map_or(crate::page::DEFAULT_PAGE_LIMIT, |limit| limit.clamp(1, crate::page::MAX_PAGE_LIMIT))
    }
}

/// Which period of the caller's rank: the query of `GET /v1/leaderboards/{board}/me` (`?at=…`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RankQuery {
    /// A time inside the period (absent: the current period).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<UnixMillis>,
}

impl RankQuery {
    /// The current period.
    pub fn new() -> Self {
        Self::default()
    }

    /// The period containing `at`.
    pub fn at(mut self, at: UnixMillis) -> Self {
        self.at = Some(at);
        self
    }
}

/// The ranks around the caller: the query of `GET /v1/leaderboards/{board}/around`
/// (`?above=5&below=5&at=…`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AroundQuery {
    /// Entries ranked above the caller (default [`DEFAULT_AROUND`], at most [`MAX_AROUND`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<u32>,
    /// Entries ranked below the caller (default [`DEFAULT_AROUND`], at most [`MAX_AROUND`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<u32>,
    /// A time inside the period (absent: the current period).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<UnixMillis>,
}

impl AroundQuery {
    /// [`DEFAULT_AROUND`] above and below, the current period.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same query with these counts.
    pub fn with_counts(mut self, above: u32, below: u32) -> Self {
        self.above = Some(above);
        self.below = Some(below);
        self
    }

    /// The same query for the period containing `at`.
    pub fn at(mut self, at: UnixMillis) -> Self {
        self.at = Some(at);
        self
    }

    /// The counts to apply: (above, below), each clamped to `0..=MAX_AROUND`.
    pub fn counts(&self) -> (u32, u32) {
        (self.above.unwrap_or(DEFAULT_AROUND).min(MAX_AROUND), self.below.unwrap_or(DEFAULT_AROUND).min(MAX_AROUND))
    }
}

// ---- typed HTTP calls (see `http_call`) ---------------------------------------------------------

/// The typed HTTP calls of this module (in their own scope: their imports stay out of the
/// module's doc-link scope).
mod calls {
    use super::*;

    use crate::http_call::{HttpCall, NoPayload, PathParams, PayloadKind, NO_PAYLOAD};
    use crate::routes::{self, HttpMethod, Route};

    const NOT_A_KEY: &str = "is not a valid board key";

    fn board(params: &PathParams) -> Result<String, ApiError> {
        params.checked("board", is_valid_board_key, NOT_A_KEY)
    }

    /// Every board: `GET /v1/leaderboards` → [`Boards`].
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ListBoards {}

    impl ListBoards {
        /// The call.
        pub fn new() -> Self {
            Self {}
        }
    }

    impl HttpCall for ListBoards {
        type Payload = NoPayload;
        type Response = Boards;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::leaderboards::BOARDS, true);
        const PAYLOAD: PayloadKind = PayloadKind::Empty;

        fn payload(&self) -> &NoPayload {
            &NO_PAYLOAD
        }

        fn from_parts(_params: &PathParams, _payload: NoPayload) -> Result<Self, ApiError> {
            Ok(Self::new())
        }
    }

    /// A page of a board, best first: `GET /v1/leaderboards/{board}?cursor=…&limit=…&at=…` →
    /// [`LeaderboardPage`].
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct GetLeaderboard {
        /// The board.
        pub board: String,
        /// Which page and period.
        pub query: TopQuery,
    }

    impl GetLeaderboard {
        /// The top of `board`'s current period.
        pub fn new(board: impl Into<String>) -> Self {
            Self { board: board.into(), query: TopQuery::new() }
        }

        /// The same call with this query.
        pub fn with_query(mut self, query: TopQuery) -> Self {
            self.query = query;
            self
        }
    }

    impl HttpCall for GetLeaderboard {
        type Payload = TopQuery;
        type Response = LeaderboardPage;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::leaderboards::BOARD, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &TopQuery {
            &self.query
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("board", &self.board)
        }

        fn from_parts(params: &PathParams, query: TopQuery) -> Result<Self, ApiError> {
            Ok(Self::new(board(params)?).with_query(query))
        }
    }

    /// Submit a score for the caller: `POST /v1/leaderboards/{board}/scores` with a
    /// [`SubmitScore`] → [`ScoreAck`].
    #[derive(Clone, Debug, PartialEq)]
    #[non_exhaustive]
    pub struct PostScore {
        /// The board.
        pub board: String,
        /// The score.
        pub submit: SubmitScore,
    }

    impl PostScore {
        /// Submit `submit` to `board`.
        pub fn new(board: impl Into<String>, submit: SubmitScore) -> Self {
            Self { board: board.into(), submit }
        }
    }

    impl HttpCall for PostScore {
        type Payload = SubmitScore;
        type Response = ScoreAck;
        const ROUTE: Route = Route::new(HttpMethod::Post, routes::leaderboards::SCORES, true);
        const PAYLOAD: PayloadKind = PayloadKind::Json;

        fn payload(&self) -> &SubmitScore {
            &self.submit
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("board", &self.board)
        }

        fn from_parts(params: &PathParams, submit: SubmitScore) -> Result<Self, ApiError> {
            Ok(Self::new(board(params)?, submit))
        }
    }

    /// The caller's rank: `GET /v1/leaderboards/{board}/me?at=…` → [`MyRank`].
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct GetMyRank {
        /// The board.
        pub board: String,
        /// Which period.
        pub query: RankQuery,
    }

    impl GetMyRank {
        /// The caller's rank in `board`'s current period.
        pub fn new(board: impl Into<String>) -> Self {
            Self { board: board.into(), query: RankQuery::new() }
        }

        /// The same call with this query.
        pub fn with_query(mut self, query: RankQuery) -> Self {
            self.query = query;
            self
        }
    }

    impl HttpCall for GetMyRank {
        type Payload = RankQuery;
        type Response = MyRank;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::leaderboards::ME, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &RankQuery {
            &self.query
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("board", &self.board)
        }

        fn from_parts(params: &PathParams, query: RankQuery) -> Result<Self, ApiError> {
            Ok(Self::new(board(params)?).with_query(query))
        }
    }

    /// The entries around the caller (the caller included, best first):
    /// `GET /v1/leaderboards/{board}/around?above=…&below=…&at=…` → [`LeaderboardPage`] (empty
    /// when the caller has no score in the period).
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct GetAroundMe {
        /// The board.
        pub board: String,
        /// How many and which period.
        pub query: AroundQuery,
    }

    impl GetAroundMe {
        /// [`DEFAULT_AROUND`] above and below the caller in `board`'s current period.
        pub fn new(board: impl Into<String>) -> Self {
            Self { board: board.into(), query: AroundQuery::new() }
        }

        /// The same call with this query.
        pub fn with_query(mut self, query: AroundQuery) -> Self {
            self.query = query;
            self
        }
    }

    impl HttpCall for GetAroundMe {
        type Payload = AroundQuery;
        type Response = LeaderboardPage;
        const ROUTE: Route = Route::new(HttpMethod::Get, routes::leaderboards::AROUND, true);
        const PAYLOAD: PayloadKind = PayloadKind::Query;

        fn payload(&self) -> &AroundQuery {
            &self.query
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("board", &self.board)
        }

        fn from_parts(params: &PathParams, query: AroundQuery) -> Result<Self, ApiError> {
            Ok(Self::new(board(params)?).with_query(query))
        }
    }
}

pub use calls::{GetAroundMe, GetLeaderboard, GetMyRank, ListBoards, PostScore};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        for good in ["highscore", "race.track-2", "0", &"k".repeat(MAX_BOARD_KEY_BYTES)] {
            assert!(is_valid_board_key(good), "{good}");
        }
        for bad in ["", "High", "-x", "a b", "a/b", &"k".repeat(MAX_BOARD_KEY_BYTES + 1), "é"] {
            assert!(!is_valid_board_key(bad), "{bad}");
        }
    }

    #[test]
    fn periods() {
        const DAY: i64 = DAY_MILLIS;
        // 1970-01-05 was a Monday (day 4).
        assert_eq!(Period::Weekly.start_of(UnixMillis(4 * DAY)), Some(UnixMillis(4 * DAY)));
        assert_eq!(Period::Weekly.start_of(UnixMillis(11 * DAY - 1)), Some(UnixMillis(4 * DAY)));
        assert_eq!(Period::Weekly.start_of(UnixMillis(11 * DAY)), Some(UnixMillis(11 * DAY)));
        assert_eq!(Period::Weekly.start_of(UnixMillis(0)), Some(UnixMillis(-3 * DAY)));
        assert_eq!(Period::Weekly.start_of(UnixMillis(-1)), Some(UnixMillis(-3 * DAY)));
        assert_eq!(Period::Daily.start_of(UnixMillis(DAY + 5)), Some(UnixMillis(DAY)));
        assert_eq!(Period::Daily.start_of(UnixMillis(-1)), Some(UnixMillis(-DAY)));
        assert_eq!(Period::Daily.end_of(UnixMillis(DAY + 5)), Some(UnixMillis(2 * DAY)));
        assert_eq!(Period::Weekly.end_of(UnixMillis(4 * DAY)), Some(UnixMillis(11 * DAY)));
        assert_eq!((Period::AllTime.start_of(UnixMillis(9)), Period::AllTime.end_of(UnixMillis(9))), (None, None));
        assert!(Period::Daily.start_of(UnixMillis(i64::MIN)).is_some() && Period::Weekly.end_of(UnixMillis(i64::MAX)).is_some());
        assert_eq!(serde_json::from_str::<Period>("\"monthly\"").ok(), Some(Period::Unknown));
        assert_eq!(serde_json::to_string(&Period::AllTime).ok().as_deref(), Some("\"all_time\""));
    }

    #[test]
    fn json_and_rules() {
        assert_eq!(serde_json::to_string(&SubmitScore::new(1200)).ok().as_deref(), Some(r#"{"score":1200}"#));
        assert!(SubmitScore::new(i64::MIN).validate(DEFAULT_MAX_METADATA_BYTES).is_err());
        assert!(SubmitScore::new(-i64::MAX).validate(DEFAULT_MAX_METADATA_BYTES).is_ok());
        assert!(SubmitScore::new(1).with_metadata(serde_json::json!("x".repeat(2000))).validate(DEFAULT_MAX_METADATA_BYTES).is_err());
        let info = BoardInfo::new("race", ScoreMode::Best, ScoreOrder::Asc, Period::Weekly);
        let json = serde_json::to_string(&info).unwrap_or_default();
        assert_eq!(json, r#"{"key":"race","mode":"best","order":"asc","period":"weekly","client_submit":true}"#);
        let old: Option<BoardInfo> = serde_json::from_str(r#"{"key":"race","mode":"best","order":"asc","period":"weekly"}"#).ok();
        assert_eq!(old.map(|b| b.client_submit), Some(true));
        assert_eq!(serde_json::from_str::<ScoreMode>("\"median\"").ok(), Some(ScoreMode::Unknown));
        assert_eq!(AroundQuery::new().counts(), (DEFAULT_AROUND, DEFAULT_AROUND));
        assert_eq!(AroundQuery::new().with_counts(0, 500).counts(), (0, MAX_AROUND));
        assert_eq!(TopQuery::new().with_limit(1000).limit_or_default(), 100);
    }
}
