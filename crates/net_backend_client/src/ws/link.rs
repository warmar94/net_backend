//! Opening one WebSocket link: TCP, TLS (our ring config), the handshake with the protocol header
//! and the Bearer token, and (first-message auth) `auth` → `auth.ok`, all under ONE deadline.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use http::header::{HeaderValue, AUTHORIZATION, RETRY_AFTER, USER_AGENT};
use net_backend_protocol::{codes, routes, ApiError, CloseCode, ErrorBody, WsAuth, WsServerFrame, PROTOCOL_HEADER, PROTOCOL_VERSION};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::WebSocketStream;

use super::{WsAuthMode, WsSettings};
use crate::http::Scheme;
use crate::{Client, Error};

/// The byte stream under a link: plain TCP or TLS.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(crate) type Link = WebSocketStream<Box<dyn Io>>;

/// Map a tungstenite error (a refused handshake becomes the server's API error).
pub(crate) fn map_ws_error(error: tungstenite::Error) -> Error {
    match error {
        tungstenite::Error::Http(response) => {
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(std::time::Duration::from_secs);
            match response.body().as_deref().and_then(|body| serde_json::from_slice::<ErrorBody>(body).ok()) {
                Some(body) => Error::api(Some(status), body.error, retry_after),
                None => Error::Status { status, retry_after },
            }
        }
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

/// Open a link with `token` (already fresh) before `deadline`.
pub(crate) async fn open(client: &Client, settings: &WsSettings, token: &str, deadline: Instant) -> Result<Link, Error> {
    let work = async {
        let base = &client.inner.http.base;
        let mut request = base.ws_url(routes::WS).into_client_request().map_err(|e| Error::invalid(format!("the WebSocket URL is not valid: {e}")))?;
        let headers = request.headers_mut();
        headers.insert(PROTOCOL_HEADER, HeaderValue::from(PROTOCOL_VERSION));
        headers.insert(USER_AGENT, HeaderValue::from_static(concat!("net_backend_client/", env!("CARGO_PKG_VERSION"))));
        if matches!(settings.auth, WsAuthMode::Header | WsAuthMode::Both) {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::invalid("the access token is not a valid header value"))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let tcp = TcpStream::connect((base.host.as_str(), base.port))
            .await
            .map_err(|e| Error::network(format!("could not connect to `{}`: {e}", base.authority), Some(false)))?;
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
        let (mut link, _) = tokio_tungstenite::client_async_with_config(request, stream, Some(config)).await.map_err(map_ws_error)?;
        if matches!(settings.auth, WsAuthMode::FirstMessage | WsAuthMode::Both) {
            link.send(Message::text(WsAuth::new(token).to_message())).await.map_err(map_ws_error)?;
            await_auth(&mut link).await?;
        }
        Ok(link)
    };
    match tokio::time::timeout_at(deadline, work).await {
        Ok(result) => result,
        Err(_) => Err(Error::timeout("the WebSocket did not connect before the deadline (TCP, TLS, handshake and authentication together)", None)),
    }
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
}
