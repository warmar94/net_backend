//! The TLS setup, in one place for every transport that needs rustls (HTTPS, and with `ws` the
//! WebSocket).
//!
//! ring is the only crypto provider. It is ALWAYS handed to rustls explicitly: never
//! `CryptoProvider::install_default()` (process-wide: it would change other crates' TLS) and never a
//! path that falls back to the process default (rustls panics there when an app also links another
//! provider, e.g. aws-lc-rs).

use std::sync::Arc;

use rustls::crypto::CryptoProvider;

/// The provider every TLS connection of this crate uses: ring's.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A fresh rustls client config: ring, TLS 1.2 + 1.3, Mozilla's root certificates (webpki-roots),
/// no ALPN (hyper-rustls sets its own; the WebSocket needs none).
pub(crate) fn client_config() -> Result<rustls::ClientConfig, String> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    Ok(rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// Bytes from the operating system's secure random source (through ring; no extra dependency).
#[cfg(any(feature = "ws", feature = "ssh"))]
pub(crate) fn random_u64() -> u64 {
    let mut bytes = [0u8; 8];
    if rustls::crypto::ring::default_provider().secure_random.fill(&mut bytes).is_err() {
        // No OS randomness: the clock still spreads clients apart.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        return u64::from(nanos);
    }
    u64::from_le_bytes(bytes)
}
