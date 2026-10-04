//! What games can hook into in the matchmaking module ([`crate::hooks`]): the game's rules.
//!
//! | Event | Kind | When |
//! |---|---|---|
//! | [`BeforeTicketCreate`] | before | a player is about to queue; change the request (e.g. put the player's stored rating into the attributes; checked again) or refuse |
//! | [`MatchmakingRound`] | before | a queue is about to be matched: the waiting tickets and the default proposal (first come, first matched, `players` per match); set [`matches`](MatchmakingRound::matches) to the game's own |
//! | [`AfterMatchFound`] | after | a match was made and its players told |
//!
//! **The game's rules** are a [`MatchmakingRound`] hook: it reads the waiting tickets (their
//! attributes, how long they waited) and answers the matches it wants, each with optional `data`
//! for the players (a lobby the hook created, a server address, the teams). Tickets it leaves out
//! keep waiting. A refusal skips the round.
//!
//! ```
//! use net_backend_server::hooks::Decision;
//! use net_backend_server::matchmaking::events::{MatchmakingRound, ProposedMatch};
//! use net_backend_server::{Config, NetBackendServer};
//!
//! let server = NetBackendServer::new(Config::default()).before::<MatchmakingRound, _, _>(|_ctx, mut round| async move {
//!     // Pairs of players whose ratings are at most 200 apart; the longer a ticket waited, the wider.
//!     let rating = |t: &net_backend_server::matchmaking::events::QueuedTicket| t.attributes.as_ref().and_then(|a| a["rating"].as_i64()).unwrap_or(1000);
//!     let mut waiting = round.tickets.clone();
//!     waiting.sort_by_key(|t| rating(t));
//!     round.matches.clear();
//!     let mut i = 0;
//!     while i + 1 < waiting.len() {
//!         let (a, b) = (&waiting[i], &waiting[i + 1]);
//!         let spread = 200 + a.waited_ms.min(b.waited_ms) / 1000 * 10;
//!         if (rating(a) - rating(b)).abs() <= spread {
//!             round.matches.push(ProposedMatch::new(vec![a.ticket, b.ticket]));
//!             i += 2;
//!         } else {
//!             i += 1;
//!         }
//!     }
//!     Ok(Decision::Continue(round))
//! });
//! # let _ = server;
//! ```

use net_backend_protocol::matchmaking::CreateTicket;
use net_backend_protocol::{TicketId, UnixMillis, UserId};
use serde_json::Value;

use crate::hooks::Event;

/// A player is about to queue. Hooks may change `request` (checked again afterwards) or refuse.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BeforeTicketCreate {
    /// The player.
    pub user: UserId,
    /// The request.
    pub request: CreateTicket,
}

impl Event for BeforeTicketCreate {
    const NAME: &'static str = "matchmaking.before_ticket";
}

/// A waiting ticket, as a [`MatchmakingRound`] sees it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct QueuedTicket {
    /// The ticket.
    pub ticket: TicketId,
    /// Its player.
    pub user: UserId,
    /// What the player (or a [`BeforeTicketCreate`] hook) attached.
    pub attributes: Option<Value>,
    /// When it was made.
    pub created_at: UnixMillis,
    /// How long it has waited, milliseconds.
    pub waited_ms: i64,
}

/// One match a [`MatchmakingRound`] wants.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ProposedMatch {
    /// The tickets of the match (at least one, each at most once per round), in the order the
    /// players are listed to them.
    pub tickets: Vec<TicketId>,
    /// What the players get with the match (`MatchFound.data`).
    pub data: Option<Value>,
}

impl ProposedMatch {
    /// A match of these tickets without data.
    pub fn new(tickets: Vec<TicketId>) -> Self {
        Self { tickets, data: None }
    }

    /// The same match with data for the players.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// A queue is about to be matched. `tickets` are the waiting tickets, oldest first; `matches`
/// holds the default proposal (first come, first matched, `players` per match). Hooks set
/// `matches` to the game's own; tickets in no match keep waiting. A match naming a ticket that is
/// gone (cancelled, timed out) or twice is dropped as a whole; its other tickets keep waiting.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MatchmakingRound {
    /// The queue.
    pub queue: String,
    /// The queue's `players` setting.
    pub players: u32,
    /// The waiting tickets, oldest first.
    pub tickets: Vec<QueuedTicket>,
    /// The matches to make.
    pub matches: Vec<ProposedMatch>,
}

impl Event for MatchmakingRound {
    const NAME: &'static str = "matchmaking.round";
}

/// A match was made: its tickets are matched and its players got `match.found`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AfterMatchFound {
    /// The queue.
    pub queue: String,
    /// The players, in the match's order.
    pub players: Vec<UserId>,
    /// Their tickets, in the same order.
    pub tickets: Vec<TicketId>,
    /// The match's data.
    pub data: Option<Value>,
}

impl Event for AfterMatchFound {
    const NAME: &'static str = "matchmaking.after_match";
}
