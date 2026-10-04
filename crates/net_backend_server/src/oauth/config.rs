//! The OpenID Connect module's settings: `[modules.oauth]` in the configuration (or
//! [`OAuth::with_config`](crate::oauth::OAuth::with_config)).
//!
//! ```toml
//! [modules.oauth]
//! clock_skew_secs = 60           # tolerance for exp / iat / nbf
//! max_token_age_secs = 600       # an ID token older than this (iat) is refused
//! require_nonce = true           # every login sends the nonce of its sign-in; each nonce counts once
//! login_per_minute = 10          # logins per client address and minute (0 = no limit)
//!
//! [modules.oauth.providers.google]
//! preset = "google"              # issuer, accepted issuers, keys URL and RS256 for Google
//! client_ids = ["1234-abc.apps.googleusercontent.com"]
//!
//! [modules.oauth.providers.company]
//! issuer = "https://login.example.com"   # keys found through /.well-known/openid-configuration
//! client_ids = ["game-desktop"]
//! algorithms = ["RS256", "ES256"]
//! ```
//!
//! Without any provider the module is registered but every login answers 404.

use std::collections::BTreeMap;

use net_backend_protocol::auth::{is_valid_provider, provider};
use serde::Deserialize;

use super::jwt::Alg;
use crate::auth::config::is_loopback_url;
use crate::error::Error;

/// Google's issuer.
pub const GOOGLE_ISSUER: &str = "https://accounts.google.com";
/// Google's keys (JWKS).
pub const GOOGLE_JWKS_URI: &str = "https://www.googleapis.com/oauth2/v3/certs";

/// The OpenID Connect module's settings. Build in code from [`OAuthConfig::default`] by changing
/// fields, or let the module read `[modules.oauth]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct OAuthConfig {
    /// The providers, by the name used in the route and as the linked identity's `provider`
    /// (`[a-z][a-z0-9_]*`, at most 32 bytes; not `steam`).
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Clock tolerance for `exp`, `iat` and `nbf`, seconds. Default 60; at most 600.
    pub clock_skew_secs: u64,
    /// The oldest ID token accepted (`iat`), seconds. Default 600; 60 to 86 400.
    pub max_token_age_secs: u64,
    /// Every login must send the nonce of its sign-in and the ID token must carry the same one;
    /// each nonce is accepted once. Default true. When false, a token without a nonce is accepted
    /// (each such token once).
    pub require_nonce: bool,
    /// Logins per client address and minute (`POST /v1/auth/oauth/{provider}`). Default 10; 0 = no
    /// limit.
    pub login_per_minute: u32,
    /// The time limit of one request to a provider (discovery, keys), seconds. Default 10; 1 to 60.
    pub http_timeout_secs: u64,
    /// How long fetched keys are used when the provider's answer has no `Cache-Control: max-age`,
    /// seconds. Default 3600.
    pub jwks_cache_secs: u64,
    /// The longest a provider's `max-age` is followed, seconds. Default 86 400.
    pub jwks_max_cache_secs: u64,
    /// The shortest time between two fetches of a provider's keys (a token with an unknown key id
    /// fetches them again, at most this often), seconds. Default 60; 1 to 3600.
    pub jwks_refetch_secs: u64,
    /// When the keys cannot be fetched, the last keys stay usable this long after they were
    /// fetched, seconds. Default 86 400.
    pub jwks_stale_secs: u64,
    /// How often used nonces are purged, seconds. Default 3600; 0 = never.
    pub purge_interval_secs: u64,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            providers: BTreeMap::new(),
            clock_skew_secs: 60,
            max_token_age_secs: 600,
            require_nonce: true,
            login_per_minute: 10,
            http_timeout_secs: 10,
            jwks_cache_secs: 3600,
            jwks_max_cache_secs: 86_400,
            jwks_refetch_secs: 60,
            jwks_stale_secs: 86_400,
            purge_interval_secs: 3600,
        }
    }
}

/// One OpenID Connect provider.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct ProviderConfig {
    /// `google`: Google's issuer (`https://accounts.google.com`, also accepted without `https://`
    /// as Google writes it in some tokens), its keys URL and RS256. Fields set here win.
    pub preset: Option<String>,
    /// The provider's name in mails ("A Google account was linked …"). Default: the preset's
    /// (`Google`), else the provider's key.
    pub display_name: Option<String>,
    /// The issuer (`iss`); with no `jwks_uri` its `/.well-known/openid-configuration` names the
    /// keys URL. `https://` (`http://` only for a loopback address).
    pub issuer: Option<String>,
    /// Every `iss` value accepted. Default: `[issuer]` (the Google preset adds `accounts.google.com`).
    pub accepted_issuers: Vec<String>,
    /// The keys URL (JWKS). Default: from the issuer's discovery document.
    pub jwks_uri: Option<String>,
    /// The game's client ids at the provider: the token's `aud` must contain one of them (and its
    /// `azp`, when present, must be one of them). Required.
    pub client_ids: Vec<String>,
    /// The signature algorithms accepted: `RS256`, `ES256`. Default both (the Google preset:
    /// `RS256`). Nothing else is ever accepted (no `none`, no HMAC).
    pub algorithms: Vec<String>,
}

impl ProviderConfig {
    /// The Google preset with these client ids.
    pub fn google(client_ids: Vec<String>) -> Self {
        Self { preset: Some("google".into()), client_ids, ..Self::default() }
    }

    /// A provider with this issuer (keys from its discovery document) and these client ids.
    pub fn new(issuer: impl Into<String>, client_ids: Vec<String>) -> Self {
        Self { issuer: Some(issuer.into()), client_ids, ..Self::default() }
    }
}

/// A provider after presets and defaults.
#[derive(Clone, Debug)]
pub(crate) struct Provider {
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) issuer: String,
    pub(crate) issuers: Vec<String>,
    pub(crate) jwks_uri: Option<String>,
    pub(crate) client_ids: Vec<String>,
    pub(crate) algorithms: Vec<Alg>,
}

fn is_web_url(url: &str) -> bool {
    let plain = url.starts_with("http://") && is_loopback_url(url);
    (url.starts_with("https://") || plain) && url.len() <= 2048 && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

impl OAuthConfig {
    /// Check every setting.
    pub fn validate(&self) -> Result<(), Error> {
        self.resolve().map(|_| ())
    }

    /// Every provider with its preset applied, or every problem.
    pub(crate) fn resolve(&self) -> Result<Vec<Provider>, Error> {
        let mut problems = Vec::new();
        if self.clock_skew_secs > 600 {
            problems.push("modules.oauth.clock_skew_secs must be at most 600".to_string());
        }
        if !(60..=86_400).contains(&self.max_token_age_secs) {
            problems.push("modules.oauth.max_token_age_secs must be between 60 and 86400".to_string());
        }
        if !(1..=60).contains(&self.http_timeout_secs) {
            problems.push("modules.oauth.http_timeout_secs must be between 1 and 60".to_string());
        }
        if !(60..=86_400).contains(&self.jwks_cache_secs) || !(60..=7 * 86_400).contains(&self.jwks_max_cache_secs) {
            problems.push("modules.oauth.jwks_cache_secs must be between 60 and 86400, jwks_max_cache_secs between 60 and 604800".to_string());
        }
        if !(1..=3600).contains(&self.jwks_refetch_secs) {
            problems.push("modules.oauth.jwks_refetch_secs must be between 1 and 3600".to_string());
        }
        if self.jwks_stale_secs > 7 * 86_400 {
            problems.push("modules.oauth.jwks_stale_secs must be at most 604800".to_string());
        }
        let mut providers = Vec::new();
        for (name, config) in &self.providers {
            let at = format!("modules.oauth.providers.{name}");
            if !is_valid_provider(name) || name == provider::STEAM {
                problems.push(format!("{at}: the name must be [a-z][a-z0-9_]*, at most 32 bytes, and not `steam`"));
                continue;
            }
            let google = match config.preset.as_deref() {
                None => false,
                Some("google") => true,
                Some(other) => {
                    problems.push(format!("{at}.preset: unknown preset `{other}` (known: google)"));
                    continue;
                }
            };
            let issuer = config.issuer.clone().or_else(|| google.then(|| GOOGLE_ISSUER.to_string()));
            let Some(issuer) = issuer else {
                problems.push(format!("{at}.issuer is required (or preset = \"google\")"));
                continue;
            };
            if !is_web_url(&issuer) {
                problems.push(format!("{at}.issuer must be an https:// URL (http:// only for a loopback address)"));
            }
            let mut issuers = config.accepted_issuers.clone();
            if issuers.is_empty() {
                issuers.push(issuer.clone());
                if google && issuer == GOOGLE_ISSUER {
                    issuers.push("accounts.google.com".into());
                }
            }
            if issuers.iter().any(|i| i.is_empty() || i.len() > 2048) {
                problems.push(format!("{at}.accepted_issuers: every issuer is 1 to 2048 bytes"));
            }
            let jwks_uri = config.jwks_uri.clone().or_else(|| (google && issuer == GOOGLE_ISSUER).then(|| GOOGLE_JWKS_URI.to_string()));
            if jwks_uri.as_deref().is_some_and(|u| !is_web_url(u)) {
                problems.push(format!("{at}.jwks_uri must be an https:// URL (http:// only for a loopback address)"));
            }
            if config.client_ids.is_empty() || config.client_ids.iter().any(|c| c.trim().is_empty() || c.len() > 255) {
                problems.push(format!("{at}.client_ids: at least one client id, each 1 to 255 bytes"));
            }
            let names: Vec<String> = if config.algorithms.is_empty() {
                if google {
                    vec!["RS256".into()]
                } else {
                    vec!["RS256".into(), "ES256".into()]
                }
            } else {
                config.algorithms.clone()
            };
            let mut algorithms = Vec::new();
            for alg in &names {
                match Alg::from_name(alg) {
                    Some(alg) if !algorithms.contains(&alg) => algorithms.push(alg),
                    Some(_) => {}
                    None => problems.push(format!("{at}.algorithms: `{alg}` is not accepted (only RS256 and ES256)")),
                }
            }
            let label = config.display_name.clone().unwrap_or_else(|| if google { "Google".into() } else { name.clone() });
            if label.trim().is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
                problems.push(format!("{at}.display_name must be 1 to 64 bytes without control characters"));
            }
            providers.push(Provider { name: name.clone(), label, issuer, issuers, jwks_uri, client_ids: config.client_ids.clone(), algorithms });
        }
        if problems.is_empty() {
            Ok(providers)
        } else {
            Err(Error::Config(problems))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(name: &str, provider: ProviderConfig) -> OAuthConfig {
        let mut config = OAuthConfig::default();
        config.providers.insert(name.into(), provider);
        config
    }

    #[test]
    fn google_preset_and_defaults() {
        assert!(OAuthConfig::default().resolve().is_ok_and(|p| p.is_empty()));
        let providers = with("google", ProviderConfig::google(vec!["abc.apps.googleusercontent.com".into()])).resolve().unwrap_or_default();
        let google = &providers[0];
        assert_eq!(google.issuer, GOOGLE_ISSUER);
        assert_eq!(google.issuers, vec![GOOGLE_ISSUER.to_string(), "accounts.google.com".to_string()]);
        assert_eq!(google.jwks_uri.as_deref(), Some(GOOGLE_JWKS_URI));
        assert_eq!(google.algorithms, vec![Alg::Rs256]);
        assert_eq!(google.label, "Google");
        let custom = with("company", ProviderConfig::new("https://login.example.com", vec!["game".into()])).resolve().unwrap_or_default();
        assert_eq!(custom[0].jwks_uri, None, "found through discovery");
        assert_eq!(custom[0].algorithms, vec![Alg::Rs256, Alg::Es256]);
        assert_eq!(custom[0].label, "company");
    }

    #[test]
    fn problems() {
        let error = |config: OAuthConfig| config.validate().err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error(with("steam", ProviderConfig::google(vec!["x".into()]))).contains("not `steam`"));
        assert!(error(with("Google", ProviderConfig::google(vec!["x".into()]))).contains("[a-z]"));
        assert!(error(with("g", ProviderConfig::google(vec![]))).contains("client_ids"));
        assert!(error(with("g", ProviderConfig::new("http://login.example.com", vec!["x".into()]))).contains("issuer must be"));
        assert!(error(with("g", ProviderConfig::new("http://127.0.0.1:9000", vec!["x".into()]))).is_empty(), "loopback http is fine");
        let mut none = ProviderConfig::new("https://a.example.com", vec!["x".into()]);
        none.algorithms = vec!["none".into(), "HS256".into()];
        let text = error(with("g", none));
        assert!(text.contains("`none` is not accepted") && text.contains("`HS256` is not accepted"), "{text}");
        let mut preset = ProviderConfig::google(vec!["x".into()]);
        preset.preset = Some("facebook".into());
        assert!(error(with("g", preset)).contains("unknown preset"));
        assert!(error(with("g", ProviderConfig::default())).contains("issuer is required"));
        let ranges = OAuthConfig { clock_skew_secs: 601, max_token_age_secs: 1, http_timeout_secs: 0, jwks_refetch_secs: 0, ..OAuthConfig::default() };
        let text = error(ranges);
        for part in ["clock_skew_secs", "max_token_age_secs", "http_timeout_secs", "jwks_refetch_secs"] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(toml::from_str::<OAuthConfig>("[providers.g]\nclient_id = \"x\"\n").is_err(), "unknown keys are refused");
        let parsed: OAuthConfig = toml::from_str("[providers.google]\npreset = \"google\"\nclient_ids = [\"a\"]\n").unwrap_or_default();
        assert!(parsed.resolve().is_ok_and(|p| p.len() == 1));
    }
}
