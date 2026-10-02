//! The HTTP transport: hyper 1 (HTTP/1.1) through hyper-util's pooled client and hyper-rustls with
//! the crate's ring config. One deadline covers connecting, sending and reading the whole answer;
//! the answer body is capped; redirects are never followed; nothing is decompressed (the client
//! never asks for compression, so there is nothing to inflate and no compression bomb).

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, DATE, RETRY_AFTER, USER_AGENT};
use http::{Method, Request};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use net_backend_protocol::{ErrorBody, HttpCall, PayloadKind, PROTOCOL_HEADER, PROTOCOL_VERSION};
use serde::de::DeserializeOwned;
use tokio::time::Instant;

use crate::Error;

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

    /// The WebSocket URL of `path` (`wss://` for `https://`).
    #[cfg(feature = "ws")]
    pub(crate) fn ws_url(&self, path: &str) -> String {
        let scheme = match self.scheme {
            Scheme::Https => "wss",
            Scheme::Http => "ws",
        };
        format!("{scheme}://{}{}{}", self.authority, self.prefix, path)
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
            PayloadKind::Json => {
                Some(Bytes::from(serde_json::to_vec(call.payload()).map_err(|e| Error::invalid(format!("the JSON body cannot be encoded: {e}")))?))
            }
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

/// The pooled HTTP(S) client.
#[derive(Clone)]
pub(crate) struct Http {
    client: HyperClient<HttpsConnector<HttpConnector>, Full<Bytes>>,
    pub(crate) base: BaseUrl,
    max_body: usize,
}

impl Http {
    pub(crate) fn new(base: BaseUrl, max_body: usize) -> Result<Self, Error> {
        let tls = crate::tls::client_config().map_err(|e| Error::Tls(format!("the TLS configuration could not be built: {e}")))?;
        let connector = hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls).https_or_http().enable_http1().build();
        let client = HyperClient::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(connector);
        Ok(Self { client, base, max_body })
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
            let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::invalid("the access token is not a valid header value"))?;
            value.set_sensitive(true);
            builder = builder.header(AUTHORIZATION, value);
        }
        let body = match &out.body {
            Some(body) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                Full::new(body.clone())
            }
            None => Full::new(Bytes::new()),
        };
        let request = builder.body(body).map_err(|e| Error::invalid(format!("the request could not be built: {e}")))?;
        let answered = AtomicBool::new(false);
        let exchange = async {
            let response = self.client.request(request).await.map_err(map_hyper_error)?;
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

/// Whether a rustls error is somewhere in the chain.
fn is_tls(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(e) = current {
        if e.downcast_ref::<rustls::Error>().is_some() {
            return true;
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.get_ref().is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some()) {
                return true;
            }
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
