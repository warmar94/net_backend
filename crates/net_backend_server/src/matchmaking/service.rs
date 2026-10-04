//! [`MatchmakingService`]: tickets, rounds, the `match.found` / `match.expired` pushes.
//!
//! **In memory:** tickets live in this server instance's memory (a ticket lasts seconds to
//! minutes and the matching pass needs every ticket of a queue in one place). Every round takes a
//! snapshot of a queue's waiting tickets, lets the game's [`MatchmakingRound`] hooks choose the
//! matches without holding a lock, then applies them under the lock: a match whose tickets are not
//! all still waiting is dropped as a whole.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use net_backend_protocol::matchmaking::{CreateTicket, MatchFound, MatchTicket, QueueInfo, Queues, TicketExpired};
use net_backend_protocol::{TicketId, UnixMillis, UserId, ValidationDetails};
use serde_json::Value;

use super::config::{MatchmakingConfig, QueueSpec};
use super::events::{AfterMatchFound, BeforeTicketCreate, MatchmakingRound, ProposedMatch, QueuedTicket};
use crate::error::AppError;
use crate::hooks::HookCtx;
use crate::rate_limit::{KeyedBuckets, RateDecision};
use crate::state::AppState;

/// One ticket.
#[derive(Clone, Debug)]
struct Ticket {
    id: TicketId,
    user: UserId,
    queue: String,
    attributes: Option<Value>,
    created_at: i64,
    /// The order tickets arrived in (first come, first matched within one millisecond too).
    seq: u64,
    expires_at: i64,
    found: Option<MatchFound>,
}

impl Ticket {
    fn view(&self) -> MatchTicket {
        let ticket = MatchTicket::new(self.id, self.queue.clone(), UnixMillis(self.created_at), UnixMillis(self.expires_at));
        match &self.found {
            Some(found) => ticket.with_match(found.clone()),
            None => ticket,
        }
    }
}

fn invalid(field: &str, problem: &str) -> AppError {
    let mut details = ValidationDetails::new();
    details.add(field, problem);
    AppError::validation(details)
}

/// A new random ticket id: a positive 63-bit number.
fn new_id() -> Result<TicketId, AppError> {
    loop {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|e| AppError::internal(std::io::Error::other(format!("the system random generator failed: {e}"))))?;
        let id = i64::from_be_bytes(bytes) & i64::MAX;
        if id != 0 {
            return Ok(TicketId(id));
        }
    }
}

struct Inner {
    config: MatchmakingConfig,
    rate: Option<KeyedBuckets<UserId>>,
    /// Every ticket, by its player (one each).
    tickets: Mutex<HashMap<UserId, Ticket>>,
    next_seq: AtomicU64,
}

/// Queues, tickets and the matching rounds. A state value (`Ext<MatchmakingService>` in
/// handlers, `state.get::<MatchmakingService>()` elsewhere) once the
/// [`Matchmaking`](super::Matchmaking) module is registered. The players' actions come through the
/// routes; server code queues and cancels for a player with the same methods, reads a ticket with
/// [`ticket_of`](Self::ticket_of), and runs a round now with [`run_round`](Self::run_round).
///
/// ```no_run
/// use net_backend_server::matchmaking::MatchmakingService;
/// use net_backend_server::protocol::UserId;
/// use net_backend_server::{AppError, AppState};
///
/// // A game rule: a player who starts a solo session leaves matchmaking.
/// async fn start_solo(state: &AppState, player: UserId) -> Result<(), AppError> {
///     if let Some(matchmaking) = state.get::<MatchmakingService>() {
///         matchmaking.cancel(player);
///     }
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct MatchmakingService(Arc<Inner>);

impl std::fmt::Debug for MatchmakingService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MatchmakingService").field(&self.0.config).finish()
    }
}

impl MatchmakingService {
    pub(crate) fn new(config: MatchmakingConfig) -> Self {
        let rate =
            (config.ticket_rate > 0).then(|| KeyedBuckets::new(config.ticket_rate, Duration::from_secs(u64::from(config.ticket_rate_window_secs)), 100_000));
        Self(Arc::new(Inner { config, rate, tickets: Mutex::new(HashMap::new()), next_seq: AtomicU64::new(0) }))
    }

    /// The settings.
    pub fn config(&self) -> &MatchmakingConfig {
        &self.0.config
    }

    fn tickets(&self) -> MutexGuard<'_, HashMap<UserId, Ticket>> {
        self.0.tickets.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn queue(&self, key: &str) -> Option<&QueueSpec> {
        self.0.config.queues.iter().find(|q| q.key == key)
    }

    /// Count one ticket of `user` against `ticket_rate` (429 `rate_limited` over it).
    pub(crate) fn check_rate(&self, user: UserId) -> Result<(), AppError> {
        match self.0.rate.as_ref().map(|rate| rate.check(user)) {
            Some(RateDecision::Deny { retry_after_ms }) => Err(AppError::rate_limited(retry_after_ms)),
            _ => Ok(()),
        }
    }

    /// The queues with how many tickets wait in each.
    pub fn queues(&self) -> Queues {
        let mut waiting: HashMap<&str, u32> = HashMap::new();
        let tickets = self.tickets();
        for ticket in tickets.values().filter(|t| t.found.is_none()) {
            *waiting.entry(ticket.queue.as_str()).or_default() += 1;
        }
        Queues::new(self.0.config.queues.iter().map(|q| QueueInfo::new(q.key.clone(), q.players, waiting.get(q.key.as_str()).copied().unwrap_or(0))).collect())
    }

    fn check_request(&self, request: &CreateTicket) -> Result<(), AppError> {
        request.validate()?;
        if self.queue(&request.queue).is_none() {
            return Err(AppError::not_found("no such queue"));
        }
        if let Some(attributes) = &request.attributes {
            let bytes = serde_json::to_vec(attributes).map_err(AppError::internal)?.len();
            if bytes > self.0.config.max_attributes_bytes {
                return Err(invalid("attributes", &format!("is larger than {} bytes", self.0.config.max_attributes_bytes)));
            }
        }
        Ok(())
    }

    /// `user` queues: the [`BeforeTicketCreate`] hooks, then the ticket (409 `conflict` while one
    /// of `user`'s waits; a matched one is replaced). The rate is the route's.
    pub async fn create(&self, state: &AppState, ctx: &HookCtx, user: UserId, request: CreateTicket) -> Result<MatchTicket, AppError> {
        self.check_request(&request)?;
        let event = state.hooks().run_before(ctx, BeforeTicketCreate { user, request }).await?;
        let request = event.request;
        self.check_request(&request)?;
        let timeout = self.queue(&request.queue).map_or(120, |q| q.timeout_secs);
        let id = new_id()?;
        let now = state.now().get();
        let mut tickets = self.tickets();
        if tickets.get(&user).is_some_and(|t| t.found.is_none()) {
            return Err(AppError::conflict("a ticket of yours waits already: cancel it first"));
        }
        if !tickets.contains_key(&user) && tickets.len() >= self.0.config.max_tickets {
            return Err(AppError::unavailable("matchmaking holds too many tickets: try again later"));
        }
        let ticket = Ticket {
            id,
            user,
            queue: request.queue,
            attributes: request.attributes,
            created_at: now,
            seq: self.0.next_seq.fetch_add(1, Ordering::Relaxed),
            expires_at: now.saturating_add(i64::from(timeout) * 1000),
            found: None,
        };
        let view = ticket.view();
        tickets.insert(user, ticket);
        Ok(view)
    }

    /// `user`'s ticket: waiting, or matched with its match (until `matched_keep_secs` after the
    /// match); `None` without one.
    pub fn ticket_of(&self, state: &AppState, user: UserId) -> Option<MatchTicket> {
        let now = state.now().get();
        self.tickets().get(&user).filter(|t| t.expires_at > now).map(Ticket::view)
    }

    /// Take `user`'s ticket out of its queue (a matched one is forgotten); `true` if there was one.
    pub fn cancel(&self, user: UserId) -> bool {
        self.tickets().remove(&user).is_some()
    }

    /// A WebSocket connection of `user` closed on this instance (the module's hook): without a
    /// connection left here, its waiting ticket is cancelled.
    pub(crate) fn disconnected(&self, state: &AppState, user: UserId) {
        if self.0.config.cancel_on_disconnect && !state.ws().is_online(user) {
            let mut tickets = self.tickets();
            if tickets.get(&user).is_some_and(|t| t.found.is_none()) {
                tickets.remove(&user);
            }
        }
    }

    /// One matching pass over every queue now: expired tickets go (`match.expired` for the waiting
    /// ones), then each queue with waiting tickets runs its [`MatchmakingRound`] hooks and the
    /// matches are made (`match.found` to each player). The module's background task runs it every
    /// `interval_ms`. Answers how many matches were made.
    pub async fn run_round(&self, state: &AppState) -> Result<usize, AppError> {
        let ctx = HookCtx::new(state.clone(), None);
        let now = state.now().get();
        let expired: Vec<Ticket> = {
            let mut tickets = self.tickets();
            let gone: Vec<UserId> = tickets.values().filter(|t| t.expires_at <= now).map(|t| t.user).collect();
            gone.iter().filter_map(|user| tickets.remove(user)).filter(|t| t.found.is_none()).collect()
        };
        for ticket in expired {
            self.push(state, ticket.user, &TicketExpired::new(ticket.id, ticket.queue));
        }
        let mut made = 0;
        for queue in &self.0.config.queues {
            let mut waiting: Vec<Ticket> = self.tickets().values().filter(|t| t.queue == queue.key && t.found.is_none()).cloned().collect();
            if waiting.is_empty() {
                continue;
            }
            waiting.sort_by_key(|t| (t.created_at, t.seq));
            let matches = waiting.chunks_exact(queue.players as usize).map(|chunk| ProposedMatch::new(chunk.iter().map(|t| t.id).collect())).collect();
            let tickets = waiting
                .iter()
                .map(|t| QueuedTicket {
                    ticket: t.id,
                    user: t.user,
                    attributes: t.attributes.clone(),
                    created_at: UnixMillis(t.created_at),
                    waited_ms: now.saturating_sub(t.created_at),
                })
                .collect();
            let round = MatchmakingRound { queue: queue.key.clone(), players: queue.players, tickets, matches };
            let round = match state.hooks().run_before(&ctx, round).await {
                Ok(round) => round,
                Err(error) => {
                    tracing::debug!(%error, queue = %queue.key, "matchmaking: a hook skipped the round");
                    continue;
                }
            };
            for found in self.apply(state, &queue.key, round.matches) {
                for (ticket, user) in found.tickets.iter().zip(&found.players) {
                    let mut push = MatchFound::new(*ticket, queue.key.clone(), found.players.clone());
                    if let Some(data) = &found.data {
                        push = push.with_data(data.clone());
                    }
                    self.push(state, *user, &push);
                }
                let event = AfterMatchFound { queue: queue.key.clone(), players: found.players, tickets: found.tickets, data: found.data };
                state.hooks().run_after(&ctx, Arc::new(event)).await;
                made += 1;
            }
        }
        Ok(made)
    }

    /// Make the matches whose tickets all still wait in `queue`, once each.
    fn apply(&self, state: &AppState, queue: &str, matches: Vec<ProposedMatch>) -> Vec<Made> {
        let keep_until = state.now().get().saturating_add(i64::from(self.0.config.matched_keep_secs) * 1000);
        let mut tickets = self.tickets();
        let by_id: HashMap<TicketId, UserId> = tickets.values().filter(|t| t.queue == queue && t.found.is_none()).map(|t| (t.id, t.user)).collect();
        let mut used = HashSet::new();
        let mut made = Vec::new();
        for proposal in matches {
            let distinct: HashSet<TicketId> = proposal.tickets.iter().copied().collect();
            let users: Option<Vec<UserId>> = proposal.tickets.iter().map(|id| by_id.get(id).copied()).collect();
            let Some(users) = users.filter(|_| !proposal.tickets.is_empty() && distinct.len() == proposal.tickets.len() && distinct.is_disjoint(&used)) else {
                tracing::debug!(queue, "matchmaking: a proposed match names a ticket that is gone or taken; it is dropped");
                continue;
            };
            used.extend(distinct);
            for (id, user) in proposal.tickets.iter().zip(&users) {
                if let Some(ticket) = tickets.get_mut(user) {
                    let mut found = MatchFound::new(*id, queue, users.clone());
                    if let Some(data) = &proposal.data {
                        found = found.with_data(data.clone());
                    }
                    ticket.found = Some(found);
                    ticket.expires_at = keep_until;
                }
            }
            made.push(Made { players: users, tickets: proposal.tickets, data: proposal.data });
        }
        made
    }

    fn push<P: net_backend_protocol::ServerPush>(&self, state: &AppState, user: UserId, push: &P) {
        if !state.config().ws.enabled {
            return;
        }
        if let Err(error) = state.ws().push_user(user, push) {
            tracing::warn!(%error, kind = P::KIND, "matchmaking: a push failed");
        }
    }
}

/// A match made in a round.
struct Made {
    players: Vec<UserId>,
    tickets: Vec<TicketId>,
    data: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_views() {
        for _ in 0..50 {
            assert!(new_id().map(|id| id.get() > 0).unwrap_or(false));
        }
        let ticket = Ticket { id: TicketId(3), user: UserId(1), queue: "duel".into(), attributes: None, created_at: 1, seq: 0, expires_at: 2, found: None };
        assert_eq!(ticket.view().status, net_backend_protocol::matchmaking::TicketStatus::Waiting);
        let service = MatchmakingService::new(MatchmakingConfig::default().with_queue(QueueSpec::new("duel", 2)));
        assert!(service.check_request(&CreateTicket::new("solo")).is_err_and(|e| e.code() == net_backend_protocol::codes::NOT_FOUND));
        let big = CreateTicket::new("duel").with_attributes(serde_json::json!("x".repeat(2000)));
        assert!(service.check_request(&big).is_err_and(|e| e.code() == net_backend_protocol::codes::VALIDATION_FAILED));
        assert_eq!(service.queues().queues, vec![QueueInfo::new("duel", 2, 0)]);
    }
}
