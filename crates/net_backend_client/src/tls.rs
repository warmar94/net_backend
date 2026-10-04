//! The TLS setup, in one place for every transport that needs rustls (HTTPS, and with `ws` the
//! WebSocket).
//!
//! ring is the only crypto provider. It is ALWAYS handed to rustls explicitly: never
//! `CryptoProvider::install_default()` (process-wide: it would change other crates' TLS) and never a
//! path that falls back to the process default (rustls panics there when an app also links another
//! provider, e.g. aws-lc-rs).
//!
//! Which server certificates are trusted:
//! - by default Mozilla's root certificates (webpki-roots), checked by rustls (webpki);
//! - with [`ClientBuilder::os_certificates`](crate::ClientBuilder::os_certificates) (feature
//!   `os-certificates`) the operating system's certificate store and its own checks, through
//!   `rustls-platform-verifier` (still ring), INSTEAD of webpki-roots;
//! - plus, in both cases, the extra root certificates given to
//!   [`ClientBuilder::root_certificates_pem`](crate::ClientBuilder::root_certificates_pem) /
//!   [`root_certificates_file`](crate::ClientBuilder::root_certificates_file).
//!
//! The config is built once per client and shared by HTTP and the WebSocket.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

use crate::Error;

/// The provider every TLS connection of this crate uses: ring's.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Where extra root certificates come from (read and parsed at `build`).
#[derive(Clone)]
pub(crate) enum PemSource {
    /// PEM text given by the app.
    Bytes(Vec<u8>),
    /// A PEM file, read at `build`.
    File(PathBuf),
}

/// What a client trusts ([`ClientBuilder`](crate::ClientBuilder)): certificates are public, so
/// `Debug` may show the file names; it shows counts only.
#[derive(Clone, Default)]
pub(crate) struct TrustSettings {
    /// Use the operating system's certificate store instead of webpki-roots.
    #[cfg(feature = "os-certificates")]
    pub(crate) os_store: bool,
    /// Extra root certificates.
    pub(crate) extra: Vec<PemSource>,
}

impl fmt::Debug for TrustSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Trust");
        #[cfg(feature = "os-certificates")]
        s.field("os_store", &self.os_store);
        s.field("extra_pem_sources", &self.extra.len()).finish()
    }
}

/// The rustls client config for `trust`: ring, TLS 1.2 + 1.3, no ALPN (hyper-rustls sets its own
/// on its copy; the WebSocket needs none). Errors: a PEM source that cannot be read or holds no
/// usable certificate is `InvalidRequest`; a verifier that cannot be built is `Tls`.
pub(crate) fn client_config(trust: &TrustSettings) -> Result<rustls::ClientConfig, Error> {
    let extra = extra_roots(&trust.extra)?;
    let builder = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(format!("the TLS configuration could not be built: {e}")))?;
    #[cfg(feature = "os-certificates")]
    if trust.os_store {
        let verifier = os_verifier(extra)?;
        return Ok(builder.dangerous().with_custom_certificate_verifier(Arc::new(verifier)).with_no_client_auth());
    }
    let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    for (index, cert) in extra.into_iter().enumerate() {
        roots.add(cert).map_err(|e| Error::invalid(format!("extra root certificate #{} is not usable as a root: {e}", index + 1)))?;
    }
    Ok(builder.with_root_certificates(roots).with_no_client_auth())
}

/// The operating system's verifier (ring), with the extra roots on top.
#[cfg(feature = "os-certificates")]
fn os_verifier(extra: Vec<CertificateDer<'static>>) -> Result<rustls_platform_verifier::Verifier, Error> {
    let failed = |e: rustls::Error| Error::Tls(format!("the operating system's certificate verifier could not be set up: {e}"));
    if extra.is_empty() {
        return rustls_platform_verifier::Verifier::new(provider()).map_err(failed);
    }
    #[cfg(not(target_os = "android"))]
    {
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider()).map_err(failed)
    }
    #[cfg(target_os = "android")]
    {
        Err(Error::invalid("extra root certificates together with the operating system's certificate store are not supported on Android"))
    }
}

/// Every certificate of every PEM source, in order. A source without any `CERTIFICATE` block is
/// refused (a wrong file must not pass silently); other blocks (e.g. a key) are skipped.
fn extra_roots(sources: &[PemSource]) -> Result<Vec<CertificateDer<'static>>, Error> {
    let mut roots = Vec::new();
    for source in sources {
        let (name, read);
        let bytes: &[u8] = match source {
            PemSource::Bytes(bytes) => {
                name = "the root certificate PEM".to_string();
                bytes
            }
            PemSource::File(path) => {
                name = format!("the root certificate file `{}`", path.display());
                read = std::fs::read(path).map_err(|e| Error::invalid(format!("{name} could not be read: {e}")))?;
                &read
            }
        };
        let before = roots.len();
        for cert in CertificateDer::pem_slice_iter(bytes) {
            roots.push(cert.map_err(|e| Error::invalid(format!("{name} is not valid PEM: {e}")))?);
        }
        if roots.len() == before {
            return Err(Error::invalid(format!("{name} holds no CERTIFICATE block")));
        }
    }
    Ok(roots)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ca_pem() -> String {
        let key = rcgen::KeyPair::generate().unwrap_or_else(|e| panic!("{e}"));
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|e| panic!("{e}"));
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.self_signed(&key).unwrap_or_else(|e| panic!("{e}")).pem()
    }

    fn trust(extra: Vec<PemSource>) -> TrustSettings {
        TrustSettings {
            #[cfg(feature = "os-certificates")]
            os_store: false,
            extra,
        }
    }

    #[test]
    fn default_trust_is_webpki_roots_without_alpn() {
        let config = client_config(&TrustSettings::default()).unwrap_or_else(|e| panic!("{e}"));
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn pem_sources_add_every_certificate_and_refuse_files_without_one() {
        let two = format!("{}\n{}", ca_pem(), ca_pem());
        assert_eq!(extra_roots(&[PemSource::Bytes(two.clone().into_bytes())]).map(|r| r.len()).ok(), Some(2));
        assert!(client_config(&trust(vec![PemSource::Bytes(two.into_bytes())])).is_ok());
        // A key block is skipped; no certificate at all is refused.
        let key_only = rcgen::KeyPair::generate().unwrap_or_else(|e| panic!("{e}")).serialize_pem();
        let with_key = format!("{key_only}\n{}", ca_pem());
        assert_eq!(extra_roots(&[PemSource::Bytes(with_key.into_bytes())]).map(|r| r.len()).ok(), Some(1));
        for bad in [key_only.as_bytes(), b"", b"not pem at all"] {
            let error = client_config(&trust(vec![PemSource::Bytes(bad.to_vec())])).err();
            assert!(matches!(error, Some(Error::InvalidRequest(ref why)) if why.contains("no CERTIFICATE")), "{error:?}");
        }
        let broken = b"-----BEGIN CERTIFICATE-----\nAAAA\n".to_vec();
        assert!(matches!(client_config(&trust(vec![PemSource::Bytes(broken)])), Err(Error::InvalidRequest(_))));
        let missing = PemSource::File(PathBuf::from("this-file-does-not-exist.pem"));
        let error = client_config(&trust(vec![missing])).err();
        assert!(matches!(error, Some(Error::InvalidRequest(ref why)) if why.contains("this-file-does-not-exist.pem")), "{error:?}");
    }

    #[test]
    #[cfg(feature = "os-certificates")]
    fn the_os_store_builds_with_and_without_extra_roots() {
        let os = TrustSettings { os_store: true, extra: Vec::new() };
        assert!(client_config(&os).is_ok_and(|c| c.alpn_protocols.is_empty()));
        let os = TrustSettings { os_store: true, extra: vec![PemSource::Bytes(ca_pem().into_bytes())] };
        assert!(client_config(&os).is_ok());
        assert!(format!("{os:?}").contains("os_store: true"));
    }
}
