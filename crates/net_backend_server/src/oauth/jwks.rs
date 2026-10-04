//! A provider's signing keys: the JWKS document, fetched over HTTPS and kept per provider.
//!
//! - **Where:** the provider's `jwks_uri`, or the `jwks_uri` of its discovery document
//!   (`{issuer}/.well-known/openid-configuration`, whose `issuer` must be the configured one;
//!   read once, then kept).
//! - **How long:** the answer's `Cache-Control: max-age` (at most `jwks_max_cache_secs`, at least
//!   60 s), else `jwks_cache_secs`.
//! - **Rotation:** a token whose key id is not among the kept keys fetches the keys again (at most
//!   once per `jwks_refetch_secs`), so a provider's new key works at once.
//! - **Failures:** when a fetch fails, the last keys stay usable for `jwks_stale_secs` after they
//!   were fetched; without usable keys the login answers 503 `unavailable`. One fetch per provider
//!   at a time; other logins wait for it.
//! - Only RSA keys (2048 bits or more) and P-256 keys meant for signatures are kept; others are
//!   skipped. Answers are read up to 256 KiB; URLs are never logged with a query.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::Value;
use tokio::sync::Mutex;

use super::config::{OAuthConfig, Provider};
use super::jwt::{b64, Alg, Jwk, PublicKey};
use crate::error::Error;

/// The largest answer read from a provider.
const MAX_ANSWER_BYTES: usize = 256 * 1024;

/// Why no key could be had.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyError {
    /// The provider's keys are known, but none fits the token (an unknown key id): the token is refused.
    NoKey(String),
    /// The keys could not be fetched and no usable earlier keys exist.
    Unavailable(String),
}

/// Keep the keys of a JWKS document this module can use.
pub(crate) fn parse_jwks(body: &[u8]) -> Result<Vec<Jwk>, String> {
    let value: Value = serde_json::from_slice(body).map_err(|_| "the keys document is not JSON".to_string())?;
    let keys = value.get("keys").and_then(Value::as_array).ok_or_else(|| "the keys document has no `keys` list".to_string())?;
    let mut out = Vec::new();
    for key in keys {
        let field = |name: &str| key.get(name).and_then(Value::as_str);
        if field("use").is_some_and(|u| u != "sig") {
            continue;
        }
        let kid = field("kid").filter(|k| k.len() <= 256).map(str::to_string);
        let alg = field("alg").map(str::to_string);
        let parsed = match field("kty") {
            Some("RSA") => match (field("n").and_then(b64), field("e").and_then(b64)) {
                (Some(n), Some(e)) => {
                    let n: Vec<u8> = n.into_iter().skip_while(|b| *b == 0).collect();
                    (n.len() >= 256 && n.len() <= 1024 && !e.is_empty() && e.len() <= 8).then_some(PublicKey::Rsa { n, e })
                }
                _ => None,
            },
            Some("EC") if field("crv") == Some("P-256") => match (field("x").and_then(b64), field("y").and_then(b64)) {
                (Some(x), Some(y)) if x.len() == 32 && y.len() == 32 => {
                    let mut point = Vec::with_capacity(65);
                    point.push(4);
                    point.extend_from_slice(&x);
                    point.extend_from_slice(&y);
                    Some(PublicKey::P256 { point })
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(key) = parsed {
            out.push(Jwk { kid, alg, key });
        }
    }
    Ok(out)
}

/// `max-age` of a `Cache-Control` value, seconds.
pub(crate) fn max_age(cache_control: &str) -> Option<u64> {
    cache_control.split(',').find_map(|directive| {
        let (name, value) = directive.trim().split_once('=')?;
        name.trim().eq_ignore_ascii_case("max-age").then(|| value.trim().trim_matches('"').parse().ok()).flatten()
    })
}

/// The key for a token: the one with its key id (and a fitting type), or, for a token without a
/// key id, the only key that fits.
pub(crate) fn pick(keys: &[Jwk], kid: Option<&str>, alg: Alg) -> Option<Jwk> {
    match kid {
        Some(kid) => keys.iter().find(|k| k.kid.as_deref() == Some(kid) && k.fits(alg)).cloned(),
        None => {
            let mut fitting = keys.iter().filter(|k| k.fits(alg));
            match (fitting.next(), fitting.next()) {
                (Some(only), None) => Some(only.clone()),
                _ => None,
            }
        }
    }
}

/// An HTTPS client for providers (hyper + rustls with ring, webpki roots).
#[derive(Clone)]
pub(crate) struct Fetcher {
    client: Client<HttpsConnector<HttpConnector>, Body>,
    timeout: Duration,
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher").field("timeout", &self.timeout).finish()
    }
}

/// A GET answer.
pub(crate) struct Fetched {
    pub(crate) body: axum::body::Bytes,
    pub(crate) max_age: Option<u64>,
}

impl Fetcher {
    pub(crate) fn new(timeout: Duration) -> Result<Self, Error> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(provider)
            .map_err(|e| Error::Startup(format!("OpenID Connect client TLS setup failed: {e}")))?
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(connector);
        Ok(Self { client, timeout })
    }

    /// GET `url` (https, or http on loopback: checked by the configuration), 200 only.
    pub(crate) async fn get(&self, url: &str) -> Result<Fetched, String> {
        let exchange = async {
            let request = http::Request::get(url)
                .header(http::header::ACCEPT, "application/json")
                .header(http::header::USER_AGENT, concat!("net_backend_server/", env!("CARGO_PKG_VERSION")))
                .body(Body::empty())
                .map_err(|_| "the URL is not valid".to_string())?;
            let response =
                self.client
                    .request(request)
                    .await
                    .map_err(|e| if e.is_connect() { "could not connect".to_string() } else { "connection error".to_string() })?;
            let status = response.status();
            let max_age = response.headers().get(http::header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).and_then(max_age);
            let body = axum::body::to_bytes(Body::new(response.into_body()), MAX_ANSWER_BYTES)
                .await
                .map_err(|_| "the answer could not be read (or is too large)".to_string())?;
            if status != http::StatusCode::OK {
                return Err(format!("HTTP {status}"));
            }
            Ok(Fetched { body, max_age })
        };
        match tokio::time::timeout(self.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(format!("no answer within {} s", self.timeout.as_secs())),
        }
    }
}

/// The settings the cache follows.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CacheRules {
    pub(crate) default_ttl: Duration,
    pub(crate) max_ttl: Duration,
    pub(crate) refetch: Duration,
    pub(crate) stale: Duration,
}

impl CacheRules {
    pub(crate) fn from_config(config: &OAuthConfig) -> Self {
        Self {
            default_ttl: Duration::from_secs(config.jwks_cache_secs),
            max_ttl: Duration::from_secs(config.jwks_max_cache_secs),
            refetch: Duration::from_secs(config.jwks_refetch_secs),
            stale: Duration::from_secs(config.jwks_stale_secs),
        }
    }
}

#[derive(Debug, Default)]
struct Kept {
    keys: Option<Arc<Vec<Jwk>>>,
    fetched_at: Option<Instant>,
    fresh_until: Option<Instant>,
    last_attempt: Option<Instant>,
    jwks_uri: Option<String>,
}

/// One provider's keys.
#[derive(Debug)]
pub(crate) struct KeySet {
    provider: Provider,
    rules: CacheRules,
    kept: Mutex<Kept>,
}

impl KeySet {
    pub(crate) fn new(provider: Provider, rules: CacheRules) -> Self {
        let kept = Kept { jwks_uri: provider.jwks_uri.clone(), ..Kept::default() };
        Self { provider, rules, kept: Mutex::new(kept) }
    }

    pub(crate) fn provider(&self) -> &Provider {
        &self.provider
    }

    /// The key for a token (see the module docs for when the keys are fetched again).
    pub(crate) async fn key(&self, fetcher: &Fetcher, kid: Option<&str>, alg: Alg) -> Result<Jwk, KeyError> {
        let mut kept = self.kept.lock().await;
        let now = Instant::now();
        let fresh = kept.keys.is_some() && kept.fresh_until.is_some_and(|until| now < until);
        let usable = fresh || (kept.keys.is_some() && kept.fetched_at.is_some_and(|at| now.duration_since(at) < self.rules.stale.max(self.rules.default_ttl)));
        let found = kept.keys.as_deref().and_then(|keys| pick(keys, kid, alg));
        if fresh {
            if let Some(key) = found {
                return Ok(key);
            }
        }
        let recently_tried = kept.last_attempt.is_some_and(|at| now.duration_since(at) < self.rules.refetch);
        if recently_tried {
            return match (usable, found) {
                (true, Some(key)) => Ok(key),
                (true, None) => Err(KeyError::NoKey("no key with this id (the keys were fetched moments ago)".into())),
                (false, _) => Err(KeyError::Unavailable("the provider's keys could not be fetched".into())),
            };
        }
        kept.last_attempt = Some(now);
        match self.fetch(fetcher, &mut kept).await {
            Ok((keys, ttl)) => {
                let key = pick(&keys, kid, alg);
                kept.keys = Some(Arc::new(keys));
                kept.fetched_at = Some(now);
                kept.fresh_until = now.checked_add(ttl);
                key.ok_or_else(|| KeyError::NoKey("no key with this id".into()))
            }
            Err(error) => {
                tracing::warn!(provider = %self.provider.name, %error, "oauth: fetching the provider's keys failed");
                match (usable, found) {
                    (true, Some(key)) => Ok(key),
                    (true, None) => Err(KeyError::NoKey("no key with this id (and the keys could not be fetched again)".into())),
                    (false, _) => Err(KeyError::Unavailable(error)),
                }
            }
        }
    }

    async fn fetch(&self, fetcher: &Fetcher, kept: &mut Kept) -> Result<(Vec<Jwk>, Duration), String> {
        let uri = match &kept.jwks_uri {
            Some(uri) => uri.clone(),
            None => {
                let uri = self.discover(fetcher).await?;
                kept.jwks_uri = Some(uri.clone());
                uri
            }
        };
        let answer = fetcher.get(&uri).await.map_err(|e| format!("keys: {e}"))?;
        let keys = parse_jwks(&answer.body)?;
        if keys.is_empty() {
            return Err("the keys document has no usable key".into());
        }
        let ttl = answer.max_age.map_or(self.rules.default_ttl, |secs| Duration::from_secs(secs).clamp(Duration::from_secs(60), self.rules.max_ttl));
        Ok((keys, ttl))
    }

    /// The `jwks_uri` of the issuer's discovery document.
    async fn discover(&self, fetcher: &Fetcher) -> Result<String, String> {
        let url = format!("{}/.well-known/openid-configuration", self.provider.issuer.trim_end_matches('/'));
        let answer = fetcher.get(&url).await.map_err(|e| format!("discovery: {e}"))?;
        let document: Value = serde_json::from_slice(&answer.body).map_err(|_| "the discovery document is not JSON".to_string())?;
        if document.get("issuer").and_then(Value::as_str) != Some(self.provider.issuer.as_str()) {
            return Err("the discovery document names another issuer".into());
        }
        let uri = document.get("jwks_uri").and_then(Value::as_str).ok_or_else(|| "the discovery document has no jwks_uri".to_string())?;
        let plain_ok = uri.starts_with("http://") && crate::auth::config::is_loopback_url(uri);
        if !(uri.starts_with("https://") || plain_ok) || uri.len() > 2048 {
            return Err("the discovery document's jwks_uri is not https://".into());
        }
        Ok(uri.to_string())
    }
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use serde_json::json;

    use super::*;

    #[test]
    fn documents() {
        let x = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let y = URL_SAFE_NO_PAD.encode([2u8; 32]);
        let n = URL_SAFE_NO_PAD.encode([0x80u8; 256]);
        let doc = json!({"keys": [
            {"kty": "EC", "crv": "P-256", "kid": "e1", "x": x, "y": y},
            {"kty": "RSA", "kid": "r1", "alg": "RS256", "use": "sig", "n": n, "e": "AQAB"},
            {"kty": "RSA", "kid": "enc", "use": "enc", "n": n, "e": "AQAB"},
            {"kty": "RSA", "kid": "small", "n": URL_SAFE_NO_PAD.encode([0x80u8; 128]), "e": "AQAB"},
            {"kty": "EC", "crv": "P-384", "kid": "p384", "x": x, "y": y},
            {"kty": "oct", "kid": "hmac", "k": "c2VjcmV0"}
        ]});
        let keys = parse_jwks(doc.to_string().as_bytes()).unwrap_or_default();
        let kids: Vec<_> = keys.iter().filter_map(|k| k.kid.clone()).collect();
        assert_eq!(kids, vec!["e1".to_string(), "r1".to_string()]);
        assert!(matches!(&keys[0].key, PublicKey::P256 { point } if point.len() == 65 && point[0] == 4));
        assert!(parse_jwks(b"[]").is_err() && parse_jwks(b"nope").is_err());
        assert_eq!(pick(&keys, Some("e1"), Alg::Es256).and_then(|k| k.kid), Some("e1".into()));
        assert_eq!(pick(&keys, Some("e1"), Alg::Rs256), None, "the type must fit");
        assert_eq!(pick(&keys, None, Alg::Rs256).and_then(|k| k.kid), Some("r1".into()), "the only RSA key");
        assert_eq!(pick(&keys, Some("nope"), Alg::Rs256), None);
    }

    #[test]
    fn cache_control() {
        assert_eq!(max_age("public, max-age=21600, must-revalidate"), Some(21600));
        assert_eq!(max_age("MAX-AGE=\"60\""), Some(60));
        assert_eq!(max_age("no-store"), None);
        assert_eq!(max_age("max-age=abc"), None);
    }
}
