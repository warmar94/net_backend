//! [`LeaderboardService`]: submitting scores and reading boards, for the routes and for server code.
//!
//! **Writes:** a submission takes the player's account lock (the `auth_users` row: MySQL
//! `FOR UPDATE`, PostgreSQL `FOR NO KEY UPDATE`, SQLite's write transaction), reads the stored
//! score with a plain read, then runs exactly one UPDATE or one INSERT: the submissions of ONE
//! player run one at a time (a `best` or `sum` board never loses a submission), players never wait
//! for each other, and no statement touches a row that is not there (no MySQL gap locks). A
//! transaction the database still aborts as a deadlock is run again ([`Retry`]).
//!
//! **Reads** need no lock: a page, a rank or the ranks around a player are plain reads; a rank is
//! 1 + the number of rows before the player's (one indexed count).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use net_backend_protocol::leaderboards::{
    AroundQuery, BoardInfo, Boards, LeaderboardEntry, LeaderboardPage, MyRank, Period, ScoreAck, ScoreMode, ScoreOrder, SubmitScore, TopQuery,
};
use net_backend_protocol::{Cursor, UnixMillis, UserId};
use serde_json::Value;

use super::config::{BoardSpec, LeaderboardsConfig};
use super::events::{AfterScoreSubmit, BeforeScoreSubmit, Submitter};
use super::store::{self, CountRow, IdRow, NameRow, Position, ScoreRow};
use crate::db::Retry;
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::rate_limit::{KeyedBuckets, RateDecision};
use crate::state::AppState;

/// How many rows the purge deletes per statement.
const PURGE_BATCH: u64 = 1000;

/// Submits scores and reads boards. A state value (`Ext<LeaderboardService>` in handlers,
/// `state.get::<LeaderboardService>()` elsewhere) once the [`Leaderboards`](super::Leaderboards)
/// module is registered. Server code submits for ANY player through it
/// ([`submit`](Self::submit): [`Submitter::Server`], also on boards without client submissions,
/// never rate-limited; the hooks run as for players) and reads any player's rank.
#[derive(Clone)]
pub struct LeaderboardService(Arc<Inner>);

struct Inner {
    config: LeaderboardsConfig,
    /// Client submissions per user (`submit_rate`; `None` = no limit).
    rate: Option<KeyedBuckets<UserId>>,
}

impl std::fmt::Debug for LeaderboardService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LeaderboardService").field(&self.0.config).finish()
    }
}

/// The period a board shows at `at`: its stored `period_start` (0 for all-time) and the bounds
/// clients see.
#[derive(Clone, Copy, Debug)]
struct PeriodOf {
    key: i64,
    start: Option<UnixMillis>,
    end: Option<UnixMillis>,
}

fn period_of(spec: &BoardSpec, at: UnixMillis) -> PeriodOf {
    let start = spec.period.start_of(at);
    PeriodOf { key: start.map_or(0, UnixMillis::get), start, end: spec.period.end_of(at) }
}

/// The rank key of a score: the score on `asc` boards, its negation on `desc` boards (scores are
/// never `i64::MIN`, so the negation always exists).
fn rank_key(order: ScoreOrder, score: i64) -> i64 {
    match order {
        ScoreOrder::Asc => score,
        _ => score.saturating_neg(),
    }
}

/// Whether `new` beats `old` on a board with this order.
fn beats(order: ScoreOrder, new: i64, old: i64) -> bool {
    match order {
        ScoreOrder::Asc => new < old,
        _ => new > old,
    }
}

/// `a + b`, kept within `-i64::MAX ..= i64::MAX`.
fn add(a: i64, b: i64) -> i64 {
    a.saturating_add(b).max(-i64::MAX)
}

fn cursor_text(p: Position) -> Cursor {
    Cursor::new(format!("{}.{}.{}", p.rank_key, p.achieved_at, p.user))
}

fn parse_cursor(cursor: &Cursor) -> Result<Position, AppError> {
    let bad = || AppError::bad_request("the cursor is not valid");
    let mut parts = cursor.as_str().split('.');
    let mut next = || parts.next().and_then(|p| p.parse::<i64>().ok()).ok_or_else(bad);
    let position = Position { rank_key: next()?, achieved_at: next()?, user: next()? };
    if parts.next().is_some() {
        return Err(bad());
    }
    Ok(position)
}

fn decode_metadata(bytes: Option<Vec<u8>>) -> Option<Value> {
    bytes.and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(|v| !v.is_null())
}

fn no_board() -> AppError {
    AppError::not_found("no such leaderboard")
}

/// The outcome of a submission inside its transaction.
struct Stored {
    score: i64,
    changed: bool,
    position: Position,
}

impl LeaderboardService {
    pub(crate) fn new(config: LeaderboardsConfig) -> Self {
        let rate =
            (config.submit_rate > 0).then(|| KeyedBuckets::new(config.submit_rate, Duration::from_secs(u64::from(config.submit_rate_window_secs)), 100_000));
        Self(Arc::new(Inner { config, rate }))
    }

    /// The settings.
    pub fn config(&self) -> &LeaderboardsConfig {
        &self.0.config
    }

    fn spec(&self, board: &str) -> Result<&BoardSpec, AppError> {
        self.0.config.board(board).ok_or_else(no_board)
    }

    fn now_or(state: &AppState, at: Option<UnixMillis>) -> UnixMillis {
        at.unwrap_or_else(|| state.now())
    }

    /// Every board with its current period, ordered by key.
    pub fn boards(&self, state: &AppState) -> Boards {
        let now = state.now();
        let mut boards: Vec<BoardInfo> = self
            .0
            .config
            .boards
            .iter()
            .map(|spec| {
                let mut info = BoardInfo::new(spec.key.clone(), spec.mode, spec.order, spec.period)
                    .with_period_bounds(spec.period.start_of(now), spec.period.end_of(now))
                    .with_client_submit(spec.client_submit);
                if let Some(name) = &spec.name {
                    info = info.with_name(name.clone());
                }
                info
            })
            .collect();
        boards.sort_by(|a, b| a.key.cmp(&b.key));
        Boards::new(boards)
    }

    // ---- reads ----------------------------------------------------------------------------------

    /// The display names of these players.
    async fn names(state: &AppState, users: &[i64]) -> Result<HashMap<i64, String>, AppError> {
        if users.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = state.db().fetch_all::<NameRow, _>(&store::names(users)).await?;
        Ok(rows.into_iter().filter_map(|r| r.display_name.map(|name| (r.id, name))).collect())
    }

    /// Entries for rows in ranking order, the first at `first_rank`.
    async fn entries(state: &AppState, rows: Vec<ScoreRow>, first_rank: u64) -> Result<Vec<LeaderboardEntry>, AppError> {
        let ids: Vec<i64> = rows.iter().map(|r| r.user_id).collect();
        let names = Self::names(state, &ids).await?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                let mut entry = LeaderboardEntry::new(first_rank.saturating_add(i as u64), UserId(row.user_id), row.score, UnixMillis(row.achieved_at));
                if let Some(name) = names.get(&row.user_id) {
                    entry = entry.with_name(name.clone());
                }
                if let Some(metadata) = decode_metadata(row.metadata) {
                    entry = entry.with_metadata(metadata);
                }
                entry
            })
            .collect())
    }

    async fn rank_of(state: &AppState, board: &str, period: i64, position: Position) -> Result<u64, AppError> {
        let before = state.db().fetch_one::<CountRow, _>(&store::count_before(board, period, position)).await?;
        Ok(u64::try_from(before.n).unwrap_or(0).saturating_add(1))
    }

    /// A page of `board`, best first (`query.at`: a time inside the period; absent: now).
    pub async fn top(&self, state: &AppState, board: &str, query: &TopQuery) -> Result<LeaderboardPage, AppError> {
        let spec = self.spec(board)?;
        let period = period_of(spec, Self::now_or(state, query.at));
        let cursor = query.cursor.as_ref().map(parse_cursor).transpose()?;
        let limit = u64::from(query.limit_or_default());
        let mut rows = state.db().fetch_all::<ScoreRow, _>(&store::page(board, period.key, cursor, limit + 1)).await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more { rows.last().map(|r| cursor_text(r.position())) } else { None };
        let first_rank = match rows.first() {
            Some(row) => Self::rank_of(state, board, period.key, row.position()).await?,
            None => 1,
        };
        let mut page = LeaderboardPage::new(board, period.start, period.end, Self::entries(state, rows, first_rank).await?);
        if let Some(next) = next {
            page = page.with_next_cursor(next);
        }
        Ok(page)
    }

    /// `user`'s entry on `board` in the period containing `at` (absent: now), with the number of
    /// players in that period.
    pub async fn rank(&self, state: &AppState, user: UserId, board: &str, at: Option<UnixMillis>) -> Result<MyRank, AppError> {
        let spec = self.spec(board)?;
        let period = period_of(spec, Self::now_or(state, at));
        let total = state.db().fetch_one::<CountRow, _>(&store::count_all(board, period.key)).await?;
        let total = u64::try_from(total.n).unwrap_or(0);
        let entry = match state.db().fetch_optional::<ScoreRow, _>(&store::score(board, period.key, user.get())).await? {
            Some(row) => {
                let rank = Self::rank_of(state, board, period.key, row.position()).await?;
                Self::entries(state, vec![row], rank).await?.pop()
            }
            None => None,
        };
        Ok(MyRank::new(board, period.start, period.end, entry, total))
    }

    /// The entries around `user` on `board` (the player included, best first); empty when the
    /// player has no score in the period.
    pub async fn around(&self, state: &AppState, user: UserId, board: &str, query: &AroundQuery) -> Result<LeaderboardPage, AppError> {
        let spec = self.spec(board)?;
        let period = period_of(spec, Self::now_or(state, query.at));
        let Some(me) = state.db().fetch_optional::<ScoreRow, _>(&store::score(board, period.key, user.get())).await? else {
            return Ok(LeaderboardPage::new(board, period.start, period.end, Vec::new()));
        };
        let (above, below) = query.counts();
        let position = me.position();
        let my_rank = Self::rank_of(state, board, period.key, position).await?;
        let mut rows =
            if above > 0 { state.db().fetch_all::<ScoreRow, _>(&store::above(board, period.key, position, u64::from(above))).await? } else { Vec::new() };
        rows.reverse();
        let first_rank = my_rank.saturating_sub(rows.len() as u64).max(1);
        rows.push(me);
        if below > 0 {
            rows.extend(state.db().fetch_all::<ScoreRow, _>(&store::page(board, period.key, Some(position), u64::from(below))).await?);
        }
        Ok(LeaderboardPage::new(board, period.start, period.end, Self::entries(state, rows, first_rank).await?))
    }

    // ---- writes ---------------------------------------------------------------------------------

    /// Count one client submission of `user` against `submit_rate` (429 `rate_limited` over it).
    pub(crate) fn check_rate(&self, user: UserId) -> Result<(), AppError> {
        match self.0.rate.as_ref().map(|rate| rate.check(user)) {
            Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
            _ => Ok(()),
        }
    }

    /// Submit a score for `user` as the server: also on boards without client submissions, never
    /// rate-limited; the hooks run with [`Submitter::Server`].
    pub async fn submit(&self, state: &AppState, user: UserId, board: &str, submit: SubmitScore) -> Result<ScoreAck, AppError> {
        self.submit_as(state, &HookCtx::new(state.clone(), None), user, board, submit, Submitter::Server).await
    }

    /// Delete `user`'s score on `board` in the period containing `at` (absent: now), e.g. after a
    /// cheat was found. `true` if there was one.
    pub async fn remove(&self, state: &AppState, user: UserId, board: &str, at: Option<UnixMillis>) -> Result<bool, AppError> {
        let spec = self.spec(board)?;
        let period = period_of(spec, Self::now_or(state, at));
        Ok(state.db().execute(&store::delete(board, period.key, user.get())).await? > 0)
    }

    /// A submission: the board's rules, the hooks, one transaction, the rank, the after hooks.
    pub(crate) async fn submit_as(
        &self,
        state: &AppState,
        ctx: &HookCtx,
        user: UserId,
        board: &str,
        submit: SubmitScore,
        submitter: Submitter,
    ) -> Result<ScoreAck, AppError> {
        let spec = self.spec(board)?.clone();
        if submitter == Submitter::Player && !spec.client_submit {
            return Err(AppError::forbidden("scores on this leaderboard are submitted by the server only"));
        }
        let max = self.0.config.max_metadata_bytes;
        submit.validate(max)?;
        let event = BeforeScoreSubmit { user_id: user, board: board.to_string(), score: submit.score, metadata: submit.metadata, submitter };
        let event = state.hooks().run_before(ctx, event).await?;
        let submit = match event.metadata {
            Some(metadata) => SubmitScore::new(event.score).with_metadata(metadata),
            None => SubmitScore::new(event.score),
        };
        submit.validate(max)?;
        let metadata = match &submit.metadata {
            Some(value) if !value.is_null() => Some(serde_json::to_vec(value).map_err(AppError::internal)?),
            _ => None,
        };
        let now = state.now().get();
        let period = period_of(&spec, UnixMillis(now));
        let mut retry = Retry::new(Retry::DEFAULT_ATTEMPTS);
        let stored = loop {
            let mut tx = state.db().begin_write().await?;
            let result = async {
                let dialect = tx.dialect();
                if tx.fetch_optional::<IdRow, _>(&store::lock_user(user.get(), dialect)).await?.is_none() {
                    return Err(AppError::not_found("no such account"));
                }
                let current = tx.fetch_optional::<ScoreRow, _>(&store::score(board, period.key, user.get())).await?;
                let write = |score: i64, achieved_at: i64, metadata: Option<Vec<u8>>| store::Write {
                    board,
                    period: period.key,
                    user: user.get(),
                    score,
                    rank_key: rank_key(spec.order, score),
                    metadata,
                    achieved_at,
                    now,
                };
                let stored = match current {
                    None => {
                        tx.execute(&store::insert(write(submit.score, now, metadata.clone()))?).await?;
                        Stored {
                            score: submit.score,
                            changed: true,
                            position: Position { rank_key: rank_key(spec.order, submit.score), achieved_at: now, user: user.get() },
                        }
                    }
                    Some(old) => {
                        let new = match spec.mode {
                            ScoreMode::Best if !beats(spec.order, submit.score, old.score) => None,
                            ScoreMode::Sum => Some(add(old.score, submit.score)),
                            _ => Some(submit.score),
                        };
                        match new {
                            None => {
                                tx.execute(&store::touch(board, period.key, user.get(), now)).await?;
                                Stored { score: old.score, changed: false, position: old.position() }
                            }
                            Some(score) => {
                                // Reaching the same score again keeps the time it was first reached.
                                let achieved_at = if score == old.score { old.achieved_at } else { now };
                                tx.execute(&store::update(write(score, achieved_at, metadata.clone()))).await?;
                                Stored {
                                    score,
                                    changed: score != old.score,
                                    position: Position { rank_key: rank_key(spec.order, score), achieved_at, user: user.get() },
                                }
                            }
                        }
                    }
                };
                Ok(stored)
            }
            .await;
            match tx.finish(result).await {
                Err(error) if retry.again(&error).await => continue,
                other => break other?,
            }
        };
        let rank = Self::rank_of(state, board, period.key, stored.position).await?;
        let after = AfterScoreSubmit {
            user_id: user,
            board: board.to_string(),
            period_start: period.start,
            submitted: submit.score,
            score: stored.score,
            changed: stored.changed,
            rank,
            submitter,
        };
        state.hooks().run_after(ctx, Arc::new(after)).await;
        Ok(ScoreAck::new(board, period.start, stored.score, submit.score, stored.changed, rank))
    }

    // ---- retention ------------------------------------------------------------------------------

    /// Delete the periods of daily / weekly boards older than `keep_periods` finished periods
    /// (in batches); how many rows were deleted. Nothing with `keep_periods = 0`.
    pub async fn purge(&self, state: &AppState) -> Result<u64, AppError> {
        let keep = i64::from(self.0.config.keep_periods);
        if keep == 0 {
            return Ok(0);
        }
        let now = state.now();
        let mut deleted = 0u64;
        for spec in &self.0.config.boards {
            let length = match spec.period {
                Period::Daily => 86_400_000i64,
                Period::Weekly => 7 * 86_400_000i64,
                _ => continue,
            };
            let Some(current) = spec.period.start_of(now) else { continue };
            // The current period plus `keep` finished ones stay.
            let cutoff = current.get().saturating_sub(keep.saturating_mul(length));
            loop {
                let ids: Vec<i64> =
                    state.db().fetch_all::<IdRow, _>(&store::expired(&spec.key, cutoff, PURGE_BATCH)).await?.into_iter().map(|r| r.id).collect();
                if ids.is_empty() {
                    break;
                }
                deleted = deleted.saturating_add(state.db().execute(&store::delete_ids(&ids)).await?);
                if (ids.len() as u64) < PURGE_BATCH {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking_rules() {
        assert_eq!(rank_key(ScoreOrder::Desc, 10), -10);
        assert_eq!(rank_key(ScoreOrder::Asc, 10), 10);
        assert_eq!(rank_key(ScoreOrder::Desc, -i64::MAX), i64::MAX);
        assert!(beats(ScoreOrder::Desc, 5, 4) && !beats(ScoreOrder::Desc, 4, 4) && beats(ScoreOrder::Asc, 3, 4));
        assert_eq!(add(i64::MAX, 5), i64::MAX);
        assert_eq!(add(-i64::MAX, -5), -i64::MAX);
        let p = Position { rank_key: -12, achieved_at: 1_790_000_000_000, user: 42 };
        assert_eq!(parse_cursor(&cursor_text(p)).ok(), Some(p));
        for bad in ["", "1.2", "1.2.3.4", "a.b.c", "1..3"] {
            assert!(parse_cursor(&Cursor::new(bad)).is_err(), "{bad}");
        }
        assert_eq!(decode_metadata(Some(b"null".to_vec())), None);
        assert_eq!(decode_metadata(Some(br#"{"car":"red"}"#.to_vec())), Some(serde_json::json!({"car": "red"})));
    }
}
