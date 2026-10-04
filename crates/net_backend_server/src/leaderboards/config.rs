//! The leaderboards module's settings: `[modules.leaderboards]` in the configuration (or
//! [`Leaderboards::with_config`](crate::leaderboards::Leaderboards::with_config)).
//!
//! ```toml
//! [modules.leaderboards]
//! submit_rate = 30               # client submissions per user: a burst of 30, then one every
//! submit_rate_window_secs = 60   # 60 / 30 = 2 s (token bucket); 0 = no limit
//! max_metadata_bytes = 1024      # the JSON stored with a score
//! keep_periods = 8               # finished periods of daily / weekly boards kept; 0 = keep all
//! purge_interval_secs = 3600     # how often old periods are deleted (0: never)
//!
//! [[modules.leaderboards.boards]]
//! key = "highscore"
//! name = "High score"
//! mode = "best"                  # best | latest | sum
//! order = "desc"                 # desc (higher is better) | asc (lower is better)
//! period = "all_time"            # all_time | daily | weekly (resets at 00:00 UTC, weeks on Monday)
//! client_submit = true           # false: only the game's server code submits
//! ```

use net_backend_protocol::leaderboards::{is_valid_board_key, Period, ScoreMode, ScoreOrder, DEFAULT_MAX_METADATA_BYTES};
use serde::Deserialize;

use crate::error::Error;

/// The longest metadata a server may allow, in bytes of its JSON.
pub const MAX_METADATA_BYTES: usize = 16 * 1024;

/// One board of the module.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct BoardSpec {
    /// The key clients address it with (`highscore`): 1-64 bytes of `[a-z0-9_.-]`.
    pub key: String,
    /// A display name (1-64 characters).
    #[serde(default)]
    pub name: Option<String>,
    /// What a new score does to the stored one. Default `best`.
    #[serde(default)]
    pub mode: ScoreMode,
    /// Which scores rank higher. Default `desc` (higher is better).
    #[serde(default)]
    pub order: ScoreOrder,
    /// When the board starts over. Default `all_time`.
    #[serde(default)]
    pub period: Period,
    /// Whether players may submit through `POST /v1/leaderboards/{board}/scores`. Default true;
    /// with false only server code ([`LeaderboardService::submit`](crate::leaderboards::LeaderboardService::submit))
    /// submits, and the route answers 403.
    #[serde(default = "yes")]
    pub client_submit: bool,
}

fn yes() -> bool {
    true
}

impl BoardSpec {
    /// A board with this key: `best`, `desc`, all-time, client submissions allowed.
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into(), name: None, mode: ScoreMode::Best, order: ScoreOrder::Desc, period: Period::AllTime, client_submit: true }
    }

    /// The same board with a display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same board with this score mode.
    pub fn with_mode(mut self, mode: ScoreMode) -> Self {
        self.mode = mode;
        self
    }

    /// The same board with this order.
    pub fn with_order(mut self, order: ScoreOrder) -> Self {
        self.order = order;
        self
    }

    /// The same board with this period.
    pub fn with_period(mut self, period: Period) -> Self {
        self.period = period;
        self
    }

    /// The same board with client submissions on or off.
    pub fn with_client_submit(mut self, allowed: bool) -> Self {
        self.client_submit = allowed;
        self
    }
}

/// The leaderboards module's settings. Build in code from [`LeaderboardsConfig::default`] by
/// changing fields, or let the module read `[modules.leaderboards]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct LeaderboardsConfig {
    /// The boards (scores of a board removed here stay in the database).
    pub boards: Vec<BoardSpec>,
    /// Client submissions per user within `submit_rate_window_secs`, as a token bucket: a burst of
    /// this many, then one every `window / submit_rate` (over it: 429 `rate_limited` +
    /// `retry_after_ms`). Default 30 (per 60 s). 0 = no limit. Server code is not limited.
    pub submit_rate: u32,
    /// The window of `submit_rate`, seconds. Default 60.
    pub submit_rate_window_secs: u32,
    /// The largest metadata of a score, in bytes of its JSON. Default 1024; at most 16 KiB.
    pub max_metadata_bytes: usize,
    /// How many finished periods of a daily / weekly board are kept (older ones are deleted by a
    /// background task). Default 8; 0 keeps every period.
    pub keep_periods: u32,
    /// How often the old periods are deleted, seconds (0: never). Default 3600.
    pub purge_interval_secs: u64,
}

impl Default for LeaderboardsConfig {
    fn default() -> Self {
        Self {
            boards: Vec::new(),
            submit_rate: 30,
            submit_rate_window_secs: 60,
            max_metadata_bytes: DEFAULT_MAX_METADATA_BYTES,
            keep_periods: 8,
            purge_interval_secs: 3600,
        }
    }
}

impl LeaderboardsConfig {
    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        let mut problems = Vec::new();
        let mut need = |ok: bool, problem: String| {
            if !ok {
                problems.push(format!("modules.leaderboards.{problem}"));
            }
        };
        need(self.submit_rate <= 100_000, "submit_rate must be at most 100000 (0 = no limit)".into());
        need((1..=86_400).contains(&self.submit_rate_window_secs), "submit_rate_window_secs must be between 1 and 86400".into());
        need((1..=MAX_METADATA_BYTES).contains(&self.max_metadata_bytes), format!("max_metadata_bytes must be between 1 and {MAX_METADATA_BYTES}"));
        need(self.keep_periods <= 100_000, "keep_periods must be at most 100000 (0 = keep all)".into());
        need(self.purge_interval_secs <= 7 * 86_400, "purge_interval_secs must be at most 604800".into());
        let mut keys = std::collections::HashSet::new();
        for board in &self.boards {
            need(is_valid_board_key(&board.key), format!("boards: `{}` is not a board key (1-64 bytes of a-z, 0-9, _ - .)", board.key));
            need(keys.insert(board.key.as_str()), format!("boards: `{}` is listed twice", board.key));
            if let Some(name) = &board.name {
                let ok = !name.trim().is_empty() && name.chars().count() <= 64 && net_backend_protocol::text::name_problem(name).is_none();
                need(ok, format!("boards: the name of `{}` must be 1-64 characters", board.key));
            }
            need(!matches!(board.mode, ScoreMode::Unknown), format!("boards: the mode of `{}` must be best, latest or sum", board.key));
            need(!matches!(board.order, ScoreOrder::Unknown), format!("boards: the order of `{}` must be desc or asc", board.key));
            need(!matches!(board.period, Period::Unknown), format!("boards: the period of `{}` must be all_time, daily or weekly", board.key));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems))
        }
    }

    /// The board with this key.
    pub fn board(&self, key: &str) -> Option<&BoardSpec> {
        self.boards.iter().find(|b| b.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_problems() {
        assert!(LeaderboardsConfig::default().validate().is_ok());
        let bad = LeaderboardsConfig {
            boards: vec![BoardSpec::new("High"), BoardSpec::new("a"), BoardSpec::new("a").with_name(" "), BoardSpec::new("b").with_mode(ScoreMode::Unknown)],
            max_metadata_bytes: 0,
            submit_rate_window_secs: 0,
            ..LeaderboardsConfig::default()
        };
        let error = bad.validate().err().map(|e| e.to_string()).unwrap_or_default();
        for part in ["`High` is not a board key", "`a` is listed twice", "the name of `a`", "the mode of `b`", "max_metadata_bytes", "submit_rate_window_secs"]
        {
            assert!(error.contains(part), "{part}: {error}");
        }
        let parsed: LeaderboardsConfig =
            toml::from_str("[[boards]]\nkey = \"race\"\nmode = \"latest\"\norder = \"asc\"\nperiod = \"weekly\"\nclient_submit = false\n").expect("toml");
        let race = parsed.board("race").expect("board");
        assert_eq!((race.mode, race.order, race.period, race.client_submit), (ScoreMode::Latest, ScoreOrder::Asc, Period::Weekly, false));
        let typo: LeaderboardsConfig = toml::from_str("[[boards]]\nkey = \"x\"\nmode = \"bets\"\n").expect("toml");
        assert!(typo.validate().is_err(), "an unknown mode is refused");
        assert!(toml::from_str::<LeaderboardsConfig>("[[boards]]\nkey = \"x\"\nreset = \"daily\"\n").is_err(), "unknown keys are refused");
    }
}
