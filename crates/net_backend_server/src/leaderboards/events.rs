//! What games can hook into in the leaderboards module ([`crate::hooks`]).
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeScoreSubmit`] | before | a score is about to be stored (a player's submission or server code's); check it against the game's rules, change `score` / `metadata`, or refuse |
//! | [`AfterScoreSubmit`] | after | the score is stored (the stored score, whether it changed, the rank) |
//!
//! Anti-cheat is the game's: a [`BeforeScoreSubmit`] hook can compare a score with what the server
//! knows (a match result, a plausible maximum) and refuse it; a board with `client_submit = false`
//! accepts scores only from server code.
//!
//! ```
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::leaderboards::events::{BeforeScoreSubmit, Submitter};
//! use net_backend_server::{AppError, Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<BeforeScoreSubmit, _, _>(|_ctx, submit| async move {
//!     // No level can give more than 100 000 points.
//!     if submit.board == "highscore" && submit.submitter == Submitter::Player && submit.score > 100_000 {
//!         return Ok(Decision::Reject(AppError::forbidden("that score is not possible")));
//!     }
//!     Ok(Decision::Continue(submit))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::{UnixMillis, UserId};
use serde_json::Value;

use crate::hooks::Event;

/// Who submits a score.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Submitter {
    /// The player, through `POST /v1/leaderboards/{board}/scores`.
    Player,
    /// Server code, through [`LeaderboardService::submit`](super::LeaderboardService::submit).
    Server,
}

/// A score is about to be stored. Hooks may change `score` and `metadata` (checked again
/// afterwards) or refuse; the other fields are for reading (changes to them are ignored).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeScoreSubmit {
    /// The player.
    pub user_id: UserId,
    /// The board.
    pub board: String,
    /// The submitted score.
    pub score: i64,
    /// The submitted metadata.
    pub metadata: Option<Value>,
    /// Who submits.
    pub submitter: Submitter,
}

impl Event for BeforeScoreSubmit {
    const NAME: &'static str = "leaderboards.before_submit";
}

/// A score is stored.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterScoreSubmit {
    /// The player.
    pub user_id: UserId,
    /// The board.
    pub board: String,
    /// The period the score went into (`None` for all-time boards).
    pub period_start: Option<UnixMillis>,
    /// The submitted score (after the before hooks).
    pub submitted: i64,
    /// The player's stored score now.
    pub score: i64,
    /// Whether the stored score changed.
    pub changed: bool,
    /// The player's rank right after the submission.
    pub rank: u64,
    /// Who submitted.
    pub submitter: Submitter,
}

impl Event for AfterScoreSubmit {
    const NAME: &'static str = "leaderboards.after_submit";
}
