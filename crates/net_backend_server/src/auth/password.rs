//! Password hashing: argon2id (PHC strings), always on tokio's blocking pool and never more than
//! `hash_concurrency` at once, so a flood of logins can neither stall the async workers nor
//! allocate unbounded memory (each hash takes `argon2_memory_kib`). The comparison inside
//! `argon2` is constant-time (`phc::Output` compares with `ctutils`).

use std::sync::Arc;
use std::time::Duration;

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use tokio::sync::{OnceCell, Semaphore};

use super::config::AuthConfig;
use crate::error::AppError;

/// The hasher: parameters, the concurrency gate and the dummy hash for unknown accounts.
pub(crate) struct Hasher {
    params: Params,
    gate: Arc<Semaphore>,
    wait: Duration,
    dummy: OnceCell<String>,
}

impl std::fmt::Debug for Hasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hasher")
            .field("m_kib", &self.params.m_cost())
            .field("t", &self.params.t_cost())
            .field("p", &self.params.p_cost())
            .field("slots", &self.gate.available_permits())
            .finish()
    }
}

fn internal(context: &str) -> AppError {
    AppError::internal(std::io::Error::other(context.to_string()))
}

impl Hasher {
    pub(crate) fn new(config: &AuthConfig) -> Result<Hasher, crate::Error> {
        let params = Params::new(config.argon2_memory_kib, config.argon2_iterations, config.argon2_parallelism, None)
            .map_err(|e| crate::Error::Config(vec![format!("modules.auth: invalid argon2 parameters: {e}")]))?;
        let slots = if config.hash_concurrency == 0 { std::thread::available_parallelism().map_or(2, |n| n.get()) } else { config.hash_concurrency };
        Ok(Hasher { params, gate: Arc::new(Semaphore::new(slots)), wait: Duration::from_secs(config.hash_queue_timeout_secs), dummy: OnceCell::new() })
    }

    fn argon2(params: Params) -> Argon2<'static> {
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
    }

    /// Run `work` on the blocking pool once a hashing slot is free (503 after waiting too long).
    async fn run<T: Send + 'static>(&self, work: impl FnOnce() -> T + Send + 'static) -> Result<T, AppError> {
        let permit = match tokio::time::timeout(self.wait, self.gate.clone().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(internal("the password hashing gate is closed")),
            Err(_) => {
                tracing::warn!("password hashing is saturated; a request waited too long");
                return Err(AppError::unavailable("the server is busy, retry later"));
            }
        };
        let result = tokio::task::spawn_blocking(move || {
            let out = work();
            drop(permit);
            out
        })
        .await;
        result.map_err(|_| internal("a password hashing task failed"))
    }

    /// The PHC string of `password` (a new random salt).
    pub(crate) async fn hash(&self, password: String) -> Result<String, AppError> {
        let params = self.params.clone();
        self.run(move || Self::argon2(params).hash_password(password.as_bytes()).map(|h| h.to_string()).map_err(|e| e.to_string()))
            .await?
            .map_err(|e| AppError::internal(std::io::Error::other(format!("password hashing failed: {e}"))))
    }

    /// Whether `password` matches `stored` (a PHC string). Without a stored hash (an unknown
    /// account, or one without a password) it checks against a dummy hash with the same cost and
    /// answers false: the same work either way, so the timing does not tell whether the account
    /// exists.
    pub(crate) async fn verify(&self, password: String, stored: Option<String>) -> Result<bool, AppError> {
        let (stored, real) = match stored {
            Some(stored) => (stored, true),
            None => (self.dummy().await?, false),
        };
        let matches = self
            .run(move || match PasswordHash::new(&stored) {
                Ok(hash) => Argon2::default().verify_password(password.as_bytes(), &hash).is_ok(),
                Err(_) => false,
            })
            .await?;
        Ok(real && matches)
    }

    /// Whether a stored hash uses other parameters than the configured ones (rehash after a
    /// successful login).
    pub(crate) fn needs_rehash(&self, stored: &str) -> bool {
        match PasswordHash::new(stored) {
            Ok(hash) => {
                hash.algorithm.as_str() != "argon2id"
                    || Params::try_from(&hash)
                        .is_ok_and(|p| p.m_cost() != self.params.m_cost() || p.t_cost() != self.params.t_cost() || p.p_cost() != self.params.p_cost())
            }
            Err(_) => true,
        }
    }

    /// Build the dummy hash now (called when the module starts): otherwise the first login for an
    /// unknown address would cost two hashes.
    pub(crate) async fn warm_up(&self) -> Result<(), AppError> {
        self.dummy().await.map(|_| ())
    }

    async fn dummy(&self) -> Result<String, AppError> {
        self.dummy
            .get_or_try_init(|| async {
                let bytes = super::tokens::random_bytes::<16>()?;
                let text: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                self.hash(text).await
            })
            .await
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap() -> Hasher {
        let config = AuthConfig { argon2_memory_kib: 64, argon2_iterations: 1, hash_concurrency: 2, ..AuthConfig::default() };
        Hasher::new(&config).unwrap_or_else(|_| panic!("hasher"))
    }

    #[tokio::test]
    async fn hash_verify_and_rehash() {
        let hasher = cheap();
        let phc = hasher.hash("correct horse battery".into()).await.unwrap_or_default();
        assert!(phc.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{phc}");
        assert!(hasher.verify("correct horse battery".into(), Some(phc.clone())).await.unwrap_or(false));
        assert!(!hasher.verify("wrong horse battery".into(), Some(phc.clone())).await.unwrap_or(true));
        assert!(hasher.dummy.get().is_none());
        assert!(!hasher.verify("correct horse battery".into(), None).await.unwrap_or(true), "no stored hash: never a match");
        // An unknown account costs the same argon2 work: a dummy hash with the same parameters.
        assert!(hasher.dummy.get().is_some_and(|d| d.starts_with("$argon2id$v=19$m=64,t=1,p=1$")));
        assert!(!hasher.verify("x".into(), Some("not a phc string".into())).await.unwrap_or(true));
        assert!(!hasher.needs_rehash(&phc));
        let stronger = Hasher::new(&AuthConfig::default()).unwrap_or_else(|_| panic!("hasher"));
        assert!(stronger.needs_rehash(&phc));
        assert!(stronger.needs_rehash("garbage"));
    }

    /// The real cost with the default parameters (OWASP: argon2id, 19 MiB, t=2, p=1). Run with
    /// `cargo test --release --lib argon2_timing -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "timing probe, run by hand in release mode"]
    async fn argon2_timing() {
        let hasher = Hasher::new(&AuthConfig::default()).unwrap_or_else(|_| panic!("hasher"));
        let rounds = 10u32;
        let started = std::time::Instant::now();
        let mut phc = String::new();
        for _ in 0..rounds {
            phc = hasher.hash("correct horse battery".into()).await.unwrap_or_default();
        }
        let hash = started.elapsed() / rounds;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            assert!(hasher.verify("correct horse battery".into(), Some(phc.clone())).await.unwrap_or(false));
        }
        let verify = started.elapsed() / rounds;
        println!("argon2id m=19456 KiB t=2 p=1: hash {hash:?}, verify {verify:?} (mean of {rounds})");
    }
}
