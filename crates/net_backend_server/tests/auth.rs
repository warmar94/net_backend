//! The auth module through the assembled router (in process): every route's happy and failure
//! paths, token expiry, rotation with the grace window and family revocation, logout variants,
//! email verification and password reset (memory mailer), Steam (fake verifier), roles, admin
//! routes, the audit log, rate limits, hooks, no enumeration, redaction, the blocking hash pool.
//!
//! The same suite runs on SQLite (in memory and a file) here, and on MySQL / PostgreSQL with
//! `NBS_TEST_MYSQL_URL` / `NBS_TEST_POSTGRES_URL` (ignored by default; CI and the test server).
#![cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::Router;
use http::{HeaderMap, Method, Request, StatusCode};
use net_backend_server::auth::events::{AfterLogin, AfterSessionsRevoked, BeforeLogin, BeforeRegister};
use net_backend_server::auth::steam::{FakeSteamVerifier, SteamIdentity};
use net_backend_server::auth::{Auth, AuthConfig, AuthService, RevocationReason, RevokedSessions};
use net_backend_server::hooks::Decision;
use net_backend_server::mail::MemoryMailer;
use net_backend_server::protocol::admin::{AdminUser, AuditEntry};
use net_backend_server::protocol::auth::{Account, AuthSession, TokenPair};
use net_backend_server::protocol::{codes, routes, CloseCode, Page, UnixMillis, UserId};
use net_backend_server::{AppError, AppState, Config, ManualClock, NetBackendServer, PreparedServer, SecretString};
use serde_json::{json, Value};
use tower::ServiceExt;

const T0: i64 = 1_800_000_000_000;
const PASSWORD: &str = "correct horse battery";

struct Fx {
    prepared: PreparedServer,
    router: Router,
    clock: Arc<ManualClock>,
    mail: Arc<MemoryMailer>,
    steam: Arc<FakeSteamVerifier>,
}

fn cheap(config: &mut AuthConfig) {
    config.argon2_memory_kib = 64;
    config.argon2_iterations = 1;
    config.hash_concurrency = 4;
    config.purge_interval_secs = 0;
    config.steam_identity = Some("test-game".into());
}

fn base_config(url: &str) -> Config {
    let mut config = Config::default();
    config.database.url = SecretString::new(url);
    config.database.migrations_dir = common::temp_dir("auth-migrations");
    config
}

async fn fixture_with(url: &str, tweak: impl FnOnce(&mut AuthConfig), extra: impl FnOnce(NetBackendServer) -> NetBackendServer) -> Fx {
    fixture_config(base_config(url), tweak, extra).await
}

async fn fixture_config(config: Config, tweak: impl FnOnce(&mut AuthConfig), extra: impl FnOnce(NetBackendServer) -> NetBackendServer) -> Fx {
    let mut auth = AuthConfig::default();
    cheap(&mut auth);
    tweak(&mut auth);
    let clock = Arc::new(ManualClock::new(UnixMillis(T0)));
    let mail = Arc::new(MemoryMailer::new());
    // Several tickets of the same player: a ticket is accepted once (replay protection).
    let mut steam = FakeSteamVerifier::new("test-game");
    for ticket in ["0a0b0c", "0a0b0d", "0a0b0e", "0a0b0f", "0a0b10"] {
        steam = steam.with_ticket(ticket, SteamIdentity::new(76561190000000001));
    }
    let steam = Arc::new(steam);
    let module = Auth::new().with_config(auth).mailer(mail.clone()).steam_verifier(steam.clone());
    let server = extra(NetBackendServer::new(config).clock(clock.clone()).module(module));
    let prepared = server.build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    Fx { prepared, router, clock, mail, steam }
}

async fn fixture(url: &str) -> Fx {
    fixture_with(url, |_| {}, |s| s).await
}

impl Fx {
    fn state(&self) -> &AppState {
        self.prepared.state()
    }

    fn service(&self) -> Arc<AuthService> {
        self.state().get::<AuthService>().expect("auth service")
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>, token: Option<&str>, ip: Option<SocketAddr>) -> (StatusCode, HeaderMap, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let mut request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request");
        if let Some(ip) = ip {
            request.extensions_mut().insert(ConnectInfo(ip));
        }
        let response = self.router.clone().oneshot(request).await.expect("infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.expect("body");
        (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value, token: Option<&str>) -> (StatusCode, Value) {
        let (status, _, body) = self.send(Method::POST, path, Some(body), token, None).await;
        (status, body)
    }

    async fn get(&self, path: &str, token: Option<&str>) -> (StatusCode, Value) {
        let (status, _, body) = self.send(Method::GET, path, None, token, None).await;
        (status, body)
    }

    async fn register(&self, email: &str) -> AuthSession {
        let (status, body) = self.post(routes::auth::REGISTER, json!({"email": email, "password": PASSWORD, "display_name": "Player"}), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_value(body).expect("an AuthSession")
    }

    async fn login(&self, email: &str, password: &str) -> (StatusCode, Value) {
        self.post(routes::auth::LOGIN, json!({"email": email, "password": password}), None).await
    }

    async fn login_ok(&self, email: &str, password: &str) -> AuthSession {
        let (status, body) = self.login(email, password).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_value(body).expect("an AuthSession")
    }

    /// Wait for the `n`-th mail to `to` and return its one-time token.
    async fn mail_token(&self, to: &str, n: usize) -> String {
        // Generous: the mail queue runs in its own task, CI runners are slow.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mails = self.mail.sent_to(to);
            if mails.len() >= n {
                let text = &mails[n - 1].text;
                let start = text.find("nbse_").expect("a token in the mail");
                return text[start..start + 69].to_string();
            }
            assert!(Instant::now() < deadline, "no mail #{n} to {to}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn make_admin(&self, user: UserId) {
        self.service().set_user_role(self.state(), user, "admin", true).await.expect("grant admin");
    }
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

fn access(session: &AuthSession) -> &str {
    session.tokens.access_token.expose()
}

fn refresh_body(pair: &TokenPair) -> Value {
    json!({"refresh_token": pair.refresh_token.expose()})
}

// ---- scenarios ----------------------------------------------------------------------------------

async fn accounts(url: &str) {
    let fx = fixture(url).await;
    let (_, info) = fx.get(routes::INFO, None).await;
    assert_eq!(info["modules"], json!(["auth"]));

    let session = fx.register("Ada@Example.com").await;
    assert_eq!(session.tokens.token_type, "Bearer");
    assert_eq!(session.account.email.as_deref(), Some("Ada@Example.com"));
    assert!(!session.account.email_verified && session.account.roles.is_empty());
    assert_eq!(session.tokens.access_expires_at, UnixMillis(T0 + 3600 * 1000));
    assert_eq!(session.tokens.refresh_expires_at, UnixMillis(T0 + 30 * 24 * 3600 * 1000));

    // The caller's account; the protocol's credential type works as the header.
    let (status, me) = fx.get(routes::account::ME, Some(access(&session))).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    let me: Account = serde_json::from_value(me).expect("Account");
    assert_eq!(me, session.account);
    assert_eq!(session.tokens.authorization_header(), format!("Bearer {}", access(&session)));

    // Update the display name.
    let (status, _, body) = fx.send(Method::PATCH, routes::account::ME, Some(json!({"display_name": "Ada L."})), Some(access(&session)), None).await;
    assert_eq!((status, body["display_name"].as_str()), (StatusCode::OK, Some("Ada L.")));
    let (status, _, body) = fx.send(Method::PATCH, routes::account::ME, Some(json!({"display_name": " bad "})), Some(access(&session)), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, codes::VALIDATION_FAILED));
    let (status, _, _) = fx.send(Method::PATCH, routes::account::ME, Some(json!({"display_name": "X"})), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Email addresses are unique case-insensitively (on every database, whatever its collation).
    let (status, body) = fx.post(routes::auth::REGISTER, json!({"email": "  ADA@example.COM ", "password": PASSWORD}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, codes::EMAIL_TAKEN));
    // Validation is the protocol's.
    let (status, body) = fx.post(routes::auth::REGISTER, json!({"email": "nope", "password": "short"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, codes::VALIDATION_FAILED));
    assert!(body["error"]["details"]["fields"]["email"].is_array() && body["error"]["details"]["fields"]["password"].is_array());
    let (status, body) = fx.post(routes::auth::REGISTER, json!({"email": 5}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, codes::BAD_REQUEST));

    // Login: any letter case of the address; wrong passwords and unknown addresses look the same.
    let again = fx.login_ok("ada@EXAMPLE.com", PASSWORD).await;
    assert_eq!(again.account.id, session.account.id);
    let (wrong_status, wrong) = fx.login("ada@example.com", "wrong password 1").await;
    let (unknown_status, unknown) = fx.login("nobody@example.com", PASSWORD).await;
    assert_eq!((wrong_status, code(&wrong)), (StatusCode::UNAUTHORIZED, codes::INVALID_CREDENTIALS));
    assert_eq!((wrong_status, &wrong), (unknown_status, &unknown), "no enumeration through login");
    let (status, body) = fx.login("ada@example.com", &"x".repeat(200)).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::INVALID_CREDENTIALS));

    // Without or with a bad token.
    let (status, body) = fx.get(routes::account::ME, None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED));
    let (status, body) = fx.get(routes::account::ME, Some("nbsa_0000")).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED));
    let (status, body) = fx.get(routes::account::ME, Some(session.tokens.refresh_token.expose())).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED), "a refresh token is no access token");

    // Closed registration.
    let closed = fixture_with(url, |c| c.allow_registration = false, |s| s).await;
    let (status, body) = closed.post(routes::auth::REGISTER, json!({"email": "x@example.com", "password": PASSWORD}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::FORBIDDEN));
}

async fn tokens(url: &str) {
    let fx = fixture(url).await;
    let session = fx.register("tok@example.com").await;

    // Access tokens expire after 1 h: `token_expired` (not `unauthorized`) so clients refresh.
    fx.clock.advance(3600 * 1000);
    let (status, body) = fx.get(routes::account::ME, Some(access(&session))).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::TOKEN_EXPIRED));
    // A route that does not need a user ignores the stale token (the client may send it anyway).
    let (status, refreshed) =
        fx.send(Method::POST, routes::auth::REFRESH, Some(refresh_body(&session.tokens)), Some(access(&session)), None).await.into_status_body();
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    let pair: TokenPair = serde_json::from_value(refreshed.clone()).expect("TokenPair");
    assert_ne!(pair.access_token.expose(), access(&session));
    assert_ne!(pair.refresh_token.expose(), session.tokens.refresh_token.expose());
    assert_eq!(fx.get(routes::account::ME, Some(pair.access_token.expose())).await.0, StatusCode::OK);

    // A retry within the 30 s grace window answers the SAME pair.
    fx.clock.advance(29_000);
    let (status, retry) = fx.post(routes::auth::REFRESH, refresh_body(&session.tokens), None).await;
    assert_eq!((status, &retry), (StatusCode::OK, &refreshed));
    // Two concurrent uses of the next token: both get the same new pair.
    let (a, b) = tokio::join!(fx.post(routes::auth::REFRESH, refresh_body(&pair), None), fx.post(routes::auth::REFRESH, refresh_body(&pair), None));
    assert_eq!(a.0, StatusCode::OK, "{}", a.1);
    assert_eq!(a, b, "concurrent refreshes agree");
    let next: TokenPair = serde_json::from_value(a.1).expect("TokenPair");

    // Reuse of the first token after the grace window: the family is revoked.
    fx.clock.advance(2_000);
    let (status, body) = fx.post(routes::auth::REFRESH, refresh_body(&session.tokens), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::REFRESH_TOKEN_REUSED));
    let (status, body) = fx.get(routes::account::ME, Some(next.access_token.expose())).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED), "every token of the family is revoked");
    let (status, body) = fx.post(routes::auth::REFRESH, refresh_body(&next), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED));

    // Other refresh failures.
    let (status, body) = fx.post(routes::auth::REFRESH, json!({"refresh_token": "nbsr_bad"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED));
    let (status, body) = fx.post(routes::auth::REFRESH, json!({"refresh_token": access(&session)}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED));
    let other = fx.login_ok("tok@example.com", PASSWORD).await;
    fx.clock.advance(31 * 24 * 3600 * 1000);
    let (status, body) = fx.post(routes::auth::REFRESH, refresh_body(&other.tokens), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED), "expired refresh token");

    // The purge removes expired rows.
    assert!(fx.service().purge(fx.state()).await.expect("purge") > 0);
    // The WebSocket hub's entry point answers like the HTTP authenticator.
    let error = fx.service().authenticate_token(fx.state(), access(&other)).await.err().map(|e| e.code().to_string());
    assert_eq!(error.as_deref(), Some(codes::UNAUTHORIZED), "purged after a week: unknown");
}

trait IntoStatusBody {
    fn into_status_body(self) -> (StatusCode, Value);
}

impl IntoStatusBody for (StatusCode, HeaderMap, Value) {
    fn into_status_body(self) -> (StatusCode, Value) {
        (self.0, self.2)
    }
}

async fn logout(url: &str) {
    let fx = fixture(url).await;
    let first = fx.register("out@example.com").await;
    let second = fx.login_ok("out@example.com", PASSWORD).await;
    let mut revocations = fx.service().subscribe_revocations();

    // With the access token: only this session.
    let (status, body) = fx.post(routes::auth::LOGOUT, json!({}), Some(access(&first))).await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({})));
    assert_eq!(fx.get(routes::account::ME, Some(access(&first))).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(fx.get(routes::account::ME, Some(access(&second))).await.0, StatusCode::OK);
    let revocation = revocations.try_recv().expect("a revocation");
    assert_eq!((revocation.reason, revocation.close_code()), (RevocationReason::Logout, CloseCode::UNAUTHORIZED));
    assert!(matches!(revocation.sessions, RevokedSessions::One(_)) && revocation.user_id == first.account.id);

    // With an expired access token: the refresh token in the body.
    fx.clock.advance(3601 * 1000);
    let (status, _) = fx.post(routes::auth::LOGOUT, json!({"refresh_token": second.tokens.refresh_token.expose()}), Some(access(&second))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = fx.post(routes::auth::REFRESH, refresh_body(&second.tokens), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED));

    // Everywhere.
    let a = fx.login_ok("out@example.com", PASSWORD).await;
    let b = fx.login_ok("out@example.com", PASSWORD).await;
    let (status, _) = fx.post(routes::auth::LOGOUT, json!({"everywhere": true}), Some(access(&a))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.get(routes::account::ME, Some(access(&b))).await.0, StatusCode::UNAUTHORIZED);

    // Neither credential.
    let (status, body) = fx.post(routes::auth::LOGOUT, json!({}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED));
    let (status, _) = fx.post(routes::auth::LOGOUT, json!({"refresh_token": b.tokens.refresh_token.expose()}), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked already");
}

async fn passwords(url: &str) {
    let fx = fixture(url).await;
    let first = fx.register("pw@example.com").await;
    let second = fx.login_ok("pw@example.com", PASSWORD).await;
    let new = "an even better password";
    let (status, body) = fx.post(routes::account::PASSWORD, json!({"current_password": "wrong password!", "new_password": new}), Some(access(&first))).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::INVALID_CREDENTIALS));
    let (status, body) = fx.post(routes::account::PASSWORD, json!({"current_password": PASSWORD, "new_password": "short"}), Some(access(&first))).await;
    assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, codes::VALIDATION_FAILED));
    let (status, _) = fx.post(routes::account::PASSWORD, json!({"current_password": PASSWORD, "new_password": new}), Some(access(&first))).await;
    assert_eq!(status, StatusCode::OK);
    // The session that changed it stays; the others are revoked.
    assert_eq!(fx.get(routes::account::ME, Some(access(&first))).await.0, StatusCode::OK);
    assert_eq!(fx.get(routes::account::ME, Some(access(&second))).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(fx.login("pw@example.com", PASSWORD).await.0, StatusCode::UNAUTHORIZED);
    fx.login_ok("pw@example.com", new).await;
}

async fn email_flows(url: &str) {
    let fx = fixture_with(url, |c| c.verify_url = Some("https://game.example.com/verify?token={token}".into()), |s| s).await;
    let session = fx.register("mail@example.com").await;
    // The verification mail went out on registration (through the queue).
    let token = fx.mail_token("mail@example.com", 1).await;
    let mail = &fx.mail.sent_to("mail@example.com")[0];
    assert!(mail.text.contains(&format!("https://game.example.com/verify?token={token}")), "{}", mail.text);
    assert!(mail.subject.contains("confirm"));

    let (status, body) = fx.post(routes::auth::VERIFY_EMAIL, json!({"token": "nbse_nope"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, codes::INVALID_TOKEN));
    let (status, _) = fx.post(routes::auth::VERIFY_EMAIL, json!({"token": token}), None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, me) = fx.get(routes::account::ME, Some(access(&session))).await;
    assert_eq!(me["email_verified"], true);
    let (status, body) = fx.post(routes::auth::VERIFY_EMAIL, json!({"token": token}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, codes::INVALID_TOKEN), "single use");
    // Resend when verified: nothing to do.
    let (status, _) = fx.post(routes::auth::RESEND_VERIFICATION, json!({}), Some(access(&session))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.post(routes::auth::RESEND_VERIFICATION, json!({}), None).await.0, StatusCode::UNAUTHORIZED);

    // An expired verification token; resend replaces it; the per-account mail limit holds.
    let other = fx.register("late@example.com").await;
    let old = fx.mail_token("late@example.com", 1).await;
    fx.clock.advance(25 * 3600 * 1000);
    let (status, body) = fx.post(routes::auth::VERIFY_EMAIL, json!({"token": old}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, codes::INVALID_TOKEN));
    let other_access = fx.post(routes::auth::REFRESH, refresh_body(&other.tokens), None).await.1["access_token"].as_str().unwrap_or_default().to_string();
    assert_eq!(fx.post(routes::auth::RESEND_VERIFICATION, json!({}), Some(&other_access)).await.0, StatusCode::OK);
    let fresh = fx.mail_token("late@example.com", 2).await;
    assert_ne!(fresh, old);
    for _ in 0..2 {
        assert_eq!(fx.post(routes::auth::RESEND_VERIFICATION, json!({}), Some(&other_access)).await.0, StatusCode::OK);
    }
    let (status, body) = fx.post(routes::auth::RESEND_VERIFICATION, json!({}), Some(&other_access)).await;
    assert_eq!((status, code(&body)), (StatusCode::TOO_MANY_REQUESTS, codes::RATE_LIMITED), "3 resends per account and hour");
    let (status, _) = fx.post(routes::auth::VERIFY_EMAIL, json!({"token": fresh}), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "replaced by a later mail");

    // Password reset: the same answer for known and unknown addresses.
    let reset_session = fx.login_ok("mail@example.com", PASSWORD).await;
    let (known_status, known) = fx.post(routes::auth::FORGOT_PASSWORD, json!({"email": "MAIL@example.com"}), None).await;
    let (unknown_status, unknown) = fx.post(routes::auth::FORGOT_PASSWORD, json!({"email": "ghost@example.com"}), None).await;
    assert_eq!((known_status, &known), (unknown_status, &unknown));
    assert_eq!(known_status, StatusCode::OK);
    let reset = fx.mail_token("mail@example.com", 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(fx.mail.sent_to("ghost@example.com").is_empty());
    let new = "a brand new password";
    let (status, body) = fx.post(routes::auth::RESET_PASSWORD, json!({"token": reset, "new_password": "short"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, codes::VALIDATION_FAILED));
    let (status, _) = fx.post(routes::auth::RESET_PASSWORD, json!({"token": reset, "new_password": new}), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.get(routes::account::ME, Some(access(&reset_session))).await.0, StatusCode::UNAUTHORIZED, "every session revoked");
    let (status, body) = fx.post(routes::auth::RESET_PASSWORD, json!({"token": reset, "new_password": new}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, codes::INVALID_TOKEN));
    assert_eq!(fx.login("mail@example.com", PASSWORD).await.0, StatusCode::UNAUTHORIZED);
    fx.login_ok("mail@example.com", new).await;
    // Expired reset tokens.
    fx.post(routes::auth::FORGOT_PASSWORD, json!({"email": "mail@example.com"}), None).await;
    let expired = fx.mail_token("mail@example.com", 3).await;
    fx.clock.advance(3601 * 1000);
    let (status, body) = fx.post(routes::auth::RESET_PASSWORD, json!({"token": expired, "new_password": new}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, codes::INVALID_TOKEN));
}

async fn steam(url: &str) {
    let fx = fixture_with(url, |c| c.steam_identity = Some("test-game".into()), |s| s).await;
    // The first login creates the account.
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "0A0B0C", "identity": "test-game"}), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let first: AuthSession = serde_json::from_value(body).expect("AuthSession");
    assert_eq!(first.account.identities[0].provider, "steam");
    assert_eq!(first.account.identities[0].subject, "76561190000000001");
    assert!(first.account.email.is_none());
    // The next login finds it.
    let (_, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "0a0b0d", "identity": "test-game"}), None).await;
    assert_eq!(body["account"]["id"], json!(first.account.id.get()));
    // A ticket works once (replay).
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "0a0b0d", "identity": "test-game"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::STEAM_AUTH_FAILED));

    for (ticket, identity) in [("ffff", "test-game"), ("0a0b0e", "other-game")] {
        let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": ticket, "identity": identity}), None).await;
        assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::STEAM_AUTH_FAILED), "{ticket} {identity}");
    }
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "xyz", "identity": "test-game"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, codes::VALIDATION_FAILED));
    fx.steam.set_unavailable(true);
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "0a0b0f", "identity": "test-game"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::STEAM_AUTH_FAILED));
    fx.steam.set_unavailable(false);

    // Publisher bans are refused (default), VAC bans allowed (default).
    fx.steam.add_ticket("bad1", SteamIdentity::new(76561190000000002).with_bans(false, true));
    fx.steam.add_ticket("acac", SteamIdentity::new(76561190000000003).with_bans(true, false));
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "bad1", "identity": "test-game"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::BANNED));
    assert_eq!(fx.post(routes::auth::STEAM, json!({"ticket_hex": "acac", "identity": "test-game"}), None).await.0, StatusCode::OK);

    // Linking: a logged-in email account adds its Steam id; an id linked elsewhere is a conflict.
    let email = fx.register("linker@example.com").await;
    fx.steam.add_ticket("1111", SteamIdentity::new(76561190000000004));
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "1111", "identity": "test-game"}), Some(access(&email))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"]["id"], json!(email.account.id.get()));
    assert_eq!(body["account"]["identities"][0]["subject"], "76561190000000004");
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "0a0b10", "identity": "test-game"}), Some(access(&email))).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, codes::CONFLICT));

    // Without a verifier: no Steam login on this server.
    let without = {
        let mut config = AuthConfig::default();
        cheap(&mut config);
        let server = NetBackendServer::new(base_config(url)).module(Auth::new().with_config(config).mailer(MemoryMailer::new()));
        let prepared = server.build().await.expect("build");
        prepared.migrate().await.expect("migrate");
        prepared.router()
    };
    let request = Request::post(routes::auth::STEAM)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"ticket_hex":"00","identity":"g"}"#))
        .expect("request");
    let (status, _, body) = common::call(&without, request).await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, codes::NOT_FOUND));
}

async fn admin(url: &str) {
    let fx = fixture(url).await;
    let boss = fx.register("boss@example.com").await;
    let player = fx.register("player@example.com").await;
    let pid = player.account.id.get();

    // Not an admin yet.
    let (status, body) = fx.get(routes::admin::USERS, Some(access(&boss))).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::FORBIDDEN));
    assert_eq!(fx.get(routes::admin::USERS, None).await.0, StatusCode::UNAUTHORIZED);
    fx.make_admin(boss.account.id).await;
    // Roles are read per request: the same token works now.
    let (status, body) = fx.get(&format!("{}?limit=1", routes::admin::USERS), Some(access(&boss))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page: Page<AdminUser> = serde_json::from_value(body).expect("Page<AdminUser>");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].account.id, player.account.id, "newest first");
    let cursor = page.next_cursor.expect("more").as_str().to_string();
    let (_, body) = fx.get(&format!("{}?limit=1&cursor={cursor}", routes::admin::USERS), Some(access(&boss))).await;
    assert_eq!(body["items"][0]["account"]["id"], json!(boss.account.id.get()));
    assert_eq!(body["items"][0]["account"]["roles"], json!(["admin"]));
    let (_, body) = fx.get(&format!("{}?q=PLAYER@", routes::admin::USERS), Some(access(&boss))).await;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(fx.get(&format!("{}?cursor=x", routes::admin::USERS), Some(access(&boss))).await.0, StatusCode::BAD_REQUEST);

    let (status, body) = fx.get(&routes::admin_user_path(player.account.id), Some(access(&boss))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active_sessions"], 1);
    assert_eq!(fx.get(&routes::admin_user_path(UserId(999_999)), Some(access(&boss))).await.0, StatusCode::NOT_FOUND);

    // Ban: sessions revoked (close 4003 for connections), logins refused with the end time.
    let mut revocations = fx.service().subscribe_revocations();
    let until = T0 + 3_600_000;
    let (status, body) = fx.post(&routes::admin_ban_path(player.account.id), json!({"reason": "cheating", "until": until}), Some(access(&boss))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let revocation = revocations.try_recv().expect("a revocation");
    assert_eq!((revocation.user_id, revocation.close_code()), (player.account.id, CloseCode::BANNED));
    // The banned player's token answers 403 `banned` (not 401), with the end time: WebSocket 4003.
    let (status, body) = fx.get(routes::account::ME, Some(access(&player))).await;
    assert_eq!((status, code(&body), &body["error"]["details"]), (StatusCode::FORBIDDEN, codes::BANNED, &json!({"until": until})));
    let (status, body) = fx.post(routes::auth::REFRESH, refresh_body(&player.tokens), None).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::BANNED));
    let error = fx.service().authenticate_token(fx.state(), access(&player)).await.err().map(|e| e.code().to_string());
    assert_eq!(error.as_deref(), Some(codes::BANNED));
    let (status, body) = fx.login("player@example.com", PASSWORD).await;
    assert_eq!((status, code(&body), &body["error"]["details"]), (StatusCode::FORBIDDEN, codes::BANNED, &json!({"until": until})));
    let (_, body) = fx.get(&routes::admin_user_path(player.account.id), Some(access(&boss))).await;
    assert_eq!(body["ban"]["reason"], "cheating");
    let (status, body) = fx.post(&routes::admin_ban_path(player.account.id), json!({"until": T0 - 1}), Some(access(&boss))).await;
    assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, codes::VALIDATION_FAILED));
    let (status, body) = fx.post(&routes::admin_ban_path(boss.account.id), json!({}), Some(access(&boss))).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, codes::CONFLICT));
    // A temporary ban runs out by itself.
    fx.clock.advance(3_600_001);
    let player = fx.login_ok("player@example.com", PASSWORD).await;
    let boss = fx.login_ok("boss@example.com", PASSWORD).await;
    // A permanent ban, then unban.
    assert_eq!(fx.post(&routes::admin_ban_path(player.account.id), json!({}), Some(access(&boss))).await.0, StatusCode::OK);
    assert_eq!(fx.login("player@example.com", PASSWORD).await.0, StatusCode::FORBIDDEN);
    assert_eq!(fx.post(&routes::admin_unban_path(player.account.id), json!({}), Some(access(&boss))).await.0, StatusCode::OK);
    let player = fx.login_ok("player@example.com", PASSWORD).await;

    // Revoke sessions.
    let (status, _, _) = fx.send(Method::DELETE, &routes::admin_sessions_path(player.account.id), None, Some(access(&boss)), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.get(routes::account::ME, Some(access(&player))).await.0, StatusCode::UNAUTHORIZED);

    // Roles over HTTP.
    let role = routes::admin_role_path(UserId(pid), "moderator").expect("valid role");
    assert_eq!(fx.send(Method::PUT, &role, None, Some(access(&boss)), None).await.0, StatusCode::OK);
    assert_eq!(fx.send(Method::PUT, &role, None, Some(access(&boss)), None).await.0, StatusCode::OK, "idempotent");
    let player = fx.login_ok("player@example.com", PASSWORD).await;
    assert_eq!(player.account.roles, vec!["moderator".to_string()]);
    assert_eq!(fx.send(Method::DELETE, &role, None, Some(access(&boss)), None).await.0, StatusCode::OK);
    let bad = format!("/v1/admin/users/{pid}/roles/Not%20Valid");
    assert_eq!(fx.send(Method::PUT, &bad, None, Some(access(&boss)), None).await.0, StatusCode::BAD_REQUEST);
    let own = routes::admin_role_path(boss.account.id, "admin").expect("valid");
    assert_eq!(fx.send(Method::DELETE, &own, None, Some(access(&boss)), None).await.0, StatusCode::CONFLICT);

    // The audit log: newest first, filters, no secrets.
    let (status, body) = fx.get(&format!("{}?action=admin.", routes::admin::AUDIT), Some(access(&boss))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page: Page<AuditEntry> = serde_json::from_value(body.clone()).expect("Page<AuditEntry>");
    let actions: Vec<&str> = page.items.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(
        actions,
        ["admin.role_revoke", "admin.role_grant", "admin.revoke_sessions", "admin.unban", "admin.ban", "admin.ban", "admin.role_grant"],
        "{body}"
    );
    assert!(page.items.windows(2).all(|w| w[0].id > w[1].id));
    assert_eq!(page.items[0].actor, Some(boss.account.id));
    assert_eq!(page.items[0].target_id.as_deref(), Some(pid.to_string().as_str()));
    let (_, body) = fx.get(&format!("{}?user={pid}&action=auth.login", routes::admin::AUDIT), Some(access(&boss))).await;
    assert!(body["items"].as_array().is_some_and(|items| !items.is_empty() && items.iter().all(|e| e["action"] == "auth.login")), "{body}");
    let (_, all) = fx.get(&format!("{}?limit=100", routes::admin::AUDIT), Some(access(&boss))).await;
    let text = all.to_string();
    assert!(!text.contains(PASSWORD) && !text.contains("nbsa_") && !text.contains("nbsr_") && !text.contains("argon2"), "no secrets in the audit log");
    assert_eq!(fx.get(&format!("{}?action=Bad", routes::admin::AUDIT), Some(access(&boss))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(fx.get(routes::admin::AUDIT, Some(access(&player))).await.0, StatusCode::FORBIDDEN);
}

async fn hooks(url: &str) {
    let logins = Arc::new(AtomicUsize::new(0));
    let revoked = Arc::new(AtomicUsize::new(0));
    let (l, r) = (logins.clone(), revoked.clone());
    let fx = fixture_with(
        url,
        |_| {},
        move |server| {
            server
                .before::<BeforeRegister, _, _>(|_ctx, mut event| async move {
                    match event.display_name.as_deref() {
                        Some("admin") => Ok(Decision::Reject(AppError::forbidden("this name is reserved"))),
                        Some(name) => {
                            event.display_name = Some(format!("[{name}]"));
                            Ok(Decision::Continue(event))
                        }
                        None => Ok(Decision::Continue(event)),
                    }
                })
                .before::<BeforeLogin, _, _>(|_ctx, event| async move {
                    if event.user_id.get() == 1 {
                        Ok(Decision::Reject(AppError::forbidden("maintenance")))
                    } else {
                        Ok(Decision::Continue(event))
                    }
                })
                .after::<AfterLogin, _, _>(move |_ctx, _event| {
                    let l = l.clone();
                    async move {
                        l.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
                .after::<AfterSessionsRevoked, _, _>(move |_ctx, _event| {
                    let r = r.clone();
                    async move {
                        r.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
        },
    )
    .await;
    let (status, body) = fx.post(routes::auth::REGISTER, json!({"email": "a@example.com", "password": PASSWORD, "display_name": "admin"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::FORBIDDEN));
    let first = fx.register("b@example.com").await;
    assert_eq!(first.account.display_name.as_deref(), Some("[Player]"), "a hook changed the name");
    assert_eq!(logins.load(Ordering::SeqCst), 1, "registration logs in");
    let second = fx.register("c@example.com").await;
    let (status, body) = fx.login("b@example.com", PASSWORD).await;
    if first.account.id.get() == 1 {
        assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::FORBIDDEN), "BeforeLogin refused");
    }
    fx.login_ok("c@example.com", PASSWORD).await;
    assert!(logins.load(Ordering::SeqCst) >= 3);
    fx.post(routes::auth::LOGOUT, json!({}), Some(access(&second))).await;
    assert_eq!(revoked.load(Ordering::SeqCst), 1);
}

async fn rate_limits(url: &str) {
    let fx = fixture_with(
        url,
        |c| {
            c.login_per_minute = 2;
            c.login_failures = 2;
        },
        |s| s,
    )
    .await;
    fx.register("rl@example.com").await;
    let ip1: SocketAddr = ([203, 0, 113, 1], 5000).into();
    let ip2: SocketAddr = ([203, 0, 113, 2], 5000).into();
    let body = json!({"email": "rl@example.com", "password": PASSWORD});
    for _ in 0..2 {
        assert_eq!(fx.send(Method::POST, routes::auth::LOGIN, Some(body.clone()), None, Some(ip1)).await.0, StatusCode::OK);
    }
    let (status, headers, refused) = fx.send(Method::POST, routes::auth::LOGIN, Some(body.clone()), None, Some(ip1)).await;
    assert_eq!((status, code(&refused)), (StatusCode::TOO_MANY_REQUESTS, codes::RATE_LIMITED));
    assert!(headers.get("retry-after").is_some() && refused["error"]["details"]["retry_after_ms"].as_u64().is_some_and(|ms| ms > 0));
    assert_eq!(fx.send(Method::POST, routes::auth::LOGIN, Some(body.clone()), None, Some(ip2)).await.0, StatusCode::OK, "another address");

    // Per account: after 2 failures the account's logins wait, also with the right password, and
    // the same for an address without an account (no enumeration through the lockout).
    for email in ["rl@example.com", "ghost@example.com"] {
        for _ in 0..2 {
            assert_eq!(fx.login(email, "wrong password!!").await.0, StatusCode::UNAUTHORIZED);
        }
        let (status, body) = fx.login(email, PASSWORD).await;
        assert_eq!((status, code(&body)), (StatusCode::TOO_MANY_REQUESTS, codes::RATE_LIMITED), "{email}");
    }

    // Behind a trusted proxy the forwarded address counts.
    let mut config = base_config(url);
    config.http.trusted_proxies = vec!["127.0.0.1".into()];
    let proxied = fixture_config(config, |c| c.login_per_minute = 1, |s| s).await;
    proxied.register("px@example.com").await;
    let proxy: SocketAddr = ([127, 0, 0, 1], 9000).into();
    let login = |client: &'static str| {
        let mut request = Request::post(routes::auth::LOGIN)
            .header("content-type", "application/json")
            .header("x-forwarded-for", client)
            .body(Body::from(json!({"email": "px@example.com", "password": PASSWORD}).to_string()))
            .expect("request");
        request.extensions_mut().insert(ConnectInfo(proxy));
        proxied.router.clone().oneshot(request)
    };
    assert_eq!(login("198.51.100.1").await.expect("ok").status(), StatusCode::OK);
    assert_eq!(login("198.51.100.1").await.expect("ok").status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(login("198.51.100.2").await.expect("ok").status(), StatusCode::OK, "each client behind the proxy has its own bucket");
}

async fn suite(url: &str) {
    accounts(url).await;
    tokens(url).await;
    logout(url).await;
    passwords(url).await;
    email_flows(url).await;
    steam(url).await;
    admin(url).await;
    hooks(url).await;
    rate_limits(url).await;
    fix_round(url).await;
}

// ---- runners ------------------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_memory_suite() {
    suite("sqlite::memory:").await;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_file_suite() {
    let dir = common::temp_dir("auth-file");
    let url = format!("sqlite:{}", dir.join("game.db").display().to_string().replace('\\', "/"));
    suite(&url).await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_MYSQL_URL (a MySQL 8 server; CI service container)"]
async fn mysql_auth_suite() {
    let base = common::env_url("NBS_TEST_MYSQL_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NBS_TEST_POSTGRES_URL (a PostgreSQL 16 server; CI service container)"]
async fn postgres_auth_suite() {
    let base = common::env_url("NBS_TEST_POSTGRES_URL");
    let (url, name) = common::fresh_database(&base).await;
    suite(&url).await;
    common::drop_database(&base, &name).await;
}

/// Hashing runs on the blocking pool: on a single-threaded runtime a timer keeps ticking while a
/// (deliberately expensive) registration hashes.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "current_thread")]
async fn hashing_never_stalls_the_runtime() {
    let fx = fixture_with(
        "sqlite::memory:",
        |c| {
            c.argon2_memory_kib = 32 * 1024;
            c.argon2_iterations = 3;
            c.send_verification_on_register = false;
        },
        |s| s,
    )
    .await;
    let done = Arc::new(std::sync::Mutex::new(false));
    let ticks = Arc::new(AtomicUsize::new(0));
    let ticker = {
        let (done, ticks) = (done.clone(), ticks.clone());
        async move {
            let mut interval = tokio::time::interval(Duration::from_millis(2));
            while !*done.lock().expect("lock") {
                interval.tick().await;
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        }
    };
    let started = Instant::now();
    let work = async {
        let session = fx.register("slow@example.com").await;
        *done.lock().expect("lock") = true;
        session
    };
    let (_session, ()) = tokio::join!(work, ticker);
    let elapsed = started.elapsed();
    let ticks = ticks.load(Ordering::SeqCst);
    // At 2 ms per tick a free runtime ticks many times while the hash runs elsewhere; a blocked
    // runtime would tick once or twice.
    assert!(ticks >= 5, "the runtime stalled: {ticks} ticks in {elapsed:?}");
}

/// No password, token or hash in logs, `Debug` output or error bodies.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "current_thread")]
async fn secrets_never_reach_logs_or_debug() {
    #[derive(Clone, Default)]
    struct Buffer(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_writer(move || writer.clone()).with_ansi(false).finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // The log mailer (default) with hidden bodies.
    let mut auth = AuthConfig::default();
    cheap(&mut auth);
    auth.smtp_password = Some(SecretString::new("SMTP-PASSWORD-NEVER-LOGGED"));
    let server = NetBackendServer::new(base_config("sqlite::memory:")).module(Auth::new().with_config(auth.clone()));
    let prepared = server.build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let request = |path: &str, body: Value, token: Option<&str>| {
        let mut request = Request::post(path).header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        request.body(Body::from(body.to_string())).expect("request")
    };
    let (_, _, session) = common::call(&router, request(routes::auth::REGISTER, json!({"email": "log@example.com", "password": PASSWORD}), None)).await;
    let access = session["tokens"]["access_token"].as_str().unwrap_or_default().to_string();
    let refresh = session["tokens"]["refresh_token"].as_str().unwrap_or_default().to_string();
    assert!(access.starts_with("nbsa_") && refresh.starts_with("nbsr_"));
    let (_, _, error) = common::call(&router, request(routes::auth::LOGIN, json!({"email": "log@example.com", "password": "WRONG-PASSWORD-xyz"}), None)).await;
    common::call(&router, request(routes::auth::REFRESH, json!({"refresh_token": refresh}), None)).await;
    common::call(&router, request(routes::auth::FORGOT_PASSWORD, json!({"email": "log@example.com"}), None)).await;
    common::call(&router, request(routes::account::PASSWORD, json!({"current_password": PASSWORD, "new_password": "NEW-PASSWORD-abc"}), Some(&access))).await;
    // The mail queue logs from its own task: wait for it (bounded), then a moment for the rest.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !String::from_utf8_lossy(&buffer.0.lock().expect("lock")).contains("mail (log mailer; body hidden") {
        assert!(Instant::now() < deadline, "the log mailer never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let logs = String::from_utf8_lossy(&buffer.0.lock().expect("lock")).into_owned();
    assert!(logs.contains("mail (log mailer; body hidden"), "the log mailer ran: {logs}");
    let service = prepared.state().get::<AuthService>().expect("service");
    let debug = format!("{service:?} {auth:?} {:?}", prepared.state().config());
    for secret in [PASSWORD, "WRONG-PASSWORD-xyz", "NEW-PASSWORD-abc", "nbsa_", "nbsr_", "nbse_", "$argon2", "SMTP-PASSWORD-NEVER-LOGGED", "log@example.com"] {
        assert!(!logs.contains(secret), "`{secret}` in the logs:\n{logs}");
        assert!(!debug.contains(secret), "`{secret}` in Debug output");
        assert!(!error.to_string().contains(secret));
    }
}

/// The auth routes are in the OpenAPI document; the admin routes only when configured.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn openapi_lists_auth_routes() {
    let fx = fixture("sqlite::memory:").await;
    let spec: Value = serde_json::from_str(fx.prepared.openapi_json()).expect("json");
    for path in [
        routes::auth::REGISTER,
        routes::auth::LOGIN,
        routes::auth::STEAM,
        routes::auth::REFRESH,
        routes::auth::LOGOUT,
        routes::auth::VERIFY_EMAIL,
        routes::auth::RESEND_VERIFICATION,
        routes::auth::FORGOT_PASSWORD,
        routes::auth::RESET_PASSWORD,
        routes::account::ME,
        routes::account::PASSWORD,
    ] {
        assert!(spec["paths"][path].is_object(), "{path} missing");
    }
    assert!(spec["paths"][routes::account::ME]["patch"].is_object());
    assert!(spec["components"]["securitySchemes"]["bearer"].is_object());
    assert!(spec["paths"][routes::admin::USERS].is_null(), "admin routes stay out of the public document");
    let documented = fixture_with("sqlite::memory:", |c| c.admin_in_openapi = true, |s| s).await;
    let spec: Value = serde_json::from_str(documented.prepared.openapi_json()).expect("json");
    for path in [
        routes::admin::USERS,
        routes::admin::USER,
        routes::admin::BAN,
        routes::admin::UNBAN,
        routes::admin::SESSIONS,
        routes::admin::ROLE,
        routes::admin::AUDIT,
    ] {
        assert!(spec["paths"][path].is_object(), "{path} missing");
    }
    // Every route the protocol defines for accounts is served (method + path); a user's storage
    // under /v1/admin belongs to the storage module.
    let accounts = |r: &&routes::Route| {
        (r.path.starts_with("/v1/auth") || r.path.starts_with("/v1/account") || r.path.starts_with("/v1/admin")) && !r.path.contains("/storage")
    };
    for route in routes::ALL.iter().filter(accounts) {
        let method = route.method.as_str().to_ascii_lowercase();
        assert!(spec["paths"][route.path][method.as_str()].is_object(), "{} {}", route.method, route.path);
    }
}

/// Settings come from `[modules.auth]` as well; giving both is refused; secrets come from files.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn settings_from_the_config_file() {
    let dir = common::temp_dir("auth-settings");
    std::fs::write(dir.join("key"), "file-secret\n").expect("write");
    let text = format!(
        "[database]\nurl = \"sqlite::memory:\"\nmigrations_dir = \"{}\"\n[modules.auth]\napp_name = \"Space Game\"\nargon2_memory_kib = 64\nargon2_iterations = 1\nsteam_web_api_key_file = \"{}\"\nsteam_identity = \"g\"\n",
        dir.join("m").display().to_string().replace('\\', "/"),
        dir.join("key").display().to_string().replace('\\', "/"),
    );
    let config = Config::from_toml_str(&text).expect("config");
    // A key without an app id is refused, and the message never quotes the key.
    let error = NetBackendServer::new(config.clone()).module(Auth::new()).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("steam_app_id and steam_web_api_key go together") && !error.contains("file-secret"), "{error}");
    let error =
        NetBackendServer::new(config).module(Auth::new().with_config(AuthConfig::default())).build().await.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error.contains("both in code"), "{error}");
    let ok = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.auth]\napp_name = \"Space Game\"\n").expect("config");
    let prepared = NetBackendServer::new(ok).module(Auth::new()).build().await.expect("build");
    assert_eq!(prepared.state().get::<AuthService>().expect("service").config().app_name, "Space Game");
    let typo = Config::from_toml_str("[database]\nurl = \"sqlite::memory:\"\n[modules.auth]\nap_name = \"x\"\n").expect("config");
    assert!(NetBackendServer::new(typo).module(Auth::new()).build().await.is_err(), "unknown keys are refused");
}

/// The auth migrations are publishable like any module's, and the app-owned copy migrates.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn auth_migrations_publish_and_run() {
    let mut config = base_config("sqlite::memory:");
    let dir = common::temp_dir("auth-publish");
    config.database.migrations_dir = dir.clone();
    let server = NetBackendServer::new(config.clone()).module(Auth::new());
    let report = server.publish_migrations("auth", &[], false).expect("publish");
    assert_eq!(report.written.len(), 3 * 29);
    assert!(dir.join("auth").join("mysql").join("202610010001_create_auth_users.sql").is_file());
    let prepared = NetBackendServer::new(config).module(Auth::new()).build().await.expect("build");
    let applied = prepared.migrate().await.expect("migrate").applied;
    assert_eq!(applied.len(), 29);
    assert!(prepared.migrate().await.expect("again").applied.is_empty());
}

// ---- fix round (review server-1b-review.md S1-S12, N1-N12) -------------------------------------

fn ip(last: u8) -> SocketAddr {
    ([203, 0, 113, last], 5000).into()
}

async fn nonce_count(fx: &Fx) -> i64 {
    use net_backend_server::sea_query::{Expr, ExprTrait, Query};
    #[derive(sqlx::FromRow)]
    struct N {
        n: i64,
    }
    let query =
        Query::select().expr_as(Expr::col("id").count(), "n").from("auth_refresh_tokens").and_where(Expr::col("rotation_nonce").is_not_null()).to_owned();
    fx.state().db().fetch_one::<N, _>(&query).await.expect("count").n
}

async fn fix_round(url: &str) {
    // S1: plain addresses only; NFC makes composed / decomposed forms one account.
    let fx = fixture(url).await;
    for bad in ["x<fixvictim@example.com>", "\"a\"@example.com", "fixvictim@example.com (x)", "<fixvictim@example.com>"] {
        let (status, body) = fx.post(routes::auth::REGISTER, json!({"email": bad, "password": PASSWORD}), None).await;
        assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, codes::VALIDATION_FAILED), "{bad}");
    }
    let composed = fx.register("zo\u{eb}.fix@example.com").await;
    assert_eq!(composed.account.email.as_deref(), Some("zo\u{eb}.fix@example.com"));
    let (status, body) = fx.post(routes::auth::REGISTER, json!({"email": "ZOE\u{308}.fix@example.com", "password": PASSWORD}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, codes::EMAIL_TAKEN), "NFC + lower case: the same address");
    // NFC passwords: composed at registration, decomposed at login.
    let (status, _) = fx.post(routes::auth::REGISTER, json!({"email": "nfcpw@example.com", "password": "p\u{e4}ssword-long-1"}), None).await;
    assert_eq!(status, StatusCode::OK);
    fx.login_ok("nfcpw@example.com", "pa\u{308}ssword-long-1").await;

    // S7: a stale Bearer on /v1/auth/steam is refused, no account is created.
    let linker = fx.register("fixlinker@example.com").await;
    let users_before = fx.service().list_users(fx.state(), &Default::default()).await.expect("list").items.len();
    fx.steam.add_ticket("f001", SteamIdentity::new(76561190000000011));
    fx.clock.advance(3601 * 1000);
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "f001", "identity": "test-game"}), Some(access(&linker))).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::TOKEN_EXPIRED));
    let users_after = fx.service().list_users(fx.state(), &Default::default()).await.expect("list").items.len();
    assert_eq!(users_before, users_after, "no silent new account");

    // S2: linking needs a recent login (a refreshed token of an old session is not enough).
    let refreshed: TokenPair = serde_json::from_value(fx.post(routes::auth::REFRESH, refresh_body(&linker.tokens), None).await.1).expect("pair");
    fx.steam.add_ticket("f002", SteamIdentity::new(76561190000000011));
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "f002", "identity": "test-game"}), Some(refreshed.access_token.expose())).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, codes::REAUTHENTICATION_REQUIRED));
    let fresh = fx.login_ok("fixlinker@example.com", PASSWORD).await;
    fx.steam.add_ticket("f003", SteamIdentity::new(76561190000000011));
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "f003", "identity": "test-game"}), Some(access(&fresh))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"]["identities"][0]["subject"], "76561190000000011");
    // The owner is told.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !fx.mail.sent_to("fixlinker@example.com").iter().any(|m| m.subject.contains("Steam account linked")) {
        assert!(Instant::now() < deadline, "no link notification");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // One Steam account per account.
    fx.steam.add_ticket("f004", SteamIdentity::new(76561190000000012));
    let (status, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "f004", "identity": "test-game"}), Some(access(&fresh))).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, codes::CONFLICT));
    // Unlink (self, recent login): then gone; again: 404.
    let unlink = routes::account_identity_path("steam").expect("path");
    assert_eq!(fx.send(Method::DELETE, &unlink, None, Some(access(&fresh)), None).await.0, StatusCode::OK);
    assert_eq!(fx.get(routes::account::ME, Some(access(&fresh))).await.1["identities"], json!([]));
    assert_eq!(fx.send(Method::DELETE, &unlink, None, Some(access(&fresh)), None).await.0, StatusCode::NOT_FOUND);
    // A Steam-only account cannot remove its only way to log in.
    fx.steam.add_ticket("f005", SteamIdentity::new(76561190000000013));
    let (_, body) = fx.post(routes::auth::STEAM, json!({"ticket_hex": "f005", "identity": "test-game"}), None).await;
    let steam_only: AuthSession = serde_json::from_value(body).expect("session");
    let (status, body) = fx.send(Method::DELETE, &unlink, None, Some(access(&steam_only)), None).await.into_status_body();
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, codes::CONFLICT));
    // An admin may.
    let boss = fx.register("fixboss@example.com").await;
    fx.make_admin(boss.account.id).await;
    let admin_unlink = routes::admin_identity_path(steam_only.account.id, "steam").expect("path");
    assert_eq!(fx.send(Method::DELETE, &admin_unlink, None, Some(access(&boss)), None).await.0, StatusCode::OK);
    // A password reset unlinks the providers (default).
    let relinked = fx.login_ok("fixlinker@example.com", PASSWORD).await;
    fx.steam.add_ticket("f006", SteamIdentity::new(76561190000000011));
    assert_eq!(fx.post(routes::auth::STEAM, json!({"ticket_hex": "f006", "identity": "test-game"}), Some(access(&relinked))).await.0, StatusCode::OK);
    let mails_before = fx.mail.sent_to("fixlinker@example.com").len();
    fx.post(routes::auth::FORGOT_PASSWORD, json!({"email": "fixlinker@example.com"}), None).await;
    let reset = fx.mail_token("fixlinker@example.com", mails_before + 1).await;
    assert_eq!(fx.post(routes::auth::RESET_PASSWORD, json!({"token": reset, "new_password": "a reset password 1"}), None).await.0, StatusCode::OK);
    let after = fx.login_ok("fixlinker@example.com", "a reset password 1").await;
    assert!(after.account.identities.is_empty(), "the reset unlinked Steam");

    // N2: a password change voids a pending reset mail.
    let before = fx.mail.sent_to("fixlinker@example.com").len();
    fx.post(routes::auth::FORGOT_PASSWORD, json!({"email": "fixlinker@example.com"}), None).await;
    let pending = fx.mail_token("fixlinker@example.com", before + 1).await;
    let (status, _) = fx
        .post(routes::account::PASSWORD, json!({"current_password": "a reset password 1", "new_password": "a changed password 2"}), Some(access(&after)))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = fx.post(routes::auth::RESET_PASSWORD, json!({"token": pending, "new_password": "attacker password 3"}), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, codes::INVALID_TOKEN));

    // N8: an admin cannot be banned over HTTP (the last-admin rule: `last_admin_keeps_the_role`).
    let other = fx.register("fixboss2@example.com").await;
    fx.make_admin(other.account.id).await;
    let (status, body) = fx.post(&routes::admin_ban_path(other.account.id), json!({}), Some(access(&boss))).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, codes::CONFLICT));
    fx.service().set_user_role(fx.state(), other.account.id, "admin", false).await.expect("two admins: one may go");

    // N12: the admin search is case-insensitive for display names on every database.
    let (_, body) = fx.get(&format!("{}?q=PLAYER&limit=100", routes::admin::USERS), Some(access(&boss))).await;
    assert!(body["items"].as_array().is_some_and(|items| items.len() >= 3), "{body}");

    // N1: an old (used) refresh token ends its own session only, never every session.
    let a = fx.login_ok("nfcpw@example.com", "p\u{e4}ssword-long-1").await;
    let b = fx.login_ok("nfcpw@example.com", "p\u{e4}ssword-long-1").await;
    let rotated: TokenPair = serde_json::from_value(fx.post(routes::auth::REFRESH, refresh_body(&a.tokens), None).await.1).expect("pair");
    let (status, _) = fx.post(routes::auth::LOGOUT, json!({"everywhere": true, "refresh_token": a.tokens.refresh_token.expose()}), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = fx.post(routes::auth::LOGOUT, json!({"refresh_token": a.tokens.refresh_token.expose()}), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.get(routes::account::ME, Some(rotated.access_token.expose())).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(fx.get(routes::account::ME, Some(access(&b))).await.0, StatusCode::OK, "the other session lives");

    // S4: the rotation nonce lives through the grace window only.
    let s = fx.login_ok("nfcpw@example.com", "p\u{e4}ssword-long-1").await;
    let base = nonce_count(&fx).await;
    let first: TokenPair = serde_json::from_value(fx.post(routes::auth::REFRESH, refresh_body(&s.tokens), None).await.1).expect("pair");
    assert_eq!(nonce_count(&fx).await, base + 1, "the nonce of the rotation just made");
    fx.clock.advance(31_000);
    let second: TokenPair = serde_json::from_value(fx.post(routes::auth::REFRESH, refresh_body(&first), None).await.1).expect("pair");
    assert_eq!(nonce_count(&fx).await, base + 1, "the older nonce (past its grace) is gone, the new one is there");
    // The grace answer still works for the newest rotation, and a later reuse revokes.
    let (status, again) = fx.post(routes::auth::REFRESH, refresh_body(&first), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["refresh_token"].as_str(), Some(second.refresh_token.expose()));
    fx.clock.advance(31_000);
    let (status, body) = fx.post(routes::auth::REFRESH, refresh_body(&s.tokens), None).await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, codes::REFRESH_TOKEN_REUSED));

    // S4: 20 parallel refreshes of one token agree on one pair.
    let p = fx.login_ok("nfcpw@example.com", "p\u{e4}ssword-long-1").await;
    let body = refresh_body(&p.tokens);
    let answers = futures_util::future::join_all((0..20).map(|_| fx.post(routes::auth::REFRESH, body.clone(), None))).await;
    assert!(answers.iter().all(|(status, _)| *status == StatusCode::OK), "{answers:?}");
    assert!(answers.windows(2).all(|w| w[0].1 == w[1].1), "every answer is the same pair");

    // S3: a stranger cannot lock the owner out from elsewhere.
    let fx = fixture_with(url, |c| c.login_failures = 2, |s| s).await;
    fx.register("lock@example.com").await;
    let good = json!({"email": "lock@example.com", "password": PASSWORD});
    let bad = json!({"email": "lock@example.com", "password": "wrong password!!"});
    for _ in 0..2 {
        assert_eq!(fx.send(Method::POST, routes::auth::LOGIN, Some(bad.clone()), None, Some(ip(66))).await.0, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(fx.send(Method::POST, routes::auth::LOGIN, Some(good.clone()), None, Some(ip(66))).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(fx.send(Method::POST, routes::auth::LOGIN, Some(good.clone()), None, Some(ip(1))).await.0, StatusCode::OK, "the owner elsewhere");
    // Above the account-wide ceiling only known networks get through.
    let fx = fixture_with(
        url,
        |c| {
            c.login_failures = 100;
            c.account_failures_per_hour = 3;
            c.login_per_minute = 1000;
        },
        |s| s,
    )
    .await;
    fx.register("ceiling@example.com").await;
    let good = json!({"email": "ceiling@example.com", "password": PASSWORD});
    let bad = json!({"email": "ceiling@example.com", "password": "wrong password!!"});
    assert_eq!(fx.send(Method::POST, routes::auth::LOGIN, Some(good.clone()), None, Some(ip(1))).await.0, StatusCode::OK);
    for n in 0..3 {
        assert_eq!(fx.send(Method::POST, routes::auth::LOGIN, Some(bad.clone()), None, Some(ip(100 + n))).await.0, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(fx.send(Method::POST, routes::auth::LOGIN, Some(good.clone()), None, Some(ip(200))).await.0, StatusCode::TOO_MANY_REQUESTS, "a new network");
    assert_eq!(fx.send(Method::POST, routes::auth::LOGIN, Some(good.clone()), None, Some(ip(1))).await.0, StatusCode::OK, "a known network");

    // N10: audit entries older than the retention go with the purge.
    let fx = fixture_with(url, |c| c.audit_retention_days = 30, |s| s).await;
    fx.register("retained@example.com").await;
    fx.clock.advance(31 * 24 * 3600 * 1000);
    fx.service().purge(fx.state()).await.expect("purge");
    let page = fx.service().audit_log(fx.state(), &net_backend_server::protocol::admin::AuditQuery::new().with_action("auth.register")).await.expect("audit");
    assert!(page.items.iter().all(|e| e.created_at.get() >= T0 + 24 * 3600 * 1000), "old entries purged");
}

/// S5 timing probe (release build, by hand): medians of failed logins for a known and an unknown
/// address. `cargo test --release --no-default-features --features sqlite --test auth login_timing -- --ignored --nocapture`
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "timing probe, run by hand in release mode"]
async fn login_timing_probe() {
    let dir = common::temp_dir("auth-timing");
    let url = format!("sqlite:{}", dir.join("t.db").display().to_string().replace('\\', "/"));
    let mut auth = AuthConfig::default();
    auth.steam_identity = Some("test-game".into());
    auth.rate_limits = false;
    let server = NetBackendServer::new(base_config(&url)).module(Auth::new().with_config(auth).mailer(MemoryMailer::new()));
    let prepared = server.build().await.expect("build");
    prepared.migrate().await.expect("migrate");
    let router = prepared.router();
    let login = |email: &str| common::post_json(routes::auth::LOGIN, json!({"email": email, "password": "wrong password!!"}).to_string());
    common::call(&router, common::post_json(routes::auth::REGISTER, json!({"email": "known@example.com", "password": PASSWORD}).to_string())).await;
    let (mut known, mut unknown) = (Vec::new(), Vec::new());
    for n in 0..60 {
        let started = Instant::now();
        common::call(&router, login("known@example.com")).await;
        known.push(started.elapsed());
        let started = Instant::now();
        common::call(&router, login(&format!("nobody{n}@example.com"))).await;
        unknown.push(started.elapsed());
    }
    known.sort();
    unknown.sort();
    println!("failed login medians (60 pairs, SQLite file, release): known {:?}, unknown {:?}", known[30], unknown[30]);
}

/// S9: a revocation made by another process (here a second server on the same database, as the
/// command line would) reaches the running server's subscribers through the revocation poll.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocations_from_other_processes_reach_subscribers() {
    let dir = common::temp_dir("auth-poll");
    let url = format!("sqlite:{}", dir.join("poll.db").display().to_string().replace('\\', "/"));
    let build = |poll: u64| {
        let mut auth = AuthConfig::default();
        cheap(&mut auth);
        auth.revocation_poll_secs = poll;
        NetBackendServer::new(base_config(&url)).module(Auth::new().with_config(auth).mailer(MemoryMailer::new()))
    };
    let running = build(1).build().await.expect("build");
    running.migrate().await.expect("migrate");
    let router = running.router();
    let (_, _, session) =
        common::call(&router, common::post_json(routes::auth::REGISTER, json!({"email": "poll@example.com", "password": PASSWORD}).to_string())).await;
    let user = UserId(session["account"]["id"].as_i64().expect("id"));
    let mut revocations = running.state().get::<AuthService>().expect("service").subscribe_revocations();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(running.serve_with_shutdown(listener, async move {
        let _ = stopped.await;
    }));
    // The modules (and the revocation poll, whose cursor starts there) have started once the
    // server answers a request: it accepts connections only after every module's start.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let answered = async {
            let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
            stream.write_all(b"GET /readyz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").await.ok()?;
            let mut head = [0u8; 12];
            stream.read_exact(&mut head).await.ok()?;
            head.starts_with(b"HTTP/1.1").then_some(())
        };
        if tokio::time::timeout(Duration::from_secs(5), answered).await.ok().flatten().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the server never answered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The "other process".
    let other = build(0).build().await.expect("build other");
    let service = other.state().get::<AuthService>().expect("service");
    let revoked = service.revoke_sessions(other.state(), net_backend_server::auth::Revocation::new(user, RevokedSessions::All, RevocationReason::Admin)).await;
    assert_eq!(revoked.ok(), Some(1));
    let got = tokio::time::timeout(Duration::from_secs(30), revocations.recv()).await.expect("within 30 s").expect("a revocation");
    assert_eq!(got.user_id, user);
    assert!(matches!(got.sessions, RevokedSessions::One(_)) && got.reason == RevocationReason::Admin);
    assert_eq!(got.close_code(), CloseCode::UNAUTHORIZED);
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(30), serving).await;
}

/// S12: a Steam verifier needs the configured identity.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn steam_identity_is_required_with_a_verifier() {
    let mut auth = AuthConfig::default();
    cheap(&mut auth);
    auth.steam_identity = None;
    let error = NetBackendServer::new(base_config("sqlite::memory:"))
        .module(Auth::new().with_config(auth).steam_verifier(FakeSteamVerifier::new("g")))
        .build()
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(error.contains("steam_identity is required"), "{error}");
}

/// N8: the last admin cannot lose the role.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn last_admin_keeps_the_role() {
    let fx = fixture("sqlite::memory:").await;
    let boss = fx.register("onlyboss@example.com").await;
    fx.make_admin(boss.account.id).await;
    let error = fx.service().set_user_role(fx.state(), boss.account.id, "admin", false).await.err().map(|e| e.code().to_string());
    assert_eq!(error.as_deref(), Some(codes::CONFLICT));
    let second = fx.register("secondboss@example.com").await;
    fx.make_admin(second.account.id).await;
    fx.service().set_user_role(fx.state(), boss.account.id, "admin", false).await.expect("two admins: one may go");
}
