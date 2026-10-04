//! Leaderboards: boards with a score mode, an order and a period; score submission with hooks; the
//! top, the caller's rank and the ranks around it: the protocol's `/v1/leaderboards` routes (cargo
//! feature `leaderboards`, module [`Leaderboards`]).
//!
//! - **Boards** come from the settings ([`BoardSpec`]: key, name, [`ScoreMode`] `best` / `latest` /
//!   `sum`, [`ScoreOrder`] `desc` / `asc`, [`Period`] `all_time` / `daily` / `weekly` with resets at
//!   00:00 UTC, weeks starting on Monday, and whether players may submit themselves).
//! - **Routes** (all need a Bearer token): `GET /v1/leaderboards` (the boards and their current
//!   periods), `GET /v1/leaderboards/{board}` (a page, best first, with a cursor),
//!   `POST /v1/leaderboards/{board}/scores` (submit for the caller), `GET .../me` (the caller's rank
//!   and the number of players), `GET .../around` (up to 50 entries above and below the caller).
//!   Every read takes `at`: a time inside the period to show (a finished period while it is kept).
//! - **Ranks** are 1-based and unique: equal scores rank by who reached the score first, then by
//!   the lower account id. A rank is computed when read (an indexed count).
//! - **Submitting:** `best` keeps the better score (and its metadata), `latest` replaces it, `sum`
//!   adds to it (saturating). One submission at a time per player (the account lock), so a sum
//!   never loses one. `client_submit = false` boards take scores from server code only
//!   ([`LeaderboardService::submit`]); players' submissions are rate-limited (`submit_rate`).
//! - **Hooks** ([`events`]): [`BeforeScoreSubmit`](events::BeforeScoreSubmit) (check, change or
//!   refuse a score: anti-cheat is the game's) and [`AfterScoreSubmit`](events::AfterScoreSubmit).
//! - **Retention:** finished periods of daily / weekly boards beyond `keep_periods` (default 8) are
//!   deleted by a background task. Deleting an account deletes its scores.
//!
//! [`ScoreMode`]: net_backend_protocol::leaderboards::ScoreMode
//! [`ScoreOrder`]: net_backend_protocol::leaderboards::ScoreOrder
//! [`Period`]: net_backend_protocol::leaderboards::Period

// Without any database backend `Db` has no variants: code after a query is unreachable.
#![cfg_attr(not(any(feature = "mysql", feature = "postgres", feature = "sqlite")), allow(unused_variables, unreachable_code, dead_code))]

pub mod config;
pub mod events;
mod migrations;
mod module;
mod openapi;
mod routes;
mod service;
mod store;

pub use config::{BoardSpec, LeaderboardsConfig};
pub use module::Leaderboards;
pub use service::LeaderboardService;
