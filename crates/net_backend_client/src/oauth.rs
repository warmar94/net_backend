//! The desktop sign-in at an OpenID Connect provider (feature `oauth`): the authorization code flow
//! with PKCE and a loopback redirect (RFC 8252), which ends with the provider's ID token for the
//! server's `POST /v1/auth/oauth/{provider}`.
//!
//! 1. A one-time listener on `127.0.0.1` (a free port) is the redirect address
//!    (`http://127.0.0.1:{port}/callback`).
//! 2. A fresh PKCE verifier (S256), `state` and nonce (random bytes from the operating system).
//! 3. The app opens the provider's sign-in page in the system browser: [`OAuthFlow::sign_in`] hands
//!    the URL to a callback the app gives (open the browser, or show / print the URL; the crate
//!    never starts a browser itself). [`print_url`] is a ready callback that prints it.
//! 4. The browser comes back to the listener with `code` and `state`: a wrong `state` is answered
//!    with an error page and ignored (the flow keeps waiting); the provider's `error` ends the flow;
//!    anything else on the port gets 404. The page the player sees says to return to the game.
//! 5. The code (with the verifier) is exchanged at the provider's token endpoint (`https://`, or
//!    `http://` only on loopback) for the ID token. The client's TLS settings and proxy apply.
//!
//! The ID token and the nonce then go to the server ([`SignedIn::token`] →
//! [`OAuthLogin`](crate::protocol::oauth::OAuthLogin)); [`Client::sign_in_oauth`](crate::Client::sign_in_oauth)
//! does all of it. [`SignedIn`] also hands over the provider's access and refresh tokens (for the
//! provider's own API), the token type, expiry and scope. In a game loop without async,
//! [`blocking::Client::start_sign_in_oauth`](crate::blocking::Client::start_sign_in_oauth) (and
//! [`start_link_oauth_sign_in`](crate::blocking::Client::start_link_oauth_sign_in)) returns a
//! cancellable [`Reply`](crate::Reply). The sign-in's own texts (the URL with `state` and nonce, the
//! redirect with the code, the verifier, the code exchange's body) are wiped when dropped. Google: [`OAuthFlow::google`] with the client id (and the client secret Google
//! gives "Desktop app" clients, which is not confidential in a desktop program).
//!
//! ```no_run
//! use net_backend_client::oauth::{print_url, OAuthFlow};
//! use net_backend_client::Client;
//!
//! # async fn run() -> Result<(), net_backend_client::Error> {
//! let client = Client::new("https://api.example.com")?;
//! let flow = OAuthFlow::google("1234-abc.apps.googleusercontent.com").client_secret("GOCSPX-desktop-secret");
//! // Open the system browser with the URL instead of printing it (e.g. with the `open` crate).
//! let session = client.sign_in_oauth("google", &flow, print_url).await?;
//! println!("logged in as account {}", session.account.id);
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use net_backend_protocol::oauth::OAuthToken;
use net_backend_protocol::Secret;
use ring::rand::{SecureRandom, SystemRandom};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::Error;

/// Google's authorization endpoint.
pub const GOOGLE_AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's token endpoint.
pub const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// How long [`OAuthFlow::sign_in`] waits for the browser by default (5 minutes).
pub const DEFAULT_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// The largest request the loopback listener reads (the browser's redirect).
const MAX_REDIRECT_BYTES: usize = 16 * 1024;

/// The settings of one provider's desktop sign-in. Build it once, run [`sign_in`](Self::sign_in)
/// per sign-in.
#[derive(Clone)]
pub struct OAuthFlow {
    authorization_endpoint: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<Secret>,
    scopes: Vec<String>,
    timeout: Duration,
    extra: Vec<(String, String)>,
}

impl fmt::Debug for OAuthFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthFlow")
            .field("authorization_endpoint", &self.authorization_endpoint)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "<redacted>"))
            .field("scopes", &self.scopes)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl OAuthFlow {
    /// A provider's endpoints (from its discovery document) and the game's client id there. Scope
    /// `openid`.
    pub fn new(authorization_endpoint: impl Into<String>, token_endpoint: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            authorization_endpoint: authorization_endpoint.into(),
            token_endpoint: token_endpoint.into(),
            client_id: client_id.into(),
            client_secret: None,
            scopes: vec!["openid".into()],
            timeout: DEFAULT_SIGN_IN_TIMEOUT,
            extra: Vec::new(),
        }
    }

    /// Google's endpoints with this client id.
    pub fn google(client_id: impl Into<String>) -> Self {
        Self::new(GOOGLE_AUTHORIZATION_ENDPOINT, GOOGLE_TOKEN_ENDPOINT, client_id)
    }

    /// The client secret, for providers that want one from installed apps (Google's "Desktop app"
    /// clients); sent only to the token endpoint.
    pub fn client_secret(mut self, secret: impl Into<Secret>) -> Self {
        self.client_secret = Some(secret.into());
        self
    }

    /// The scopes (default `openid`; `openid` is always sent). E.g. `["openid", "email"]` for the
    /// email address in the ID token.
    pub fn scopes<I: IntoIterator<Item = S>, S: Into<String>>(mut self, scopes: I) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        if !self.scopes.iter().any(|s| s == "openid") {
            self.scopes.insert(0, "openid".into());
        }
        self
    }

    /// How long to wait for the browser to come back (default 5 minutes; at least 1 second).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_secs(1));
        self
    }

    /// An extra query parameter for the sign-in page (e.g. `prompt=select_account`, `login_hint`).
    /// The flow's own parameters cannot be replaced.
    pub fn param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra.push((name.into(), value.into()));
        self
    }

    /// Run the sign-in: listen on loopback, hand the sign-in URL to `open`, wait for the browser,
    /// exchange the code (with the default TLS settings and the environment's proxy settings,
    /// `HTTPS_PROXY` / `NO_PROXY`, for the token endpoint's host). `open` returning an error ends
    /// the flow ([`Error::OAuth`]). The returned ID token has not been checked by anyone yet: the
    /// server checks it.
    pub async fn sign_in<F>(&self, open: F) -> Result<SignedIn, Error>
    where
        F: FnOnce(&str) -> Result<(), String>,
    {
        self.sign_in_with(&crate::Client::builder("http://127.0.0.1:1").build()?, open).await
    }

    /// [`sign_in`](Self::sign_in) with the TLS settings and proxy setting of `client` (the proxy
    /// decided for the token endpoint's own host).
    pub(crate) async fn sign_in_with<F>(&self, client: &crate::Client, open: F) -> Result<SignedIn, Error>
    where
        F: FnOnce(&str) -> Result<(), String>,
    {
        crate::runtime::current()?;
        let started = Instant::now();
        let deadline = started.checked_add(self.timeout).unwrap_or(started);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| Error::invalid(format!("could not listen on loopback: {e}")))?;
        let port = listener.local_addr().map_err(|e| Error::invalid(format!("could not listen on loopback: {e}")))?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let verifier = Zeroizing::new(random_text(32)?);
        let challenge = URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()));
        let state = Zeroizing::new(random_text(16)?);
        let nonce = Secret::new(random_text(16)?);
        let url = self.authorization_url(&redirect_uri, &challenge, &state, nonce.expose())?;
        open(&url).map_err(|why| Error::OAuth(format!("the sign-in page could not be opened: {why}")))?;
        drop(url);
        let code = wait_for_code(&listener, &state, deadline).await?;
        drop(listener);
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", self.client_id.as_str()),
            ("code_verifier", verifier.as_str()),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.expose()));
        }
        let body = form_body(&form);
        drop(form);
        let exchange_deadline = Instant::now().checked_add(client.inner.timeout).unwrap_or(deadline);
        let answer = client.inner.http.post_form(&self.token_endpoint, Bytes::from_owner(body), exchange_deadline).await?;
        let mut tokens = token_answer(answer.status, &answer.body)?;
        tokens.nonce = nonce;
        Ok(tokens)
    }

    /// The sign-in page URL (it carries `state` and the nonce: built in a buffer that is wiped
    /// when dropped).
    fn authorization_url(&self, redirect_uri: &str, challenge: &str, state: &str, nonce: &str) -> Result<Zeroizing<String>, Error> {
        let base = &self.authorization_endpoint;
        let plain_ok = base.starts_with("http://") && crate::http::BaseUrl::parse(base.split('?').next().unwrap_or(base)).is_ok_and(|b| b.is_loopback());
        if !(base.starts_with("https://") || plain_ok) {
            return Err(Error::invalid("the authorization endpoint must be https:// (http:// only for a loopback address)"));
        }
        let scope = self.scopes.join(" ");
        let mut params: Vec<(&str, &str)> = vec![
            ("response_type", "code"),
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", redirect_uri),
            ("scope", scope.as_str()),
            ("state", state),
            ("nonce", nonce),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ];
        for (name, value) in &self.extra {
            if !params.iter().any(|(n, _)| *n == name.as_str()) {
                params.push((name.as_str(), value.as_str()));
            }
        }
        let joiner = if base.contains('?') { '&' } else { '?' };
        let mut url = Zeroizing::new(String::with_capacity(base.len().saturating_add(1).saturating_add(encoded_pairs_len(&params))));
        url.push_str(base);
        url.push(joiner);
        encode_pairs_into(&params, &mut url);
        Ok(url)
    }
}

/// What a finished sign-in gives: the provider's ID token, the other tokens its token endpoint
/// sent, and the nonce of this sign-in. Every token is a [`Secret`] (`Debug` prints
/// `<redacted>`; wiped when dropped).
///
/// The access and refresh tokens are the provider's, for the provider's own API (a profile
/// picture, a game service of the provider); the server never needs them.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SignedIn {
    /// The ID token (a compact JWT; not checked by the client).
    pub id_token: Secret,
    /// The nonce this sign-in put into the request.
    pub nonce: Secret,
    /// The provider's access token, if its token endpoint sent one.
    pub access_token: Option<Secret>,
    /// The provider's refresh token, if sent.
    pub refresh_token: Option<Secret>,
    /// The token type (`Bearer`), if sent.
    pub token_type: Option<String>,
    /// How long the access token is valid, if sent.
    pub expires_in: Option<Duration>,
    /// The scopes the provider granted, if sent.
    pub scope: Option<String>,
}

impl SignedIn {
    /// The body of the server's login route.
    pub fn token(&self) -> OAuthToken {
        OAuthToken::new(self.id_token.clone()).with_nonce(self.nonce.clone())
    }
}

/// A ready `open` callback for [`OAuthFlow::sign_in`]: prints the URL to stderr for the player to
/// open (command-line tools).
pub fn print_url(url: &str) -> Result<(), String> {
    eprintln!("Open this address in your browser to sign in:\n{url}");
    Ok(())
}

/// `bytes` random bytes from the operating system, base64url.
fn random_text(bytes: usize) -> Result<String, Error> {
    let mut buffer = Zeroizing::new(vec![0u8; bytes]);
    SystemRandom::new().fill(&mut buffer).map_err(|_| Error::invalid("the operating system's random source failed"))?;
    Ok(URL_SAFE_NO_PAD.encode(&*buffer))
}

/// The token endpoint's answer: the tokens (the nonce is set by the caller), or why not. The
/// token texts are moved out of the decoded JSON into [`Secret`]s (no further copies).
fn token_answer(status: u16, body: &[u8]) -> Result<SignedIn, Error> {
    let value: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    if !(200..300).contains(&status) {
        let code = value.as_ref().and_then(|v| v.get("error")).and_then(serde_json::Value::as_str).map_or_else(|| "no error code".to_string(), short);
        return Err(Error::OAuth(format!("the token endpoint refused the code (HTTP {status}, {code})")));
    }
    let Some(serde_json::Value::Object(mut map)) = value else {
        return Err(Error::OAuth(format!("the token endpoint's answer (HTTP {status}) is not a JSON object")));
    };
    let mut take = |key: &str| match map.remove(key) {
        Some(serde_json::Value::String(text)) => Some(text),
        _ => None,
    };
    let id_token = take("id_token").map(Secret::new).ok_or_else(|| Error::OAuth("the token answer has no id_token (is `openid` among the scopes?)".into()))?;
    let access_token = take("access_token").map(Secret::new);
    let refresh_token = take("refresh_token").map(Secret::new);
    let token_type = take("token_type");
    let scope = take("scope");
    let expires_in = match map.get("expires_in") {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
    .map(Duration::from_secs);
    Ok(SignedIn { id_token, nonce: Secret::new(String::new()), access_token, refresh_token, token_type, expires_in, scope })
}

/// Whether a byte stays as it is in a percent-encoded value (the unreserved characters).
fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// The length of `text` percent-encoded.
fn encoded_len(text: &str) -> usize {
    text.bytes().map(|b| if unreserved(b) { 1usize } else { 3 }).fold(0usize, usize::saturating_add)
}

/// The length of `name=value&…` for `pairs`.
fn encoded_pairs_len(pairs: &[(&str, &str)]) -> usize {
    pairs.iter().map(|(k, v)| encoded_len(k).saturating_add(encoded_len(v)).saturating_add(1)).fold(pairs.len().saturating_sub(1), usize::saturating_add)
}

/// Append `text` percent-encoded (everything but the unreserved characters) to `out`.
fn encode_into(text: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in text.bytes() {
        if unreserved(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
}

/// Append `name=value&…` (percent-encoded) to `out`.
fn encode_pairs_into(pairs: &[(&str, &str)], out: &mut String) {
    for (n, (name, value)) in pairs.iter().enumerate() {
        if n > 0 {
            out.push('&');
        }
        encode_into(name, out);
        out.push('=');
        encode_into(value, out);
    }
}

/// An `application/x-www-form-urlencoded` body of `pairs` (the code, the verifier, the client
/// secret) in one exactly sized buffer (no reallocation leaves a copy behind) that is wiped when the
/// last `Bytes` made from it is dropped.
fn form_body(pairs: &[(&str, &str)]) -> Zeroizing<Vec<u8>> {
    let mut text = Zeroizing::new(String::with_capacity(encoded_pairs_len(pairs)));
    encode_pairs_into(pairs, &mut text);
    // Moves the allocation (no copy) into the wiped byte buffer.
    Zeroizing::new(std::mem::take(&mut *text).into_bytes())
}

/// Percent-decode a query value (`+` is a space) into a buffer that is wiped when dropped (it
/// may be the authorization code).
fn decode(text: &str) -> Zeroizing<String> {
    let bytes = text.as_bytes();
    let mut out = Zeroizing::new(Vec::with_capacity(bytes.len()));
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    Zeroizing::new(String::from_utf8_lossy(&out).into_owned())
}

/// A provider's text for a message: short, printable.
fn short(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(80).collect()
}

/// What one request to the listener asked for.
enum Redirect {
    Code(Zeroizing<String>),
    Refused(Zeroizing<String>),
    WrongState,
    Other,
}

fn parse_redirect(head: &str, state: &str) -> Redirect {
    let Some(target) = head.lines().next().and_then(|line| line.strip_prefix("GET ")).and_then(|rest| rest.split(' ').next()) else {
        return Redirect::Other;
    };
    let Some(query) = target.strip_prefix("/callback?") else { return Redirect::Other };
    let mut code = None;
    let mut error = None;
    let mut got_state = None;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match name {
            "code" => code = Some(decode(value)),
            "error" => error = Some(decode(value)),
            "state" => got_state = Some(decode(value)),
            _ => {}
        }
    }
    let state_ok = got_state.as_deref().is_some_and(|s| s.len() == state.len() && s.bytes().zip(state.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0);
    if !state_ok {
        return Redirect::WrongState;
    }
    match (code, error) {
        (_, Some(error)) => Redirect::Refused(error),
        (Some(code), None) if !code.is_empty() => Redirect::Code(code),
        _ => Redirect::Other,
    }
}

fn page(status: &str, text: &str) -> String {
    let body = format!("<!doctype html><meta charset=\"utf-8\"><title>Sign-in</title><p>{text}</p>");
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The time a connection to the listener gets to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// At most this many connections to the listener are read at once.
const MAX_OPEN_CONNECTIONS: usize = 16;

/// What happened on the listener.
enum ListenerEvent {
    Accepted(std::io::Result<(TcpStream, std::net::SocketAddr)>),
    Served(Result<Option<Result<Zeroizing<String>, Error>>, tokio::task::JoinError>),
}

/// Accept requests on the listener until the browser brings the code (or the deadline). Every
/// connection is read on its own task, so a connection that sends nothing (a browser's idle
/// pre-connection, another local process) never holds up the real redirect.
async fn wait_for_code(listener: &TcpListener, state: &str, deadline: Instant) -> Result<Zeroizing<String>, Error> {
    let state = std::sync::Arc::new(Zeroizing::new(state.to_string()));
    // Dropped on return: the connections still open are closed.
    let mut open = tokio::task::JoinSet::new();
    loop {
        let event = std::future::poll_fn(|cx| {
            if let std::task::Poll::Ready(Some(served)) = open.poll_join_next(cx) {
                return std::task::Poll::Ready(ListenerEvent::Served(served));
            }
            if open.len() < MAX_OPEN_CONNECTIONS {
                if let std::task::Poll::Ready(accepted) = listener.poll_accept(cx) {
                    return std::task::Poll::Ready(ListenerEvent::Accepted(accepted));
                }
            }
            std::task::Poll::Pending
        });
        match tokio::time::timeout_at(deadline, event).await {
            Err(_) => return Err(Error::OAuth("the browser did not come back before the time limit".into())),
            Ok(ListenerEvent::Accepted(Err(e))) => return Err(Error::OAuth(format!("the loopback listener failed: {e}"))),
            Ok(ListenerEvent::Accepted(Ok((stream, _)))) => {
                open.spawn(serve_redirect(stream, std::sync::Arc::clone(&state)));
            }
            Ok(ListenerEvent::Served(Ok(Some(outcome)))) => return outcome,
            Ok(ListenerEvent::Served(_)) => {}
        }
    }
}

/// Read one request to the listener and answer it; the code (or the provider's refusal) when it
/// is the redirect with the right `state`.
async fn serve_redirect(mut stream: TcpStream, state: std::sync::Arc<Zeroizing<String>>) -> Option<Result<Zeroizing<String>, Error>> {
    let Ok(Ok(head)) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await else { return None };
    let (answer, outcome) = match parse_redirect(&head, &state) {
        Redirect::Code(code) => (page("200 OK", "You can close this tab and return to the game."), Some(Ok(code))),
        Redirect::Refused(error) => (
            page("200 OK", "The sign-in was not completed. Return to the game."),
            Some(Err(Error::OAuth(format!("the provider answered `{}`", short(&error))))),
        ),
        Redirect::WrongState => (page("400 Bad Request", "This sign-in link is not the current one."), None),
        Redirect::Other => (page("404 Not Found", "Not found."), None),
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = stream.write_all(answer.as_bytes()).await;
        let _ = stream.shutdown().await;
    })
    .await;
    outcome
}

/// The request head (up to the empty line), at most [`MAX_REDIRECT_BYTES`] (plus one read). It
/// holds the authorization code: read into buffers that never grow (no copy left behind) and are
/// wiped when dropped.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<Zeroizing<String>> {
    const CHUNK: usize = 1024;
    let mut buffer = Zeroizing::new(Vec::with_capacity(MAX_REDIRECT_BYTES + CHUNK));
    let mut chunk = Zeroizing::new([0u8; CHUNK]);
    loop {
        let n = stream.read(&mut chunk[..]).await?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") || buffer.len() >= MAX_REDIRECT_BYTES {
            break;
        }
    }
    Ok(Zeroizing::new(String::from_utf8_lossy(&buffer).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_redirects() {
        let flow = OAuthFlow::google("cid").scopes(["email"]).param("prompt", "select_account").param("state", "evil");
        let url = flow.authorization_url("http://127.0.0.1:5/callback", "chal", "st", "no").map(|u| u.to_string()).unwrap_or_default();
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id=cid"), "{url}");
        for part in [
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2Fcallback",
            "scope=openid%20email",
            "state=st",
            "nonce=no",
            "code_challenge=chal",
            "code_challenge_method=S256",
            "prompt=select_account",
        ] {
            assert!(url.contains(part), "{part}: {url}");
        }
        assert!(!url.contains("evil"), "the flow's own parameters are not replaced");
        assert!(OAuthFlow::new("http://login.example.com/auth", "https://t", "c").authorization_url("r", "c", "s", "n").is_err());
        assert!(format!("{:?}", OAuthFlow::google("c").client_secret("very-secret")).contains("<redacted>"));
        assert!(!format!("{:?}", OAuthFlow::google("c").client_secret("very-secret")).contains("very-secret"));

        assert!(matches!(parse_redirect("GET /callback?code=a%2Fb&state=st HTTP/1.1\r\n", "st"), Redirect::Code(c) if c.as_str() == "a/b"));
        assert!(
            matches!(parse_redirect("GET /callback?error=access_denied&state=st HTTP/1.1\r\n", "st"), Redirect::Refused(e) if e.as_str() == "access_denied")
        );
        assert!(matches!(parse_redirect("GET /callback?code=a&state=other HTTP/1.1\r\n", "st"), Redirect::WrongState));
        assert!(matches!(parse_redirect("GET /callback?code=a HTTP/1.1\r\n", "st"), Redirect::WrongState));
        assert!(matches!(parse_redirect("GET /favicon.ico HTTP/1.1\r\n", "st"), Redirect::Other));
        assert!(matches!(parse_redirect("POST /callback?code=a&state=st HTTP/1.1\r\n", "st"), Redirect::Other));
        assert_eq!(decode("a%20b+c%zz%4").as_str(), "a b c%zz%4");
        assert_eq!(random_text(32).map(|t| t.len()).unwrap_or(0), 43);
    }

    #[test]
    fn the_code_exchange_body_is_one_exactly_sized_wiped_buffer() {
        let pairs = [("grant_type", "authorization_code"), ("code", "4/0Ab+c d"), ("code_verifier", "ver~-._"), ("client_secret", "s%cr&t=ä")];
        let body = form_body(&pairs);
        assert_eq!(
            std::str::from_utf8(&body).ok(),
            Some("grant_type=authorization_code&code=4%2F0Ab%2Bc%20d&code_verifier=ver~-._&client_secret=s%25cr%26t%3D%C3%A4")
        );
        // Exactly sized: the text was never moved to a bigger allocation (which would leave an
        // unwiped copy behind); `Zeroizing` wipes this one when the request's last `Bytes` is dropped.
        assert_eq!(body.len(), body.capacity());
        assert_eq!(encoded_pairs_len(&[]), 0);
        let url = OAuthFlow::google("cid").authorization_url("http://127.0.0.1:5/callback", "chal", "st", "no").unwrap_or_default();
        assert_eq!(url.len(), url.capacity(), "the sign-in URL (state, nonce) is exactly sized too");
    }

    #[test]
    fn the_token_answer_hands_every_token_over() {
        let body =
            br#"{"id_token":"id.jwt.sig","access_token":"ya29.fake","refresh_token":"1//fake","token_type":"Bearer","expires_in":3599,"scope":"openid email"}"#;
        let signed = token_answer(200, body).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(signed.id_token.expose(), "id.jwt.sig");
        assert_eq!(signed.access_token.as_ref().map(Secret::expose), Some("ya29.fake"));
        assert_eq!(signed.refresh_token.as_ref().map(Secret::expose), Some("1//fake"));
        assert_eq!(
            (signed.token_type.as_deref(), signed.expires_in, signed.scope.as_deref()),
            (Some("Bearer"), Some(Duration::from_secs(3599)), Some("openid email"))
        );
        let shown = format!("{signed:?}");
        assert!(!shown.contains("id.jwt.sig") && !shown.contains("ya29.fake") && !shown.contains("1//fake"), "{shown}");
        // Only the ID token is required; `expires_in` as text is read too.
        let signed = token_answer(200, br#"{"id_token":"x","expires_in":"60","access_token":7}"#).unwrap_or_else(|e| panic!("{e}"));
        assert!(signed.access_token.is_none() && signed.refresh_token.is_none() && signed.expires_in == Some(Duration::from_secs(60)));
        assert!(matches!(token_answer(200, br#"{"access_token":"a"}"#), Err(Error::OAuth(why)) if why.contains("no id_token")));
        assert!(matches!(token_answer(200, b"not json"), Err(Error::OAuth(why)) if why.contains("not a JSON object")));
        assert!(matches!(token_answer(400, br#"{"error":"invalid_grant"}"#), Err(Error::OAuth(why)) if why.contains("HTTP 400, invalid_grant")));
        assert!(matches!(token_answer(502, b"<html>"), Err(Error::OAuth(why)) if why.contains("no error code")));
    }
}
