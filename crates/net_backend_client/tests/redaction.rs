//! Secrets never reach the logs: every `tracing` event of the process (every level, TRACE
//! included; the client, the in-process server, the libraries) and every `log` record of the
//! client's side (TRACE included: tungstenite and russh log through `log`) is captured while the
//! session runs
//! through register, login (also a wrong password), a refresh, a 401 → refresh → retry, the
//! WebSocket with header and first-message authentication (each refused once with an expired
//! token and refreshed), a refused unknown token, a reused refresh token, and logout; with feature
//! `ssh` also SSH logins (encrypted key + passphrase, a wrong passphrase, a password,
//! keyboard-interactive), a command line, stdin and output holding secrets, and a reconnect. No
//! captured line holds a password, a token, a passphrase (also hex-encoded, as tungstenite prints
//! frame payloads) or a `Bearer` header. Own test binary: it installs a process-wide subscriber and
//! logger. The in-process server's own `log` records (its threads) are not the client's and are left
//! out.

mod common;
#[cfg(feature = "ssh")]
#[path = "common/mock_ssh.rs"]
mod mock_ssh;

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use common::{email, Server, SERVER_THREADS};
use net_backend_client::protocol::auth::{AccessToken, GetAccount, LoginRequest, RefreshToken, RegisterRequest, TokenPair};
use net_backend_client::protocol::UnixMillis;
use net_backend_client::{Client, Error};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

const PASSWORD: &str = "fake-pass-redact-7f3a1c";
const WRONG_PASSWORD: &str = "fake-wrong-pass-91b2e4";
const FAKE_ACCESS: &str = "nbsa_fake-unknown-access-3c3c";
const FAKE_REFRESH: &str = "nbsr_fake-unknown-refresh-4d4d";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &Attributes<'_>) -> Id {
        let mut line = String::new();
        span.record(&mut Fields(&mut line));
        self.push(span.metadata().target(), &line);
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, values: &Record<'_>) {
        let mut line = String::new();
        values.record(&mut Fields(&mut line));
        self.push("span", &line);
    }
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        self.push(event.metadata().target(), &line);
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// The `log` crate's records of every thread but the test server's.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<String>>);

impl log::Log for LogCapture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        if std::thread::current().name().is_some_and(|name| name.starts_with(SERVER_THREADS)) {
            return;
        }
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(all, "{}: {}", record.target(), record.args());
    }
    fn flush(&self) {}
}

fn hex(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

impl Capture {
    fn push(&self, target: &str, line: &str) {
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        all.push_str(target);
        all.push_str(": ");
        all.push_str(line);
        all.push('\n');
    }
}

/// A token pair the server never issued (far from expiry: no refresh is tried before a call).
fn fake_pair() -> TokenPair {
    let later = UnixMillis(UnixMillis::now().get() + 3_600_000);
    TokenPair::new(AccessToken::new(FAKE_ACCESS), later, RefreshToken::new(FAKE_REFRESH), later)
}

/// Every token the session held so far.
fn remember(seen: &mut Vec<String>, client: &Client) {
    if let Some(pair) = client.tokens() {
        seen.push(pair.access_token.expose().to_string());
        seen.push(pair.refresh_token.expose().to_string());
    }
}

#[test]
fn no_secret_is_ever_logged() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(capture.clone()).unwrap_or_else(|e| panic!("{e}"));
    let log_capture = LogCapture::default();
    log::set_boxed_logger(Box::new(log_capture.clone())).unwrap_or_else(|e| panic!("{e}"));
    log::set_max_level(log::LevelFilter::Trace);
    let server = Server::start();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    let mut secrets: Vec<String> = vec![PASSWORD.into(), WRONG_PASSWORD.into(), FAKE_ACCESS.into(), FAKE_REFRESH.into()];
    runtime.block_on(http_and_ws(&server, &mut secrets));
    #[cfg(feature = "ssh")]
    secrets.extend(runtime.block_on(ssh_part()));
    drop(server);
    let logs = capture.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert!(logs.contains("net_backend_client"), "the client logged nothing:\n{logs}");
    assert!(logs.contains("the session ended"), "the reuse ending the session is logged:\n{logs}");
    for secret in &secrets {
        assert!(!logs.contains(secret.as_str()), "a secret was logged:\n{logs}");
    }
    assert!(!logs.contains("Bearer "), "an Authorization value was logged:\n{logs}");
    let records = log_capture.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
    #[cfg(feature = "ws")]
    assert!(records.contains("tungstenite"), "tungstenite's TRACE records were captured:\n{records}");
    #[cfg(feature = "ssh")]
    assert!(records.contains("russh"), "russh's records were captured:\n{records}");
    for secret in &secrets {
        assert!(!records.contains(secret.as_str()) && !records.contains(&hex(secret)), "a secret was in a `log` record:\n{records}");
    }
    let lower = records.to_ascii_lowercase();
    assert!(!lower.contains("bearer ") && !lower.contains("authorization"), "an Authorization header was in a `log` record:\n{records}");
}

async fn http_and_ws(server: &Server, secrets: &mut Vec<String>) {
    // Register, a wrong password, a login, an explicit refresh.
    let client = Client::builder(&server.base).refresh_margin(Duration::ZERO).build().expect("client");
    client.register(RegisterRequest::new(email("redact"), PASSWORD)).await.expect("register");
    remember(secrets, &client);
    let wrong = server.client().login(LoginRequest::new(email("redact"), WRONG_PASSWORD)).await.expect_err("wrong password");
    assert_eq!(wrong.status(), Some(401), "{wrong:?}");
    let other = server.client();
    other.login(LoginRequest::new(email("redact"), PASSWORD)).await.expect("login");
    remember(secrets, &other);
    client.refresh().await.expect("refresh");
    remember(secrets, &client);
    // 401 token_expired -> refresh -> one retry (the server's clock passes the expiry).
    server.advance(Duration::from_secs(3601));
    client.call(&GetAccount::new()).await.expect("401 -> refresh -> retry");
    remember(secrets, &client);
    #[cfg(feature = "ws")]
    {
        use net_backend_client::ws::{WsAuthMode, WsSettings};
        for mode in [WsAuthMode::Header, WsAuthMode::FirstMessage, WsAuthMode::Both] {
            server.advance(Duration::from_secs(3601));
            let ws = client.connect_ws(WsSettings::default().with_auth(mode)).await.expect("refused once, refreshed, connected");
            remember(secrets, &client);
            ws.close();
            ws.closed().await;
        }
        // A token nobody issued: refused for good, never refreshed into anything.
        let stranger = server.client();
        stranger.resume(fake_pair());
        let refused = stranger.connect_ws(WsSettings::default().with_auth(WsAuthMode::Both)).await.expect_err("unknown token");
        assert!(refused.needs_login() || refused.status() == Some(401), "{refused:?}");
    }
    // The same over HTTP: 401, one refresh with an unknown refresh token, the session ends.
    let stranger = server.client();
    stranger.resume(fake_pair());
    let refused = stranger.call(&GetAccount::new()).await.expect_err("unknown token");
    assert!(refused.needs_login(), "{refused:?}");
    // A reused refresh token (after the grace window) ends the session.
    let old = other.tokens().expect("tokens");
    other.refresh().await.expect("rotate");
    remember(secrets, &other);
    server.advance(Duration::from_secs(40));
    let reuser = server.client();
    reuser.resume(old);
    let ended = reuser.refresh().await.expect_err("reused");
    assert!(matches!(ended, Error::SessionEnded { .. }), "{ended:?}");
    // Logout (the access token expired: the refresh token in the body does it).
    client.logout().await.expect("logout");
    assert!(matches!(client.call(&GetAccount::new()).await, Err(Error::NotLoggedIn)));
}

/// SSH to the mock: an encrypted key and its passphrase, a wrong passphrase, a password,
/// keyboard-interactive, a command line, stdin and output with secrets, a lost connection and its
/// reconnect. Returns the secrets used.
#[cfg(feature = "ssh")]
async fn ssh_part() -> Vec<String> {
    use mock_ssh::{random_key, write_key, MockOptions, MockSshServer};
    use net_backend_client::ssh::{SshAuth, SshCommand, SshEvent, SshPromptAnswers, SshReconnect, SshSession, SshTarget};

    const PHRASE: &str = "fake-ssh-phrase-5e1f";
    const WRONG_PHRASE: &str = "fake-ssh-wrong-0a0a";
    const SSH_PASSWORD: &str = "fake-ssh-pass-77aa";
    const CODE: &str = "fake-ssh-code-31d2";
    const COMMAND: &str = "fake-ssh-cmd-9b9b";
    const STDIN: &str = "fake-ssh-stdin-6e6e";
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("tmp")
        .join(format!("client-redaction-{}", std::process::id()));
    let client_key = random_key();
    let key = dir.join("id_test");
    write_key(&client_key, &key, Some(PHRASE)).expect("key");
    let options = MockOptions { password: Some(SSH_PASSWORD.into()), keyboard_interactive: Some((SSH_PASSWORD.into(), CODE.into())), ..MockOptions::default() };
    let mock = MockSshServer::start_with("127.0.0.1:0", "tester", client_key.public_key().clone(), options).expect("mock");
    let target = || SshTarget::new("127.0.0.1", "tester").with_port(mock.port()).trust_host_key_fingerprint(mock.fingerprint());
    let wrong = SshSession::connect(target().with_auth(SshAuth::key_file_with_passphrase(&key, WRONG_PHRASE))).await.expect_err("wrong passphrase");
    assert!(matches!(wrong, Error::AuthFailed(_)), "{wrong:?}");
    assert!(SshSession::connect(target().with_auth(SshAuth::password(SSH_PASSWORD))).await.is_ok());
    let answers = SshPromptAnswers::new().answer_containing("password", SSH_PASSWORD).answer_containing("code", CODE);
    assert!(SshSession::connect(target().with_auth(SshAuth::keyboard_interactive(answers))).await.is_ok());
    let reconnect = SshReconnect::default().with_base(Duration::from_millis(20)).with_jitter(false);
    let ssh =
        SshSession::connect(target().with_auth(SshAuth::key_file_with_passphrase(&key, PHRASE)).with_reconnect(reconnect)).await.expect("key + passphrase");
    let out = ssh.run(format!("echo {COMMAND}")).await.expect("echo");
    assert_eq!(out.stdout_text().trim(), COMMAND);
    let out = ssh.run(SshCommand::new("cat").with_stdin(STDIN.as_bytes().to_vec())).await.expect("cat");
    assert_eq!(out.stdout, STDIN.as_bytes());
    let mut events = ssh.events();
    mock.drop_connections();
    let deadline = tokio::time::Instant::now() + common::WAIT;
    loop {
        match tokio::time::timeout_at(deadline, events.next()).await {
            Ok(Some(SshEvent::Connected { .. })) => break,
            Ok(Some(_)) => {}
            other => panic!("no reconnect: {other:?}"),
        }
    }
    assert!(ssh.run(format!("stderr {COMMAND}")).await.is_ok());
    ssh.close().await;
    let _ = std::fs::remove_dir_all(&dir);
    [PHRASE, WRONG_PHRASE, SSH_PASSWORD, CODE, COMMAND, STDIN].iter().map(|s| (*s).to_string()).collect()
}
