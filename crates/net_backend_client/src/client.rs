//! [`Client`] (async, tokio) and [`ClientBuilder`]: typed calls, the session and its refresh.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use net_backend_protocol::auth::{AuthSession, LoginRequest, LogoutRequest, RefreshRequest, RegisterRequest, SteamLoginRequest, TokenPair};
use net_backend_protocol::oauth::OAuthLogin;
use net_backend_protocol::{codes, GetServerInfo, HttpCall, ServerInfo};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::http::{Answer, BaseUrl, Http, Outgoing, ProxySetting};
use crate::session::{Session, TokenUpdates, UNCERTAIN_RETRY_WINDOW};
use crate::tls::{PemSource, TrustSettings};
use crate::token_file::{LoadError, TokenFile};
use crate::Error;

/// The default deadline of one call: 15 s.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// The default limit of one HTTP answer body: 10 MiB (a full storage batch is about 4 MiB).
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
/// The longest deadline accepted: 1 hour.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

/// Settings for a [`Client`] ([`Client::builder`]). Nothing is checked or opened until
/// [`build`](Self::build).
#[derive(Clone)]
pub struct ClientBuilder {
    url: String,
    timeout: Duration,
    max_response_bytes: usize,
    refresh_margin: Duration,
    allow_insecure_http: bool,
    tokens: Option<TokenPair>,
    proxy: ProxySetting,
    trust: TrustSettings,
    token_file: Option<TokenFile>,
    #[cfg(feature = "http2")]
    http2: bool,
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let proxy = match &self.proxy {
            ProxySetting::Env => "environment",
            ProxySetting::Url(_) => "explicit",
            ProxySetting::Off => "off",
        };
        f.debug_struct("ClientBuilder")
            .field("url", &self.url)
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("refresh_margin", &self.refresh_margin)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("tokens", &self.tokens.is_some())
            .field("proxy", &proxy)
            .field("trust", &self.trust)
            .field("token_file", &self.token_file.as_ref().map(TokenFile::path))
            .finish()
    }
}

impl ClientBuilder {
    fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            timeout: DEFAULT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            refresh_margin: Duration::from_secs(net_backend_protocol::auth::ACCESS_TOKEN_REFRESH_MARGIN_SECS),
            allow_insecure_http: false,
            tokens: None,
            proxy: ProxySetting::Env,
            trust: TrustSettings::default(),
            token_file: None,
            #[cfg(feature = "http2")]
            http2: false,
        }
    }

    /// The deadline of one call (default 15 s, clamped to 1 ms..=1 h): waiting for a token refresh,
    /// connecting, sending and reading the answer together.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = clamp_timeout(timeout);
        self
    }

    /// The largest answer body accepted (default 10 MiB, at least 1 KiB); a bigger one is
    /// [`Error::BodyTooLarge`].
    pub fn max_response_bytes(mut self, bytes: usize) -> Self {
        self.max_response_bytes = bytes.max(1024);
        self
    }

    /// Refresh the access token when less than this is left (default 60 s, the protocol's margin;
    /// at most 1 h), but never before half of its lifetime has passed.
    pub fn refresh_margin(mut self, margin: Duration) -> Self {
        self.refresh_margin = margin.min(MAX_TIMEOUT);
        self
    }

    /// Allow plain `http://` (and `ws://`) to a host that is not loopback (default `false`: such a
    /// URL is refused at `build`). Loopback (`localhost`, `127.0.0.1`, `::1`) is always allowed.
    /// Tokens travel in clear text over plain HTTP: only for a trusted local network.
    pub fn allow_insecure_http(mut self, allow: bool) -> Self {
        self.allow_insecure_http = allow;
        self
    }

    /// Start with these tokens (the app's stored session; same as [`Client::resume`]).
    pub fn tokens(mut self, tokens: TokenPair) -> Self {
        self.tokens = Some(tokens);
        self
    }

    /// Send every HTTP request and the WebSocket through this HTTP proxy
    /// (`http://host:port`, or `http://user:password@host:port` for `Proxy-Authorization: Basic`)
    /// as an HTTP CONNECT tunnel: TLS runs end to end through it. Replaces the proxy settings of
    /// the environment. A loopback server is always reached directly.
    pub fn proxy(mut self, url: &str) -> Self {
        self.proxy = ProxySetting::Url(url.to_string());
        self
    }

    /// Connect directly, whatever the environment's proxy settings say.
    pub fn no_proxy(mut self) -> Self {
        self.proxy = ProxySetting::Off;
        self
    }

    /// Also trust the root certificates in this PEM text (every `CERTIFICATE` block; other blocks
    /// are skipped), e.g. the certificate of a self-signed development server or a company CA. They
    /// apply to HTTPS and the WebSocket, on top of webpki-roots (or of the operating system's store
    /// with [`os_certificates`](Self::os_certificates)). Can be called more than once. Checked at
    /// [`build`](Self::build): PEM without a certificate, or a certificate that cannot be a root,
    /// is `InvalidRequest`.
    pub fn root_certificates_pem(mut self, pem: impl AsRef<[u8]>) -> Self {
        self.trust.extra.push(PemSource::Bytes(pem.as_ref().to_vec()));
        self
    }

    /// Like [`root_certificates_pem`](Self::root_certificates_pem), with the PEM read from this
    /// file at [`build`](Self::build) (a file that cannot be read is `InvalidRequest`).
    pub fn root_certificates_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.trust.extra.push(PemSource::File(path.into()));
        self
    }

    /// Trust the operating system's certificate store and certificate checks (Windows, macOS / iOS,
    /// Linux / BSD system CA files, Android) instead of the built-in Mozilla roots (webpki-roots),
    /// e.g. for a company proxy or CA installed on the machine (default `false`). Extra roots from
    /// [`root_certificates_pem`](Self::root_certificates_pem) are added on top (not on Android).
    /// Uses `rustls-platform-verifier` with ring. On Android, that crate needs its JNI
    /// initialization first (see its documentation).
    #[cfg(feature = "os-certificates")]
    #[cfg_attr(docsrs, doc(cfg(feature = "os-certificates")))]
    pub fn os_certificates(mut self, on: bool) -> Self {
        self.trust.os_store = on;
        self
    }

    /// Offer HTTP/2 to `https://` servers through ALPN (default `false`). The server picks: HTTP/2
    /// when it supports it, else HTTP/1.1. Plain `http://` stays HTTP/1.1; the WebSocket is not
    /// affected.
    #[cfg(feature = "http2")]
    #[cfg_attr(docsrs, doc(cfg(feature = "http2")))]
    pub fn http2(mut self, on: bool) -> Self {
        self.http2 = on;
        self
    }

    /// Keep the session in this file ([`TokenFile`]): `build` loads it (the session resumes; the
    /// next call refreshes an expired access token), and every change of the tokens is written to
    /// it (login, registration, every refresh, [`Client::resume`]); a logout, a refused refresh or
    /// [`Client::forget_session`] deletes it. Written atomically and owner-only (Unix `0600`;
    /// Windows: the folder's inherited ACL, so keep it under the user's profile).
    ///
    /// The file stores the server URL: a file written for another server is not used. A damaged
    /// file is not used either (a warning is logged; the next login overwrites it). Tokens given to
    /// [`tokens`](Self::tokens) take the place of the file's and are written to it. A file that
    /// exists but cannot be read makes `build` fail with `InvalidRequest`. A failed write is logged
    /// as a warning (the file name and the I/O error, never a token); the session goes on, and
    /// [`Client::token_updates`] still reports every pair.
    pub fn token_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.token_file = Some(TokenFile::new(path));
        self
    }

    /// Check the URL (and the proxy settings, the extra root certificates) and build the client.
    /// Nothing connects until the first call; the only I/O is reading the root certificate files
    /// and the token file. Needs no runtime (the first call does).
    ///
    /// The proxy, unless [`proxy`](Self::proxy) or [`no_proxy`](Self::no_proxy) was called,
    /// comes from the environment as `build` reads it: `HTTPS_PROXY` for an `https://` server,
    /// `HTTP_PROXY` for `http://`, `ALL_PROXY` for both, `NO_PROXY` (comma-separated hosts,
    /// domains and IP ranges, `*` for all) to skip it; the lowercase names work too. A loopback
    /// server never uses a proxy. A proxy URL that is not `http://` (e.g. `socks5://`) is
    /// refused with `InvalidRequest`.
    pub fn build(self) -> Result<Client, Error> {
        let base = BaseUrl::parse(&self.url)?;
        if base.scheme == crate::http::Scheme::Http && !base.is_loopback() && !self.allow_insecure_http {
            return Err(Error::invalid(format!(
                "plain http:// to `{}` is refused: use https://, a loopback host, or ClientBuilder::allow_insecure_http(true)",
                base.host
            )));
        }
        #[cfg(feature = "http2")]
        let http2 = self.http2;
        #[cfg(not(feature = "http2"))]
        let http2 = false;
        let http = Http::new(base, self.max_response_bytes, self.proxy.clone(), &self.trust, http2)?;
        let mut session = Session::new(self.refresh_margin);
        let mut tokens = self.tokens;
        if let Some(file) = self.token_file {
            let server = http.base.url("");
            if tokens.is_none() {
                tokens = match file.load_checked(&server) {
                    Ok(stored) => stored,
                    // Damaged: start without a session; the next login overwrites it.
                    Err(LoadError::Damaged(error)) => {
                        tracing::warn!("net_backend_client: {error}; starting without a stored session");
                        None
                    }
                    Err(LoadError::Unreadable(error)) => return Err(error),
                };
            }
            session = session.with_file(file, server);
        }
        if let Some(tokens) = tokens {
            session.set(tokens, None).now();
        }
        Ok(Client { inner: Arc::new(Inner { http, session, timeout: self.timeout, refreshing: Mutex::new(None) }) })
    }
}

type RefreshOutcome = Option<Result<TokenPair, Error>>;

/// A deadline `timeout` from now.
fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout).unwrap_or(now)
}

/// A call's deadline as accepted: 1 ms..=[`MAX_TIMEOUT`].
fn clamp_timeout(timeout: Duration) -> Duration {
    timeout.clamp(Duration::from_millis(1), MAX_TIMEOUT)
}

pub(crate) struct Inner {
    pub(crate) http: Http,
    pub(crate) session: Session,
    pub(crate) timeout: Duration,
    /// The refresh in flight (single-flight): every caller that needs a token waits for this one.
    refreshing: Mutex<Option<watch::Receiver<RefreshOutcome>>>,
}

/// The async client (tokio). Cheap to clone: clones share the connection pool and the session.
///
/// Every call has ONE deadline ([`ClientBuilder::timeout`]) and gets exactly one answer. Calls
/// that need an access token refresh it first when it is about to expire (one refresh at a time,
/// shared by every caller), and a call answered 401 is retried once after a refresh.
///
/// Call it from inside a tokio runtime (outside one the call answers `InvalidRequest`, never a
/// panic); keep one client per runtime. Without a runtime use [`blocking::Client`](crate::blocking::Client).
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<Inner>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").field("server", &self.inner.http.base.url("")).field("logged_in", &self.inner.session.tokens().is_some()).finish()
    }
}

impl Client {
    /// A client for the server at `url` (`https://api.example.com`, or `http://127.0.0.1:8080`
    /// for a local server), with the default settings.
    pub fn new(url: &str) -> Result<Self, Error> {
        Self::builder(url).build()
    }

    /// Settings for a client.
    pub fn builder(url: &str) -> ClientBuilder {
        ClientBuilder::new(url)
    }

    fn deadline(&self) -> Instant {
        deadline_after(self.inner.timeout)
    }

    /// Any typed call of the protocol (or a game's own route implementing
    /// [`HttpCall`]): its method, path and payload, the Bearer token when the route needs one (refreshed first if it is
    /// about to expire) and on logout (which takes it instead of the refresh token), the answer decoded as `C::Response`, errors as [`Error::Api`] with the
    /// server's code.
    ///
    /// Login, registration, Steam login and logout go through the session methods
    /// ([`login`](Self::login), …) so the tokens are kept; calling them here works too but the
    /// session does not see the tokens.
    pub async fn call<C: HttpCall>(&self, call: &C) -> Result<C::Response, Error> {
        self.call_with_timeout(call, self.inner.timeout).await
    }

    /// [`call`](Self::call) with its own deadline instead of the builder's
    /// ([`ClientBuilder::timeout`]), clamped to 1 ms..=1 h: waiting for a token refresh,
    /// connecting, sending and reading the answer together. A longer deadline than the builder's
    /// holds too (a slow export, a large batch); a shorter one ends the call early with
    /// [`Error::Timeout`]. A token refresh the call waits for keeps its own deadline (it is shared).
    pub async fn call_with_timeout<C: HttpCall>(&self, call: &C, timeout: Duration) -> Result<C::Response, Error> {
        crate::runtime::current()?;
        let deadline = deadline_after(clamp_timeout(timeout));
        let out = Outgoing::for_call(call)?;
        if !C::ROUTE.auth {
            // Logout takes a Bearer token instead of the refresh token in the body: the current
            // access token goes along (unless expired), as `logout` sends it.
            let bearer = (C::ROUTE.path == net_backend_protocol::routes::auth::LOGOUT && !self.inner.session.expired())
                .then(|| self.inner.session.access().map(|(token, _, _)| token))
                .flatten();
            return self.inner.http.send(&out, bearer.as_ref().map(net_backend_protocol::AccessToken::expose), deadline).await?.decode();
        }
        self.send_authed(&out, deadline).await?.decode()
    }

    /// Send with the Bearer token; on a 401, refresh once and send once more.
    async fn send_authed(&self, out: &Outgoing, deadline: Instant) -> Result<Answer, Error> {
        let (token, generation) = self.access_token(deadline).await?;
        let answer = self.inner.http.send(out, Some(token.expose()), deadline).await?;
        if answer.status != 401 {
            return Ok(answer);
        }
        // A 401 means the handler never ran: one refresh (unless another caller already brought a
        // new token), then one more try.
        let current = self.inner.session.access().map(|(_, g, _)| g);
        if current == Some(generation) {
            self.refresh_shared(deadline).await?;
        }
        let (token, _) = self.access_token(deadline).await?;
        self.inner.http.send(out, Some(token.expose()), deadline).await
    }

    /// The access token to send now, refreshed first when it is about to expire. A refresh that
    /// fails for a passing reason (network, 5xx) while the old token is still valid keeps the old one.
    pub(crate) async fn access_token(&self, deadline: Instant) -> Result<(net_backend_protocol::AccessToken, u64), Error> {
        let (token, generation, wants_refresh) = self.inner.session.access().ok_or(Error::NotLoggedIn)?;
        if !wants_refresh {
            return Ok((token, generation));
        }
        match self.refresh_shared(deadline).await {
            Ok(pair) => {
                let generation = self.inner.session.access().map_or(generation, |(_, g, _)| g);
                Ok((pair.access_token, generation))
            }
            Err(error @ (Error::SessionEnded { .. } | Error::NotLoggedIn)) => Err(error),
            Err(error) if self.inner.session.expired() => Err(error),
            Err(error) => {
                tracing::debug!("net_backend_client: refresh failed ({error}); the current access token is still valid");
                Ok((token, generation))
            }
        }
    }

    /// Refresh single-flight: join the refresh in flight, or start one (as its own task: a caller
    /// that gives up never cancels a refresh that may already have reached the server).
    pub(crate) async fn refresh_shared(&self, deadline: Instant) -> Result<TokenPair, Error> {
        let handle = crate::runtime::current()?;
        let mut receiver = {
            let mut slot = self.inner.refreshing.lock().unwrap_or_else(PoisonError::into_inner);
            match slot.as_ref() {
                // (A refresh task that died without an answer has dropped its sender: start anew.)
                Some(receiver) if receiver.has_changed().is_ok() => receiver.clone(),
                _ => {
                    let (sender, receiver) = watch::channel::<RefreshOutcome>(None);
                    *slot = Some(receiver.clone());
                    let inner = Arc::clone(&self.inner);
                    handle.spawn(async move {
                        let outcome = refresh_task(&inner).await;
                        *inner.refreshing.lock().unwrap_or_else(PoisonError::into_inner) = None;
                        let _ = sender.send(Some(outcome));
                    });
                    receiver
                }
            }
        };
        loop {
            if let Some(outcome) = receiver.borrow_and_update().clone() {
                return outcome;
            }
            match tokio::time::timeout_at(deadline, receiver.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(Error::Shutdown),
                Err(_) => return Err(Error::timeout("not sent: the token refresh did not finish before the deadline", Some(false))),
            }
        }
    }

    /// Server facts: `GET /v1/info` (no session needed). Check
    /// `info.supports(net_backend_protocol::PROTOCOL_VERSION)` and the modules you need.
    pub async fn info(&self) -> Result<ServerInfo, Error> {
        self.call(&GetServerInfo::new()).await
    }

    /// Create an account and log in: the session keeps the tokens.
    pub async fn register(&self, request: RegisterRequest) -> Result<AuthSession, Error> {
        self.session_call(&request, false).await
    }

    /// Log in with email + password: the session keeps the tokens.
    pub async fn login(&self, request: LoginRequest) -> Result<AuthSession, Error> {
        self.session_call(&request, false).await
    }

    /// Log in (or create the account) with a Steam Web API ticket: the session keeps the tokens.
    /// No Bearer token is sent (with one, the server links Steam instead: [`link_steam`](Self::link_steam)).
    pub async fn login_steam(&self, request: SteamLoginRequest) -> Result<AuthSession, Error> {
        self.session_call(&request, false).await
    }

    /// Link Steam to the logged-in account (needs a login younger than 10 minutes, otherwise 403
    /// `reauthentication_required`): the same route with the Bearer token.
    pub async fn link_steam(&self, request: SteamLoginRequest) -> Result<AuthSession, Error> {
        self.session_call(&request, true).await
    }

    /// Log in (or create the account) with an OpenID Connect provider's ID token
    /// (`POST /v1/auth/oauth/{provider}`): the session keeps the tokens. No Bearer token is sent
    /// (with one, the server links the provider instead: [`link_oauth`](Self::link_oauth)). With
    /// the feature `oauth`, `sign_in_oauth` gets the token through the
    /// system browser first.
    pub async fn login_oauth(&self, login: OAuthLogin) -> Result<AuthSession, Error> {
        self.session_call(&login, false).await
    }

    /// Link an OpenID Connect provider account to the logged-in account (needs a login younger
    /// than 10 minutes, otherwise 403 `reauthentication_required`; a provider account linked to
    /// another account answers 409 `conflict`).
    pub async fn link_oauth(&self, login: OAuthLogin) -> Result<AuthSession, Error> {
        self.session_call(&login, true).await
    }

    /// Sign in at an OpenID Connect provider in the system browser ([`crate::oauth`]: PKCE, a
    /// loopback redirect; `open` gets the sign-in URL), then log in at the server with the ID
    /// token (feature `oauth`). The token endpoint is reached with this client's TLS settings and
    /// proxy setting, the proxy decided for the token endpoint's own host.
    #[cfg(feature = "oauth")]
    #[cfg_attr(docsrs, doc(cfg(feature = "oauth")))]
    pub async fn sign_in_oauth<F>(&self, provider: &str, flow: &crate::oauth::OAuthFlow, open: F) -> Result<AuthSession, Error>
    where
        F: FnOnce(&str) -> Result<(), String>,
    {
        let signed = flow.sign_in_with(self, open).await?;
        self.login_oauth(OAuthLogin::new(provider, signed.token())).await
    }

    /// [`sign_in_oauth`](Self::sign_in_oauth), linking the provider account to the logged-in
    /// account instead of logging in (needs a recent login).
    #[cfg(feature = "oauth")]
    #[cfg_attr(docsrs, doc(cfg(feature = "oauth")))]
    pub async fn link_oauth_sign_in<F>(&self, provider: &str, flow: &crate::oauth::OAuthFlow, open: F) -> Result<AuthSession, Error>
    where
        F: FnOnce(&str) -> Result<(), String>,
    {
        let signed = flow.sign_in_with(self, open).await?;
        self.link_oauth(OAuthLogin::new(provider, signed.token())).await
    }

    async fn session_call<C: HttpCall<Response = AuthSession>>(&self, call: &C, authed: bool) -> Result<AuthSession, Error> {
        crate::runtime::current()?;
        let deadline = self.deadline();
        let out = Outgoing::for_call(call)?;
        let answer = if authed { self.send_authed(&out, deadline).await? } else { self.inner.http.send(&out, None, deadline).await? };
        let session: AuthSession = answer.decode()?;
        self.inner.session.set(session.tokens.clone(), answer.server_now).finish().await;
        Ok(session)
    }

    /// Get a new token pair now (single-flight with any automatic refresh). The old refresh token
    /// is used up; [`token_updates`](Self::token_updates) reports the new pair.
    pub async fn refresh(&self) -> Result<TokenPair, Error> {
        crate::runtime::current()?;
        if self.inner.session.tokens().is_none() {
            return Err(Error::NotLoggedIn);
        }
        self.refresh_shared(self.deadline()).await
    }

    /// Log out this session (its tokens are revoked on the server; open WebSockets of the session
    /// are closed with 4001). Sends the refresh token in the body, so it works after the access
    /// token expired. On success (or when the server says the session is already gone) the tokens
    /// are dropped and [`token_updates`](Self::token_updates) reports `None`; on a network error
    /// they are kept (try again, or [`forget_session`](Self::forget_session)).
    pub async fn logout(&self) -> Result<(), Error> {
        self.logout_with(LogoutRequest::this_session()).await
    }

    /// Log out every session of the account (every device).
    pub async fn logout_everywhere(&self) -> Result<(), Error> {
        self.logout_with(LogoutRequest::everywhere()).await
    }

    async fn logout_with(&self, request: LogoutRequest) -> Result<(), Error> {
        crate::runtime::current()?;
        let deadline = self.deadline();
        // The session's lineage, not the pair's generation: a refresh that completes while the
        // logout is on its way rotates the pair of the SAME session, which the logout revokes too.
        let (tokens, lineage) = {
            let state = self.inner.session.lock();
            (state.tokens.clone().ok_or(Error::NotLoggedIn)?, state.lineage)
        };
        let request = request.with_refresh_token(tokens.refresh_token.clone());
        let out = Outgoing::for_call(&request)?;
        let bearer = (!self.inner.session.expired()).then(|| tokens.access_token.clone());
        let answer = self.inner.http.send(&out, bearer.as_ref().map(net_backend_protocol::AccessToken::expose), deadline).await?;
        match answer.decode::<net_backend_protocol::Ack>() {
            Ok(_) => {
                self.inner.session.clear_lineage(lineage).finish().await;
                Ok(())
            }
            Err(error) if error.status() == Some(401) => {
                self.inner.session.clear_lineage(lineage).finish().await;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Use these tokens (the app's stored session). The access token's expiry is taken from the
    /// pair; an expired one is refreshed at the next call. [`token_updates`](Self::token_updates)
    /// reports the pair. A [token file](ClientBuilder::token_file) is written on the calling
    /// thread before this returns.
    pub fn resume(&self, tokens: TokenPair) {
        self.inner.session.set(tokens, None).now();
    }

    /// Drop the tokens locally without telling the server (the session stays valid there until it
    /// expires or is logged out elsewhere). A [token file](ClientBuilder::token_file) is deleted
    /// on the calling thread before this returns.
    pub fn forget_session(&self) {
        self.inner.session.clear().now();
    }

    /// The current tokens, if logged in (store them like a password).
    pub fn tokens(&self) -> Option<TokenPair> {
        self.inner.session.tokens()
    }

    /// Whether there is a session (tokens).
    pub fn is_logged_in(&self) -> bool {
        self.inner.session.tokens().is_some()
    }

    /// Every change of the tokens: persist each new pair (the refresh token rotates).
    pub fn token_updates(&self) -> TokenUpdates {
        self.inner.session.subscribe()
    }

    /// The server's base URL as given.
    pub fn server_url(&self) -> String {
        self.inner.http.base.url("")
    }
}

/// One refresh, run as its own task. Rotation rules (the server's 30 s grace window answers the
/// SAME pair to a repeated refresh token; a reuse after it revokes the session):
/// - an attempt that surely never left (connect failure) is not a risk: the error goes back;
/// - an attempt whose fate is unknown (timeout, reset) is retried at once while it is younger than
///   20 s (the server answers the same pair if it got the first one);
/// - a refused refresh (401, 403 `banned`, `refresh_token_reused`) ends the session.
async fn refresh_task(inner: &Arc<Inner>) -> Result<TokenPair, Error> {
    let mut attempt: u32 = 0;
    loop {
        let (refresh_token, generation) = {
            let state = inner.session.lock();
            match state.tokens.as_ref() {
                Some(tokens) => (tokens.refresh_token.clone(), state.generation),
                None => return Err(Error::NotLoggedIn),
            }
        };
        let now = Instant::now();
        let deadline = now.checked_add(inner.timeout).unwrap_or(now);
        let out = Outgoing::for_call(&RefreshRequest::new(refresh_token))?;
        let result = match inner.http.send(&out, None, deadline).await {
            Ok(answer) => answer.decode::<TokenPair>().map(|pair| (pair, answer.server_now)),
            Err(error) => Err(error),
        };
        match result {
            Ok((pair, server_now)) => {
                // Checked and stored under one lock: a logout or a new login meanwhile wins.
                let Some(write) = inner.session.set_if(pair.clone(), server_now, generation) else {
                    // The session changed meanwhile (logout, a new login): this pair is not wanted.
                    return inner.session.tokens().ok_or(Error::NotLoggedIn);
                };
                write.finish().await;
                return Ok(pair);
            }
            Err(error) if error.ends_session() => {
                let code = error.code().unwrap_or(codes::UNAUTHORIZED).to_string();
                tracing::info!("net_backend_client: the session ended: the server refused the refresh ({code})");
                inner.session.clear_if(generation).finish().await;
                return Err(Error::SessionEnded { code });
            }
            Err(error) => {
                let maybe_sent = matches!(error, Error::Network { sent: None, .. } | Error::Timeout { sent: None, .. });
                if !maybe_sent {
                    return Err(error);
                }
                let since = {
                    let mut state = inner.session.lock();
                    if state.generation != generation {
                        return Err(error);
                    }
                    *state.uncertain_since.get_or_insert(now)
                };
                attempt = attempt.saturating_add(1);
                let pause = Duration::from_millis(250u64.saturating_mul(1 << attempt.min(4)));
                if attempt > 4 || Instant::now().saturating_duration_since(since).saturating_add(pause) >= UNCERTAIN_RETRY_WINDOW {
                    return Err(error);
                }
                tokio::time::sleep(pause).await;
            }
        }
    }
}
