//! [`OAuthService`]: checks ID tokens and logs players in through the accounts module.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use net_backend_protocol::auth::{AuthSession, LinkedIdentity};
use net_backend_protocol::codes;
use net_backend_protocol::oauth::OAuthToken;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::config::OAuthConfig;
use super::events::BeforeOAuthLogin;
use super::jwks::{CacheRules, Fetcher, KeyError, KeySet};
use super::jwt::{self, Rules, Verified};
use super::store;
use crate::auth::{AuthContext, AuthService, ReqInfo};
use crate::error::{AppError, Error};
use crate::hooks::HookCtx;
use crate::state::AppState;

struct Inner {
    config: OAuthConfig,
    providers: BTreeMap<String, Arc<KeySet>>,
    fetcher: Fetcher,
}

/// The OpenID Connect service (a state value: `Ext<OAuthService>`, `state.get::<OAuthService>()`).
/// Cheap to clone.
#[derive(Clone)]
pub struct OAuthService(Arc<Inner>);

impl std::fmt::Debug for OAuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthService").field("providers", &self.0.providers.keys().collect::<Vec<_>>()).finish_non_exhaustive()
    }
}

fn refused() -> AppError {
    AppError::new(codes::OAUTH_FAILED, "the ID token was refused; sign in at the provider again")
}

/// A token's replay key: the SHA-256 of its nonce, or of the whole token when it has none.
fn token_key(verified: &Verified, token: &str) -> String {
    let digest = match &verified.nonce {
        Some(nonce) => Sha256::digest(format!("nonce:{nonce}").as_bytes()),
        None => Sha256::digest(format!("token:{token}").as_bytes()),
    };
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl OAuthService {
    pub(crate) fn new(config: OAuthConfig) -> Result<Self, Error> {
        let rules = CacheRules::from_config(&config);
        let providers = config.resolve()?.into_iter().map(|p| (p.name.clone(), Arc::new(KeySet::new(p, rules)))).collect();
        let fetcher = Fetcher::new(Duration::from_secs(config.http_timeout_secs))?;
        Ok(Self(Arc::new(Inner { config, providers, fetcher })))
    }

    /// The settings.
    pub fn config(&self) -> &OAuthConfig {
        &self.0.config
    }

    /// The configured provider names.
    pub fn providers(&self) -> Vec<String> {
        self.0.providers.keys().cloned().collect()
    }

    /// Check an ID token of `provider` (signature, issuer, audience, times, nonce) WITHOUT using
    /// it up: what the token says, or the error a login would answer.
    async fn verify(&self, state: &AppState, provider: &KeySet, token: &OAuthToken) -> Result<Verified, AppError> {
        let config = &self.0.config;
        let id_token = token.id_token.expose();
        let parsed = jwt::parse(id_token, &provider.provider().algorithms).map_err(|refusal| {
            tracing::info!(provider = %provider.provider().name, reason = %refusal.0, "oauth: a token was refused");
            refused()
        })?;
        let key = match provider.key(&self.0.fetcher, parsed.kid.as_deref(), parsed.alg).await {
            Ok(key) => key,
            Err(KeyError::NoKey(reason)) => {
                tracing::info!(provider = %provider.provider().name, %reason, "oauth: a token was refused");
                return Err(refused());
            }
            Err(KeyError::Unavailable(reason)) => {
                tracing::warn!(provider = %provider.provider().name, %reason, "oauth: no keys to check a token");
                return Err(AppError::unavailable("the login provider's keys could not be fetched; retry later"));
            }
        };
        parsed.verify(&key).map_err(|refusal| {
            tracing::info!(provider = %provider.provider().name, reason = %refusal.0, "oauth: a token was refused");
            refused()
        })?;
        let rules = Rules {
            now: state.now().get().div_euclid(1000),
            skew: i64::try_from(config.clock_skew_secs).unwrap_or(600),
            max_age: i64::try_from(config.max_token_age_secs).unwrap_or(86_400),
            nonce: token.nonce.as_ref().map(|n| n.expose()),
            require_nonce: config.require_nonce,
        };
        jwt::check_claims(&parsed.claims, provider.provider(), rules).map_err(|refusal| {
            tracing::info!(provider = %provider.provider().name, reason = %refusal.0, "oauth: a token was refused");
            refused()
        })
    }

    /// `POST /v1/auth/oauth/{provider}`: check the token, use it up, then log in, link (with a
    /// Bearer token) or create the account.
    pub(crate) async fn login(
        &self,
        state: &AppState,
        info: &ReqInfo,
        current: Option<AuthContext>,
        provider: &str,
        token: OAuthToken,
    ) -> Result<AuthSession, AppError> {
        token.validate()?;
        let Some(keys) = self.0.providers.get(provider).cloned() else {
            return Err(AppError::not_found("no such login provider on this server"));
        };
        let auth = state.get::<AuthService>().ok_or_else(|| AppError::internal(std::io::Error::other("the oauth module needs the auth module")))?;
        let verified = self.verify(state, &keys, &token).await?;
        // Used once: the insert is the check (on every instance).
        let skew_ms = i64::try_from(self.0.config.clock_skew_secs).unwrap_or(600).saturating_mul(1000);
        let expires_at = verified.expires_at.saturating_mul(1000).saturating_add(skew_ms);
        let key = token_key(&verified, token.id_token.expose());
        match state.db().execute(&store::insert_used(provider, &key, expires_at)?).await {
            Ok(_) => {}
            Err(error) if error.is_unique_violation() => {
                tracing::info!(provider, "oauth: a nonce or token came a second time");
                return Err(AppError::new(codes::OAUTH_FAILED, "this sign-in was used already; sign in at the provider again"));
            }
            Err(error) => return Err(error.into()),
        }
        let event = BeforeOAuthLogin {
            provider: provider.to_string(),
            subject: verified.subject.clone(),
            issuer: verified.issuer.clone(),
            email: verified.email.clone(),
            email_verified: verified.email_verified,
            name: verified.name.clone(),
            linking_to: current.as_ref().map(|c| c.user_id),
        };
        state.hooks().run_before(&HookCtx::new(state.clone(), info.request_id.clone()), event).await?;
        let data = json!({ "provider": provider, "subject": verified.subject });
        let identity = LinkedIdentity::new(provider, verified.subject.clone());
        auth.identity_login(state, info, current, &identity, &keys.provider().label, data).await
    }

    /// Delete used nonces whose tokens expired; the number of rows.
    pub async fn purge(&self, state: &AppState) -> Result<u64, AppError> {
        let now = state.now().get();
        let mut total = 0;
        loop {
            let ids: Vec<i64> = state.db().fetch_all::<store::IdRow, _>(&store::expired(now, 1000)).await?.into_iter().map(|r| r.id).collect();
            if ids.is_empty() {
                return Ok(total);
            }
            total += state.db().execute(&store::delete_ids(&ids)).await?;
            if ids.len() < 1000 {
                return Ok(total);
            }
        }
    }
}
