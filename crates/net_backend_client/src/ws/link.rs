//! Opening one WebSocket link: TCP (through a proxy's CONNECT tunnel when one applies), TLS (our
//! ring config), the handshake with the protocol header and the Bearer token, and (first-message
//! auth) `auth` → `auth.ok`, all under ONE deadline.
//!
//! The crate writes the upgrade request and the `auth` frame itself, each from a buffer that is
//! wiped when dropped, and reads and checks the server's answer (RFC 6455 section 4.1) before
//! tungstenite takes the stream: tungstenite logs its own handshake request (headers included)
//! and every frame it sends (payload included) at TRACE through the `log` crate, so neither the
//! `Authorization` header nor the token passes through it.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use http::header::{HeaderMap, CONNECTION, RETRY_AFTER, SEC_WEBSOCKET_ACCEPT, TRANSFER_ENCODING, UPGRADE};
use net_backend_protocol::{codes, kinds, routes, ApiError, CloseCode, ErrorBody, WsAuth, WsServerFrame, PROTOCOL_HEADER, PROTOCOL_VERSION};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::handshake::client::{generate_key, Response};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::handshake::machine::TryParse;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::WebSocketStream;
use zeroize::Zeroizing;

use super::{WsAuthMode, WsSettings};
use crate::http::{BaseUrl, Scheme};
use crate::{Client, Error};

/// The byte stream under a link: plain TCP or TLS.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(crate) type Link = WebSocketStream<Box<dyn Io>>;

/// The most of a refused handshake's body that is read (the server's JSON error is small).
const MAX_REFUSAL_BODY: usize = 64 * 1024;
/// The longest answer header to the upgrade request accepted.
const MAX_ANSWER_HEADER: usize = 16 * 1024;

/// A refused handshake as the server's API error (its JSON body), else its status.
fn refused(status: u16, headers: &HeaderMap, body: Option<&[u8]>) -> Error {
    let retry_after = headers.get(RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok()).map(std::time::Duration::from_secs);
    match body.and_then(|body| serde_json::from_slice::<ErrorBody>(body).ok()) {
        Some(body) => Error::api(Some(status), body.error, retry_after),
        None => Error::Status { status, retry_after },
    }
}

/// Map a tungstenite error.
pub(crate) fn map_ws_error(error: tungstenite::Error) -> Error {
    match error {
        tungstenite::Error::Http(response) => refused(response.status().as_u16(), response.headers(), response.body().as_deref()),
        tungstenite::Error::Io(io) => io_error(io),
        tungstenite::Error::Tls(tls) => Error::Tls(tls.to_string()),
        tungstenite::Error::Capacity(capacity) => Error::network(format!("message too large: {capacity}"), None),
        other => Error::network(other.to_string(), None),
    }
}

fn io_error(error: std::io::Error) -> Error {
    if error.get_ref().is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some()) {
        Error::Tls(error.to_string())
    } else {
        Error::network(error.to_string(), None)
    }
}

/// The HTTP/1.1 upgrade request for `base`, with `Authorization: Bearer <token>` when a token is
/// given, in one exactly sized buffer that is wiped when dropped. A token that cannot be a header
/// value is `InvalidRequest`.
fn upgrade_request(base: &BaseUrl, key: &str, token: Option<&str>) -> Result<Zeroizing<Vec<u8>>, Error> {
    const BEARER: &str = "Authorization: Bearer ";
    if token.is_some_and(|t| !t.bytes().all(|b| b == b'\t' || (0x20..0x7f).contains(&b))) {
        return Err(Error::invalid("the access token is not a valid header value"));
    }
    let version = PROTOCOL_VERSION.to_string();
    let user_agent = concat!("net_backend_client/", env!("CARGO_PKG_VERSION"));
    let headers: [(&str, &str); 7] = [
        ("Host", &base.authority),
        ("Connection", "Upgrade"),
        ("Upgrade", "websocket"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", key),
        (PROTOCOL_HEADER, &version),
        ("User-Agent", user_agent),
    ];
    let size = 4
        + base.prefix.len()
        + routes::WS.len()
        + 11
        + headers.iter().map(|(n, v)| n.len() + v.len() + 4).sum::<usize>()
        + token.map_or(0, |t| BEARER.len() + t.len() + 2)
        + 2;
    let mut request = Zeroizing::new(Vec::with_capacity(size));
    request.extend_from_slice(b"GET ");
    request.extend_from_slice(base.prefix.as_bytes());
    request.extend_from_slice(routes::WS.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\n");
    for (name, value) in headers {
        request.extend_from_slice(name.as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    if let Some(token) = token {
        request.extend_from_slice(BEARER.as_bytes());
        request.extend_from_slice(token.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    Ok(request)
}

/// The HTTP/1.1 upgrade to a WebSocket over `stream`, written and checked by the crate (see the
/// module docs), then the `auth` frame when `auth` is given, then the stream (with any bytes the
/// server sent after its `101` answer) handed to tungstenite without a handshake of its own.
async fn upgrade(mut stream: Box<dyn Io>, base: &BaseUrl, token: Option<&str>, auth: Option<Bytes>, config: WebSocketConfig) -> Result<Link, Error> {
    let key = generate_key();
    let request = upgrade_request(base, &key, token)?;
    stream.write_all(&request).await.map_err(io_error)?;
    stream.flush().await.map_err(io_error)?;
    drop(request);
    // The answer header; the bytes after it are kept (the start of the WebSocket stream, or of a
    // refusal's body).
    let mut received = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let (size, response) = loop {
        let parsed = Response::try_parse(&received).map_err(|e| Error::network(format!("the WebSocket handshake answer is not valid HTTP: {e}"), None))?;
        if let Some(parsed) = parsed {
            break parsed;
        }
        if received.len() > MAX_ANSWER_HEADER {
            return Err(Error::network(format!("the WebSocket handshake answer's header is longer than {MAX_ANSWER_HEADER} bytes"), None));
        }
        match stream.read(&mut chunk).await.map_err(io_error)? {
            0 => return Err(Error::network("the server closed the connection during the WebSocket handshake", None)),
            n => received.extend_from_slice(&chunk[..n]),
        }
    };
    let rest = received.split_off(size);
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        let body = refusal_body(&mut stream, response.headers(), rest).await;
        return Err(refused(response.status().as_u16(), response.headers(), body.as_deref()));
    }
    if !is_upgrade(response.headers(), &derive_accept_key(key.as_bytes())) {
        return Err(Error::network("the server's answer to the WebSocket handshake is not a valid upgrade", None));
    }
    if let Some(message) = auth {
        let frame = masked_text_frame(&message, mask()?);
        stream.write_all(&frame).await.map_err(io_error)?;
        stream.flush().await.map_err(io_error)?;
    }
    Ok(WebSocketStream::from_partially_read(stream, rest, Role::Client, Some(config)).await)
}

/// The body of a refused handshake (at most [`MAX_REFUSAL_BODY`]): by `Content-Length`, chunked,
/// or up to the connection's end. `None` when it cannot be read whole.
async fn refusal_body(stream: &mut Box<dyn Io>, headers: &HeaderMap, mut body: Vec<u8>) -> Option<Vec<u8>> {
    let chunked = headers.get(TRANSFER_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let length = headers.get(http::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<usize>().ok());
    if length.is_some_and(|n| n > MAX_REFUSAL_BODY) {
        return None;
    }
    let mut chunk = [0u8; 4096];
    loop {
        if chunked {
            match dechunk(&body) {
                Dechunked::Done(decoded) => return Some(decoded),
                Dechunked::Bad => return None,
                Dechunked::Partial => {}
            }
        } else if let Some(length) = length {
            if body.len() >= length {
                body.truncate(length);
                return Some(body);
            }
        }
        if body.len() > MAX_REFUSAL_BODY.saturating_add(4096) {
            return None;
        }
        match stream.read(&mut chunk).await {
            Ok(0) if !chunked && length.is_none() => return Some(body),
            Ok(0) | Err(_) => return None,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
}

/// A chunked body (RFC 9112 section 7.1), decoded as far as it has arrived.
#[derive(Debug, PartialEq)]
enum Dechunked {
    Done(Vec<u8>),
    Partial,
    Bad,
}

fn dechunk(mut raw: &[u8]) -> Dechunked {
    let mut out = Vec::new();
    loop {
        let Some(end) = raw.windows(2).position(|w| w == b"\r\n") else { return Dechunked::Partial };
        let line = std::str::from_utf8(&raw[..end]).unwrap_or("");
        let Ok(size) = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16) else { return Dechunked::Bad };
        if size > MAX_REFUSAL_BODY || out.len().saturating_add(size) > MAX_REFUSAL_BODY {
            return Dechunked::Bad;
        }
        raw = &raw[end + 2..];
        if size == 0 {
            return Dechunked::Done(out);
        }
        if raw.len() < size + 2 {
            return Dechunked::Partial;
        }
        if &raw[size..size + 2] != b"\r\n" {
            return Dechunked::Bad;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

/// Whether a 101 answer accepts this WebSocket upgrade (RFC 6455 section 4.1).
fn is_upgrade(headers: &HeaderMap, accept: &str) -> bool {
    fn has(headers: &HeaderMap, name: http::header::HeaderName, wanted: &str) -> bool {
        headers.get(name).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|token| token.trim().eq_ignore_ascii_case(wanted)))
    }
    has(headers, UPGRADE, "websocket")
        && has(headers, CONNECTION, "upgrade")
        && headers.get(SEC_WEBSOCKET_ACCEPT).is_some_and(|v| v.as_bytes() == accept.as_bytes())
}

/// A random frame mask (RFC 6455 section 5.3) from the TLS provider's random source.
fn mask() -> Result<[u8; 4], Error> {
    let mut mask = [0u8; 4];
    rustls::crypto::ring::default_provider().secure_random.fill(&mut mask).map_err(|_| Error::invalid("no random source for the WebSocket frame mask"))?;
    Ok(mask)
}

/// One final, masked text frame (a client's frame, RFC 6455 section 5.2) in a buffer that is
/// wiped when dropped.
fn masked_text_frame(payload: &[u8], mask: [u8; 4]) -> Zeroizing<Vec<u8>> {
    let mut frame = Zeroizing::new(Vec::with_capacity(payload.len().saturating_add(14)));
    frame.push(0x81); // FIN + text
    match (u8::try_from(payload.len()), u16::try_from(payload.len())) {
        (Ok(short), _) if short < 126 => frame.push(0x80 | short),
        (_, Ok(medium)) => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&medium.to_be_bytes());
        }
        _ => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().zip(mask.iter().cycle()).map(|(byte, m)| byte ^ m));
    frame
}

/// Open a link with `token` (already fresh) before `deadline`.
pub(crate) async fn open(client: &Client, settings: &WsSettings, token: &str, deadline: Instant) -> Result<Link, Error> {
    let work = async {
        let base = &client.inner.http.base;
        let tcp = match &client.inner.http.proxy {
            Some(proxy) => proxy.connect(&base.host, base.port).await?,
            None => TcpStream::connect((base.host.as_str(), base.port))
                .await
                .map_err(|e| Error::network(format!("could not connect to `{}`: {e}", base.authority), Some(false)))?,
        };
        let _ = tcp.set_nodelay(true);
        let stream: Box<dyn Io> = match base.scheme {
            Scheme::Http => Box::new(tcp),
            Scheme::Https => {
                let config = Arc::clone(&client.inner.http.tls);
                let name = rustls::pki_types::ServerName::try_from(base.host.clone()).map_err(|_| Error::invalid("the host is not a valid TLS server name"))?;
                let tls = tokio_rustls::TlsConnector::from(config).connect(name, tcp).await.map_err(|e| Error::Tls(e.to_string()))?;
                Box::new(tls)
            }
        };
        let config = WebSocketConfig::default().max_message_size(Some(settings.max_message_bytes)).max_frame_size(Some(settings.max_message_bytes));
        let header = matches!(settings.auth, WsAuthMode::Header | WsAuthMode::Both);
        let first_message = matches!(settings.auth, WsAuthMode::FirstMessage | WsAuthMode::Both);
        let auth = if first_message { Some(auth_message(token)?) } else { None };
        let mut link = upgrade(stream, base, header.then_some(token), auth, config).await?;
        if first_message {
            await_auth(&mut link).await?;
        }
        Ok(link)
    };
    match tokio::time::timeout_at(deadline, work).await {
        Ok(result) => result,
        Err(_) => Err(Error::timeout("the WebSocket did not connect before the deadline (TCP, TLS, handshake and authentication together)", None)),
    }
}

/// The first-message `auth` text (`{"type":"auth","data":{"token":…}}`) in a buffer that is wiped
/// when dropped.
fn auth_message(token: &str) -> Result<Bytes, Error> {
    #[derive(serde::Serialize)]
    struct Frame<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        data: &'a WsAuth,
    }
    let auth = WsAuth::new(token);
    crate::http::wiped_json(&Frame { kind: kinds::AUTH, data: &auth }).map_err(|e| Error::invalid(format!("the auth message cannot be encoded: {e}")))
}

/// Read until `auth.ok` (or `auth.failed`, or a close).
async fn await_auth(link: &mut Link) -> Result<(), Error> {
    while let Some(message) = link.next().await {
        match message.map_err(map_ws_error)? {
            Message::Text(text) => match WsServerFrame::parse(text.as_str()) {
                Ok(WsServerFrame::AuthOk(_)) => return Ok(()),
                Ok(WsServerFrame::AuthFailed(error)) => return Err(Error::api(None, error, None)),
                _ => {}
            },
            Message::Close(frame) => {
                let (code, reason) = frame.map_or((CloseCode(1005), String::new()), |f| (CloseCode(u16::from(f.code)), f.reason.as_str().to_string()));
                return Err(Error::Closed { code, reason });
            }
            _ => {}
        }
    }
    Err(Error::network("the connection closed before `auth.ok`", None))
}

/// Whether a failed connection attempt may be retried later (a network loss, a busy or
/// overloaded server), or is final (refused credentials, a ban, an unsupported version).
pub(crate) fn is_permanent(error: &Error) -> bool {
    match error {
        Error::Api { status: Some(status), .. } => matches!(*status, 400..=499) && !matches!(*status, 408 | 429),
        Error::Api { status: None, error, .. } => !is_temporary_code(error),
        Error::Closed { code, .. } => code.is_permanent(),
        Error::SessionEnded { .. } | Error::NotLoggedIn | Error::InvalidRequest(_) => true,
        _ => false,
    }
}

fn is_temporary_code(error: &ApiError) -> bool {
    error.is(codes::UNAVAILABLE) || error.is(codes::HOOK_TIMEOUT) || error.is(codes::RATE_LIMITED) || error.is(codes::INTERNAL)
}

/// Whether a refused attempt deserves ONE refresh and one more try (a token the server did not take).
pub(crate) fn wants_refresh(error: &Error) -> bool {
    match error {
        Error::Api { status: Some(401), .. } => true,
        Error::Api { status: None, error, .. } => error.is(codes::TOKEN_EXPIRED) || error.is(codes::UNAUTHORIZED),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_answers_are_classified() {
        let api = |status, code: &str| Error::api(status, ApiError::new(code, ""), None);
        assert!(wants_refresh(&api(Some(401), codes::TOKEN_EXPIRED)) && is_permanent(&api(Some(401), codes::TOKEN_EXPIRED)));
        assert!(is_permanent(&api(Some(403), codes::BANNED)) && !wants_refresh(&api(Some(403), codes::BANNED)));
        assert!(!is_permanent(&api(Some(429), codes::RATE_LIMITED)));
        assert!(!is_permanent(&api(Some(503), codes::UNAVAILABLE)));
        assert!(!is_permanent(&Error::Status { status: 502, retry_after: None }));
        assert!(is_permanent(&api(None, codes::UNSUPPORTED_PROTOCOL)));
        assert!(wants_refresh(&api(None, codes::TOKEN_EXPIRED)));
        assert!(is_permanent(&Error::Closed { code: CloseCode::BANNED, reason: String::new() }));
        assert!(!is_permanent(&Error::Closed { code: CloseCode::TRY_AGAIN_LATER, reason: String::new() }));
        assert!(!is_permanent(&Error::network("reset", None)));
    }

    #[test]
    fn the_wiped_auth_message_is_the_protocols_frame() {
        let ours = auth_message("nbsa_fake_token").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(std::str::from_utf8(&ours).ok(), Some(WsAuth::new("nbsa_fake_token").to_message().as_str()));
    }

    #[test]
    fn our_auth_frame_is_a_masked_text_frame_tungstenite_reads() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
        use tokio_tungstenite::tungstenite::protocol::frame::FrameHeader;
        for len in [0usize, 1, 125, 126, 127, 65_535, 65_536, 70_000] {
            let payload: Vec<u8> = (0..len).map(|i| b'a' + u8::try_from(i % 26).unwrap_or(0)).collect();
            let mask = [0x12, 0x34, 0x56, 0x78];
            let frame = masked_text_frame(&payload, mask);
            let mut cursor = std::io::Cursor::new(frame.as_slice());
            let (header, length) = FrameHeader::parse(&mut cursor).unwrap_or_else(|e| panic!("{e}")).unwrap_or_else(|| panic!("{len}: incomplete"));
            assert!(header.is_final && header.opcode == OpCode::Data(Data::Text) && header.mask == Some(mask), "{len}: {header:?}");
            assert_eq!(length, len as u64);
            let start = usize::try_from(cursor.position()).unwrap_or(usize::MAX);
            let unmasked: Vec<u8> = frame[start..].iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m).collect();
            assert_eq!(unmasked, payload, "{len}");
        }
        let (a, b) = (mask().unwrap_or_else(|e| panic!("{e}")), mask().unwrap_or_else(|e| panic!("{e}")));
        assert!(a != b || mask().ok() != Some(a), "random masks");
    }

    #[test]
    fn the_upgrade_request_is_written_exactly_in_one_wiped_buffer() {
        let base = BaseUrl::parse("https://[::1]:8443/game").unwrap_or_else(|e| panic!("{e}"));
        let request = upgrade_request(&base, "dGhlIHNhbXBsZSBub25jZQ==", Some("nbsa_fake")).unwrap_or_else(|e| panic!("{e}"));
        let expected = format!(
            "GET /game/v1/ws HTTP/1.1\r\nHost: [::1]:8443\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nx-net-backend-protocol: {PROTOCOL_VERSION}\r\nUser-Agent: net_backend_client/{}\r\n\
             Authorization: Bearer nbsa_fake\r\n\r\n",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(std::str::from_utf8(&request).ok(), Some(expected.as_str()));
        // Exactly sized: never moved to a bigger allocation (which would leave a copy behind).
        assert_eq!(request.len(), request.capacity());
        let plain = upgrade_request(&BaseUrl::parse("http://127.0.0.1:8080").unwrap_or_else(|e| panic!("{e}")), "k", None).unwrap_or_else(|e| panic!("{e}"));
        assert!(plain.starts_with(b"GET /v1/ws HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n") && plain.len() == plain.capacity());
        assert!(!plain.windows(13).any(|w| w == b"Authorization"));
        // A token that would break the header (or smuggle another one) is refused, never written.
        for bad in ["nbsa\r\nX-Evil: 1", "nbsa\n", "nbsa\u{7f}", "nbsä"] {
            assert!(matches!(upgrade_request(&base, "k", Some(bad)), Err(Error::InvalidRequest(_))), "{bad:?}");
        }
    }

    #[test]
    fn chunked_refusal_bodies_are_decoded() {
        assert_eq!(dechunk(b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n"), Dechunked::Done(b"hello world".to_vec()));
        assert_eq!(dechunk(b"5\r\nhel"), Dechunked::Partial);
        assert_eq!(dechunk(b"5\r\nhello\r\n"), Dechunked::Partial);
        assert_eq!(dechunk(b"zz\r\n"), Dechunked::Bad);
        assert_eq!(dechunk(b"2\r\nhello\r\n"), Dechunked::Bad);
        assert_eq!(dechunk(b"fffffff\r\n"), Dechunked::Bad);
    }
}
