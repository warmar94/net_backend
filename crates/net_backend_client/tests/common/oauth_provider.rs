//! A local mock OpenID Connect identity provider (discovery, JWKS, a token endpoint that checks the
//! client, the code and the PKCE verifier) and a "browser" that plays the player: the `open`
//! callback reads the sign-in URL and calls the loopback redirect itself. Used by `tests/oauth.rs`
//! and `tests/redaction.rs` (feature `oauth`). No real provider, no real browser.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use net_backend_client::oauth::OAuthFlow;
use net_backend_server::axum::extract::State;
use net_backend_server::axum::routing::{get, post};
use net_backend_server::axum::{Json, Router};
use net_backend_server::oauth::ProviderConfig;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::common::Server;

pub const CLIENT_ID: &str = "desktop-game";
pub const SECRET: &str = "fake-desktop-secret-2c9e";
/// How long the provider's access tokens are valid (its `expires_in`).
pub const EXPIRES_IN: u64 = 3599;
/// The scopes the provider says it granted.
pub const GRANTED_SCOPE: &str = "openid email";

fn enc(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub struct Pending {
    pub nonce: String,
    pub challenge: String,
    pub redirect_uri: String,
    pub sub: String,
}

pub struct Provider {
    pub issuer: String,
    key: EcdsaKeyPair,
    pub codes: Mutex<HashMap<String, Pending>>,
    /// Every secret the token endpoint saw or issued: codes, verifiers, the client secret, the ID,
    /// access and refresh tokens.
    pub secrets: Mutex<Vec<String>>,
}

impl Provider {
    fn id_token(&self, sub: &str, nonce: &str) -> String {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let header = json!({"alg": "ES256", "kid": "k1", "typ": "JWT"});
        let claims = json!({"iss": self.issuer, "aud": CLIENT_ID, "sub": sub, "iat": now, "exp": now + 3600, "nonce": nonce});
        let input = format!("{}.{}", enc(header.to_string().as_bytes()), enc(claims.to_string().as_bytes()));
        let sig = self.key.sign(&SystemRandom::new(), input.as_bytes()).expect("sign");
        format!("{input}.{}", enc(sig.as_ref()))
    }

    /// The access token the provider issues for `code`.
    pub fn access_token_for(code: &str) -> String {
        format!("fake-provider-access-{code}")
    }

    /// The refresh token the provider issues for `code`.
    pub fn refresh_token_for(code: &str) -> String {
        format!("fake-provider-refresh-{code}")
    }
}

async fn discovery(State(p): State<Arc<Provider>>) -> Json<Value> {
    Json(
        json!({"issuer": p.issuer, "jwks_uri": format!("{}/jwks", p.issuer), "authorization_endpoint": format!("{}/authorize", p.issuer), "token_endpoint": format!("{}/token", p.issuer)}),
    )
}

async fn jwks(State(p): State<Arc<Provider>>) -> Json<Value> {
    let point = p.key.public_key().as_ref();
    Json(json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "k1", "x": enc(&point[1..33]), "y": enc(&point[33..65])}]}))
}

async fn token(State(p): State<Arc<Provider>>, body: String) -> (http::StatusCode, Json<Value>) {
    let form = query(&format!("?{body}"));
    let refuse = |error: &str| (http::StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
    let field = |name: &str| form.get(name).cloned().unwrap_or_default();
    p.secrets.lock().expect("lock").extend([field("code"), field("code_verifier"), field("client_secret")]);
    if field("grant_type") != "authorization_code" || field("client_id") != CLIENT_ID || field("client_secret") != SECRET {
        return refuse("invalid_client");
    }
    let Some(pending) = p.codes.lock().expect("lock").remove(&field("code")) else { return refuse("invalid_grant") };
    let challenge = enc(ring::digest::digest(&ring::digest::SHA256, field("code_verifier").as_bytes()).as_ref());
    if challenge != pending.challenge || field("redirect_uri") != pending.redirect_uri {
        return refuse("invalid_grant");
    }
    let id_token = p.id_token(&pending.sub, &pending.nonce);
    let (access, refresh) = (Provider::access_token_for(&field("code")), Provider::refresh_token_for(&field("code")));
    p.secrets.lock().expect("lock").extend([id_token.clone(), access.clone(), refresh.clone()]);
    (
        http::StatusCode::OK,
        Json(
            json!({"access_token": access, "refresh_token": refresh, "token_type": "Bearer", "expires_in": EXPIRES_IN, "scope": GRANTED_SCOPE, "id_token": id_token}),
        ),
    )
}

pub async fn start_provider() -> Arc<Provider> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let issuer = format!("http://{}", listener.local_addr().expect("addr"));
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).expect("pkcs8");
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).expect("key");
    let provider = Arc::new(Provider { issuer, key, codes: Mutex::new(HashMap::new()), secrets: Mutex::new(Vec::new()) });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/token", post(token))
        .with_state(provider.clone());
    tokio::spawn(async move {
        let _ = net_backend_server::axum::serve(listener, app).await;
    });
    provider
}

pub fn server_for(provider: &Provider) -> Server {
    let issuer = provider.issuer.clone();
    Server::start_with(move |setup| {
        setup.oauth.providers.insert("test".into(), ProviderConfig::new(issuer, vec![CLIENT_ID.into()]));
        setup.oauth.login_per_minute = 0;
    })
}

pub fn flow(provider: &Provider) -> OAuthFlow {
    OAuthFlow::new(format!("{}/authorize", provider.issuer), format!("{}/token", provider.issuer), CLIENT_ID)
        .client_secret(SECRET)
        .timeout(Duration::from_secs(20))
}

/// What the "browser" does with the sign-in URL.
#[derive(Clone, Copy)]
pub enum Browser {
    /// Signs `sub` in and comes back with a code.
    SignIn,
    /// A stranger calls the redirect with another state first, then the real sign-in.
    WrongStateFirst,
    /// The player declines (`error=access_denied`).
    Decline,
    /// Comes back with a code the provider does not know.
    UnknownCode,
    /// Never comes back.
    Never,
    /// Opens an idle connection to the redirect first (as a browser's pre-connection: it sends
    /// nothing and stays open), then signs in.
    PreconnectFirst,
}

pub fn query(url: &str) -> HashMap<String, String> {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| {
            let mut out = Vec::new();
            let bytes = v.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() {
                    out.push(u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"), 16).unwrap_or(0));
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            (k.to_string(), String::from_utf8_lossy(&out).into_owned())
        })
        .collect()
}

/// A plain GET to the loopback redirect (what the browser does); the status line.
pub async fn visit(url: String) -> String {
    let rest = url.strip_prefix("http://").unwrap_or(&url);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = tokio::net::TcpStream::connect(host).await.expect("connect to the redirect");
    stream.write_all(format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.expect("write");
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer).await;
    answer.lines().next().unwrap_or_default().to_string()
}

/// The `open` callback: checks the sign-in URL, then plays the browser (in the background). The
/// page statuses the browser saw go to `seen`; the URL's `state` and `nonce` go to the provider's
/// `secrets`.
pub fn browser(provider: Arc<Provider>, sub: &str, mode: Browser, seen: Arc<Mutex<Vec<String>>>) -> impl FnOnce(&str) -> Result<(), String> + Send + 'static {
    let sub = sub.to_string();
    move |url: &str| {
        let q = query(url);
        assert!(url.starts_with(&format!("{}/authorize?", provider.issuer)), "{url}");
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert!(q.get("scope").is_some_and(|s| s.split(' ').any(|s| s == "openid")));
        assert!(!url.contains(SECRET), "the client secret never goes to the browser");
        let redirect = q.get("redirect_uri").cloned().unwrap_or_default();
        assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"), "{redirect}");
        let state = q.get("state").cloned().unwrap_or_default();
        let nonce = q.get("nonce").cloned().unwrap_or_default();
        provider.secrets.lock().expect("lock").extend([state.clone(), nonce.clone()]);
        let code = format!("code-{state}");
        provider.codes.lock().expect("lock").insert(
            code.clone(),
            Pending { nonce, challenge: q.get("code_challenge").cloned().unwrap_or_default(), redirect_uri: redirect.clone(), sub: sub.clone() },
        );
        tokio::spawn(async move {
            let back = |query: String| format!("{redirect}?{query}");
            let status = match mode {
                Browser::SignIn => visit(back(format!("code={code}&state={state}"))).await,
                Browser::WrongStateFirst => {
                    let stranger = visit(back(format!("code={code}&state=forged"))).await;
                    seen.lock().expect("lock").push(stranger);
                    let favicon = visit(redirect.replace("/callback", "/favicon.ico")).await;
                    seen.lock().expect("lock").push(favicon);
                    visit(back(format!("code={code}&state={state}"))).await
                }
                Browser::Decline => visit(back(format!("error=access_denied&state={state}"))).await,
                Browser::UnknownCode => visit(back(format!("code=nope&state={state}"))).await,
                Browser::Never => String::new(),
                Browser::PreconnectFirst => {
                    let host = redirect.trim_start_matches("http://").trim_end_matches("/callback").to_string();
                    let idle = tokio::net::TcpStream::connect(&host).await.expect("pre-connect");
                    let status = visit(back(format!("code={code}&state={state}"))).await;
                    drop(idle);
                    status
                }
            };
            seen.lock().expect("lock").push(status);
        });
        Ok(())
    }
}
