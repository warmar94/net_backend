//! The desktop sign-in (feature `oauth`) against a local mock identity provider and the real server
//! (its `oauth` module pointed at the mock). The "browser" is this test: the `open` callback reads
//! the sign-in URL and calls the loopback redirect itself. No real provider, no real browser.
//!
//! - the whole flow: PKCE S256 checked by the mock's token endpoint, the nonce in the ID token,
//!   the server's login (account with the linked identity), linking to a password account;
//! - the provider's access token, refresh token, expiry and scope handed to the app (redacted in
//!   `Debug`);
//! - a redirect with the wrong `state` is ignored (the flow keeps waiting), the provider's `error`
//!   ends the flow, a browser that never comes back hits the time limit, an `open` callback that
//!   fails, a token endpoint that refuses the code;
//! - an idle pre-connection to the redirect (as browsers open) does not hold up the redirect;
//! - the token endpoint gets the proxy decided for its own host (not the game server's);
//! - the blocking client: `sign_in_oauth`, `link_oauth_sign_in`, and `start_sign_in_oauth` /
//!   `start_link_oauth_sign_in` polled from a game loop and cancelled (the loopback listener
//!   closed, never sent).

mod common;
#[path = "common/oauth_provider.rs"]
mod oauth_provider;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{email, PASSWORD, WAIT};
use net_backend_client::oauth::OAuthFlow;
use net_backend_client::protocol::auth::{GetAccount, RegisterRequest};
use net_backend_client::protocol::oauth::{OAuthLogin, OAuthToken};
use net_backend_client::Error;
use oauth_provider::{browser, flow, query, server_for, start_provider, Browser, Provider, CLIENT_ID, EXPIRES_IN, GRANTED_SCOPE};

/// Poll `reply` like a game loop until its answer arrives (one overall deadline).
fn take<T>(reply: &mut net_backend_client::Reply<T>) -> Result<T, Error> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(answer) = reply.try_take() {
            return answer;
        }
        assert!(Instant::now() < deadline, "no answer");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_desktop_flow_logs_in_and_links() {
    let provider = start_provider().await;
    let server = server_for(&provider);
    let client = server.client();
    let seen = Arc::new(Mutex::new(Vec::new()));

    let session = client.sign_in_oauth("test", &flow(&provider), browser(provider.clone(), "sub-1", Browser::SignIn, seen.clone())).await.expect("signed in");
    assert!(client.is_logged_in());
    let me = client.call(&GetAccount::new()).await.expect("account");
    assert_eq!(me.id, session.account.id);
    assert_eq!(me.identities.iter().map(|i| (i.provider.as_str(), i.subject.as_str())).collect::<Vec<_>>(), vec![("test", "sub-1")]);
    common::until("the browser's page", || !seen.lock().expect("lock").is_empty()).await;
    assert_eq!(seen.lock().expect("lock")[0], "HTTP/1.1 200 OK");

    // Again: the same account.
    let again = server.client();
    let second = again.sign_in_oauth("test", &flow(&provider), browser(provider.clone(), "sub-1", Browser::SignIn, seen.clone())).await.expect("again");
    assert_eq!(second.account.id, session.account.id);

    // A password account links the provider account (recent login).
    let linker = server.client();
    let registered = linker.register(RegisterRequest::new(email("oauth-linker"), PASSWORD)).await.expect("register");
    let linked = linker.link_oauth_sign_in("test", &flow(&provider), browser(provider.clone(), "sub-2", Browser::SignIn, seen.clone())).await.expect("linked");
    assert_eq!(linked.account.id, registered.account.id);
    // Linking a provider account that belongs to another account: 409, never a merge.
    let error = linker.link_oauth_sign_in("test", &flow(&provider), browser(provider.clone(), "sub-1", Browser::SignIn, seen.clone())).await.err();
    assert_eq!(error.and_then(|e| e.status()), Some(409));

    // The plain call with a token the app got elsewhere (e.g. the device flow): a used nonce is refused.
    let signed = flow(&provider).sign_in(browser(provider.clone(), "sub-3", Browser::SignIn, seen.clone())).await.expect("signed");
    let plain = server.client();
    plain.login_oauth(OAuthLogin::new("test", signed.token())).await.expect("login with the token");
    let replay = server.client().login_oauth(OAuthLogin::new("test", OAuthToken::new(signed.id_token.clone()).with_nonce(signed.nonce.clone()))).await.err();
    assert!(replay.is_some_and(|e| e.is(net_backend_client::protocol::codes::OAUTH_FAILED)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_flow_refuses_what_it_must() {
    let provider = start_provider().await;
    let server = server_for(&provider);
    let client = server.client();
    let seen = Arc::new(Mutex::new(Vec::new()));

    // A redirect with a forged state (and a stray request) is answered and ignored; the real one wins.
    client.sign_in_oauth("test", &flow(&provider), browser(provider.clone(), "sub-9", Browser::WrongStateFirst, seen.clone())).await.expect("signed in");
    common::until("the browser's pages", || seen.lock().expect("lock").len() >= 3).await;
    let pages = seen.lock().expect("lock").clone();
    assert_eq!(pages[..3], ["HTTP/1.1 400 Bad Request".to_string(), "HTTP/1.1 404 Not Found".to_string(), "HTTP/1.1 200 OK".to_string()]);

    // The player declines; the provider refuses the code; the browser never comes back; open fails.
    let declined = server.client().sign_in_oauth("test", &flow(&provider), browser(provider.clone(), "x", Browser::Decline, seen.clone())).await;
    assert!(matches!(&declined, Err(Error::OAuth(why)) if why.contains("access_denied")), "{declined:?}");
    let unknown = server.client().sign_in_oauth("test", &flow(&provider), browser(provider.clone(), "x", Browser::UnknownCode, seen.clone())).await;
    assert!(matches!(&unknown, Err(Error::OAuth(why)) if why.contains("invalid_grant")), "{unknown:?}");
    let short = flow(&provider).timeout(Duration::from_secs(1));
    let never = server.client().sign_in_oauth("test", &short, browser(provider.clone(), "x", Browser::Never, seen.clone())).await;
    assert!(matches!(&never, Err(Error::OAuth(why)) if why.contains("time limit")), "{never:?}");
    let failed = server.client().sign_in_oauth("test", &flow(&provider), |_url: &str| Err("no browser here".to_string())).await;
    assert!(matches!(&failed, Err(Error::OAuth(why)) if why.contains("no browser here")), "{failed:?}");
    assert_eq!(failed.err().and_then(|e| e.was_sent()), Some(false));
    // A plain-http endpoint off loopback is refused before anything happens.
    let remote = OAuthFlow::new("http://login.example.com/authorize", format!("{}/token", provider.issuer), CLIENT_ID);
    let refused = server.client().sign_in_oauth("test", &remote, |_url: &str| Ok(())).await;
    assert!(matches!(refused, Err(Error::InvalidRequest(_))));
}

#[test]
fn the_blocking_client_signs_in() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("runtime");
    let provider = runtime.block_on(start_provider());
    let server = server_for(&provider);
    let client = net_backend_client::blocking::Client::new(&server.base).expect("client");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let session = client.sign_in_oauth("test", &flow(&provider), browser(provider.clone(), "sub-b", Browser::SignIn, seen)).expect("signed in");
    assert!(client.is_logged_in());
    assert_eq!(client.call(&GetAccount::new()).expect("account").id, session.account.id);
    drop(runtime);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_providers_other_tokens_reach_the_app() {
    let provider = start_provider().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let signed = flow(&provider).sign_in(browser(provider.clone(), "sub-t", Browser::SignIn, seen)).await.expect("signed in");
    let secrets = provider.secrets.lock().expect("lock").clone();
    let code = secrets.iter().find(|s| s.starts_with("code-")).cloned().expect("the code reached the token endpoint");
    assert!(secrets.contains(&signed.id_token.expose().to_string()), "the ID token the provider issued");
    assert_eq!(signed.access_token.as_ref().map(|t| t.expose().to_string()), Some(Provider::access_token_for(&code)));
    assert_eq!(signed.refresh_token.as_ref().map(|t| t.expose().to_string()), Some(Provider::refresh_token_for(&code)));
    assert_eq!(signed.token_type.as_deref(), Some("Bearer"));
    assert_eq!(signed.expires_in, Some(Duration::from_secs(EXPIRES_IN)));
    assert_eq!(signed.scope.as_deref(), Some(GRANTED_SCOPE));
    assert!(!signed.nonce.is_empty() && secrets.contains(&signed.nonce.expose().to_string()), "the nonce of the sign-in URL");
    // `Debug` shows none of the secrets.
    let shown = format!("{signed:?}");
    for secret in [signed.id_token.expose(), signed.nonce.expose(), &Provider::access_token_for(&code), &Provider::refresh_token_for(&code)] {
        assert!(!shown.contains(secret), "{shown}");
    }
    assert!(shown.contains("<redacted>") && shown.contains(GRANTED_SCOPE), "{shown}");
}

#[test]
fn a_game_loop_signs_in_without_blocking_and_can_cancel() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("runtime");
    let provider = runtime.block_on(start_provider());
    let server = server_for(&provider);
    let client = net_backend_client::blocking::Client::new(&server.base).expect("client");

    // Polled every "frame" until the player is back and the server's login is done.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut reply = client.start_sign_in_oauth("test", &flow(&provider), browser(provider.clone(), "sub-loop", Browser::SignIn, seen));
    let mut frames = 0u32;
    let session = loop {
        if let Some(answer) = reply.try_take() {
            break answer.expect("signed in");
        }
        frames += 1;
        assert!(frames < 12_000, "no answer");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(client.is_logged_in());
    assert_eq!(client.call(&GetAccount::new()).expect("account").id, session.account.id);

    // The player never comes back: the game cancels. Never sent; the loopback listener is closed.
    let (sender, redirect) = std::sync::mpsc::channel::<String>();
    let waiting = net_backend_client::blocking::Client::new(&server.base).expect("client");
    let mut reply = waiting.start_sign_in_oauth("test", &flow(&provider), move |url: &str| {
        let _ = sender.send(query(url).get("redirect_uri").cloned().unwrap_or_default());
        Ok(())
    });
    let redirect = redirect.recv_timeout(WAIT).expect("the sign-in URL");
    let port: u16 = redirect.trim_start_matches("http://127.0.0.1:").trim_end_matches("/callback").parse().expect("port");
    let listening = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(10));
    assert!(listening.is_ok(), "the listener is open while the sign-in waits");
    drop(listening);
    assert!(reply.try_take().is_none(), "still waiting for the browser");
    reply.cancel();
    let error = take(&mut reply).expect_err("cancelled");
    assert!(matches!(error, Error::Cancelled { sent: Some(false), .. }), "{error:?}");
    assert_eq!(error.was_sent(), Some(false));
    let after = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(10));
    assert!(after.is_err(), "the loopback listener is closed after the cancel");
    assert!(!waiting.is_logged_in());
    drop(runtime);
}

#[test]
fn the_blocking_client_links_a_provider_account() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("runtime");
    let provider = runtime.block_on(start_provider());
    let server = server_for(&provider);
    let client = net_backend_client::blocking::Client::new(&server.base).expect("client");
    let registered = client.register(RegisterRequest::new(email("oauth-blocking-linker"), PASSWORD)).expect("register");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let linked = client.link_oauth_sign_in("test", &flow(&provider), browser(provider.clone(), "sub-bl", Browser::SignIn, seen)).expect("linked");
    assert_eq!(linked.account.id, registered.account.id);
    let me = client.call(&GetAccount::new()).expect("account");
    assert_eq!(me.identities.iter().map(|i| (i.provider.as_str(), i.subject.as_str())).collect::<Vec<_>>(), vec![("test", "sub-bl")]);

    // Without blocking (a game loop): polled every frame until the link is done.
    let looping = net_backend_client::blocking::Client::new(&server.base).expect("client");
    let registered = looping.register(RegisterRequest::new(email("oauth-loop-linker"), PASSWORD)).expect("register");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut reply = looping.start_link_oauth_sign_in("test", &flow(&provider), browser(provider.clone(), "sub-bll", Browser::SignIn, seen));
    let mut frames = 0u32;
    let linked = loop {
        if let Some(answer) = reply.try_take() {
            break answer.expect("linked");
        }
        frames += 1;
        assert!(frames < 12_000, "no answer");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(linked.account.id, registered.account.id);
    let me = looping.call(&GetAccount::new()).expect("account");
    assert_eq!(me.identities.iter().map(|i| (i.provider.as_str(), i.subject.as_str())).collect::<Vec<_>>(), vec![("test", "sub-bll")]);

    // The player never comes back: the game cancels. Never sent; the loopback listener is closed;
    // nothing is linked.
    let (sender, redirect) = std::sync::mpsc::channel::<String>();
    let mut reply = looping.start_link_oauth_sign_in("test", &flow(&provider), move |url: &str| {
        let _ = sender.send(query(url).get("redirect_uri").cloned().unwrap_or_default());
        Ok(())
    });
    let redirect = redirect.recv_timeout(WAIT).expect("the sign-in URL");
    let port: u16 = redirect.trim_start_matches("http://127.0.0.1:").trim_end_matches("/callback").parse().expect("port");
    assert!(reply.try_take().is_none(), "still waiting for the browser");
    reply.cancel();
    let error = take(&mut reply).expect_err("cancelled");
    assert!(matches!(error, Error::Cancelled { sent: Some(false), .. }), "{error:?}");
    let after = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(10));
    assert!(after.is_err(), "the loopback listener is closed after the cancel");
    assert_eq!(looping.call(&GetAccount::new()).expect("account").identities.len(), 1, "nothing more linked");
    drop(runtime);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_pre_connection_does_not_hold_up_the_redirect() {
    let provider = start_provider().await;
    let server = server_for(&provider);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    server.client().sign_in_oauth("test", &flow(&provider), browser(provider.clone(), "sub-pre", Browser::PreconnectFirst, seen)).await.expect("signed in");
    // The idle connection gets 10 s to send its request; the redirect is served meanwhile.
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
}

/// A CONNECT proxy on 127.0.0.1 that records each request line and refuses it (403).
async fn recording_proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let lines = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&lines);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 8192 {
                    match socket.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_string();
                seen.lock().expect("lock").push(line);
                let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await;
            });
        }
    });
    (url, lines)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_token_endpoint_gets_the_proxy_of_its_own_host() {
    let provider = start_provider().await;
    let (proxy, lines) = recording_proxy().await;
    let seen = Arc::new(Mutex::new(Vec::new()));

    // The game server is loopback (always direct); the token endpoint is not: through the proxy.
    let remote_token = OAuthFlow::new(format!("{}/authorize", provider.issuer), "https://idp.example.com/token", CLIENT_ID).timeout(Duration::from_secs(20));
    let client = net_backend_client::Client::builder("http://127.0.0.1:9").proxy(&proxy).build().expect("client");
    let failed = client.sign_in_oauth("test", &remote_token, browser(provider.clone(), "sub-p1", Browser::SignIn, seen.clone())).await;
    assert!(failed.is_err(), "the proxy refuses the tunnel");
    assert_eq!(*lines.lock().expect("lock"), ["CONNECT idp.example.com:443 HTTP/1.1".to_string()]);
    // With no_proxy nothing goes through it.
    let direct = net_backend_client::Client::builder("http://127.0.0.1:9").no_proxy().build().expect("client");
    let _ = direct.sign_in_oauth("test", &remote_token, browser(provider.clone(), "sub-p2", Browser::SignIn, seen.clone())).await;
    assert_eq!(lines.lock().expect("lock").len(), 1, "no_proxy: direct");

    // The reverse: the game server goes through the proxy, the (loopback) token endpoint does not.
    lines.lock().expect("lock").clear();
    let client = net_backend_client::Client::builder("https://game.example.com").proxy(&proxy).build().expect("client");
    let failed = client.sign_in_oauth("test", &flow(&provider), browser(provider.clone(), "sub-p3", Browser::SignIn, seen)).await;
    assert!(failed.is_err(), "the server login goes to the refusing proxy");
    let secrets = provider.secrets.lock().expect("lock").clone();
    assert!(secrets.iter().any(|s| s.starts_with("code-")), "the code exchange reached the loopback token endpoint directly");
    assert_eq!(*lines.lock().expect("lock"), ["CONNECT game.example.com:443 HTTP/1.1".to_string()], "only the server login used the proxy");
}
