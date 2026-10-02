//! The session: the current [`TokenPair`], when its access token expires (on this machine's
//! monotonic clock, corrected by the server's `Date`), and [`TokenUpdates`] for the app to persist
//! every new pair.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use net_backend_protocol::auth::TokenPair;
use net_backend_protocol::{AccessToken, UnixMillis};
use tokio::sync::watch;
use tokio::time::Instant;

/// How long after a refresh attempt whose fate is unknown the same refresh token is still retried
/// automatically: well inside the server's 30 s grace window (a retry inside it answers the SAME
/// pair; a reuse after it revokes the session).
pub(crate) const UNCERTAIN_RETRY_WINDOW: Duration = Duration::from_secs(20);

pub(crate) struct State {
    pub(crate) tokens: Option<TokenPair>,
    /// Grows with every change of `tokens` (a refresh that started before a change is discarded).
    pub(crate) generation: u64,
    /// When the access token expires, on this machine's monotonic clock.
    pub(crate) access_deadline: Option<Instant>,
    /// When to refresh it: the margin before the expiry, but never earlier than half its lifetime
    /// (a server with very short tokens must not get a refresh before every call).
    pub(crate) refresh_at: Option<Instant>,
    /// A refresh with the current refresh token may have reached the server, since this moment.
    pub(crate) uncertain_since: Option<Instant>,
}

pub(crate) struct Session {
    state: Mutex<State>,
    updates: watch::Sender<Option<TokenPair>>,
    /// Refresh this long before the expiry (at most half the token's lifetime).
    margin: Duration,
}

impl Session {
    pub(crate) fn new(margin: Duration) -> Self {
        let (updates, _) = watch::channel(None);
        Self { state: Mutex::new(State { tokens: None, generation: 0, access_deadline: None, refresh_at: None, uncertain_since: None }), updates, margin }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Store a new pair. `server_now` (the answer's `Date`) corrects for a wrong local clock; without
    /// it the local wall clock is used.
    pub(crate) fn set(&self, pair: TokenPair, server_now: Option<i64>) {
        let now = server_now.unwrap_or_else(|| UnixMillis::now().get());
        let left = u64::try_from(pair.access_expires_at.get().saturating_sub(now)).unwrap_or(0);
        let start = Instant::now();
        let lifetime = Duration::from_millis(left);
        let deadline = start.checked_add(lifetime).unwrap_or(start);
        let refresh_at = start.checked_add(lifetime.saturating_sub(self.margin).max(lifetime / 2)).unwrap_or(start);
        {
            let mut state = self.lock();
            state.tokens = Some(pair.clone());
            state.generation = state.generation.wrapping_add(1);
            state.access_deadline = Some(deadline);
            state.refresh_at = Some(refresh_at);
            state.uncertain_since = None;
        }
        self.updates.send_replace(Some(pair));
    }

    /// Forget the pair (logout, the session ended). Reports `None` if there was one.
    pub(crate) fn clear(&self) {
        let had = {
            let mut state = self.lock();
            let had = state.tokens.take().is_some();
            state.generation = state.generation.wrapping_add(1);
            state.access_deadline = None;
            state.refresh_at = None;
            state.uncertain_since = None;
            had
        };
        if had {
            self.updates.send_replace(None);
        }
    }

    /// Forget the pair only if it is still the one of `generation`.
    pub(crate) fn clear_if(&self, generation: u64) {
        if self.lock().generation == generation {
            self.clear();
        }
    }

    pub(crate) fn tokens(&self) -> Option<TokenPair> {
        self.lock().tokens.clone()
    }

    /// The access token to use now and its generation, and whether it should be refreshed first.
    pub(crate) fn access(&self) -> Option<(AccessToken, u64, bool)> {
        let state = self.lock();
        let tokens = state.tokens.as_ref()?;
        Some((tokens.access_token.clone(), state.generation, self.wants_refresh(&state)))
    }

    /// Whether the access token is past its expiry (on this machine's clock).
    pub(crate) fn expired(&self) -> bool {
        self.lock().access_deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// Refresh at `refresh_at`; but after an attempt whose fate is unknown and that is older than the
    /// retry window, only once the token has really expired (a reuse after the server's grace window
    /// revokes the session, so it is not risked while the token still works).
    fn wants_refresh(&self, state: &State) -> bool {
        let (Some(deadline), Some(refresh_at)) = (state.access_deadline, state.refresh_at) else { return false };
        let now = Instant::now();
        if state.uncertain_since.is_some_and(|since| now.saturating_duration_since(since) > UNCERTAIN_RETRY_WINDOW) {
            return now >= deadline;
        }
        now >= refresh_at
    }

    pub(crate) fn subscribe(&self) -> TokenUpdates {
        TokenUpdates { receiver: self.updates.subscribe() }
    }
}

/// Every change of the session's tokens, for the app to persist them ([`Client::token_updates`](crate::Client::token_updates)).
///
/// `Some(pair)` after a login, a registration, a [`resume`](crate::Client::resume) and every
/// refresh (the refresh token **rotates**: the app must store each new pair, or the next start
/// has a used-up refresh token); `None` when the session ended (logout, a refused refresh). Only
/// the latest value is kept: a slow reader sees the newest pair, never a stale one.
///
/// Usable from async code ([`changed`](Self::changed)) and from a game loop without a runtime
/// ([`try_changed`](Self::try_changed)).
pub struct TokenUpdates {
    receiver: watch::Receiver<Option<TokenPair>>,
}

impl std::fmt::Debug for TokenUpdates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenUpdates").field("logged_in", &self.receiver.borrow().is_some()).finish()
    }
}

impl TokenUpdates {
    /// The current tokens (without marking them seen).
    pub fn latest(&self) -> Option<TokenPair> {
        self.receiver.borrow().clone()
    }

    /// Wait for the next change: `Some(Some(pair))` new tokens, `Some(None)` the session ended,
    /// `None` the client is gone (every clone dropped).
    pub async fn changed(&mut self) -> Option<Option<TokenPair>> {
        self.receiver.changed().await.ok()?;
        Some(self.receiver.borrow_and_update().clone())
    }

    /// The change since the last call, if any (never blocks; no runtime needed).
    pub fn try_changed(&mut self) -> Option<Option<TokenPair>> {
        match self.receiver.has_changed() {
            Ok(true) => Some(self.receiver.borrow_and_update().clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use net_backend_protocol::RefreshToken;

    use super::*;

    fn pair(access_in_ms: i64) -> TokenPair {
        let now = UnixMillis::now().get();
        TokenPair::new(AccessToken::new("nbsa_fake"), UnixMillis(now + access_in_ms), RefreshToken::new("nbsr_fake"), UnixMillis(now + 86_400_000))
    }

    #[test]
    fn refresh_is_wanted_inside_the_margin_but_not_before_half_the_lifetime() {
        let session = Session::new(Duration::from_secs(60));
        assert!(session.access().is_none());
        session.set(pair(3_600_000), None);
        let at = |s: &Session| {
            let state = s.lock();
            state.refresh_at.zip(state.access_deadline).map(|(r, d)| d.saturating_duration_since(r))
        };
        assert_eq!(session.access().map(|a| a.2), Some(false));
        assert!(at(&session).is_some_and(|before| before > Duration::from_secs(59) && before <= Duration::from_secs(60)), "60 s before the expiry");
        // A 30 s token: refreshed after half its life (15 s), not before every call.
        session.set(pair(30_000), None);
        assert_eq!(session.access().map(|a| a.2), Some(false));
        assert!(at(&session).is_some_and(|before| before > Duration::from_secs(14) && before <= Duration::from_secs(15)));
        session.set(pair(-1_000), None);
        assert_eq!(session.access().map(|a| a.2), Some(true), "expired: refresh first");
        // The server's clock says the token has 2 minutes left, whatever this machine's clock says.
        let server_now = UnixMillis::now().get() - 600_000;
        let p = TokenPair::new(AccessToken::new("a"), UnixMillis(server_now + 120_000), RefreshToken::new("r"), UnixMillis(server_now + 86_400_000));
        session.set(p, Some(server_now));
        assert_eq!(session.access().map(|a| a.2), Some(false));
        assert!(!session.expired());
    }

    #[test]
    fn an_old_uncertain_refresh_waits_for_the_real_expiry() {
        let session = Session::new(Duration::from_secs(60));
        session.set(pair(-1_000), None);
        session.lock().access_deadline = Instant::now().checked_add(Duration::from_secs(30));
        session.lock().uncertain_since = Instant::now().checked_sub(Duration::from_secs(25));
        assert_eq!(session.access().map(|a| a.2), Some(false), "still valid: not risked");
        session.lock().access_deadline = Instant::now().checked_sub(Duration::from_millis(1));
        assert_eq!(session.access().map(|a| a.2), Some(true), "expired: tried");
    }

    #[test]
    fn updates_report_changes_only() {
        let session = Session::new(Duration::from_secs(60));
        let mut updates = session.subscribe();
        assert!(updates.try_changed().is_none());
        session.set(pair(3_600_000), None);
        assert!(matches!(updates.try_changed(), Some(Some(_))));
        assert!(updates.try_changed().is_none());
        session.clear();
        assert!(matches!(updates.try_changed(), Some(None)));
        session.clear();
        assert!(updates.try_changed().is_none(), "nothing to clear: no update");
        assert!(!format!("{updates:?}").contains("nbsa"));
    }
}
