//! [`Error`]: why a call did not succeed. Every call gets exactly one answer; this is the error half.

use std::fmt;
use std::time::Duration;

use net_backend_protocol::{codes, ApiError, CloseCode};

/// Why a call, a connection or a command did not succeed.
///
/// The kinds are `#[non_exhaustive]`: match them with a `_` arm. Texts are what the dependency
/// (hyper, rustls, tungstenite, russh, serde_json) or the server reported, never a guess.
/// `Display` and `Debug` never show a body, a token or a header value; `Decode`'s message (which
/// can quote the body) is only in its field.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// The request could not be built (bad URL, a path parameter that would need escaping, a body
    /// that cannot be encoded, plain `http://` to a host that is not loopback, a blocking call made
    /// on the client's own runtime thread, an async call made outside a tokio runtime, …). Never sent.
    InvalidRequest(String),
    /// A network failure: DNS, connect, reset, protocol (the dependency's words). `sent` says
    /// whether the request may have reached the server (see [`Error::was_sent`]).
    #[non_exhaustive]
    Network {
        /// What went wrong.
        message: String,
        /// `Some(false)`: never sent (DNS / connect failed); `None`: unknown.
        sent: Option<bool>,
    },
    /// A TLS failure (handshake, certificate). Happens before any request byte is written.
    Tls(String),
    /// The call took longer than its deadline (one deadline per call: waiting for a token refresh,
    /// connecting, sending and reading the answer together).
    #[non_exhaustive]
    Timeout {
        /// What was still running.
        message: String,
        /// `Some(false)`: it never went out; `None`: it may or may not have reached the server.
        sent: Option<bool>,
    },
    /// The server refused the request with the protocol's error body: an HTTP 4xx / 5xx with
    /// `{"error":{…}}`, a WebSocket answer `{"ok":false,"error":{…}}`, or a refused WebSocket
    /// handshake. Branch on [`code`](Error::code).
    #[non_exhaustive]
    Api {
        /// The HTTP status (`None` for a WebSocket answer).
        status: Option<u16>,
        /// The server's error (`code`, `message`, `details`).
        error: ApiError,
        /// How long to wait before trying again, when the server said (`details.retry_after_ms`, or
        /// the `Retry-After` header).
        retry_after: Option<Duration>,
    },
    /// The server (or a proxy in front of it) answered a status outside 200–299 without the
    /// protocol's error body (a proxy's 502 page, a redirect: redirects are never followed).
    #[non_exhaustive]
    Status {
        /// The HTTP status.
        status: u16,
        /// The `Retry-After` header, if any.
        retry_after: Option<Duration>,
    },
    /// A success answer whose body is not the expected JSON. `Display` and `Debug` do not show the
    /// message (it can quote the body).
    #[non_exhaustive]
    Decode {
        /// The HTTP status (`None` for a WebSocket answer or push).
        status: Option<u16>,
        /// What serde_json reported. It can quote part of the body: do not log it in release builds.
        message: String,
    },
    /// The answer was bigger than its limit (an HTTP body, a WebSocket message, an SSH command's
    /// output, an SFTP download). The request went out.
    #[non_exhaustive]
    BodyTooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// The request was bigger than its limit and was refused before anything was sent (a
    /// WebSocket message over 1 MiB, an SSH command line over 64 KiB, an SFTP upload over the
    /// transfer limit).
    #[non_exhaustive]
    RequestTooLarge {
        /// The limit in bytes.
        limit: u64,
        /// The request's size in bytes.
        size: u64,
    },
    /// The call needs a session and there is none (log in, register or `resume` first). Never sent.
    NotLoggedIn,
    /// The session ended: the server refused the refresh token (`refresh_token_reused`,
    /// `unauthorized`, `banned`, …) or closed the WebSocket for good after a failed refresh. The
    /// tokens were dropped and [`TokenUpdates`](crate::TokenUpdates) reported `None`: log in again.
    #[non_exhaustive]
    SessionEnded {
        /// The server's error code (e.g. `refresh_token_reused`, `banned`).
        code: String,
    },
    /// The server closed the WebSocket with a close frame (4001 revoked, 4003 banned, 4009
    /// replaced, 4010 unsupported protocol, …): its code and reason.
    #[non_exhaustive]
    Closed {
        /// The close code.
        code: CloseCode,
        /// The close reason (may be empty).
        reason: String,
    },
    /// The WebSocket went away (or never came up, or was closed by the app) before the request was
    /// answered.
    #[non_exhaustive]
    Disconnected {
        /// Why.
        reason: String,
        /// `Some(true)`: the request had gone out (it may have reached the server);
        /// `Some(false)`: it never went out; `None`: about the connection itself.
        sent: Option<bool>,
    },
    /// A push stream fell behind: this many pushes were dropped for it (the stream's buffer is
    /// bounded). Resync what you show (reload the chat history, for example).
    #[non_exhaustive]
    Lagged {
        /// How many pushes were missed.
        missed: u64,
    },
    /// The client (its runtime thread, a WebSocket connection or an SSH session) shut down before
    /// an answer arrived.
    Shutdown,
    /// The app cancelled the request ([`Reply::cancel`](crate::Reply::cancel)) before its answer
    /// arrived; a late answer is dropped.
    #[non_exhaustive]
    Cancelled {
        /// `Some(false)`: it never went out; `Some(true)`: it had gone out (a WebSocket request
        /// written to the connection; it may have run); `None`: an HTTP request that was already
        /// handed to the connection (it may have reached the server).
        sent: Option<bool>,
    },
    /// The SSH server's host key could not be verified (feature `ssh`): unknown, changed or
    /// revoked. The connection was closed before authentication: nothing was sent.
    #[non_exhaustive]
    HostKey {
        /// The host as looked up in known_hosts (`host`, or `[host]:port` for a port other than 22).
        host: String,
        /// The server key's fingerprint as OpenSSH shows it (`SHA256:…`): public, safe to show.
        fingerprint: String,
        /// What is wrong.
        problem: HostKeyProblem,
    },
    /// The SSH server accepted none of the authentication methods (feature `ssh`), or a key could
    /// not be loaded. The text names the methods tried (key file names, not paths), never a secret.
    AuthFailed(String),
    /// An SSH protocol error (feature `ssh`): no common algorithm, a refused channel or subsystem,
    /// the Terrapin refusal, an SFTP error status, … (the dependency's or the server's words).
    Ssh(String),
}

/// Why an SSH host key was refused ([`Error::HostKey`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HostKeyProblem {
    /// The host is in no known_hosts file that was read, and no pinned fingerprint matches. Add the
    /// key to known_hosts (after checking the fingerprint on the server) or pin it.
    Unknown,
    /// known_hosts lists this host with a different key of the same type: possibly a
    /// man-in-the-middle attack, or the server was reinstalled. Never accepted automatically.
    Changed,
    /// The key is marked `@revoked` in known_hosts (or a revoked line for the host could not be
    /// read, which is treated the same way).
    Revoked,
}

impl fmt::Display for HostKeyProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HostKeyProblem::Unknown => "unknown host key",
            HostKeyProblem::Changed => "the host key CHANGED",
            HostKeyProblem::Revoked => "the host key is REVOKED",
        })
    }
}

impl Error {
    /// What this error says about whether the request reached the network: `Some(false)` never
    /// sent (invalid, not logged in, too large, a refused host key or SSH login, a timeout or
    /// disconnect before it went out), `Some(true)` the server answered (`Api`, `Status`,
    /// `Decode`, `BodyTooLarge`) or it went out before a loss, `None` unknown ("maybe").
    pub fn was_sent(&self) -> Option<bool> {
        match self {
            Error::InvalidRequest(_) | Error::NotLoggedIn | Error::RequestTooLarge { .. } | Error::Tls(_) => Some(false),
            Error::HostKey { .. } | Error::AuthFailed(_) => Some(false),
            Error::Network { sent, .. } | Error::Timeout { sent, .. } | Error::Disconnected { sent, .. } | Error::Cancelled { sent } => *sent,
            Error::Api { .. } | Error::Status { .. } | Error::Decode { .. } | Error::BodyTooLarge { .. } => Some(true),
            _ => None,
        }
    }

    /// The server's error, for [`Api`](Error::Api).
    pub fn api_error(&self) -> Option<&ApiError> {
        match self {
            Error::Api { error, .. } => Some(error),
            _ => None,
        }
    }

    /// The server's error code (`Api`), or the code that ended the session (`SessionEnded`).
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Api { error, .. } => Some(error.code.as_str()),
            Error::SessionEnded { code } => Some(code.as_str()),
            _ => None,
        }
    }

    /// Whether the code is `code` (see [`codes`]).
    pub fn is(&self, code: &str) -> bool {
        self.code() == Some(code)
    }

    /// The HTTP status, for `Api` (over HTTP), `Status` and `Decode` (over HTTP).
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Api { status, .. } | Error::Decode { status, .. } => *status,
            Error::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// How long the server asked to wait before trying again (a 429 `rate_limited`, a 503).
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Api { retry_after, .. } | Error::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The close code, for [`Closed`](Error::Closed).
    pub fn close_code(&self) -> Option<CloseCode> {
        match self {
            Error::Closed { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Whether the player must log in again: [`SessionEnded`](Error::SessionEnded), [`NotLoggedIn`](Error::NotLoggedIn),
    /// or a WebSocket closed with 4001.
    pub fn needs_login(&self) -> bool {
        matches!(self, Error::SessionEnded { .. } | Error::NotLoggedIn) || self.close_code() == Some(CloseCode::UNAUTHORIZED)
    }

    /// An `Api` error from the server's error and status, with `retry_after` taken from
    /// `details.retry_after_ms` (or the given header value).
    pub(crate) fn api(status: Option<u16>, error: ApiError, header_retry: Option<Duration>) -> Self {
        let from_details = error.details.as_ref().and_then(|d| d.get("retry_after_ms")).and_then(serde_json::Value::as_u64).map(Duration::from_millis);
        Error::Api { status, error, retry_after: from_details.or(header_retry) }
    }

    pub(crate) fn network(message: impl Into<String>, sent: Option<bool>) -> Self {
        Error::Network { message: message.into(), sent }
    }

    pub(crate) fn timeout(message: impl Into<String>, sent: Option<bool>) -> Self {
        Error::Timeout { message: message.into(), sent }
    }

    #[cfg(any(feature = "ws", feature = "ssh"))]
    pub(crate) fn disconnected(reason: impl Into<String>, sent: Option<bool>) -> Self {
        Error::Disconnected { reason: reason.into(), sent }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Error::InvalidRequest(message.into())
    }

    /// Whether a refresh answer means the session is over (the refresh token will never work again).
    pub(crate) fn ends_session(&self) -> bool {
        match self {
            Error::Api { status: Some(status), error, .. } => {
                matches!(*status, 401 | 403) || error.is(codes::REFRESH_TOKEN_REUSED) || error.is(codes::BANNED) || error.is(codes::UNAUTHORIZED)
            }
            _ => false,
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidRequest(why) => f.debug_tuple("InvalidRequest").field(why).finish(),
            Error::Network { message, sent } => f.debug_struct("Network").field("message", message).field("sent", sent).finish(),
            Error::Tls(why) => f.debug_tuple("Tls").field(why).finish(),
            Error::Timeout { message, sent } => f.debug_struct("Timeout").field("message", message).field("sent", sent).finish(),
            Error::Api { status, error, retry_after } => f
                .debug_struct("Api")
                .field("status", status)
                .field("code", &error.code)
                .field("message", &error.message)
                .field("retry_after", retry_after)
                .finish(),
            Error::Status { status, retry_after } => f.debug_struct("Status").field("status", status).field("retry_after", retry_after).finish(),
            Error::Decode { status, message } => f.debug_struct("Decode").field("status", status).field("message_len", &message.len()).finish(),
            Error::BodyTooLarge { limit } => f.debug_struct("BodyTooLarge").field("limit", limit).finish(),
            Error::RequestTooLarge { limit, size } => f.debug_struct("RequestTooLarge").field("limit", limit).field("size", size).finish(),
            Error::NotLoggedIn => f.write_str("NotLoggedIn"),
            Error::SessionEnded { code } => f.debug_struct("SessionEnded").field("code", code).finish(),
            Error::Closed { code, reason } => f.debug_struct("Closed").field("code", &code.get()).field("reason", reason).finish(),
            Error::Disconnected { reason, sent } => f.debug_struct("Disconnected").field("reason", reason).field("sent", sent).finish(),
            Error::Lagged { missed } => f.debug_struct("Lagged").field("missed", missed).finish(),
            Error::Shutdown => f.write_str("Shutdown"),
            Error::Cancelled { sent } => f.debug_struct("Cancelled").field("sent", sent).finish(),
            Error::HostKey { host, fingerprint, problem } => {
                f.debug_struct("HostKey").field("host", host).field("fingerprint", fingerprint).field("problem", problem).finish()
            }
            Error::AuthFailed(why) => f.debug_tuple("AuthFailed").field(why).finish(),
            Error::Ssh(why) => f.debug_tuple("Ssh").field(why).finish(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidRequest(why) => write!(f, "invalid request: {why}"),
            Error::Network { message, .. } => write!(f, "network error: {message}"),
            Error::Tls(why) => write!(f, "TLS error: {why}"),
            Error::Timeout { message, .. } => write!(f, "timed out ({message})"),
            Error::Api { status: Some(status), error, .. } => write!(f, "the server refused the request (HTTP {status}, {error})"),
            Error::Api { status: None, error, .. } => write!(f, "the server refused the request ({error})"),
            Error::Status { status, .. } => write!(f, "HTTP status {status} without an API error body"),
            Error::Decode { status: Some(status), .. } => write!(f, "the answer (HTTP {status}) is not the expected JSON"),
            Error::Decode { status: None, .. } => f.write_str("the answer is not the expected JSON"),
            Error::BodyTooLarge { limit } => write!(f, "the answer is larger than the limit of {limit} bytes"),
            Error::RequestTooLarge { limit, size } => write!(f, "the request ({size} bytes) is larger than the limit of {limit} bytes; not sent"),
            Error::NotLoggedIn => f.write_str("not logged in (no session tokens)"),
            Error::SessionEnded { code } => write!(f, "the session ended ({code}); log in again"),
            Error::Closed { code, reason } if reason.is_empty() => write!(f, "closed by the server (code {code})"),
            Error::Closed { code, reason } => write!(f, "closed by the server (code {code}: {reason})"),
            Error::Disconnected { reason, sent: Some(true) } => write!(f, "disconnected after the request was sent: {reason}"),
            Error::Disconnected { reason, sent: Some(false) } => write!(f, "disconnected, the request was never sent: {reason}"),
            Error::Disconnected { reason, sent: None } => write!(f, "disconnected: {reason}"),
            Error::Lagged { missed } => write!(f, "{missed} pushes were missed (the reader fell behind)"),
            Error::Shutdown => f.write_str("the client shut down before an answer arrived"),
            Error::Cancelled { sent: Some(false) } => f.write_str("cancelled before it was sent"),
            Error::Cancelled { sent: Some(true) } => f.write_str("cancelled after it was sent"),
            Error::Cancelled { sent: None } => f.write_str("cancelled (it may have reached the server)"),
            Error::HostKey { host, fingerprint, problem } => write!(f, "SSH host key check failed for `{host}`: {problem} ({fingerprint})"),
            Error::AuthFailed(why) => write!(f, "SSH authentication failed: {why}"),
            Error::Ssh(why) => write!(f, "SSH error: {why}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_comes_from_the_details_first() {
        let error = ApiError::new(codes::RATE_LIMITED, "slow down").with_details(serde_json::json!({"retry_after_ms": 1500}));
        let e = Error::api(Some(429), error, Some(Duration::from_secs(9)));
        assert_eq!(e.retry_after(), Some(Duration::from_millis(1500)));
        assert_eq!(e.status(), Some(429));
        assert!(e.is(codes::RATE_LIMITED));
        assert_eq!(e.was_sent(), Some(true));
        let e = Error::api(Some(503), ApiError::new(codes::UNAVAILABLE, ""), Some(Duration::from_secs(5)));
        assert_eq!(e.retry_after(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn display_and_debug_hide_decode_messages() {
        let e = Error::Decode { status: Some(200), message: "expected nbsa_secret".into() };
        assert!(!format!("{e} {e:?}").contains("nbsa_secret"));
        assert_eq!(Error::timeout("not sent: waiting", Some(false)).was_sent(), Some(false));
        assert!(Error::Closed { code: CloseCode::UNAUTHORIZED, reason: String::new() }.needs_login());
        assert!(!Error::Closed { code: CloseCode::BANNED, reason: String::new() }.needs_login());
    }
}
