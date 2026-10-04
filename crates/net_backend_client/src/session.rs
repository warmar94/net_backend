//! The session: the current [`TokenPair`], when its access token expires (on this machine's
//! monotonic clock, corrected by the server's `Date`), and [`TokenUpdates`] for the app to persist
//! every new pair.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use net_backend_protocol::auth::TokenPair;
use net_backend_protocol::{AccessToken, UnixMillis};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::token_file::TokenFile;

/// How long after a refresh attempt whose fate is unknown the same refresh token is still retried
/// automatically: well inside the server's 30 s grace window (a retry inside it answers the SAME
/// pair; a reuse after it revokes the session).
pub(crate) const UNCERTAIN_RETRY_WINDOW: Duration = Duration::from_secs(20);

pub(crate) struct State {
    pub(crate) tokens: Option<TokenPair>,
    /// Grows with every change of `tokens` (a refresh that started before a change is discarded).
    pub(crate) generation: u64,
    /// Grows with every NEW session (a login, a registration, [`Client::resume`](crate::Client::resume))
    /// and when the tokens are dropped; a refresh keeps it (the server's session is the same). A
    /// logout drops the tokens of its own session only, also when a refresh replaced them meanwhile.
    pub(crate) lineage: u64,
    /// When the access token expires, on this machine's monotonic clock.
    pub(crate) access_deadline: Option<Instant>,
    /// When to refresh it: the margin before the expiry, but never earlier than half its lifetime
    /// (a server with very short tokens must not get a refresh before every call).
    pub(crate) refresh_at: Option<Instant>,
    /// A refresh with the current refresh token may have reached the server, since this moment.
    pub(crate) uncertain_since: Option<Instant>,
}

/// The token file kept up to date, and the server URL stored in it.
struct FileSink {
    file: TokenFile,
    server: String,
    /// The generation on disk: a write of an older state that comes late is skipped, so the file
    /// always ends at the newest tokens.
    written: Mutex<u64>,
}

impl FileSink {
    /// Write `tokens` of `generation` (or remove the file for `None`). A failure is logged (the
    /// file name and the I/O error, never a token); the session goes on.
    fn write(&self, generation: u64, tokens: Option<&TokenPair>) {
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        if generation <= *written {
            return;
        }
        *written = generation;
        let result = match tokens {
            Some(tokens) => self.file.save(&self.server, tokens),
            None => self.file.remove(),
        };
        if let Err(error) = result {
            tracing::warn!("net_backend_client: {error}");
        }
    }
}

/// The token file write that follows one change of the tokens (nothing without a token file):
/// [`now`](Self::now) on this thread, or [`finish`](Self::finish) on tokio's blocking pool (no
/// file I/O on an async worker).
#[must_use = "the token file is written by `now` or `finish`"]
pub(crate) struct FileWrite(Option<(Arc<FileSink>, u64, Option<TokenPair>)>);

impl FileWrite {
    /// Write on this thread (the synchronous calls: build, `resume`, `forget_session`).
    pub(crate) fn now(self) {
        if let Some((sink, generation, tokens)) = self.0 {
            sink.write(generation, tokens.as_ref());
        }
    }

    /// Write on tokio's blocking pool and wait for it (outside a runtime: on this thread).
    pub(crate) async fn finish(self) {
        let Some((sink, generation, tokens)) = self.0 else { return };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                if let Err(error) = handle.spawn_blocking(move || sink.write(generation, tokens.as_ref())).await {
                    tracing::warn!("net_backend_client: the token file write did not run ({error})");
                }
            }
            Err(_) => sink.write(generation, tokens.as_ref()),
        }
    }
}

pub(crate) struct Session {
    state: Mutex<State>,
    updates: watch::Sender<Option<TokenPair>>,
    /// Refresh this long before the expiry (at most half the token's lifetime).
    margin: Duration,
    /// The token file kept up to date with every change of the tokens.
    file: Option<Arc<FileSink>>,
}

impl Session {
    pub(crate) fn new(margin: Duration) -> Self {
        let (updates, _) = watch::channel(None);
        Self {
            state: Mutex::new(State { tokens: None, generation: 0, lineage: 0, access_deadline: None, refresh_at: None, uncertain_since: None }),
            updates,
            margin,
            file: None,
        }
    }

    /// Keep `file` up to date with every change of the tokens (before any is set).
    pub(crate) fn with_file(mut self, file: TokenFile, server: String) -> Self {
        self.file = Some(Arc::new(FileSink { file, server, written: Mutex::new(0) }));
        self
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The file write for the state just changed (called with the state lock held).
    fn file_write(&self, state: &State) -> FileWrite {
        FileWrite(self.file.as_ref().map(|sink| (Arc::clone(sink), state.generation, state.tokens.clone())))
    }

    /// Store `pair` and report it, both under the state lock (the reports come in the order of
    /// the changes). `server_now` (the answer's `Date`) corrects for a wrong local clock; without
    /// it the local wall clock is used.
    fn store(&self, state: &mut State, pair: TokenPair, server_now: Option<i64>) -> FileWrite {
        let now = server_now.unwrap_or_else(|| UnixMillis::now().get());
        let left = u64::try_from(pair.access_expires_at.get().saturating_sub(now)).unwrap_or(0);
        let start = Instant::now();
        let lifetime = Duration::from_millis(left);
        state.tokens = Some(pair.clone());
        state.generation = state.generation.wrapping_add(1);
        state.access_deadline = Some(start.checked_add(lifetime).unwrap_or(start));
        state.refresh_at = Some(start.checked_add(lifetime.saturating_sub(self.margin).max(lifetime / 2)).unwrap_or(start));
        state.uncertain_since = None;
        self.updates.send_replace(Some(pair));
        self.file_write(state)
    }

    /// Store the pair of a NEW session (login, registration, resume).
    pub(crate) fn set(&self, pair: TokenPair, server_now: Option<i64>) -> FileWrite {
        let mut state = self.lock();
        state.lineage = state.lineage.wrapping_add(1);
        self.store(&mut state, pair, server_now)
    }

    /// Store a refreshed pair only if the tokens are still those of `generation`, checked and
    /// stored under one lock (a logout or a new login meanwhile wins). `None`: not stored.
    pub(crate) fn set_if(&self, pair: TokenPair, server_now: Option<i64>, generation: u64) -> Option<FileWrite> {
        let mut state = self.lock();
        if state.generation != generation || state.tokens.is_none() {
            return None;
        }
        Some(self.store(&mut state, pair, server_now))
    }

    /// Forget the pair under the lock (reports `None` if there was one).
    fn drop_tokens(&self, state: &mut State) -> FileWrite {
        let had = state.tokens.take().is_some();
        state.generation = state.generation.wrapping_add(1);
        state.lineage = state.lineage.wrapping_add(1);
        state.access_deadline = None;
        state.refresh_at = None;
        state.uncertain_since = None;
        if !had {
            return FileWrite(None);
        }
        self.updates.send_replace(None);
        self.file_write(state)
    }

    /// Forget the pair (the app forgets the session).
    pub(crate) fn clear(&self) -> FileWrite {
        let mut state = self.lock();
        self.drop_tokens(&mut state)
    }

    /// Forget the pair only if it is still the one of `generation` (a refused refresh).
    pub(crate) fn clear_if(&self, generation: u64) -> FileWrite {
        let mut state = self.lock();
        if state.generation != generation {
            return FileWrite(None);
        }
        self.drop_tokens(&mut state)
    }

    /// Forget the pair if it still belongs to the session of `lineage`, refreshed meanwhile or
    /// not (the logout of that session).
    pub(crate) fn clear_lineage(&self, lineage: u64) -> FileWrite {
        let mut state = self.lock();
        if state.lineage != lineage {
            return FileWrite(None);
        }
        self.drop_tokens(&mut state)
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
        session.set(pair(3_600_000), None).now();
        let at = |s: &Session| {
            let state = s.lock();
            state.refresh_at.zip(state.access_deadline).map(|(r, d)| d.saturating_duration_since(r))
        };
        assert_eq!(session.access().map(|a| a.2), Some(false));
        assert!(at(&session).is_some_and(|before| before > Duration::from_secs(59) && before <= Duration::from_secs(60)), "60 s before the expiry");
        // A 30 s token: refreshed after half its life (15 s), not before every call.
        session.set(pair(30_000), None).now();
        assert_eq!(session.access().map(|a| a.2), Some(false));
        assert!(at(&session).is_some_and(|before| before > Duration::from_secs(14) && before <= Duration::from_secs(15)));
        session.set(pair(-1_000), None).now();
        assert_eq!(session.access().map(|a| a.2), Some(true), "expired: refresh first");
        // The server's clock says the token has 2 minutes left, whatever this machine's clock says.
        let server_now = UnixMillis::now().get() - 600_000;
        let p = TokenPair::new(AccessToken::new("a"), UnixMillis(server_now + 120_000), RefreshToken::new("r"), UnixMillis(server_now + 86_400_000));
        session.set(p, Some(server_now)).now();
        assert_eq!(session.access().map(|a| a.2), Some(false));
        assert!(!session.expired());
    }

    #[test]
    fn an_old_uncertain_refresh_waits_for_the_real_expiry() {
        let session = Session::new(Duration::from_secs(60));
        session.set(pair(-1_000), None).now();
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
        session.set(pair(3_600_000), None).now();
        assert!(matches!(updates.try_changed(), Some(Some(_))));
        assert!(updates.try_changed().is_none());
        session.clear().now();
        assert!(matches!(updates.try_changed(), Some(None)));
        session.clear().now();
        assert!(updates.try_changed().is_none(), "nothing to clear: no update");
        assert!(!format!("{updates:?}").contains("nbsa"));
    }

    /// A refresh answer that lands after a logout dropped the tokens is not stored (the check and
    /// the store under one lock), and the updates end at `None`.
    #[test]
    fn a_refresh_landing_after_a_logout_is_discarded() {
        let session = Session::new(Duration::from_secs(60));
        let mut updates = session.subscribe();
        session.set(pair(3_600_000), None).now();
        assert!(matches!(updates.try_changed(), Some(Some(_))));
        // The refresh task read the generation and sent its request ...
        let generation = session.lock().generation;
        // ... the logout completes meanwhile ...
        let lineage = session.lock().lineage;
        session.clear_lineage(lineage).now();
        // ... then the refresh's answer arrives: refused, nothing re-installed.
        assert!(session.set_if(pair(3_600_000), None, generation).is_none());
        assert!(session.tokens().is_none());
        assert!(matches!(updates.try_changed(), Some(None)), "the last report is the logout's None");
        assert!(updates.try_changed().is_none());
    }

    /// A refresh that completes while a logout is in flight is dropped by the logout (the same
    /// session); a NEW login meanwhile is kept.
    #[test]
    fn a_logout_drops_its_session_also_after_a_refresh_but_keeps_a_new_login() {
        let session = Session::new(Duration::from_secs(60));
        session.set(pair(3_600_000), None).now();
        // The logout read its tokens and sent its request ...
        let lineage = session.lock().lineage;
        // ... a refresh completes meanwhile (a new pair of the same session) ...
        let generation = session.lock().generation;
        assert!(session.set_if(pair(3_600_000), None, generation).is_some());
        // ... the logout's success drops the refreshed pair too.
        session.clear_lineage(lineage).now();
        assert!(session.tokens().is_none(), "the revoked session's refreshed pair is gone");

        session.set(pair(3_600_000), None).now();
        let lineage = session.lock().lineage;
        session.set(pair(3_600_000), None).now(); // a new login while the logout was in flight
        session.clear_lineage(lineage).now();
        assert!(session.tokens().is_some(), "a new login is not logged out by an older logout");
    }

    /// The token file ends at the newest state, whatever order the writes run in.
    #[test]
    fn a_late_write_of_an_older_state_is_skipped() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("session-unit-{}", std::process::id()));
        let path = dir.join("session.json");
        let session = Session::new(Duration::from_secs(60)).with_file(TokenFile::new(&path), "https://api.example.com".into());
        let first = session.set(pair(3_600_000), None);
        let second = session.clear();
        second.now();
        first.now();
        assert!(!path.exists(), "the older write (the pair) came late and was skipped");
        session.set(pair(3_600_000), None).now();
        assert!(path.exists(), "a newer one is written");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
