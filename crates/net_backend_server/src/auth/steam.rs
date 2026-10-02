//! Steam login: a [`SteamVerifier`] checks a Web API ticket and answers the SteamID.
//!
//! - `SteamWebApiVerifier` (feature `steam`): `GET {base}/ISteamUserAuth/AuthenticateUserTicket/v1/`
//!   with `key` (the publisher Web API key), `appid`, `ticket` (hex) and `identity` (the string the
//!   game passed to `GetAuthTicketForWebApi`; tickets for another identity are refused by Steam).
//!   Official reference: <https://partner.steamgames.com/doc/webapi/ISteamUserAuth> ("requires a
//!   publisher API key … MUST be called from a secure server"). The answer is read strictly: only
//!   `response.params` with `result: "OK"` and a non-zero `steamid` succeeds (`ownersteamid`,
//!   `vacbanned`, `publisherbanned` optional); `response.error.errordesc` is a refusal; anything
//!   else is a failure. HTTPS through hyper + rustls with ring and the webpki roots.
//! - [`FakeSteamVerifier`]: a fixed table of tickets, for tests and local development.
//! - An app may implement [`SteamVerifier`] itself (another HTTP client, a cache).
//!
//! The Web API key travels in the query string, as Steam requires; the server never logs URLs.

use std::collections::HashMap;
use std::sync::Mutex;

use futures_util::future::BoxFuture;

/// What Steam said about a valid ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SteamIdentity {
    /// The player's SteamID64.
    pub steam_id: u64,
    /// The owner of the game copy when it is borrowed (Family Sharing); `None` or equal to
    /// `steam_id` otherwise.
    pub owner_steam_id: Option<u64>,
    /// The account has a VAC ban.
    pub vac_banned: bool,
    /// The publisher banned the account.
    pub publisher_banned: bool,
}

impl SteamIdentity {
    /// A valid ticket of this player, owning the game, without bans.
    pub fn new(steam_id: u64) -> Self {
        Self { steam_id, owner_steam_id: None, vac_banned: false, publisher_banned: false }
    }

    /// The same identity playing a copy borrowed from `owner`.
    pub fn with_owner(mut self, owner: u64) -> Self {
        self.owner_steam_id = Some(owner);
        self
    }

    /// The same identity with ban flags.
    pub fn with_bans(mut self, vac_banned: bool, publisher_banned: bool) -> Self {
        self.vac_banned = vac_banned;
        self.publisher_banned = publisher_banned;
        self
    }

    /// Whether the copy is borrowed (the owner is someone else).
    pub fn is_borrowed(&self) -> bool {
        self.owner_steam_id.is_some_and(|owner| owner != 0 && owner != self.steam_id)
    }
}

/// Why a ticket was not accepted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SteamError {
    /// Steam refused the ticket (invalid, expired, another identity or app).
    #[error("Steam refused the ticket: {0}")]
    Rejected(String),
    /// Steam could not be asked (network, timeout, an unexpected answer, a refused key).
    #[error("Steam could not be asked: {0}")]
    Unavailable(String),
}

/// Checks a Steam Web API ticket.
pub trait SteamVerifier: Send + Sync + 'static {
    /// Check `ticket_hex` (validated hex) for `identity`.
    fn verify<'a>(&'a self, ticket_hex: &'a str, identity: &'a str) -> BoxFuture<'a, Result<SteamIdentity, SteamError>>;
}

impl<V: SteamVerifier> SteamVerifier for std::sync::Arc<V> {
    fn verify<'a>(&'a self, ticket_hex: &'a str, identity: &'a str) -> BoxFuture<'a, Result<SteamIdentity, SteamError>> {
        (**self).verify(ticket_hex, identity)
    }
}

/// A verifier with a fixed table of tickets (tests, local development). Unknown tickets are
/// refused; a ticket checked for another identity than it was added with is refused too.
///
/// ```
/// use net_backend_server::auth::steam::{FakeSteamVerifier, SteamIdentity};
///
/// let steam = FakeSteamVerifier::new("my-game").with_ticket("0a0b0c", SteamIdentity::new(76561190000000001));
/// // .module(Auth::new().steam_verifier(steam))
/// # let _ = steam;
/// ```
#[derive(Debug)]
pub struct FakeSteamVerifier {
    identity: String,
    tickets: Mutex<HashMap<String, SteamIdentity>>,
    unavailable: Mutex<bool>,
}

impl FakeSteamVerifier {
    /// A fake that accepts tickets for this identity string.
    pub fn new(identity: impl Into<String>) -> Self {
        Self { identity: identity.into(), tickets: Mutex::new(HashMap::new()), unavailable: Mutex::new(false) }
    }

    /// The same fake accepting `ticket_hex` (compared case-insensitively) as `identity`.
    pub fn with_ticket(self, ticket_hex: impl Into<String>, identity: SteamIdentity) -> Self {
        self.add_ticket(ticket_hex, identity);
        self
    }

    /// Accept one more ticket.
    pub fn add_ticket(&self, ticket_hex: impl Into<String>, identity: SteamIdentity) {
        self.tickets.lock().unwrap_or_else(|e| e.into_inner()).insert(ticket_hex.into().to_ascii_lowercase(), identity);
    }

    /// Act as if Steam could not be reached.
    pub fn set_unavailable(&self, unavailable: bool) {
        *self.unavailable.lock().unwrap_or_else(|e| e.into_inner()) = unavailable;
    }
}

impl SteamVerifier for FakeSteamVerifier {
    fn verify<'a>(&'a self, ticket_hex: &'a str, identity: &'a str) -> BoxFuture<'a, Result<SteamIdentity, SteamError>> {
        Box::pin(async move {
            if *self.unavailable.lock().unwrap_or_else(|e| e.into_inner()) {
                return Err(SteamError::Unavailable("the fake is set to unavailable".into()));
            }
            if identity != self.identity {
                return Err(SteamError::Rejected("Invalid identity".into()));
            }
            self.tickets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&ticket_hex.to_ascii_lowercase())
                .cloned()
                .ok_or_else(|| SteamError::Rejected("Invalid ticket".into()))
        })
    }
}

/// Parse the body of `AuthenticateUserTicket`.
#[cfg(any(feature = "steam", test))]
pub(crate) fn parse_answer(body: &[u8]) -> Result<SteamIdentity, SteamError> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| SteamError::Unavailable("the answer is not JSON".into()))?;
    let response = value.get("response").ok_or_else(|| SteamError::Unavailable("the answer has no `response`".into()))?;
    if let Some(error) = response.get("error") {
        let description = error.get("errordesc").and_then(|d| d.as_str()).unwrap_or("unknown error");
        let code = error.get("errorcode").and_then(|c| c.as_i64()).unwrap_or(0);
        // Short, plain text only (it ends up in logs).
        let description: String = description.chars().filter(|c| !c.is_control()).take(120).collect();
        return Err(SteamError::Rejected(format!("{description} (code {code})")));
    }
    let params = response.get("params").ok_or_else(|| SteamError::Unavailable("the answer has neither `params` nor `error`".into()))?;
    // Fail closed: only the success shape counts (`result` must be "OK"; missing = not understood).
    match params.get("result").and_then(|r| r.as_str()) {
        Some("OK") => {}
        Some(result) => {
            let result: String = result.chars().filter(|c| !c.is_control()).take(60).collect();
            return Err(SteamError::Rejected(format!("result {result}")));
        }
        None => return Err(SteamError::Unavailable("the answer has no `result`".into())),
    }
    let id = |name: &str| -> Option<u64> {
        let field = params.get(name)?;
        field.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| field.as_u64())
    };
    let steam_id = id("steamid").filter(|id| *id != 0).ok_or_else(|| SteamError::Unavailable("the answer has no steamid".into()))?;
    let flag = |name: &str| params.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(SteamIdentity {
        steam_id,
        owner_steam_id: id("ownersteamid").filter(|o| *o != 0),
        vac_banned: flag("vacbanned"),
        publisher_banned: flag("publisherbanned"),
    })
}

/// Percent-encode a query value (RFC 3986 unreserved characters pass).
#[cfg(any(feature = "steam", test))]
pub(crate) fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(feature = "steam")]
#[cfg_attr(docsrs, doc(cfg(feature = "steam")))]
pub use web_api::SteamWebApiVerifier;

#[cfg(feature = "steam")]
mod web_api {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use futures_util::future::BoxFuture;
    use hyper_rustls::HttpsConnector;
    use hyper_util::client::legacy::connect::HttpConnector;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    use super::{encode_query, parse_answer, SteamError, SteamIdentity, SteamVerifier};
    use crate::config::SecretString;
    use crate::error::Error;

    /// The largest answer read from Steam.
    const MAX_ANSWER_BYTES: usize = 64 * 1024;

    /// The real verifier: Steam's `ISteamUserAuth/AuthenticateUserTicket` Web API over HTTPS
    /// (hyper + rustls with ring, webpki roots; no OpenSSL). Cargo feature `steam`.
    pub struct SteamWebApiVerifier {
        client: Client<HttpsConnector<HttpConnector>, Body>,
        base_url: String,
        key: SecretString,
        app_id: u32,
        timeout: Duration,
    }

    impl std::fmt::Debug for SteamWebApiVerifier {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SteamWebApiVerifier").field("base_url", &self.base_url).field("app_id", &self.app_id).field("key", &self.key).finish()
        }
    }

    impl SteamWebApiVerifier {
        /// A verifier for this app with this publisher key. `base_url` is
        /// `https://partner.steam-api.com` in production (`http://` only for loopback tests).
        pub fn new(base_url: impl Into<String>, key: SecretString, app_id: u32, timeout: Duration) -> Result<Self, Error> {
            let base_url = base_url.into();
            if !(base_url.starts_with("https://") || (base_url.starts_with("http://") && crate::auth::config::is_loopback_url(&base_url))) {
                return Err(Error::Config(vec!["the Steam Web API URL must be https:// (http:// only for a loopback address)".into()]));
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let connector = hyper_rustls::HttpsConnectorBuilder::new()
                .with_provider_and_webpki_roots(provider)
                .map_err(|e| Error::Startup(format!("Steam client TLS setup failed: {e}")))?
                .https_or_http()
                .enable_http1()
                .build();
            let client = Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(connector);
            Ok(Self { client, base_url: base_url.trim_end_matches('/').to_string(), key, app_id, timeout })
        }

        async fn ask(&self, ticket_hex: &str, identity: &str) -> Result<SteamIdentity, SteamError> {
            let url = format!(
                "{}/ISteamUserAuth/AuthenticateUserTicket/v1/?key={}&appid={}&ticket={}&identity={}",
                self.base_url,
                encode_query(self.key.expose()),
                self.app_id,
                encode_query(ticket_hex),
                encode_query(identity)
            );
            let request = http::Request::get(url)
                .header(http::header::ACCEPT, "application/json")
                .body(Body::empty())
                .map_err(|_| SteamError::Unavailable("could not build the request".into()))?;
            // Never format the request or its URI: the key is in the query string.
            let response = self.client.request(request).await.map_err(|e| SteamError::Unavailable(format!("request failed: {}", connection_error(&e))))?;
            let status = response.status();
            let body = axum::body::to_bytes(Body::new(response.into_body()), MAX_ANSWER_BYTES)
                .await
                .map_err(|_| SteamError::Unavailable("the answer could not be read (or is too large)".into()))?;
            match status.as_u16() {
                200 => parse_answer(&body),
                // Steam answers 400 with an error body for a bad ticket.
                400 => match parse_answer(&body) {
                    Err(SteamError::Rejected(reason)) => Err(SteamError::Rejected(reason)),
                    _ => Err(SteamError::Rejected("bad request".into())),
                },
                401 | 403 => Err(SteamError::Unavailable(format!("Steam refused the Web API key or app id (HTTP {status})"))),
                _ => Err(SteamError::Unavailable(format!("HTTP {status}"))),
            }
        }
    }

    /// A connection error's kind without its URL (hyper-util's message never contains the URI,
    /// but the source chain is reduced to its first line anyway).
    fn connection_error(error: &hyper_util::client::legacy::Error) -> String {
        if error.is_connect() {
            "could not connect".into()
        } else {
            "connection error".into()
        }
    }

    impl SteamVerifier for SteamWebApiVerifier {
        fn verify<'a>(&'a self, ticket_hex: &'a str, identity: &'a str) -> BoxFuture<'a, Result<SteamIdentity, SteamError>> {
            Box::pin(async move {
                match tokio::time::timeout(self.timeout, self.ask(ticket_hex, identity)).await {
                    Ok(result) => result,
                    Err(_) => Err(SteamError::Unavailable(format!("no answer within {} s", self.timeout.as_secs()))),
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        let ok = br#"{"response":{"params":{"result":"OK","steamid":"76561190000000001","ownersteamid":"76561190000000002","vacbanned":false,"publisherbanned":true}}}"#;
        let identity = parse_answer(ok).unwrap_or_else(|_| SteamIdentity::new(1));
        assert_eq!(identity.steam_id, 76561190000000001);
        assert_eq!(identity.owner_steam_id, Some(76561190000000002));
        assert!(identity.is_borrowed() && identity.publisher_banned && !identity.vac_banned);
        let own = parse_answer(br#"{"response":{"params":{"result":"OK","steamid":"5","ownersteamid":"5"}}}"#);
        assert!(own.is_ok_and(|i| !i.is_borrowed()));
        let refused = parse_answer(br#"{"response":{"error":{"errorcode":101,"errordesc":"Invalid ticket"}}}"#);
        assert_eq!(refused, Err(SteamError::Rejected("Invalid ticket (code 101)".into())));
        assert!(matches!(parse_answer(b"<html>"), Err(SteamError::Unavailable(_))));
        assert!(matches!(parse_answer(br#"{"response":{"params":{"result":"OK"}}}"#), Err(SteamError::Unavailable(_))));
        assert!(matches!(parse_answer(br#"{"response":{"params":{"steamid":"5"}}}"#), Err(SteamError::Unavailable(_))), "no result: not accepted");
        assert!(matches!(parse_answer(br#"{"response":{"params":{"result":"OK","steamid":"0"}}}"#), Err(SteamError::Unavailable(_))));
        assert!(matches!(parse_answer(br#"{"response":{"params":{"result":"Invalid","steamid":"5"}}}"#), Err(SteamError::Rejected(_))));
        assert_eq!(encode_query("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }

    #[tokio::test]
    async fn fake() {
        let fake = FakeSteamVerifier::new("game").with_ticket("0A0B", SteamIdentity::new(7));
        assert_eq!(fake.verify("0a0b", "game").await.map(|i| i.steam_id), Ok(7));
        assert!(matches!(fake.verify("0a0b", "other").await, Err(SteamError::Rejected(_))));
        assert!(matches!(fake.verify("ffff", "game").await, Err(SteamError::Rejected(_))));
        fake.set_unavailable(true);
        assert!(matches!(fake.verify("0a0b", "game").await, Err(SteamError::Unavailable(_))));
    }
}
