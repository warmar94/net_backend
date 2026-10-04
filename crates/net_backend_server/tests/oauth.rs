//! The OpenID Connect module against a local mock OpenID provider (an axum server on loopback that
//! serves a discovery document and a JWKS; test tokens are signed here with ring, RS256 with a
//! throwaway RSA key and ES256 with fresh P-256 keys). Nothing talks to a real provider.
//!
//! - logins: a first login creates the account, the next one finds it; linking with a recent login,
//!   refusing a merge, one account per provider, re-authentication, unlinking, the last way to log
//!   in kept; the hooks;
//! - the security checks: wrong audience / issuer / azp, expired, too old, not yet valid, a bad
//!   signature, `alg: none`, HMAC with the RSA public key as its secret (algorithm confusion), a key
//!   of the wrong type, an algorithm the provider does not allow, the nonce (missing, different,
//!   used twice, the same nonce in a new token), a token without a nonce used twice;
//! - the keys: discovery once, rotation (a new key id fetches again, at most once per window), a
//!   failed fetch (503), recovery, a discovery document naming another issuer;
//! - the wiring: no providers, settings from the file, OpenAPI, `/v1/info`, the login rate.
//!
//! The account part runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(all(feature = "oauth", any(feature = "mysql", feature = "postgres", feature = "sqlite")))]
// The provider fakes serve the SQLite-only tests too; without SQLite (the MySQL / PostgreSQL CI runs)
// part of them stays unused.
#![cfg_attr(not(feature = "sqlite"), allow(dead_code, unused_imports))]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use http::{Method, Request, StatusCode};
use net_backend_server::auth::events::{BeforeLogin, BeforeRegister, LoginMethod};
use net_backend_server::auth::{Auth, AuthConfig};
use net_backend_server::hooks::Decision;
use net_backend_server::oauth::events::BeforeOAuthLogin;
use net_backend_server::oauth::{OAuth, OAuthConfig, OAuthService, ProviderConfig};
use net_backend_server::protocol::{codes, routes, UnixMillis};
use net_backend_server::{AppError, Config, ManualClock, NetBackendServer, PreparedServer, SecretString};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, RsaKeyPair, RsaPublicKeyComponents, ECDSA_P256_SHA256_FIXED_SIGNING, RSA_PKCS1_SHA256};
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const NOW: i64 = T0 / 1000;
const CLIENT: &str = "game-client.example";
const PASSWORD: &str = "correct horse battery";

/// A throwaway 2048-bit RSA key made for this test only (PKCS#1 DER, base64). It signs nothing
/// outside this file.
const TEST_RSA_KEY: &str = concat!(
    "MIIEogIBAAKCAQEApEW58jSHq4RX15spjE1b/9DOJV1dUU4TGMF5jXANJPovK4Q6MZeFM5gftMP4q8aOGXsPAw3iUrlNSECSSsTz",
    "J/8K6oqAwaZHtUxvPslgAdnwlGB8K9K41t9hiV7BFSsUJRAZ94N5+/rjngqUaEGYN0pM5dzvMUgeQgP9p/ecRu/LKm3HQOYLx4XT",
    "gp8payz98i1WCvAOWi2iqdZr4mu5Vk6JwkE5iB7uK024uNGuRPWigR4gi6/sTN487+zSSw+tIoPuFUPj1/0w/f9pLzdPsMvBtt3g",
    "SXIWAvcUhk3iKkE8F4ZIi3j666bSHOo3W0ytqGz8eJgT7Pifljp/pMgdMwIDAQABAoIBAAdbVAuDzLuirqhqO38cC822FTVZLA+z",
    "FmnaaE4sQXpxdeFWB6Em7wEzg9/9kspmlwCPIUn6ujMIN2zP731HurgE1QFR+JgzkSyOYsEGFbWfhAWxGH6B7mM5F84mHzGKf1l1",
    "kiQikDj3sG/oe2L75Qw82JrGvTOQzkIYmaiHD0mh6x9q8O1eDnGygkHxuTHZNr1MBoVA79aszk/lJDlkzDuH1pICqkgYoLD5QAAK",
    "6ucn1LyGMDvytETCURftWWtJ2fUkBMcwUGISbw436p07hVMsbdjHipm4lXc/nTloSjP+sjd4xtRBO67c+mz33Sp/e0BMSsNO4p7D",
    "oP9/ybqHFbkCgYEA4DYc2d5JVF7v4ROAGiJHCqLFOaKFEOnHz+vCysk+F8lzOh9O5F3MvAACSKDoeExHpnsgiVZsheOiJDtjqP2V",
    "ErAGhrHZF+pLGGs8fNiXDeENpdgHQjkNyDrnN4mQ7ALX1UIf8ndy/mPyb93xoT6Dpz7DoZ7uD/CCzbX+a9J05M8CgYEAu5AXVHTB",
    "ioKVFCZg/QFW8Lx0xjUA6yfyQO5yKzaSDrmYk5i4kMJyObSAeMpKHtnTaaHHGOWiR6sJhRozCrgKwwG2Xh/l/jj8OXyb8vRQT+GO",
    "vrKraKkafyoYBV6lGKtjoGLgVdBVMMS0AnrH53XInF5cxIMrX7A0mP7IDP2aol0CgYAMMkVdgJ8CjOuFldb5FPZCWNpbqUCNy/nH",
    "kK6W812CU74F4mAbQhL6AxIcu0wKBzQ6lSYO8nmSyvAuAmEId0rdql+gghoqF9+f42116R5GbgCdDeRPMOVUCAg92Cje/cSZ4C/2",
    "s5K4zd0JQsx7Ffh5Z4uixg9zJIUpBYZifR9ItwKBgEZ/hVVYQTrHlDMrrb7LFxuLKjUpzPuWWybuuPjnHQTt25x2hcDbZUWtQ7Cj",
    "EDMCWsVUalpATbu0XPKrg03fGSRs61f7k133m04cOR2bmOg9doLU8zp2fSAY+UhjZ5ibKuoo3/tBQBQBi0t3TNYB3nJvwVyXlOD1",
    "gP+UnCrN06Z9AoGAKSLF9f32E0Okk2gLky8eCP2izRewja6H7gp6muD2eC8aytOVVtHo0Shhttym3sPs9KPilVR0+24aeida2yQn",
    "1CSO7ODvqRsZgZOAIEaLfZ+WC40/TIH3heeftYEI1ng2h6sESqE+ggNbjkgE478fDFJAkNDKuGj03QroSOBecjY=",
);

fn enc(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

// ---- signing keys ------------------------------------------------------------------------------

enum Signer {
    Rsa(RsaKeyPair),
    Ec(EcdsaKeyPair),
}

struct Key {
    kid: String,
    signer: Signer,
}

impl Key {
    fn rsa(kid: &str) -> Self {
        let der = STANDARD.decode(TEST_RSA_KEY).expect("base64");
        Self { kid: kid.into(), signer: Signer::Rsa(RsaKeyPair::from_der(&der).expect("rsa key")) }
    }

    fn ec(kid: &str) -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).expect("pkcs8");
        Self { kid: kid.into(), signer: Signer::Ec(EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).expect("ec key")) }
    }

    fn alg(&self) -> &'static str {
        match self.signer {
            Signer::Rsa(_) => "RS256",
            Signer::Ec(_) => "ES256",
        }
    }

    /// The RSA modulus (big-endian), for the confusion test.
    fn rsa_public_der(&self) -> Vec<u8> {
        match &self.signer {
            Signer::Rsa(pair) => pair.public().as_ref().to_vec(),
            Signer::Ec(_) => Vec::new(),
        }
    }

    fn jwk(&self) -> Value {
        match &self.signer {
            Signer::Rsa(pair) => {
                let parts = RsaPublicKeyComponents::<Vec<u8>>::from(pair.public());
                json!({"kty": "RSA", "kid": self.kid, "use": "sig", "alg": "RS256", "n": enc(&parts.n), "e": enc(&parts.e)})
            }
            Signer::Ec(pair) => {
                let point = pair.public_key().as_ref();
                json!({"kty": "EC", "crv": "P-256", "kid": self.kid, "x": enc(&point[1..33]), "y": enc(&point[33..65])})
            }
        }
    }

    fn sign_raw(&self, input: &str) -> Vec<u8> {
        let rng = SystemRandom::new();
        match &self.signer {
            Signer::Rsa(pair) => {
                let mut sig = vec![0; pair.public().modulus_len()];
                pair.sign(&RSA_PKCS1_SHA256, &rng, input.as_bytes(), &mut sig).expect("sign");
                sig
            }
            Signer::Ec(pair) => pair.sign(&rng, input.as_bytes()).expect("sign").as_ref().to_vec(),
        }
    }

    /// A token with this header and these claims.
    fn token_with(&self, header: Value, claims: &Value) -> String {
        let input = format!("{}.{}", enc(header.to_string().as_bytes()), enc(claims.to_string().as_bytes()));
        format!("{input}.{}", enc(&self.sign_raw(&input)))
    }

    fn token(&self, claims: &Value) -> String {
        self.token_with(json!({"alg": self.alg(), "kid": self.kid, "typ": "JWT"}), claims)
    }
}

// ---- the mock provider ---------------------------------------------------------------------------

#[derive(Default)]
struct ProviderState {
    issuer: Mutex<String>,
    /// The issuer the discovery document names (normally the same).
    named_issuer: Mutex<Option<String>>,
    keys: Mutex<Vec<Value>>,
    status: AtomicU16,
    discovery_hits: AtomicUsize,
    jwks_hits: AtomicUsize,
}

struct MockProvider {
    state: Arc<ProviderState>,
    issuer: String,
}

async fn discovery(State(state): State<Arc<ProviderState>>) -> (StatusCode, Json<Value>) {
    state.discovery_hits.fetch_add(1, Ordering::SeqCst);
    let issuer = state.issuer.lock().expect("lock").clone();
    let named = state.named_issuer.lock().expect("lock").clone().unwrap_or_else(|| issuer.clone());
    let status = StatusCode::from_u16(state.status.load(Ordering::SeqCst)).unwrap_or(StatusCode::OK);
    (status, Json(json!({"issuer": named, "jwks_uri": format!("{issuer}/jwks"), "authorization_endpoint": format!("{issuer}/authorize")})))
}

async fn jwks(State(state): State<Arc<ProviderState>>) -> (StatusCode, [(http::HeaderName, &'static str); 1], Json<Value>) {
    state.jwks_hits.fetch_add(1, Ordering::SeqCst);
    let status = StatusCode::from_u16(state.status.load(Ordering::SeqCst)).unwrap_or(StatusCode::OK);
    let keys = state.keys.lock().expect("lock").clone();
    (status, [(http::header::CACHE_CONTROL, "public, max-age=3600")], Json(json!({ "keys": keys })))
}

impl MockProvider {
    async fn start(keys: &[&Key]) -> Self {
        let state = Arc::new(ProviderState { status: AtomicU16::new(200), ..ProviderState::default() });
        *state.keys.lock().expect("lock") = keys.iter().map(|k| k.jwk()).collect();
        let app = Router::new().route("/.well-known/openid-configuration", get(discovery)).route("/jwks", get(jwks)).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let issuer = format!("http://{addr}");
        *state.issuer.lock().expect("lock") = issuer.clone();
        Self { state, issuer }
    }

    fn set_keys(&self, keys: &[&Key]) {
        *self.state.keys.lock().expect("lock") = keys.iter().map(|k| k.jwk()).collect();
    }

    fn set_status(&self, status: u16) {
        self.state.status.store(status, Ordering::SeqCst);
    }

    fn jwks_hits(&self) -> usize {
        self.state.jwks_hits.load(Ordering::SeqCst)
    }

    fn discovery_hits(&self) -> usize {
        self.state.discovery_hits.load(Ordering::SeqCst)
    }

    /// The claims of a valid token for `sub` with `nonce`.
    fn claims(&self, sub: &str, nonce: &str) -> Value {
        json!({"iss": self.issuer, "aud": CLIENT, "sub": sub, "iat": NOW - 5, "exp": NOW + 3600, "nonce": nonce, "email": format!("{sub}@mail.example"), "email_verified": true, "name": "Test Player"})
    }
}

// ---- the server ------------------------------------------------------------------------------------

struct Server {
    prepared: PreparedServer,
    router: Router,
    clock: Arc<ManualClock>,
    hooks: Arc<Mutex<Vec<String>>>,
}

fn oauth_config(provider: &MockProvider, tweak: impl FnOnce(&mut OAuthConfig)) -> OAuthConfig {
    let mut config = OAuthConfig::default();
    config.providers.insert("test".into(), ProviderConfig::new(provider.issuer.clone(), vec![CLIENT.into(), "other-client.example".into()]));
    config.jwks_refetch_secs = 1;
    config.login_per_minute = 0;
    tweak(&mut config);
    config
}

async fn start(url: &str, oauth: OAuthConfig) -> Server {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("oauth-migrations");
    let mut auth = AuthConfig::default();
    auth.argon2_memory_kib = 64;
    auth.argon2_iterations = 1;
    auth.purge_interval_secs = 0;
    auth.rate_limits = false;
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let hooks = Arc::new(Mutex::new(Vec::new()));
    let (seen_login, seen_register, seen_oauth) = (hooks.clone(), hooks.clone(), hooks.clone());
    let prepared = NetBackendServer::new(config)
        .clock(clock.clone())
        .module(Auth::new().with_config(auth))
        .module(OAuth::new().with_config(oauth))
        .before::<BeforeOAuthLogin, _, _>(move |_ctx, event| {
            let seen = seen_oauth.clone();
            async move {
                seen.lock().expect("lock").push(format!("oauth:{}:{}:{}", event.provider, event.subject, event.email_verified));
                if event.subject == "refused-by-hook" {
                    return Ok(Decision::Reject(AppError::forbidden("not this one")));
                }
                Ok(Decision::Continue(event))
            }
        })
        .before::<BeforeRegister, _, _>(move |_ctx, mut event| {
            let seen = seen_register.clone();
            async move {
                if let Some(identity) = &event.identity {
                    seen.lock().expect("lock").push(format!("register:{}:{}", identity.provider, identity.subject));
                    event.display_name = Some("Signed In".into());
                }
                Ok(Decision::Continue(event))
            }
        })
        .before::<BeforeLogin, _, _>(move |_ctx, event| {
            let seen = seen_login.clone();
            async move {
                if event.method == LoginMethod::OpenId {
                    let identity = event.identity.as_ref().map(|i| format!("{}:{}", i.provider, i.subject)).unwrap_or_default();
                    seen.lock().expect("lock").push(format!("login:{identity}"));
                }
                Ok(Decision::Continue(event))
            }
        })
        .build()
        .await
        .expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    Server { prepared, router, clock, hooks }
}

impl Server {
    async fn send(&self, method: Method, path: &str, body: Option<Value>, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let mut request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request");
        // A client address (the per-address login rate needs one).
        request.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 40000))));
        let (status, _, body) = common::call(&self.router, request).await;
        (status, body)
    }

    async fn login(&self, provider: &str, id_token: &str, nonce: Option<&str>, bearer: Option<&str>) -> (StatusCode, Value) {
        let mut body = json!({ "id_token": id_token });
        if let Some(nonce) = nonce {
            body["nonce"] = json!(nonce);
        }
        self.send(Method::POST, &format!("/v1/auth/oauth/{provider}"), Some(body), bearer).await
    }

    async fn login_ok(&self, id_token: &str, nonce: &str) -> (i64, String) {
        let (status, body) = self.login("test", id_token, Some(nonce), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (body["account"]["id"].as_i64().expect("id"), body["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    async fn refused(&self, id_token: &str, nonce: Option<&str>) -> String {
        let (status, body) = self.login("test", id_token, nonce, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body["error"]["code"], codes::OAUTH_FAILED, "{body}");
        body["error"]["message"].as_str().unwrap_or_default().to_string()
    }

    async fn register(&self, email: &str) -> (i64, String) {
        let (status, body) = self.send(Method::POST, routes::auth::REGISTER, Some(json!({"email": email, "password": PASSWORD})), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (body["account"]["id"].as_i64().expect("id"), body["tokens"]["access_token"].as_str().expect("token").to_string())
    }

    async fn identities(&self, token: &str) -> Vec<String> {
        let (status, body) = self.send(Method::GET, routes::account::ME, None, Some(token)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["identities"]
            .as_array()
            .map(|list| list.iter().map(|i| format!("{}:{}", i["provider"].as_str().unwrap_or(""), i["subject"].as_str().unwrap_or(""))).collect())
            .unwrap_or_default()
    }
}

// ---- the account part (every database) ---------------------------------------------------------------

/// The claims of a token made 11 minutes after the start.
fn later(provider: &MockProvider, sub: &str, nonce: &str) -> Value {
    let mut claims = provider.claims(sub, nonce);
    claims["iat"] = json!(NOW + 11 * 60);
    claims["exp"] = json!(NOW + 11 * 60 + 3600);
    claims
}

/// First login creates, the next finds; links (recent login, no merge, one per provider); unlink;
/// the hooks; replays.
async fn logins_and_links(url: &str) {
    let key = Key::ec("ec-1");
    let provider = MockProvider::start(&[&key]).await;
    let server = start(url, oauth_config(&provider, |_| {})).await;

    // A first login creates the account (BeforeRegister sets the name), the next one finds it.
    let (alice, alice_token) = server.login_ok(&key.token(&provider.claims("sub-alice", "n-1")), "n-1").await;
    let (again, _) = server.login_ok(&key.token(&provider.claims("sub-alice", "n-2")), "n-2").await;
    assert_eq!(alice, again);
    assert_eq!(server.identities(&alice_token).await, vec!["test:sub-alice".to_string()]);
    let (_, me) = server.send(Method::GET, routes::account::ME, None, Some(&alice_token)).await;
    assert_eq!(me["display_name"], "Signed In");
    assert!(me.get("email").is_none_or(Value::is_null), "the provider's email is not stored: {me}");
    {
        let hooks = server.hooks.lock().expect("lock").clone();
        assert_eq!(
            hooks,
            vec!["oauth:test:sub-alice:true", "register:test:sub-alice", "login:test:sub-alice", "oauth:test:sub-alice:true", "login:test:sub-alice"]
        );
    }

    // The same nonce again (same token, or a new token with it) is refused; so is a token
    // without a nonce once the server requires one.
    let replay = key.token(&provider.claims("sub-alice", "n-1"));
    assert!(server.refused(&replay, Some("n-1")).await.contains("used already"));

    // A password account links a provider account (recent login).
    let (bob, bob_token) = server.register("bob@mail.example").await;
    let (status, body) = server.login("test", &key.token(&provider.claims("sub-bob", "n-3")), Some("n-3"), Some(&bob_token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"]["id"].as_i64(), Some(bob));
    assert_eq!(server.identities(&bob_token).await, vec!["test:sub-bob".to_string()]);
    let (status, body) = server.login("test", &key.token(&provider.claims("sub-bob", "n-4")), Some("n-4"), None).await;
    assert_eq!((status, body["account"]["id"].as_i64()), (StatusCode::OK, Some(bob)), "the linked account logs in");

    // Never a merge: alice's provider account cannot be linked to bob; bob has one of this provider.
    let (status, body) = server.login("test", &key.token(&provider.claims("sub-alice", "n-5")), Some("n-5"), Some(&bob_token)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = server.login("test", &key.token(&provider.claims("sub-other", "n-6")), Some("n-6"), Some(&bob_token)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"]["message"].as_str().unwrap_or_default().contains("unlink it first"), "{body}");

    // An invalid Bearer token is refused, never treated as no token.
    let (status, _) = server.login("test", &key.token(&provider.claims("sub-x", "n-7")), Some("n-7"), Some("nbsa_not-a-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Linking needs a recent login: 11 minutes after carol's login her session is too old.
    let (_, carol_token) = server.register("carol@mail.example").await;
    server.clock.advance(11 * 60 * 1000);
    let (status, body) = server.login("test", &key.token(&later(&provider, "sub-carol", "n-8")), Some("n-8"), Some(&carol_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], codes::REAUTHENTICATION_REQUIRED);

    // Unlink: with a fresh login; the provider account then makes a new account of its own.
    let (status, body) = server.send(Method::POST, routes::auth::LOGIN, Some(json!({"email": "bob@mail.example", "password": PASSWORD})), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bob_fresh = body["tokens"]["access_token"].as_str().expect("token").to_string();
    let (status, body) = server.send(Method::DELETE, "/v1/account/identities/test", None, Some(&bob_fresh)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(server.identities(&bob_fresh).await.is_empty());
    let (status, body) = server.login("test", &key.token(&later(&provider, "sub-bob", "n-9")), Some("n-9"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(body["account"]["id"].as_i64(), Some(bob), "unlinked: a new account");
    // Alice's only way to log in is the provider: it stays (her session is old too, so log in again).
    let (_, alice_fresh) = server.login_ok(&key.token(&later(&provider, "sub-alice", "n-10")), "n-10").await;
    let (status, body) = server.send(Method::DELETE, "/v1/account/identities/test", None, Some(&alice_fresh)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // A hook refuses.
    let (status, body) = server.login("test", &key.token(&later(&provider, "refused-by-hook", "n-11")), Some("n-11"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Used nonces are purged once their tokens expired (+ skew).
    let service = server.prepared.state().get::<OAuthService>().expect("service");
    assert_eq!(service.purge(server.prepared.state()).await.expect("purge"), 0);
    server.clock.advance(3 * 3600 * 1000);
    assert!(service.purge(server.prepared.state()).await.expect("purge") >= 10);
}

async fn database(backend: &str) -> (String, Option<(String, String)>) {
    match backend {
        "memory" => ("sqlite::memory:".into(), None),
        "file" => {
            let dir = common::temp_dir("oauth-file");
            (format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/")), None)
        }
        #[cfg(any(feature = "mysql", feature = "postgres"))]
        base => {
            let (url, name) = common::fresh_database(base).await;
            (url, Some((base.to_string(), name)))
        }
        #[cfg(not(any(feature = "mysql", feature = "postgres")))]
        other => panic!("no backend for {other}"),
    }
}

async fn suite(backend: &str) {
    let (url, cleanup) = database(backend).await;
    logins_and_links(&url).await;
    #[cfg(any(feature = "mysql", feature = "postgres"))]
    if let Some((base, name)) = cleanup {
        common::drop_database(&base, &name).await;
    }
    #[cfg(not(any(feature = "mysql", feature = "postgres")))]
    let _ = cleanup;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_suite() {
    suite("memory").await;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_file_suite() {
    suite("file").await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_oauth_suite() {
    suite(&common::env_url("NBS_TEST_MYSQL_URL")).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_oauth_suite() {
    suite(&common::env_url("NBS_TEST_POSTGRES_URL")).await;
}

// ---- security checks (SQLite) ------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn security_checks() {
    let rsa = Key::rsa("rsa-1");
    let ec = Key::ec("ec-1");
    let provider = MockProvider::start(&[&rsa, &ec]).await;
    let server = start(
        "sqlite::memory:",
        oauth_config(&provider, |config| {
            config.providers.insert("only_es".into(), {
                let mut p = ProviderConfig::new(provider.issuer.clone(), vec![CLIENT.into()]);
                p.algorithms = vec!["ES256".into()];
                p
            });
        }),
    )
    .await;

    // Both algorithms work.
    server.login_ok(&rsa.token(&provider.claims("sub-1", "a-1")), "a-1").await;
    server.login_ok(&ec.token(&provider.claims("sub-1", "a-2")), "a-2").await;

    let with = |key: &Key, field: &str, value: Value, nonce: &str| {
        let mut claims = provider.claims("sub-1", nonce);
        claims[field] = value;
        key.token(&claims)
    };
    // Audience, issuer, azp.
    server.refused(&with(&rsa, "aud", json!("someone-else"), "b-1"), Some("b-1")).await;
    server.refused(&with(&rsa, "aud", json!(["someone-else", CLIENT]), "b-2"), Some("b-2")).await;
    server.refused(&with(&rsa, "azp", json!("someone-else"), "b-3"), Some("b-3")).await;
    server.refused(&with(&rsa, "iss", json!("https://evil.example"), "b-4"), Some("b-4")).await;
    // Times: expired (beyond the 60 s skew), issued in the future, too old, not yet valid.
    server.refused(&with(&rsa, "exp", json!(NOW - 61), "b-5"), Some("b-5")).await;
    server.refused(&with(&rsa, "iat", json!(NOW + 120), "b-6"), Some("b-6")).await;
    server.refused(&with(&rsa, "iat", json!(NOW - 3600), "b-7"), Some("b-7")).await;
    server.refused(&with(&rsa, "nbf", json!(NOW + 120), "b-8"), Some("b-8")).await;
    server.login_ok(&with(&rsa, "exp", json!(NOW - 30), "b-9"), "b-9").await;

    // A bad signature: another key under the same key id; a changed payload.
    let impostor = Key::ec("ec-1");
    server.refused(&impostor.token(&provider.claims("sub-1", "c-1")), Some("c-1")).await;
    let good = rsa.token(&provider.claims("sub-1", "c-2"));
    let parts: Vec<&str> = good.split('.').collect();
    let forged = format!("{}.{}.{}", parts[0], enc(provider.claims("sub-admin", "c-2").to_string().as_bytes()), parts[2]);
    server.refused(&forged, Some("c-2")).await;

    // Algorithm confusion: `none`; HMAC keyed with the RSA public key; an ES256 header naming the
    // RSA key's id; RS256 at a provider that allows only ES256.
    let claims = provider.claims("sub-1", "d-1");
    let none = format!("{}.{}.", enc(json!({"alg": "none", "kid": "rsa-1"}).to_string().as_bytes()), enc(claims.to_string().as_bytes()));
    server.refused(&none, Some("d-1")).await;
    let header = enc(json!({"alg": "HS256", "kid": "rsa-1"}).to_string().as_bytes());
    let input = format!("{header}.{}", enc(claims.to_string().as_bytes()));
    let mac = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &rsa.rsa_public_der()), input.as_bytes());
    server.refused(&format!("{input}.{}", enc(mac.as_ref())), Some("d-1")).await;
    let crossed = ec.token_with(json!({"alg": "ES256", "kid": "rsa-1"}), &claims);
    server.refused(&crossed, Some("d-1")).await;
    let (status, body) = server.login("only_es", &rsa.token(&provider.claims("sub-1", "d-2")), Some("d-2"), None).await;
    assert_eq!((status, body["error"]["code"].clone()), (StatusCode::UNAUTHORIZED, json!(codes::OAUTH_FAILED)));
    let (status, _) = server.login("only_es", &ec.token(&provider.claims("sub-1", "d-3")), Some("d-3"), None).await;
    assert_eq!(status, StatusCode::OK);

    // Nonces: none sent, another one sent, used twice, the same nonce in a new token.
    server.refused(&rsa.token(&provider.claims("sub-1", "e-1")), None).await;
    server.refused(&rsa.token(&provider.claims("sub-1", "e-1")), Some("e-other")).await;
    server.login_ok(&rsa.token(&provider.claims("sub-1", "e-2")), "e-2").await;
    server.refused(&rsa.token(&provider.claims("sub-1", "e-2")), Some("e-2")).await;
    server.refused(&ec.token(&provider.claims("sub-1", "e-2")), Some("e-2")).await;
    let mut bare = provider.claims("sub-1", "x");
    bare.as_object_mut().map(|o| o.remove("nonce"));
    server.refused(&rsa.token(&bare), None).await;

    // Shapes: not a JWT (422), an unknown provider (404).
    let (status, _) = server.login("test", "not-a-jwt", Some("f-1"), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = server.login("nobody", &rsa.token(&provider.claims("sub-1", "f-2")), Some("f-2"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Without required nonces: a token without one logs in once.
    let lax_provider = MockProvider::start(&[&rsa]).await;
    let lax = start("sqlite::memory:", oauth_config(&lax_provider, |c| c.require_nonce = false)).await;
    let mut bare = lax_provider.claims("sub-1", "x");
    bare.as_object_mut().map(|o| o.remove("nonce"));
    let token = rsa.token(&bare);
    let (status, body) = lax.login("test", &token, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = lax.login("test", &token, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the same token twice");
}

// ---- keys: discovery, rotation, failures (SQLite) ------------------------------------------------------

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keys_rotation_and_failures() {
    let first = Key::ec("k-1");
    let provider = MockProvider::start(&[&first]).await;
    let server = start("sqlite::memory:", oauth_config(&provider, |_| {})).await;
    server.login_ok(&first.token(&provider.claims("sub-1", "r-1")), "r-1").await;
    server.login_ok(&first.token(&provider.claims("sub-1", "r-2")), "r-2").await;
    assert_eq!((provider.discovery_hits(), provider.jwks_hits()), (1, 1), "discovery once, keys kept");

    // Rotation: the provider publishes a new key; a token with its id fetches the keys again.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let second = Key::ec("k-2");
    provider.set_keys(&[&second]);
    server.login_ok(&second.token(&provider.claims("sub-1", "r-3")), "r-3").await;
    assert_eq!(provider.jwks_hits(), 2);
    // Unknown key ids within the refetch window fetch nothing more.
    let stranger = Key::ec("k-unknown");
    for n in 0..5 {
        let nonce = format!("r-4-{n}");
        server.refused(&stranger.token(&provider.claims("sub-1", &nonce)), Some(&nonce)).await;
    }
    assert_eq!(provider.jwks_hits(), 2, "at most one fetch per window");
    // The retired key is gone after the fetch.
    server.refused(&first.token(&provider.claims("sub-1", "r-5")), Some("r-5")).await;

    // The provider fails: the kept keys still work, an unknown key id is refused (401, not 503).
    tokio::time::sleep(Duration::from_millis(1100)).await;
    provider.set_status(500);
    server.login_ok(&second.token(&provider.claims("sub-1", "r-6")), "r-6").await;
    server.refused(&stranger.token(&provider.claims("sub-1", "r-7")), Some("r-7")).await;

    // A server that never got keys answers 503 while the provider fails, and recovers.
    let down = MockProvider::start(&[&first]).await;
    down.set_status(500);
    let fresh = start("sqlite::memory:", oauth_config(&down, |_| {})).await;
    let (status, body) = fresh.login("test", &first.token(&down.claims("sub-1", "s-1")), Some("s-1"), None).await;
    assert_eq!((status, body["error"]["code"].clone()), (StatusCode::SERVICE_UNAVAILABLE, json!(codes::UNAVAILABLE)), "{body}");
    let (status, _) = fresh.login("test", &first.token(&down.claims("sub-1", "s-2")), Some("s-2"), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "within the window: no new fetch");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    down.set_status(200);
    fresh.login_ok(&first.token(&down.claims("sub-1", "s-3")), "s-3").await;

    // A discovery document that names another issuer is not followed.
    let liar = MockProvider::start(&[&first]).await;
    *liar.state.named_issuer.lock().expect("lock") = Some("https://evil.example".into());
    let fooled = start("sqlite::memory:", oauth_config(&liar, |_| {})).await;
    let (status, _) = fooled.login("test", &first.token(&liar.claims("sub-1", "t-1")), Some("t-1"), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(liar.jwks_hits(), 0);

    // A configured jwks_uri skips discovery.
    let direct = MockProvider::start(&[&first]).await;
    let pinned = start(
        "sqlite::memory:",
        oauth_config(&direct, |config| {
            if let Some(p) = config.providers.get_mut("test") {
                p.jwks_uri = Some(format!("{}/jwks", direct.issuer));
            }
        }),
    )
    .await;
    pinned.login_ok(&first.token(&direct.claims("sub-1", "u-1")), "u-1").await;
    assert_eq!((direct.discovery_hits(), direct.jwks_hits()), (0, 1));
}

// ---- wiring (SQLite) -----------------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn wiring_documents_and_rate() {
    let config = || {
        let mut config = Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        config
    };
    let error = NetBackendServer::new(config()).module(OAuth::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("needs the module `auth`"), "{error}");

    // No providers: registered, every login answers 404.
    let prepared = NetBackendServer::new(config()).module(Auth::new()).module(OAuth::new()).build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let (status, _, _) = common::call(&router, common::post_json("/v1/auth/oauth/google", json!({"id_token": "a.b.c", "nonce": "n"}).to_string())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let spec: Value = serde_json::from_str(prepared.openapi_json()).expect("json");
    assert!(spec["paths"][routes::auth::OAUTH]["post"].is_object());
    let (_, _, info) = common::call(&router, common::get(routes::INFO)).await;
    assert_eq!(info["modules"], json!(["auth", "oauth"]));

    // Settings from the file; given twice; unknown keys; a bad provider.
    let file = Config::from_toml_str(
        "[database]\nurl = \"sqlite::memory:\"\n[modules.oauth.providers.google]\npreset = \"google\"\nclient_ids = [\"abc.apps.googleusercontent.com\"]\n",
    )
    .expect("config");
    let ok = NetBackendServer::new(file.clone()).module(Auth::new()).module(OAuth::new()).build().await.expect("from the file");
    assert_eq!(ok.state().get::<OAuthService>().expect("service").providers(), vec!["google".to_string()]);
    let twice = NetBackendServer::new(file).module(Auth::new()).module(OAuth::new().with_config(OAuthConfig::default())).build().await.err();
    assert!(twice.map(|e| e.to_string()).unwrap_or_default().contains("both in code"));
    let bad = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.oauth.providers.google]\npreset = \"google\"\nclient_id = \"x\"\n")
        .expect("config");
    assert!(NetBackendServer::new(bad).module(Auth::new()).module(OAuth::new()).build().await.is_err(), "unknown keys are refused");
    let none = Config::from_toml_str(
        "[database]\nurl = \"sqlite::memory:\"\n[modules.oauth.providers.g]\nissuer = \"https://a.example\"\nclient_ids = [\"x\"]\nalgorithms = [\"none\"]\n",
    )
    .expect("config");
    let error = NetBackendServer::new(none).module(Auth::new()).module(OAuth::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("only RS256 and ES256"), "{error}");

    // The login rate per client address.
    let key = Key::ec("k");
    let provider = MockProvider::start(&[&key]).await;
    let server = start("sqlite::memory:", oauth_config(&provider, |c| c.login_per_minute = 2)).await;
    for n in 0..2 {
        let nonce = format!("q-{n}");
        server.login_ok(&key.token(&provider.claims("sub-1", &nonce)), &nonce).await;
    }
    let (status, body) = server.login("test", &key.token(&provider.claims("sub-1", "q-3")), Some("q-3"), None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}
