//! Connections: TCP or TLS (rustls + ring, webpki roots), HTTP/1.1 keep-alive clients and WebSockets,
//! all over the same stream type.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{header, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use net_backend_protocol::{ErrorBody, HttpCall, PayloadKind};
use rustls::pki_types::ServerName;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::WebSocketStream;

/// A byte stream: plain TCP or TLS.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A connected stream.
pub type Stream = Box<dyn Io>;

/// A WebSocket over a [`Stream`].
pub type Ws = WebSocketStream<Stream>;

/// How long a TCP connect, TLS handshake or WebSocket handshake may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long one HTTP request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The server: `http://host:port` or `https://host[:port]`.
#[derive(Clone)]
pub struct Target {
    host: String,
    port: u16,
    authority: String,
    tls: Option<(TlsConnector, ServerName<'static>)>,
    ws_url: String,
}

impl Target {
    /// Parse the base URL (no path).
    pub fn parse(base: &str) -> Result<Self, String> {
        let uri: http::Uri = base.parse().map_err(|e| format!("invalid base URL `{base}`: {e}"))?;
        let host = uri.host().ok_or_else(|| format!("no host in `{base}`"))?.trim_start_matches('[').trim_end_matches(']').to_string();
        let (secure, default_port) = match uri.scheme_str() {
            Some("https") => (true, 443),
            Some("http") => (false, 80),
            _ => return Err(format!("the base URL must start with http:// or https:// (`{base}`)")),
        };
        if uri.path() != "/" && !uri.path().is_empty() {
            return Err(format!("the base URL must not have a path (`{base}`)"));
        }
        let port = uri.port_u16().unwrap_or(default_port);
        let authority = uri.authority().map(|a| a.as_str().to_string()).unwrap_or_else(|| host.clone());
        let tls = if secure {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let mut config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("TLS setup: {e}"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
            // HTTP/1.1 only: the WebSocket upgrade and the keep-alive clients need it.
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            let name = ServerName::try_from(host.clone()).map_err(|e| format!("invalid TLS name `{host}`: {e}"))?;
            Some((TlsConnector::from(Arc::new(config)), name))
        } else {
            None
        };
        let ws_url = format!("{}://{authority}{}", if secure { "wss" } else { "ws" }, net_backend_protocol::routes::WS);
        Ok(Self { host, port, authority, tls, ws_url })
    }

    /// A new connection.
    pub async fn connect(&self) -> Result<Stream, String> {
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((self.host.as_str(), self.port)))
            .await
            .map_err(|_| "connect: timed out".to_string())?
            .map_err(|e| format!("connect: {}", e.kind()))?;
        let _ = tcp.set_nodelay(true);
        match &self.tls {
            None => Ok(Box::new(tcp)),
            Some((connector, name)) => {
                let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name.clone(), tcp))
                    .await
                    .map_err(|_| "TLS: timed out".to_string())?
                    .map_err(|e| format!("TLS: {e}"))?;
                Ok(Box::new(tls))
            }
        }
    }

    /// A WebSocket at `/v1/ws`, authenticated with the `Authorization: Bearer` header.
    pub async fn ws(&self, token: &str) -> Result<Ws, String> {
        let stream = self.connect().await?;
        let mut request = self.ws_url.as_str().into_client_request().map_err(|e| format!("WS request: {e}"))?;
        let bearer = format!("Bearer {token}").parse().map_err(|_| "token is not a valid header value".to_string())?;
        request.headers_mut().insert(header::AUTHORIZATION, bearer);
        // The protocol's 1 MiB message limit.
        let config = WebSocketConfig::default().max_message_size(Some(1 << 20)).max_frame_size(Some(1 << 20));
        match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::client_async_with_config(request, stream, Some(config))).await {
            Err(_) => Err("WS handshake: timed out".into()),
            Ok(Ok((ws, _))) => Ok(ws),
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => Err(format!("WS handshake: HTTP {}", response.status().as_u16())),
            Ok(Err(e)) => Err(format!("WS handshake: {e}")),
        }
    }
}

/// The answer to an HTTP request: the status and the body.
pub struct Answer {
    pub status: StatusCode,
    pub body: Bytes,
}

impl Answer {
    /// The protocol error code of a non-2xx answer (`http_<status>` if the body is not an error body).
    pub fn error_code(&self) -> String {
        serde_json::from_slice::<ErrorBody>(&self.body).map(|b| b.error.code).unwrap_or_else(|_| format!("http_{}", self.status.as_u16()))
    }

    /// The body decoded as `T` (a 2xx answer).
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_slice(&self.body).map_err(|e| format!("unexpected answer: {e}"))
    }
}

/// One HTTP/1.1 keep-alive connection (reopened when the server or a proxy closed it).
pub struct Http {
    target: Target,
    sender: Option<SendRequest<Full<Bytes>>>,
}

impl Http {
    /// A client; it connects on the first request.
    pub fn new(target: &Target) -> Self {
        Self { target: target.clone(), sender: None }
    }

    async fn sender(&mut self) -> Result<&mut SendRequest<Full<Bytes>>, String> {
        if self.sender.as_ref().is_none_or(|s| s.is_closed()) {
            let io = TokioIo::new(self.target.connect().await?);
            let (sender, connection) = hyper::client::conn::http1::handshake(io).await.map_err(|e| format!("HTTP handshake: {e}"))?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            self.sender = Some(sender);
        }
        let sender = self.sender.as_mut().ok_or("no connection")?;
        sender.ready().await.map_err(|e| format!("HTTP: {e}"))?;
        Ok(sender)
    }

    /// One request. A transport error drops the connection (the next request opens a new one).
    pub async fn request(&mut self, method: Method, path_and_query: &str, token: Option<&str>, body: Option<Vec<u8>>) -> Result<Answer, String> {
        let authority = self.target.authority.clone();
        let mut builder = Request::builder().method(method).uri(path_and_query).header(header::HOST, authority);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match body {
            Some(bytes) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Full::new(Bytes::from(bytes))
            }
            None => Full::new(Bytes::new()),
        };
        let request = builder.body(body).map_err(|e| format!("request: {e}"))?;
        let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
            let sender = self.sender().await?;
            let response = sender.send_request(request).await.map_err(|e| format!("HTTP: {e}"))?;
            let status = response.status();
            let body = response.into_body().collect().await.map_err(|e| format!("HTTP body: {e}"))?.to_bytes();
            Ok::<Answer, String>(Answer { status, body })
        })
        .await
        .unwrap_or_else(|_| Err("HTTP: timed out".into()));
        if result.is_err() {
            self.sender = None;
        }
        result
    }

    /// A typed call of the protocol: method, path and payload from its [`HttpCall`].
    pub async fn call<C: HttpCall>(&mut self, call: &C, token: Option<&str>) -> Result<Answer, String> {
        let path = call.path().ok_or("a path parameter cannot be sent")?;
        let method = Method::from_bytes(C::ROUTE.method.as_str().as_bytes()).map_err(|e| e.to_string())?;
        let body = match C::PAYLOAD {
            PayloadKind::Json => Some(serde_json::to_vec(call.payload()).map_err(|e| e.to_string())?),
            PayloadKind::Empty => None,
            // Not used by the scenarios (they send no query calls).
            _ => return Err("query-string calls are not supported by this tool".into()),
        };
        self.request(method, &path, token, body).await
    }
}
