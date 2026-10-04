//! The HTTP transport: hyper 1 (HTTP/1.1; with feature `http2` and `ClientBuilder::http2` also
//! HTTP/2 when an HTTPS server picks it through ALPN) through hyper-util's pooled client and hyper-rustls with
//! the crate's ring config. One deadline covers connecting, sending and reading the whole answer;
//! the answer body is capped; redirects are never followed; nothing is decompressed (the client
//! never asks for compression, so there is nothing to inflate and no compression bomb). A proxy
//! (from the environment or the builder) is an HTTP CONNECT tunnel, decided per destination host
//! (the server, an identity provider's token endpoint), never used for loopback.

use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, DATE, RETRY_AFTER, USER_AGENT};
use http::{Method, Request, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::proxy::matcher::Matcher;
use hyper_util::rt::TokioExecutor;
use net_backend_protocol::{ErrorBody, HttpCall, PayloadKind, PROTOCOL_HEADER, PROTOCOL_VERSION};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::Error;

/// Where the proxy comes from ([`ClientBuilder::proxy`](crate::ClientBuilder::proxy)).
#[derive(Clone, Debug, Default)]
pub(crate) enum ProxySetting {
    /// `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` (and the lowercase forms).
    #[default]
    Env,
    /// This proxy for every host that is not loopback.
    Url(String),
    /// No proxy.
    Off,
}

/// The HTTP proxy every connection of a client goes through: an HTTP CONNECT tunnel.
#[derive(Clone)]
pub(crate) struct Proxy {
    pub(crate) uri: Uri,
    /// `Proxy-Authorization: Basic …` (from `user:password@` in the proxy URL).
    pub(crate) auth: Option<HeaderValue>,
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the credentials.
        f.debug_struct("Proxy").field("host", &self.uri.host()).field("port", &self.uri.port_u16()).field("auth", &self.auth.is_some()).finish()
    }
}

impl Proxy {
    /// The proxy for `base` under `setting`: none for loopback, none when the rules say so.
    /// Only `http://` proxies (HTTP CONNECT) are used: another scheme is refused, so a request
    /// never bypasses a proxy that was asked for.
    pub(crate) fn resolve(setting: &ProxySetting, base: &BaseUrl) -> Result<Option<Self>, Error> {
        if base.is_loopback() {
            return Ok(None);
        }
        let matcher = match setting {
            ProxySetting::Off => return Ok(None),
            ProxySetting::Env => Matcher::from_env(),
            ProxySetting::Url(url) => {
                let uri: Uri = url.trim().parse().map_err(|_| Error::invalid("the proxy URL does not parse (use http://host:port)"))?;
                if uri.scheme_str() != Some("http") || uri.host().is_none_or(str::is_empty) {
                    return Err(Error::invalid("the proxy URL must be http://[user:password@]host:port (an HTTP CONNECT proxy)"));
                }
                Matcher::builder().all(url.trim().to_string()).build()
            }
        };
        Self::from_matcher(&matcher, base)
    }

    fn from_matcher(matcher: &Matcher, base: &BaseUrl) -> Result<Option<Self>, Error> {
        let destination: Uri = base.url("/").parse().map_err(|_| Error::invalid("the server URL is not valid"))?;
        let Some(intercept) = matcher.intercept(&destination) else { return Ok(None) };
        if intercept.uri().scheme_str() != Some("http") {
            return Err(Error::invalid(format!(
                "the proxy for `{}` is not an http:// proxy (only HTTP CONNECT proxies are used); set ClientBuilder::proxy or ClientBuilder::no_proxy",
                base.host
            )));
        }
        Ok(Some(Self { uri: intercept.uri().clone(), auth: intercept.basic_auth().cloned() }))
    }

    fn tunnel(&self) -> Tunnel<HttpConnector> {
        let tunnel = Tunnel::new(self.uri.clone(), HttpConnector::new());
        match &self.auth {
            Some(auth) => tunnel.with_auth(auth.clone()),
            None => tunnel,
        }
    }

    /// A TCP stream to `host:port` through the proxy (the CONNECT answered 2xx), for the
    /// WebSocket. The crate writes the `CONNECT` request itself, from a buffer that is wiped when
    /// dropped (it carries `Proxy-Authorization`), and reads exactly the proxy's answer header.
    #[cfg(feature = "ws")]
    pub(crate) async fn connect(&self, host: &str, port: u16) -> Result<tokio::net::TcpStream, Error> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let fail = |why: String| Error::network(format!("could not connect through the proxy: {why}"), Some(false));
        let proxy_host = self.uri.host().unwrap_or_default();
        let proxy_host = proxy_host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(proxy_host);
        let mut tcp = tokio::net::TcpStream::connect((proxy_host, self.uri.port_u16().unwrap_or(80))).await.map_err(|e| fail(e.to_string()))?;
        let target = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
        let request = connect_request(&target, self.auth.as_ref().map(HeaderValue::as_bytes));
        tcp.write_all(&request).await.map_err(|e| fail(e.to_string()))?;
        tcp.flush().await.map_err(|e| fail(e.to_string()))?;
        drop(request);
        // One byte at a time up to the blank line: the bytes after it belong to the tunnel.
        let mut answer = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        while !answer.ends_with(b"\r\n\r\n") {
            if answer.len() >= MAX_CONNECT_ANSWER {
                return Err(fail(format!("its answer header is longer than {MAX_CONNECT_ANSWER} bytes")));
            }
            match tcp.read(&mut byte).await {
                Ok(0) => return Err(fail("it closed the connection".into())),
                Ok(_) => answer.push(byte[0]),
                Err(e) => return Err(fail(e.to_string())),
            }
        }
        match connect_status(&answer) {
            Some(status) if (200..300).contains(&status) => Ok(tcp),
            Some(407) => Err(fail("proxy authorization required (407)".into())),
            Some(status) => Err(fail(format!("it answered {status}"))),
            None => Err(fail("its answer is not HTTP".into())),
        }
    }
}

/// The longest `CONNECT` answer header accepted.
#[cfg(feature = "ws")]
const MAX_CONNECT_ANSWER: usize = 16 * 1024;

/// `CONNECT target HTTP/1.1` with `Host` and, when given, `Proxy-Authorization`, in one exactly
/// sized buffer that is wiped when dropped.
#[cfg(feature = "ws")]
fn connect_request(target: &str, authorization: Option<&[u8]>) -> Zeroizing<Vec<u8>> {
    const AUTH: &[u8] = b"Proxy-Authorization: ";
    let size = 8 + target.len() + 17 + target.len() + 2 + authorization.map_or(0, |a| AUTH.len() + a.len() + 2) + 2;
    let mut request = Zeroizing::new(Vec::with_capacity(size));
    request.extend_from_slice(b"CONNECT ");
    request.extend_from_slice(target.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(target.as_bytes());
    request.extend_from_slice(b"\r\n");
    if let Some(authorization) = authorization {
        request.extend_from_slice(AUTH);
        request.extend_from_slice(authorization);
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    request
}

/// The status code of an HTTP/1.x answer header (`HTTP/1.1 200 Connection established`).
#[cfg(feature = "ws")]
fn connect_status(head: &[u8]) -> Option<u16> {
    let line = head.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split_ascii_whitespace();
    if !parts.next()?.starts_with("HTTP/1.") {
        return None;
    }
    let status = parts.next()?;
    if status.len() != 3 {
        return None;
    }
    status.parse().ok()
}

/// `Bearer <token>` as a header value, built in a pre-sized buffer that is wiped when dropped.
/// The header value itself (inside the request) is not wiped.
pub(crate) fn bearer_header(token: &str) -> Result<HeaderValue, Error> {
    let mut text = Zeroizing::new(String::with_capacity(token.len().saturating_add(7)));
    text.push_str("Bearer ");
    text.push_str(token);
    let mut value = HeaderValue::from_str(&text).map_err(|_| Error::invalid("the access token is not a valid header value"))?;
    value.set_sensitive(true);
    Ok(value)
}

/// `value` as JSON in one exactly sized buffer that is overwritten with zeros when the last copy
/// of the returned `Bytes` is dropped (a request body can hold a password or a refresh token).
pub(crate) fn wiped_json<T: Serialize + ?Sized>(value: &T) -> Result<Bytes, serde_json::Error> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)?;
    let mut buffer = Zeroizing::new(Vec::with_capacity(count.0));
    serde_json::to_writer(&mut *buffer, value)?;
    Ok(Bytes::from_owner(buffer))
}

/// `http://` or `https://`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scheme {
    Http,
    Https,
}

/// The server's base URL: scheme, authority (`host[:port]`) and an optional path prefix (a server
/// mounted below a path by its proxy).
#[derive(Clone, Debug)]
pub(crate) struct BaseUrl {
    pub(crate) scheme: Scheme,
    pub(crate) host: String,
    #[cfg_attr(not(feature = "ws"), allow(dead_code))]
    pub(crate) port: u16,
    pub(crate) authority: String,
    pub(crate) prefix: String,
}

impl BaseUrl {
    /// Parse `https://host[:port][/prefix]` (or `http://`). No query, fragment or user info.
    pub(crate) fn parse(url: &str) -> Result<Self, Error> {
        let bad = |why: &str| Error::invalid(format!("the server URL is not valid: {why}"));
        let uri: http::Uri = url.trim().parse().map_err(|_| bad("it does not parse as a URL"))?;
        let scheme = match uri.scheme_str() {
            Some("https") => Scheme::Https,
            Some("http") => Scheme::Http,
            _ => return Err(bad("it must start with https:// (or http://)")),
        };
        let authority = uri.authority().ok_or_else(|| bad("no host"))?;
        if authority.as_str().contains('@') {
            return Err(bad("user info (`user@`) is not supported"));
        }
        let host = authority.host().trim_start_matches('[').trim_end_matches(']').to_string();
        if host.is_empty() {
            return Err(bad("no host"));
        }
        let port = authority.port_u16().unwrap_or(match scheme {
            Scheme::Https => 443,
            Scheme::Http => 80,
        });
        if uri.query().is_some() || url.contains('#') {
            return Err(bad("a query or fragment is not allowed"));
        }
        let prefix = uri.path().trim_end_matches('/').to_string();
        Ok(Self { scheme, host, port, authority: authority.as_str().to_string(), prefix })
    }

    /// Whether the host is loopback (`localhost`, `127.0.0.0/8`, `::1`).
    pub(crate) fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost") || self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }

    /// The full URL of `path` (which starts with `/`).
    pub(crate) fn url(&self, path_and_query: &str) -> String {
        let scheme = match self.scheme {
            Scheme::Https => "https",
            Scheme::Http => "http",
        };
        format!("{scheme}://{}{}{}", self.authority, self.prefix, path_and_query)
    }
}

/// A request ready to go: method, path (with query) and body.
#[derive(Clone)]
pub(crate) struct Outgoing {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) body: Option<Bytes>,
}

impl Outgoing {
    /// The request for a typed call: its method, its path (refused when a parameter would need
    /// escaping) and its payload as a JSON body or query string.
    pub(crate) fn for_call<C: HttpCall>(call: &C) -> Result<Self, Error> {
        let method = Method::from_bytes(C::ROUTE.method.as_str().as_bytes()).map_err(|_| Error::invalid("unknown HTTP method"))?;
        let mut path = call.path().ok_or_else(|| Error::invalid(format!("a path parameter of `{}` is missing or would need escaping", C::ROUTE.path)))?;
        let body = match C::PAYLOAD {
            PayloadKind::Json => Some(wiped_json(call.payload()).map_err(|e| Error::invalid(format!("the JSON body cannot be encoded: {e}")))?),
            PayloadKind::Query => {
                let pairs = net_backend_protocol::http_call::query_pairs(call.payload()).map_err(|e| Error::invalid(e.message))?;
                if !pairs.is_empty() {
                    path.push('?');
                    let encoded: Vec<String> = pairs.iter().map(|(k, v)| format!("{}={}", encode(k), encode(v))).collect();
                    path.push_str(&encoded.join("&"));
                }
                None
            }
            _ => None,
        };
        Ok(Self { method, path, body })
    }
}

/// Percent-encode everything but the unreserved characters (`A-Z a-z 0-9 - . _ ~`).
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// What came back: status, the headers the client uses, the body.
pub(crate) struct Answer {
    pub(crate) status: u16,
    pub(crate) retry_after: Option<Duration>,
    /// The server's clock (`Date`), in Unix milliseconds.
    pub(crate) server_now: Option<i64>,
    pub(crate) body: Bytes,
}

impl Answer {
    /// The body decoded as `T` (2xx), or the error the answer carries.
    pub(crate) fn decode<T: DeserializeOwned>(&self) -> Result<T, Error> {
        if (200..300).contains(&self.status) {
            let body: &[u8] = if self.body.iter().all(u8::is_ascii_whitespace) { b"null" } else { &self.body };
            serde_json::from_slice(body).map_err(|e| Error::Decode { status: Some(self.status), message: e.to_string() })
        } else {
            Err(self.error())
        }
    }

    /// The error of a non-2xx answer: the protocol's error body, or the bare status.
    pub(crate) fn error(&self) -> Error {
        match serde_json::from_slice::<ErrorBody>(&self.body) {
            Ok(body) => Error::api(Some(self.status), body.error, self.retry_after),
            Err(_) => Error::Status { status: self.status, retry_after: self.retry_after },
        }
    }
}

/// A request body: in memory, or streamed (an upload read from disk while it is sent).
pub(crate) type ReqBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// A body in memory.
pub(crate) fn full(bytes: Bytes) -> ReqBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

/// The connection pool: straight to the server, or through a proxy's CONNECT tunnel.
#[derive(Clone)]
enum Pool {
    Direct(HyperClient<HttpsConnector<HttpConnector>, ReqBody>),
    Tunnel(HyperClient<HttpsConnector<Tunnel<HttpConnector>>, ReqBody>),
}

/// A response whose body is read by the caller (downloads).
pub(crate) struct Streamed {
    pub(crate) status: u16,
    pub(crate) headers: http::HeaderMap,
    pub(crate) body: hyper::body::Incoming,
}

/// The pooled HTTP(S) client.
#[derive(Clone)]
pub(crate) struct Http {
    pool: Pool,
    pub(crate) base: BaseUrl,
    max_body: usize,
    /// The proxy every connection to the server goes through (HTTP and the WebSocket).
    #[cfg_attr(not(feature = "ws"), allow(dead_code))]
    pub(crate) proxy: Option<Proxy>,
    /// Where the proxy comes from: decided again for another host (an identity provider's token
    /// endpoint).
    #[cfg_attr(not(feature = "oauth"), allow(dead_code))]
    proxy_setting: ProxySetting,
    /// Whether HTTP/2 is offered (for a pool to another host).
    #[cfg_attr(not(feature = "oauth"), allow(dead_code))]
    http2: bool,
    /// The client's TLS config without ALPN (the trusted roots), shared with the WebSocket.
    #[cfg_attr(not(feature = "ws"), allow(dead_code))]
    pub(crate) tls: Arc<rustls::ClientConfig>,
}

/// hyper-rustls around `inner`: HTTP/1.1, plus HTTP/2 in the ALPN offer when asked.
#[cfg(feature = "http2")]
fn https<C>(tls: rustls::ClientConfig, inner: C, http2: bool) -> HttpsConnector<C> {
    let builder = hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls).https_or_http().enable_http1();
    if http2 {
        builder.enable_http2().wrap_connector(inner)
    } else {
        builder.wrap_connector(inner)
    }
}

/// hyper-rustls around `inner`: HTTP/1.1.
#[cfg(not(feature = "http2"))]
fn https<C>(tls: rustls::ClientConfig, inner: C, _http2: bool) -> HttpsConnector<C> {
    hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls).https_or_http().enable_http1().wrap_connector(inner)
}

impl Http {
    /// The client for `base`, its proxy decided by `proxy_setting` (none for loopback).
    pub(crate) fn new(base: BaseUrl, max_body: usize, proxy_setting: ProxySetting, trust: &crate::tls::TrustSettings, http2: bool) -> Result<Self, Error> {
        let proxy = Proxy::resolve(&proxy_setting, &base)?;
        let tls = Arc::new(crate::tls::client_config(trust)?);
        let pool = Self::pool(&tls, proxy.as_ref(), http2);
        Ok(Self { pool, base, max_body, proxy, proxy_setting, http2, tls })
    }

    /// A connection pool: straight, or through `proxy`'s CONNECT tunnel.
    fn pool(tls: &rustls::ClientConfig, proxy: Option<&Proxy>, http2: bool) -> Pool {
        let builder = HyperClient::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).clone();
        match proxy {
            None => {
                let mut direct = HttpConnector::new();
                direct.enforce_http(false);
                Pool::Direct(builder.build(https(tls.clone(), direct, http2)))
            }
            Some(proxy) => Pool::Tunnel(builder.build(https(tls.clone(), proxy.tunnel(), http2))),
        }
    }

    /// Send `out` (with `Authorization: Bearer <token>` when given) and read the whole answer
    /// before `deadline`.
    pub(crate) async fn send(&self, out: &Outgoing, bearer: Option<&str>, deadline: Instant) -> Result<Answer, Error> {
        let mut builder = Request::builder()
            .method(out.method.clone())
            .uri(self.base.url(&out.path))
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, concat!("net_backend_client/", env!("CARGO_PKG_VERSION")))
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string());
        if let Some(token) = bearer {
            builder = builder.header(AUTHORIZATION, bearer_header(token)?);
        }
        let body = match &out.body {
            Some(body) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                full(body.clone())
            }
            None => full(Bytes::new()),
        };
        let request = builder.body(body).map_err(|e| Error::invalid(format!("the request could not be built: {e}")))?;
        self.exchange(request, deadline).await
    }

    /// POST a form to an absolute URL (an identity provider's token endpoint): `https://`, or
    /// `http://` only to a loopback host. The client's TLS settings and limits; the proxy decided
    /// for THIS URL by the client's proxy setting (the environment's `HTTPS_PROXY` / `NO_PROXY`,
    /// the builder's proxy, or none; never for loopback), as for the server; no Bearer token.
    #[cfg(feature = "oauth")]
    pub(crate) async fn post_form(&self, url: &str, form: Bytes, deadline: Instant) -> Result<Answer, Error> {
        let target = BaseUrl::parse(url.split(['?', '#']).next().unwrap_or(url))?;
        if target.scheme == Scheme::Http && !target.is_loopback() {
            return Err(Error::invalid("the identity provider's token endpoint must be https:// (http:// only for a loopback address)"));
        }
        let request = Request::builder()
            .method(Method::POST)
            .uri(url)
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, concat!("net_backend_client/", env!("CARGO_PKG_VERSION")))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(full(form))
            .map_err(|e| Error::invalid(format!("the request could not be built: {e}")))?;
        let proxy = Proxy::resolve(&self.proxy_setting, &target)?;
        let same = match (&proxy, &self.proxy) {
            (None, None) => true,
            (Some(a), Some(b)) => a.uri == b.uri && a.auth == b.auth,
            _ => false,
        };
        if same {
            self.exchange(request, deadline).await
        } else {
            // Another route than the server's: a pool of its own for this one exchange.
            self.exchange_on(&Self::pool(&self.tls, proxy.as_ref(), self.http2), request, deadline).await
        }
    }

    /// The request head every call to the server carries (+ the Bearer token when given).
    fn head(&self, method: Method, path: &str, bearer: Option<&str>) -> Result<http::request::Builder, Error> {
        let mut builder = Request::builder()
            .method(method)
            .uri(self.base.url(path))
            .header(USER_AGENT, concat!("net_backend_client/", env!("CARGO_PKG_VERSION")))
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string());
        if let Some(token) = bearer {
            builder = builder.header(AUTHORIZATION, bearer_header(token)?);
        }
        Ok(builder)
    }

    /// POST a streamed body of `length` bytes (`content_type`) to `path` and read the JSON answer
    /// before `deadline`.
    pub(crate) async fn send_body(
        &self,
        path: &str,
        content_type: &str,
        length: u64,
        body: ReqBody,
        bearer: Option<&str>,
        deadline: Instant,
    ) -> Result<Answer, Error> {
        let request = self
            .head(Method::POST, path, bearer)?
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, content_type)
            .header(http::header::CONTENT_LENGTH, length)
            .body(body)
            .map_err(|e| Error::invalid(format!("the request could not be built: {e}")))?;
        self.exchange(request, deadline).await
    }

    /// GET `path` and hand the answer's body to the caller unread (headers before `deadline`).
    pub(crate) async fn open(&self, path: &str, bearer: Option<&str>, extra: Option<(http::HeaderName, String)>, deadline: Instant) -> Result<Streamed, Error> {
        let mut builder = self.head(Method::GET, path, bearer)?;
        if let Some((name, value)) = extra {
            builder = builder.header(name, value);
        }
        let request = builder.body(full(Bytes::new())).map_err(|e| Error::invalid(format!("the request could not be built: {e}")))?;
        crate::runtime::mark_handed();
        let pending = match &self.pool {
            Pool::Direct(client) => client.request(request),
            Pool::Tunnel(client) => client.request(request),
        };
        match tokio::time::timeout_at(deadline, pending).await {
            Ok(Ok(response)) => {
                let (parts, body) = response.into_parts();
                Ok(Streamed { status: parts.status.as_u16(), headers: parts.headers, body })
            }
            Ok(Err(error)) => Err(map_hyper_error(error)),
            Err(_) => Err(Error::timeout("no answer before the deadline", None)),
        }
    }

    /// Read a small error body of a streamed answer (at most 64 KiB) as the protocol's error.
    pub(crate) async fn error_of(streamed: Streamed, deadline: Instant) -> Error {
        let status = streamed.status;
        let retry_after = streamed.headers.get(RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok()).map(Duration::from_secs);
        let read = tokio::time::timeout_at(deadline, Limited::new(streamed.body, 64 * 1024).collect()).await;
        let body = match read {
            Ok(Ok(collected)) => collected.to_bytes(),
            _ => Bytes::new(),
        };
        Answer { status, retry_after, server_now: None, body }.error()
    }

    /// Send a built request and read the whole answer before `deadline`.
    async fn exchange(&self, request: Request<ReqBody>, deadline: Instant) -> Result<Answer, Error> {
        self.exchange_on(&self.pool, request, deadline).await
    }

    /// [`exchange`](Self::exchange) through `pool`.
    async fn exchange_on(&self, pool: &Pool, request: Request<ReqBody>, deadline: Instant) -> Result<Answer, Error> {
        let answered = AtomicBool::new(false);
        crate::runtime::mark_handed();
        let pending = match pool {
            Pool::Direct(client) => client.request(request),
            Pool::Tunnel(client) => client.request(request),
        };
        let exchange = async {
            let response = pending.await.map_err(map_hyper_error)?;
            answered.store(true, Ordering::Relaxed);
            let status = response.status().as_u16();
            let headers = response.headers();
            let retry_after = headers.get(RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok()).map(Duration::from_secs);
            let server_now = headers.get(DATE).and_then(|v| v.to_str().ok()).and_then(parse_http_date);
            let body = Limited::new(response.into_body(), self.max_body).collect().await.map_err(|e| {
                if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                    Error::BodyTooLarge { limit: u64::try_from(self.max_body).unwrap_or(u64::MAX) }
                } else {
                    Error::network(format!("reading the answer failed: {}", chain(e.as_ref())), Some(true))
                }
            })?;
            Ok(Answer { status, retry_after, server_now, body: body.to_bytes() })
        };
        match tokio::time::timeout_at(deadline, exchange).await {
            Ok(result) => result,
            Err(_) if answered.load(Ordering::Relaxed) => Err(Error::timeout("the answer did not arrive completely before the deadline", Some(true))),
            Err(_) => Err(Error::timeout("no answer before the deadline", None)),
        }
    }
}

/// A source chain as one text (hyper's top-level errors say little on their own).
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    let mut depth = 0;
    while let Some(inner) = source {
        let part = inner.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = inner.source();
        depth += 1;
        if depth > 8 {
            break;
        }
    }
    text
}

/// Whether a rustls error is somewhere in the chain. hyper-rustls hands a failed handshake on as
/// an `io::Error` that wraps tokio-rustls's `io::Error` that wraps the `rustls::Error`; `source()`
/// skips the middle one, so wrapped I/O errors are unwrapped here too.
fn is_tls(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(e) = current {
        if e.is::<rustls::Error>() {
            return true;
        }
        let mut io = e.downcast_ref::<std::io::Error>();
        for _ in 0..8 {
            let Some(inner) = io.and_then(std::io::Error::get_ref) else { break };
            if inner.is::<rustls::Error>() {
                return true;
            }
            io = inner.downcast_ref::<std::io::Error>();
        }
        current = e.source();
    }
    false
}

fn map_hyper_error(error: hyper_util::client::legacy::Error) -> Error {
    if is_tls(&error) {
        return Error::Tls(chain(&error));
    }
    if error.is_connect() {
        Error::network(format!("could not connect: {}", chain(&error)), Some(false))
    } else {
        Error::network(chain(&error), None)
    }
}

/// An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`, the only format servers may send) as Unix
/// milliseconds.
pub(crate) fn parse_http_date(text: &str) -> Option<i64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = parts.as_slice() else { return None };
    let day: i64 = day.parse().ok()?;
    let month: i64 = match *month {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = year.parse().ok()?;
    let mut hms = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (hms.next()??, hms.next()??, hms.next()??);
    if !(1..=31).contains(&day) || !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..61).contains(&s) || !(1970..=9999).contains(&year) {
        return None;
    }
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400) + h * 3600 + m * 60 + s) * 1000)
}

#[cfg(test)]
mod tests {
    use net_backend_protocol::storage::{GetObject, ObjectVersion, RemoveObject};
    use net_backend_protocol::{chat::ListRooms, PageRequest};

    use super::*;

    #[test]
    fn base_urls() {
        let base = BaseUrl::parse("https://api.example.com").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((base.port, base.prefix.as_str(), base.is_loopback()), (443, "", false));
        assert_eq!(base.url("/v1/info"), "https://api.example.com/v1/info");
        let base = BaseUrl::parse("http://127.0.0.1:8080/game/").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((base.port, base.prefix.as_str(), base.is_loopback()), (8080, "/game", true));
        assert_eq!(base.url("/v1/info"), "http://127.0.0.1:8080/game/v1/info");
        assert!(BaseUrl::parse("http://[::1]:9/").is_ok_and(|b| b.is_loopback()));
        assert!(BaseUrl::parse("http://localhost").is_ok_and(|b| b.is_loopback()));
        for bad in ["", "api.example.com", "ftp://x", "https://", "https://u:p@x.example", "https://x.example/?a=1", "https://x.example/#f"] {
            assert!(BaseUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn calls_become_paths_and_queries() {
        let out = Outgoing::for_call(&GetObject::new("saves", "slot-1")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_none()), ("GET", "/v1/storage/saves/slot-1", true));
        assert!(Outgoing::for_call(&GetObject::new("saves", "../x")).is_err(), "never sent");
        let out = Outgoing::for_call(&ListRooms::new().with_page(PageRequest::first().with_limit(20))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(out.path, "/v1/chat/rooms?limit=20");
        let out = Outgoing::for_call(&RemoveObject::new("saves", "a")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str()), ("DELETE", "/v1/storage/saves/a"));
        let out = Outgoing::for_call(&RemoveObject::new("saves", "a").if_version(ObjectVersion::new(3))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(out.path, "/v1/storage/saves/a?if_version=3");
        assert_eq!(encode("a b/c+ä~"), "a%20b%2Fc%2B%C3%A4~");
        use net_backend_protocol::leaderboards::{AroundQuery, GetAroundMe, GetLeaderboard, PostScore, SubmitScore, TopQuery};
        let top = GetLeaderboard::new("highscore").with_query(TopQuery::new().after(net_backend_protocol::Cursor::new("-5.9.1")).with_limit(10));
        let out = Outgoing::for_call(&top).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str()), ("GET", "/v1/leaderboards/highscore?cursor=-5.9.1&limit=10"));
        let out = Outgoing::for_call(&GetAroundMe::new("highscore").with_query(AroundQuery::new().with_counts(2, 0))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(out.path, "/v1/leaderboards/highscore/around?above=2&below=0");
        let out = Outgoing::for_call(&PostScore::new("highscore", SubmitScore::new(1200))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("POST", "/v1/leaderboards/highscore/scores", true));
        assert!(Outgoing::for_call(&GetLeaderboard::new("high score")).is_err(), "never sent");
        use net_backend_protocol::notifications::{CountNotifications, DeleteNotification, MarkNotifications, NotificationQuery};
        let out = Outgoing::for_call(&NotificationQuery::new().unread_only().with_limit(5)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str()), ("GET", "/v1/notifications?limit=5&unread_only=true"));
        let out = Outgoing::for_call(&CountNotifications::new()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(out.path, "/v1/notifications/count");
        let out = Outgoing::for_call(&MarkNotifications::all_read()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("POST", "/v1/notifications/mark", true));
        let out = Outgoing::for_call(&DeleteNotification::new(net_backend_protocol::NotificationId(31))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str()), ("DELETE", "/v1/notifications/31"));
        use net_backend_protocol::friends::{AcceptFriend, AddFriend, BlockUser, ListFriendRequests, RequestQuery};
        let out = Outgoing::for_call(&AddFriend::by_code("K7M2Q9XD")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("POST", "/v1/friends/requests", true));
        let out = Outgoing::for_call(&ListFriendRequests::sent().with_query(RequestQuery::sent().with_limit(5))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(out.path, "/v1/friends/requests?direction=sent&limit=5");
        let out = Outgoing::for_call(&AcceptFriend::new(net_backend_protocol::UserId(7))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_none()), ("POST", "/v1/friends/requests/7/accept", true));
        let out = Outgoing::for_call(&BlockUser::new(net_backend_protocol::UserId(7))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str()), ("PUT", "/v1/friends/blocks/7"));
        use net_backend_protocol::friends::{GetFriendSettings, SteamMatch, UpdateFriendSettings};
        let out = Outgoing::for_call(&SteamMatch::new([76_561_201_960_265_729])).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("POST", "/v1/friends/steam", true));
        let out = Outgoing::for_call(&GetFriendSettings::new()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_none()), ("GET", "/v1/friends/settings", true));
        let out = Outgoing::for_call(&UpdateFriendSettings::new().steam_findable(false)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("PUT", "/v1/friends/settings", true));
        use net_backend_protocol::groups::{EditGroup, GroupQuery, GroupRole, ListGroups, SetMemberRole, UpdateGroup};
        use net_backend_protocol::GroupId;
        let out = Outgoing::for_call(&ListGroups::new().with_query(GroupQuery::starting_with("night owls"))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(out.path, "/v1/groups?query=night%20owls");
        let out = Outgoing::for_call(&EditGroup::new(GroupId(5), UpdateGroup::new().with_open(true))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("PATCH", "/v1/groups/5", true));
        let out = Outgoing::for_call(&SetMemberRole::new(GroupId(5), net_backend_protocol::UserId(7), GroupRole::Admin)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str()), ("PUT", "/v1/groups/5/members/7/role"));
        use net_backend_protocol::lobbies::{JoinLobbyByCode, KickFromLobby, LobbySearch};
        use net_backend_protocol::matchmaking::{CancelTicket, CreateTicket};
        use net_backend_protocol::LobbyId;
        let out = Outgoing::for_call(&LobbySearch::new().with_filter("mode", "ranked")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("POST", "/v1/lobbies/search", true));
        let out = Outgoing::for_call(&JoinLobbyByCode::new("K7M2-Q9XD")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("POST", "/v1/lobbies/join", true));
        let out = Outgoing::for_call(&KickFromLobby::new(LobbyId(7), net_backend_protocol::UserId(9))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str()), ("DELETE", "/v1/lobbies/7/members/9"));
        let out = Outgoing::for_call(&CreateTicket::new("duel")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("POST", "/v1/matchmaking/ticket", true));
        let out = Outgoing::for_call(&CancelTicket::new()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_none()), ("DELETE", "/v1/matchmaking/ticket", true));
        use net_backend_protocol::chat::{CreateRoom, EditMessage, KickFromRoom, MarkRead, MyRooms, UnreadQuery};
        use net_backend_protocol::{MessageId, RoomId};
        let out = Outgoing::for_call(&EditMessage::new(RoomId(12), MessageId(981), "hello again")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("PATCH", "/v1/chat/rooms/12/messages/981", true));
        let out = Outgoing::for_call(&MarkRead::new(RoomId(12), MessageId(981))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("PUT", "/v1/chat/rooms/12/read", true));
        let out = Outgoing::for_call(&UnreadQuery::new(vec![RoomId(12)])).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str()), ("POST", "/v1/chat/unread"));
        let out = Outgoing::for_call(&CreateRoom::new("Den")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str(), out.body.is_some()), ("POST", "/v1/chat/rooms", true));
        let out = Outgoing::for_call(&MyRooms::new().with_page(PageRequest::first().with_limit(5))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(out.path, "/v1/chat/rooms/mine?limit=5");
        let out = Outgoing::for_call(&KickFromRoom::new(RoomId(12), net_backend_protocol::UserId(7))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((out.method.as_str(), out.path.as_str()), ("DELETE", "/v1/chat/rooms/12/members/7"));
    }

    #[test]
    fn proxies_follow_the_rules_and_never_loopback() {
        let https = BaseUrl::parse("https://api.example.com").unwrap_or_else(|e| panic!("{e}"));
        let plain = BaseUrl::parse("http://lan.example.com:8080").unwrap_or_else(|e| panic!("{e}"));
        let local = BaseUrl::parse("http://127.0.0.1:8080").unwrap_or_else(|e| panic!("{e}"));
        let matcher = || {
            Matcher::builder()
                .https("http://user:fake-pw@proxy.example.com:3128")
                .http("http://plain-proxy.example.com:80")
                .no("internal.example.com, 10.0.0.0/8")
        };
        let proxy = Proxy::from_matcher(&matcher().build(), &https).unwrap_or_else(|e| panic!("{e}")).unwrap_or_else(|| panic!("intercepted"));
        assert_eq!((proxy.uri.host(), proxy.uri.port_u16(), proxy.auth.is_some()), (Some("proxy.example.com"), Some(3128), true));
        assert!(!format!("{proxy:?}").contains("fake-pw"), "never the credentials");
        let proxy = Proxy::from_matcher(&matcher().build(), &plain).unwrap_or_else(|e| panic!("{e}")).unwrap_or_else(|| panic!("intercepted"));
        assert_eq!((proxy.uri.host(), proxy.auth.is_none()), (Some("plain-proxy.example.com"), true));
        for skipped in ["https://internal.example.com", "https://a.internal.example.com", "https://10.1.2.3"] {
            let base = BaseUrl::parse(skipped).unwrap_or_else(|e| panic!("{e}"));
            assert!(Proxy::from_matcher(&matcher().build(), &base).is_ok_and(|p| p.is_none()), "{skipped}: NO_PROXY");
        }
        let socks = Matcher::builder().all("socks5://127.0.0.1:1080").build();
        assert!(matches!(Proxy::from_matcher(&socks, &https), Err(Error::InvalidRequest(_))), "never bypass a proxy that was asked for");
        // Loopback never uses a proxy, whatever is set; `Off` never does.
        assert!(Proxy::resolve(&ProxySetting::Url("http://proxy.example.com:3128".into()), &local).is_ok_and(|p| p.is_none()));
        assert!(Proxy::resolve(&ProxySetting::Off, &https).is_ok_and(|p| p.is_none()));
        assert!(Proxy::resolve(&ProxySetting::Url("http://proxy.example.com:3128".into()), &https).is_ok_and(|p| p.is_some()));
        assert!(Proxy::resolve(&ProxySetting::Url("ftp://proxy.example.com".into()), &https).is_err());
    }

    #[test]
    fn secret_buffers_hold_exactly_the_text() {
        let value = serde_json::json!({"password": "fake-pw-1234", "n": [1, 2]});
        let body = wiped_json(&value).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(serde_json::to_vec(&value).ok().as_deref(), Some(&body[..]));
        let header = bearer_header("nbsa_fake").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((header.to_str().ok(), header.is_sensitive()), (Some("Bearer nbsa_fake"), true));
        assert!(bearer_header("bad\ntoken").is_err());
    }

    #[cfg(feature = "ws")]
    #[test]
    fn the_websocket_connect_request_is_written_exactly_in_one_wiped_buffer() {
        let request = connect_request("game.test:443", Some(b"Basic dXNlcjpmYWtlLXB3"));
        assert_eq!(&request[..], b"CONNECT game.test:443 HTTP/1.1\r\nHost: game.test:443\r\nProxy-Authorization: Basic dXNlcjpmYWtlLXB3\r\n\r\n");
        assert_eq!(request.len(), request.capacity(), "exactly sized: no copy left behind by a reallocation");
        let request = connect_request("[::1]:8080", None);
        assert_eq!(&request[..], b"CONNECT [::1]:8080 HTTP/1.1\r\nHost: [::1]:8080\r\n\r\n");
        assert_eq!(request.len(), request.capacity());
        assert_eq!(connect_status(b"HTTP/1.1 200 Connection established\r\n\r\n"), Some(200));
        assert_eq!(connect_status(b"HTTP/1.0 407 Proxy Authentication Required\r\n\r\n"), Some(407));
        assert_eq!(connect_status(b"SSH-2.0-OpenSSH\r\n\r\n"), None);
        assert_eq!(connect_status(b"HTTP/1.1 2000 x\r\n\r\n"), None);
    }

    #[test]
    fn http_dates() {
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777_000));
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Tue, 29 Feb 2028 23:59:59 GMT"), Some(1_835_481_599_000));
        for bad in ["", "Sunday, 06-Nov-94 08:49:37 GMT", "Sun, 06 Foo 1994 08:49:37 GMT", "Sun, 06 Nov 1994 25:00:00 GMT", "Sun, 06 Nov 1994 08:49 GMT"] {
            assert_eq!(parse_http_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn answers_decode_errors_honestly() {
        let answer = Answer {
            status: 429,
            retry_after: Some(Duration::from_secs(2)),
            server_now: None,
            body: Bytes::from_static(br#"{"error":{"code":"rate_limited","message":"x","details":{"retry_after_ms":700}}}"#),
        };
        let error = answer.decode::<serde_json::Value>().err().unwrap_or(Error::Shutdown);
        assert_eq!((error.code(), error.retry_after()), (Some("rate_limited"), Some(Duration::from_millis(700))));
        let answer = Answer { status: 502, retry_after: None, server_now: None, body: Bytes::from_static(b"<html>bad gateway</html>") };
        assert!(matches!(answer.decode::<serde_json::Value>(), Err(Error::Status { status: 502, .. })));
        let answer = Answer { status: 200, retry_after: None, server_now: None, body: Bytes::from_static(b"  ") };
        assert_eq!(answer.decode::<Option<u32>>().ok(), Some(None));
        let answer = Answer { status: 200, retry_after: None, server_now: None, body: Bytes::from_static(b"{\"x\":1}") };
        assert!(matches!(answer.decode::<u32>(), Err(Error::Decode { status: Some(200), .. })));
    }
}
