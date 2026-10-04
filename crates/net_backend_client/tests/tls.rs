//! Which certificates the client trusts, on loopback only: a TLS front (a throwaway CA and a
//! `localhost` certificate) in front of the real test server for HTTPS and the WebSocket; extra
//! root certificates from PEM text and from a file; a wrong CA; with feature `os-certificates` the
//! operating system's store alone (refuses the throwaway CA) and with the extra root; with feature
//! `http2` an HTTP/2-only TLS server (ALPN `h2`).

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use common::{email, Server, PASSWORD};
use net_backend_client::protocol::auth::{GetAccount, RegisterRequest};
use net_backend_client::{Client, ClientBuilder, Error};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::{TcpListener, TcpStream};

/// A throwaway CA (its PEM) and a server config with a `localhost` certificate it signed.
struct Pki {
    ca_pem: String,
    server: rustls::ServerConfig,
}

fn pki() -> Pki {
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    // Named: Windows builds the chain by the issuer's name (an empty name fails its check).
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "net_backend_client test CA");
    ca_params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign];
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).expect("ca");
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let mut leaf_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("leaf params");
    leaf_params.distinguished_name.push(rcgen::DnType::CommonName, "localhost");
    leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = leaf_params.signed_by(&leaf_key, &ca).expect("leaf");
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let chain: Vec<CertificateDer<'static>> = vec![leaf.der().clone()];
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .expect("server config");
    Pki { ca_pem: ca.pem(), server }
}

/// A TLS front on loopback: every connection is decrypted and piped to `backend`. Returns the
/// `https://localhost:<port>` URL; the task ends with the test's runtime.
async fn tls_front(config: rustls::ServerConfig, backend: SocketAddr) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let Ok(mut plain) = TcpStream::connect(backend).await else { return };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
            });
        }
    });
    format!("https://localhost:{port}")
}

fn backend(server: &Server) -> SocketAddr {
    server.base.trim_start_matches("http://").parse().expect("the test server's address")
}

/// Register over HTTPS and (feature `ws`) make one WebSocket request over WSS.
async fn exercise(client: &Client, name: &str) {
    let session = client.register(RegisterRequest::new(email(name), PASSWORD)).await.expect("register over https");
    let me = client.call(&GetAccount::new()).await.expect("an authed call over https");
    assert_eq!(me.id, session.account.id);
    #[cfg(feature = "ws")]
    {
        let ws = client.connect_ws(net_backend_client::ws::WsSettings::default()).await.expect("wss connect");
        let echoed = ws.request(&common::Echo { text: "over tls".into() }).await.expect("wss request");
        assert_eq!((echoed.text.as_str(), echoed.user), ("over tls", session.account.id.0));
        ws.close();
    }
}

fn is_tls(result: &Result<impl std::fmt::Debug, Error>) -> bool {
    matches!(result, Err(Error::Tls(_)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extra_root_certificates_reach_a_self_signed_server_over_https_and_wss() {
    let server = Server::start();
    let pki = pki();
    let url = tls_front(pki.server, backend(&server)).await;

    // Mozilla's roots alone: the throwaway CA is unknown.
    let plain = Client::new(&url).expect("client");
    let refused = plain.info().await;
    assert!(is_tls(&refused), "{refused:?}");
    assert_eq!(refused.err().and_then(|e| e.was_sent()), Some(false));

    // The CA as PEM text.
    let client = Client::builder(&url).root_certificates_pem(&pki.ca_pem).build().expect("client");
    exercise(&client, "tls-pem").await;

    // The CA from a file (next to other blocks: a key is skipped).
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("tls-roots");
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("dev-ca.pem");
    let other_key = rcgen::KeyPair::generate().expect("key").serialize_pem();
    std::fs::write(&file, format!("{other_key}\n{}", pki.ca_pem)).expect("write pem");
    let client = Client::builder(&url).root_certificates_file(&file).build().expect("client");
    exercise(&client, "tls-file").await;

    // A different CA is no help; a file without a certificate or a missing file fails at build.
    let wrong = Client::builder(&url).root_certificates_pem(pki_ca_only()).build().expect("client");
    assert!(is_tls(&wrong.info().await));
    std::fs::write(dir.join("key-only.pem"), &other_key).expect("write key");
    let no_cert = Client::builder(&url).root_certificates_file(dir.join("key-only.pem")).build();
    assert!(matches!(no_cert, Err(Error::InvalidRequest(ref why)) if why.contains("no CERTIFICATE")), "{no_cert:?}");
    let missing = Client::builder(&url).root_certificates_file(dir.join("missing.pem")).build();
    assert!(matches!(missing, Err(Error::InvalidRequest(ref why)) if why.contains("missing.pem")), "{missing:?}");
}

/// Only a CA's PEM (unrelated to any server).
fn pki_ca_only() -> String {
    pki().ca_pem
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_builder_debug_shows_counts_not_certificates() {
    let pki = pki();
    let builder: ClientBuilder = Client::builder("https://api.example.com").root_certificates_pem(&pki.ca_pem).token_file("session.json");
    let debug = format!("{builder:?}");
    assert!(debug.contains("extra_pem_sources: 1") && debug.contains("session.json"), "{debug}");
    assert!(!debug.contains("BEGIN"), "{debug}");
}

#[cfg(feature = "os-certificates")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_os_store_refuses_an_unknown_ca_and_takes_extra_roots() {
    let server = Server::start();
    let pki = pki();
    let url = tls_front(pki.server, backend(&server)).await;
    let os_only = Client::builder(&url).os_certificates(true).build().expect("client");
    let refused = os_only.info().await;
    assert!(is_tls(&refused), "the throwaway CA is in no system store: {refused:?}");
    let client = Client::builder(&url).os_certificates(true).root_certificates_pem(&pki.ca_pem).build().expect("client");
    exercise(&client, "tls-os").await;
}

#[cfg(feature = "http2")]
mod http2 {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    use super::*;

    /// An HTTP/2-only TLS server (ALPN `h2` only) that answers every request 404 with the
    /// protocol's error body and counts the HTTP/2 requests.
    async fn h2_server(mut config: rustls::ServerConfig, seen: Arc<AtomicUsize>) -> String {
        config.alpn_protocols = vec![b"h2".to_vec()];
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, seen) = (acceptor.clone(), Arc::clone(&seen));
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                        if request.version() == hyper::Version::HTTP_2 {
                            seen.fetch_add(1, Ordering::SeqCst);
                        }
                        async move {
                            let body = Bytes::from_static(br#"{"error":{"code":"not_found","message":"h2 test server"}}"#);
                            Ok::<_, Infallible>(
                                hyper::Response::builder().status(404).header("content-type", "application/json").body(Full::new(body)).expect("response"),
                            )
                        }
                    });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new()).serve_connection(TokioIo::new(tls), service).await;
                });
            }
        });
        format!("https://localhost:{port}")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http2_is_offered_only_when_asked_and_falls_back_to_http1() {
        let pki = pki();
        let seen = Arc::new(AtomicUsize::new(0));
        let url = h2_server(pki.server.clone(), Arc::clone(&seen)).await;

        // Not asked: only http/1.1 is offered and spoken; the h2-only server never sees a request.
        let h1 = Client::builder(&url).root_certificates_pem(&pki.ca_pem).build().expect("client");
        let refused = h1.info().await;
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(seen.load(Ordering::SeqCst), 0);

        // Asked: HTTP/2 through ALPN; the server's answer comes back as the protocol error.
        let h2 = Client::builder(&url).root_certificates_pem(&pki.ca_pem).http2(true).build().expect("client");
        for _ in 0..3 {
            let answer = h2.info().await;
            assert!(matches!(answer, Err(ref e) if e.status() == Some(404) && e.code() == Some("not_found")), "{answer:?}");
        }
        assert_eq!(seen.load(Ordering::SeqCst), 3, "every request went over HTTP/2");

        // A server that does not speak HTTP/2 (no ALPN): HTTP/1.1 as before, WebSocket included.
        let server = Server::start();
        let url = tls_front(pki.server, backend(&server)).await;
        let client = Client::builder(&url).root_certificates_pem(&pki.ca_pem).http2(true).build().expect("client");
        exercise(&client, "tls-h2-fallback").await;
    }
}
