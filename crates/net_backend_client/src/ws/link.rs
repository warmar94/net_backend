//! Opening one WebSocket link: TCP, TLS (our ring config), the handshake with the protocol header
//! and the Bearer token, and (first-message auth) `auth` → `auth.ok`, all under ONE deadline.
//!
//! The upgrade request goes out through hyper and the `auth` frame is written by this module
//! before tungstenite takes the stream: tungstenite logs its own handshake request (headers
//! included) and every frame it sends (payload included) at TRACE through the `log` crate, so
//! neither the `Authorization` header nor the token passes through it.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use http::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONNECTION, RETRY_AFTER, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE, USER_AGENT};
use http::StatusCode;
use http_body_util::{BodyExt, Empty, Limited};
use hyper_util::rt::TokioIo;
use net_backend_protocol::{codes, kinds, routes, ApiError, CloseCode, ErrorBody, WsAuth, WsServerFrame, PROTOCOL_HEADER, PROTOCOL_VERSION};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::WebSocketStream;
use zeroize::Zeroizing;

use super::{WsAuthMode, WsSettings};
use crate::http::Scheme;
use crate::{Client, Error};

/// The byte stream under a link: plain TCP or TLS.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(crate) type Link = WebSocketStream<Box<dyn Io>>;

/// The most of a refused handshake's body that is read (the server's JSON error is small).
const MAX_REFUSAL_BODY: usize = 64 * 1024;

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

/// A hyper error of the handshake (an I/O error inside it keeps its TLS classification).
fn hyper_error(error: &hyper::Error) -> Error {
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        if let Some(io) = inner.downcast_ref::<std::io::Error>() {
            if io.get_ref().is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some()) {
                return Error::Tls(format!("the WebSocket handshake failed: {io}"));
            }
        }
        source = inner.source();
    }
    Error::network(format!("the WebSocket handshake failed: {error}"), None)
}

/// The HTTP/1.1 upgrade to a WebSocket over `stream` through hyper (which logs no headers), then
/// the `auth` frame when `auth` is given (written here, see the module docs), then the stream
/// handed to tungstenite without a handshake of its own.
async fn upgrade(stream: Box<dyn Io>, mut request: http::Request<()>, auth: Option<Bytes>, config: WebSocketConfig) -> Result<Link, Error> {
    let accept = request.headers().get(SEC_WEBSOCKET_KEY).map(|key| derive_accept_key(key.as_bytes())).ok_or_else(|| Error::invalid("no WebSocket key"))?;
    // The origin form on the wire (`GET /v1/ws HTTP/1.1`); the `Host` header is set already.
    let path = match request.uri().path_and_query() {
        Some(path) => http::Uri::from_maybe_shared(Bytes::copy_from_slice(path.as_str().as_bytes())),
        None => Ok(http::Uri::from_static("/")),
    };
    *request.uri_mut() = path.map_err(|e| Error::invalid(format!("the WebSocket URL is not valid: {e}")))?;
    let request = request.map(|()| Empty::<Bytes>::new());
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.map_err(|e| hyper_error(&e))?;
    // The connection future ends by itself once it read a 101 (its stream then goes to the
    // upgrade, with any bytes read past the answer), or when the connection closes.
    let mut connection = std::pin::pin!(connection.with_upgrades());
    let mut ended = false;
    let response = {
        let mut send = std::pin::pin!(sender.send_request(request));
        tokio::select! {
            biased;
            response = &mut send => response,
            result = &mut connection => {
                result.map_err(|e| hyper_error(&e))?;
                ended = true;
                send.await
            }
        }
    };
    drop(sender);
    let response = response.map_err(|e| hyper_error(&e))?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let (parts, body) = response.into_parts();
        let collect = Limited::new(body, MAX_REFUSAL_BODY).collect();
        let body = if ended {
            collect.await.ok()
        } else {
            tokio::select! {
                biased;
                body = collect => body.ok(),
                _ = &mut connection => None,
            }
        };
        return Err(refused(parts.status.as_u16(), &parts.headers, body.map(|collected| collected.to_bytes()).as_deref()));
    }
    if !is_upgrade(response.headers(), &accept) {
        return Err(Error::network("the server's answer to the WebSocket handshake is not a valid upgrade", None));
    }
    let upgrade = hyper::upgrade::on(response);
    if !ended {
        connection.await.map_err(|e| hyper_error(&e))?;
    }
    let mut io = TokioIo::new(upgrade.await.map_err(|e| hyper_error(&e))?);
    if let Some(message) = auth {
        let frame = masked_text_frame(&message, mask()?);
        io.write_all(&frame).await.map_err(io_error)?;
        io.flush().await.map_err(io_error)?;
    }
    let io: Box<dyn Io> = Box::new(io);
    Ok(WebSocketStream::from_raw_socket(io, Role::Client, Some(config)).await)
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
        let mut request = base.ws_url(routes::WS).into_client_request().map_err(|e| Error::invalid(format!("the WebSocket URL is not valid: {e}")))?;
        let headers = request.headers_mut();
        headers.insert(PROTOCOL_HEADER, HeaderValue::from(PROTOCOL_VERSION));
        headers.insert(USER_AGENT, HeaderValue::from_static(concat!("net_backend_client/", env!("CARGO_PKG_VERSION"))));
        if matches!(settings.auth, WsAuthMode::Header | WsAuthMode::Both) {
            headers.insert(AUTHORIZATION, crate::http::bearer_header(token)?);
        }
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
                let config = crate::tls::client_config().map_err(|e| Error::Tls(format!("the TLS configuration could not be built: {e}")))?;
                let name = rustls::pki_types::ServerName::try_from(base.host.clone()).map_err(|_| Error::invalid("the host is not a valid TLS server name"))?;
                let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(|e| Error::Tls(e.to_string()))?;
                Box::new(tls)
            }
        };
        let config = WebSocketConfig::default().max_message_size(Some(settings.max_message_bytes)).max_frame_size(Some(settings.max_message_bytes));
        let first_message = matches!(settings.auth, WsAuthMode::FirstMessage | WsAuthMode::Both);
        let auth = if first_message { Some(auth_message(token)?) } else { None };
        let mut link = upgrade(stream, request, auth, config).await?;
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
}
