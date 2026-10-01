//! The real outbound clients against local fakes on 127.0.0.1 (never a real service): the Steam
//! Web API verifier (feature `steam`) against a tiny HTTP server, the SMTP mailer (feature `smtp`)
//! against a tiny SMTP listener. Each fake lives only for its test.
#![cfg(all(any(feature = "steam", feature = "smtp"), feature = "sqlite"))]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(feature = "steam")]
mod steam {
    use std::collections::HashMap;

    use axum::extract::Query;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::{Json, Router};
    use net_backend_server::auth::steam::{SteamError, SteamVerifier, SteamWebApiVerifier};
    use net_backend_server::auth::{Auth, AuthConfig};
    use net_backend_server::protocol::routes;
    use net_backend_server::{NetBackendServer, SecretString};
    use serde_json::{json, Value};

    use super::*;

    const KEY: &str = "TEST-PUBLISHER-KEY";

    /// A fake `ISteamUserAuth/AuthenticateUserTicket`: records the queries it saw.
    async fn fake_steam() -> (String, Arc<Mutex<Vec<HashMap<String, String>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let app = Router::new().route(
            "/ISteamUserAuth/AuthenticateUserTicket/v1/",
            get(move |Query(query): Query<HashMap<String, String>>| {
                let log = log.clone();
                async move {
                    log.lock().expect("lock").push(query.clone());
                    if query.get("key").map(String::as_str) != Some(KEY) {
                        return (StatusCode::FORBIDDEN, Json(Value::Null));
                    }
                    match query.get("ticket").map(String::as_str) {
                        Some("5105") => {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                            (StatusCode::OK, Json(Value::Null))
                        }
                        Some("0a0b") if query.get("identity").map(String::as_str) == Some("my game/1") && query.get("appid").map(String::as_str) == Some("480") => (
                            StatusCode::OK,
                            Json(json!({"response":{"params":{"result":"OK","steamid":"76561190000000009","ownersteamid":"76561190000000009","vacbanned":false,"publisherbanned":false}}})),
                        ),
                        _ => (StatusCode::OK, Json(json!({"response":{"error":{"errorcode":101,"errordesc":"Invalid ticket"}}}))),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn web_api_verifier_against_a_local_fake() {
        let (url, seen) = fake_steam().await;
        // A generous timeout for the answering fake (slow CI runners); the slow ticket gets its own 1 s one.
        let verifier = SteamWebApiVerifier::new(url.clone(), SecretString::new(KEY), 480, Duration::from_secs(20)).expect("verifier");
        assert!(!format!("{verifier:?}").contains(KEY), "the key never shows in Debug");
        let identity = verifier.verify("0a0b", "my game/1").await.expect("valid ticket");
        assert_eq!(identity.steam_id, 76561190000000009);
        assert!(!identity.is_borrowed());
        // The exact parameters Steam documents: key, appid, ticket (hex), identity (encoded).
        let query = seen.lock().expect("lock")[0].clone();
        assert_eq!(query.get("key").map(String::as_str), Some(KEY));
        assert_eq!(query.get("appid").map(String::as_str), Some("480"));
        assert_eq!(query.get("ticket").map(String::as_str), Some("0a0b"));
        assert_eq!(query.get("identity").map(String::as_str), Some("my game/1"));

        assert_eq!(verifier.verify("ffff", "my game/1").await, Err(SteamError::Rejected("Invalid ticket (code 101)".into())));
        let impatient = SteamWebApiVerifier::new(url.clone(), SecretString::new(KEY), 480, Duration::from_secs(1)).expect("verifier");
        let slow = impatient.verify("5105", "my game/1").await;
        assert!(matches!(&slow, Err(SteamError::Unavailable(m)) if m.contains("no answer")), "{slow:?}");
        let wrong_key = SteamWebApiVerifier::new(url, SecretString::new("WRONG"), 480, Duration::from_secs(20)).expect("verifier");
        let refused = wrong_key.verify("0a0b", "my game/1").await;
        assert!(matches!(&refused, Err(SteamError::Unavailable(m)) if m.contains("403") && !m.contains("WRONG")), "{refused:?}");
        // Nobody listening.
        let closed = SteamWebApiVerifier::new("http://127.0.0.1:9", SecretString::new(KEY), 480, Duration::from_secs(2)).expect("verifier");
        assert!(matches!(closed.verify("0a0b", "g").await, Err(SteamError::Unavailable(_))));
        // Plain http only for loopback.
        assert!(SteamWebApiVerifier::new("http://steam.example.com", SecretString::new(KEY), 480, Duration::from_secs(1)).is_err());
    }

    #[tokio::test]
    async fn steam_login_with_the_built_in_verifier() {
        let (url, _) = fake_steam().await;
        let mut auth = AuthConfig::default();
        auth.argon2_memory_kib = 64;
        auth.argon2_iterations = 1;
        auth.steam_app_id = Some(480);
        auth.steam_web_api_key = Some(SecretString::new(KEY));
        auth.steam_identity = Some("my game/1".into());
        auth.steam_api_url = url;
        let mut config = net_backend_server::Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        let prepared = NetBackendServer::new(config).module(Auth::new().with_config(auth)).build().await.expect("build");
        prepared.migrate().await.expect("migrate");
        let router = prepared.router();
        let request = common::post_json(routes::auth::STEAM, json!({"ticket_hex": "0a0b", "identity": "my game/1"}).to_string());
        let (status, _, body) = common::call(&router, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["account"]["identities"][0]["subject"], "76561190000000009");
        let request = common::post_json(routes::auth::STEAM, json!({"ticket_hex": "0a0b", "identity": "another"}).to_string());
        assert_eq!(common::call(&router, request).await.0, StatusCode::UNAUTHORIZED);
    }
}

#[cfg(feature = "smtp")]
mod smtp {
    use net_backend_server::auth::{Auth, AuthConfig, MailerKind, SmtpTls};
    use net_backend_server::mail::{Mail, Mailer, SmtpMailer};
    use net_backend_server::protocol::routes;
    use net_backend_server::{NetBackendServer, SecretString};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    /// A minimal SMTP server without STARTTLS: records every line it receives.
    async fn fake_smtp() -> (u16, Arc<Mutex<String>>) {
        let transcript = Arc::new(Mutex::new(String::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let log = transcript.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    let _ = write.write_all(b"220 fake.localhost ESMTP\r\n").await;
                    let mut in_data = false;
                    while let Ok(Some(line)) = lines.next_line().await {
                        log.lock().expect("lock").push_str(&format!("{line}\n"));
                        let answer: &[u8] = if in_data {
                            if line == "." {
                                in_data = false;
                                b"250 2.0.0 queued\r\n"
                            } else {
                                continue;
                            }
                        } else {
                            let upper = line.to_ascii_uppercase();
                            if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                                b"250 fake.localhost\r\n"
                            } else if upper.starts_with("DATA") {
                                in_data = true;
                                b"354 go ahead\r\n"
                            } else if upper.starts_with("QUIT") {
                                let _ = write.write_all(b"221 bye\r\n").await;
                                break;
                            } else {
                                b"250 OK\r\n"
                            }
                        };
                        if write.write_all(answer).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (port, transcript)
    }

    #[tokio::test]
    async fn smtp_mailer_against_a_local_fake() {
        let (port, transcript) = fake_smtp().await;
        let mailer =
            SmtpMailer::new("127.0.0.1", Some(port), SmtpTls::None, None, "Space Game <no-reply@example.com>", Duration::from_secs(20)).expect("mailer");
        mailer.send(&Mail::new("ada@example.com", "Hello there", "the body text")).await.expect("sent");
        let text = transcript.lock().expect("lock").clone();
        assert!(text.contains("MAIL FROM:<no-reply@example.com>"), "{text}");
        assert!(text.contains("RCPT TO:<ada@example.com>"), "{text}");
        assert!(text.contains("Subject: Hello there") && text.contains("the body text"), "{text}");
        // STARTTLS is required in that mode: a server that does not offer it is refused.
        let strict = SmtpMailer::new("127.0.0.1", Some(port), SmtpTls::Starttls, None, "no-reply@example.com", Duration::from_secs(20)).expect("mailer");
        assert!(strict.send(&Mail::new("ada@example.com", "x", "y")).await.is_err());
        // A bad recipient is an error, never a panic; a display-name form is never re-parsed into
        // another mailbox (review S1).
        assert!(mailer.send(&Mail::new("not an address", "x", "y")).await.is_err());
        for lenient in ["x<victim@example.com>", "\"a\" <victim@example.com>", "<victim@example.com>"] {
            assert!(mailer.send(&Mail::new(lenient, "x", "y")).await.is_err(), "{lenient}");
        }
        assert!(!transcript.lock().expect("lock").contains("victim@example.com"), "nothing went to the victim");
    }

    #[tokio::test]
    async fn auth_sends_through_smtp() {
        let (port, transcript) = fake_smtp().await;
        let mut auth = AuthConfig::default();
        auth.argon2_memory_kib = 64;
        auth.argon2_iterations = 1;
        auth.mailer = MailerKind::Smtp;
        auth.smtp_host = Some("127.0.0.1".into());
        auth.smtp_port = Some(port);
        auth.smtp_tls = SmtpTls::None;
        auth.mail_from = Some("no-reply@example.com".into());
        auth.verify_url = Some("https://game.example.com/verify?token={token}".into());
        let mut config = net_backend_server::Config::default();
        config.database.url = SecretString::new("sqlite::memory:");
        let prepared = NetBackendServer::new(config.clone()).module(Auth::new().with_config(auth.clone())).build().await.expect("build");
        prepared.migrate().await.expect("migrate");
        let request =
            common::post_json(routes::auth::REGISTER, serde_json::json!({"email": "ada@example.com", "password": "correct horse battery"}).to_string());
        assert_eq!(common::call(&prepared.router(), request).await.0, http::StatusCode::OK);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        // The body's long link line arrives quoted-printable encoded; check subject and recipient.
        while !(transcript.lock().expect("lock").contains("confirm your email address")
            && transcript.lock().expect("lock").contains("RCPT TO:<ada@example.com>"))
        {
            assert!(std::time::Instant::now() < deadline, "no verification mail arrived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // No encryption is only allowed towards this machine.
        auth.smtp_host = Some("smtp.example.com".into());
        let error = NetBackendServer::new(config).module(Auth::new().with_config(auth)).build().await.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("smtp_tls = \"none\""), "{error}");
    }
}
