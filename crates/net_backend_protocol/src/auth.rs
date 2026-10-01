//! Accounts and sessions: register, login (email + password or a Steam ticket), refresh, logout,
//! the caller's [`Account`], email verification and password reset. Routes: [`routes::auth`](crate::routes::auth)
//! and [`routes::account`](crate::routes::account).
//!
//! A login answers an [`AuthSession`]: the [`Account`] and a [`TokenPair`]. The access token goes
//! into `Authorization: Bearer …` (HTTP and the WebSocket handshake) or into the first WebSocket
//! message; it lives [`DEFAULT_ACCESS_TOKEN_TTL_SECS`] by default. The refresh token gets a new
//! pair at [`routes::auth::REFRESH`](crate::routes::auth::REFRESH) and **rotates**: each one works
//! once, and using one again later revokes the whole session ([`codes::REFRESH_TOKEN_REUSED`](crate::codes::REFRESH_TOKEN_REUSED)).
//!
//! **Refresh concurrency.** Within [`REFRESH_REUSE_GRACE_SECS`] of its first use, presenting the
//! same refresh token again answers the SAME new pair (an HTTP retry or two systems refreshing at
//! once do not log the player out); after the grace window a reuse revokes the family. Clients
//! still refresh single-flight and keep the old pair until the new one is stored.
//!
//! **Expiry and the WebSocket.** An open socket is NOT closed when its access token expires; only
//! a revocation (logout, password change, ban, admin) closes it, with 4001 / 4003. A handshake
//! with an expired token is refused with HTTP 401 `token_expired`, and `bevy_net_backend` does not
//! retry a 401 by itself. The client recipe: refresh when less than
//! [`ACCESS_TOKEN_REFRESH_MARGIN_SECS`] are left (and before reconnecting); on a WebSocket that
//! went `Disconnected` with a handshake 401 or close 4001, refresh once and connect again (if the
//! refresh fails: log in again). A new `auth` message on an open socket re-authenticates it with a
//! fresh token (answered `auth.ok` / `auth.failed`, see [`envelope`](crate::envelope)).
//!
//! Secrets ([`Password`], [`AccessToken`], [`RefreshToken`], [`Secret`]) never show in `Debug`
//! output, and a decode error never quotes them; they serialize as plain strings because they
//! must reach the other side.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{ApiError, ValidationDetails};
use crate::ids::UserId;
use crate::text;
use crate::time::UnixMillis;

/// How long after its first use a refresh token still answers the same new pair (a retry), in
/// seconds. After that, a reuse revokes the token family.
pub const REFRESH_REUSE_GRACE_SECS: u64 = 30;
/// Clients refresh the access token when less than this many seconds of it are left.
pub const ACCESS_TOKEN_REFRESH_MARGIN_SECS: u64 = 60;

/// The default access-token lifetime (1 hour; the server may configure another).
pub const DEFAULT_ACCESS_TOKEN_TTL_SECS: u64 = 60 * 60;
/// The default refresh-token lifetime (30 days; the server may configure another).
pub const DEFAULT_REFRESH_TOKEN_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// The shortest password, in characters (Unicode scalar values).
pub const PASSWORD_MIN_CHARS: usize = 10;
/// The longest password, in bytes of UTF-8 (password hashing cost grows with length).
pub const PASSWORD_MAX_BYTES: usize = 128;
/// The longest email address, in bytes.
pub const EMAIL_MAX_BYTES: usize = 254;
/// The longest display name, in characters.
pub const DISPLAY_NAME_MAX_CHARS: usize = 32;
/// The longest Steam ticket, in hex characters: 8192, above Steam's own 2560-byte ticket buffer
/// (`GetTicketForWebApiResponse_t::m_rgubTicket`, 5120 hex characters).
pub const STEAM_TICKET_MAX_HEX: usize = 8192;
// Steam's ticket buffer is 2560 bytes = 5120 hex characters.
const _: () = assert!(STEAM_TICKET_MAX_HEX >= 2 * 2560);

/// Login providers named in [`LinkedIdentity::provider`]. New providers (OAuth, planned for a
/// later version) add constants here; the field is a string so old clients still decode them.
pub mod provider {
    /// Steam (a Web API ticket checked by the server; the subject is the SteamID64 as a string).
    pub const STEAM: &str = "steam";
}

macro_rules! secret_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        ///
        /// `Debug` prints `<redacted>`; there is no `Display`. It serializes as a plain JSON string.
        /// No `PartialEq`: compare secrets in constant time (servers compare hashes). Decoding
        /// anything but a JSON string fails with a fixed message that never quotes the value.
        #[derive(Clone, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserialize_secret(deserializer).map(Self)
            }
        }

        impl $name {
            /// Wrap a secret.
            pub fn new(secret: impl Into<String>) -> Self {
                Self(secret.into())
            }

            /// The secret itself. Never log it.
            pub fn expose(&self) -> &str {
                &self.0
            }

            /// The secret, consuming the wrapper. Never log it.
            pub fn into_inner(self) -> String {
                self.0
            }

            /// Whether it is empty.
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }

        impl From<String> for $name {
            fn from(secret: String) -> Self {
                Self(secret)
            }
        }

        impl From<&str> for $name {
            fn from(secret: &str) -> Self {
                Self(secret.to_string())
            }
        }
    };
}

secret_type!(
    /// A password.
    Password
);
secret_type!(
    /// An access token (`Authorization: Bearer <token>`).
    AccessToken
);
secret_type!(
    /// A refresh token (single use; rotates).
    RefreshToken
);
secret_type!(
    /// Any other secret: a one-time email / reset token, a Steam ticket.
    Secret
);

/// A secret is a JSON string; anything else fails WITHOUT quoting the value (serde's own message
/// would, e.g. ``invalid type: integer `98765432109` ``, and servers put such messages into logs).
fn deserialize_secret<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    struct SecretVisitor;

    impl<'de> serde::de::Visitor<'de> for SecretVisitor {
        type Value = String;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a string")
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
            Ok(value.to_string())
        }

        fn visit_string<E: serde::de::Error>(self, value: String) -> Result<String, E> {
            Ok(value)
        }

        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<String, E> {
            Err(E::custom(NOT_A_STRING))
        }

        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<String, E> {
            Err(E::custom(NOT_A_STRING))
        }

        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<String, E> {
            Err(E::custom(NOT_A_STRING))
        }

        fn visit_i128<E: serde::de::Error>(self, _: i128) -> Result<String, E> {
            Err(E::custom(NOT_A_STRING))
        }

        fn visit_u128<E: serde::de::Error>(self, _: u128) -> Result<String, E> {
            Err(E::custom(NOT_A_STRING))
        }

        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<String, E> {
            Err(E::custom(NOT_A_STRING))
        }

        fn visit_char<E: serde::de::Error>(self, value: char) -> Result<String, E> {
            Ok(value.to_string())
        }

        fn visit_bytes<E: serde::de::Error>(self, _: &[u8]) -> Result<String, E> {
            Err(E::custom(NOT_A_STRING))
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<String, E> {
            Err(E::custom(NOT_A_STRING))
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<String, E> {
            Err(E::custom(NOT_A_STRING))
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, _: A) -> Result<String, A::Error> {
            Err(serde::de::Error::custom(NOT_A_STRING))
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, _: A) -> Result<String, A::Error> {
            Err(serde::de::Error::custom(NOT_A_STRING))
        }
    }

    // `deserialize_any`, not `deserialize_string`: serde_json answers a non-string to the latter
    // itself, quoting the value, before the visitor is asked.
    deserializer.deserialize_any(SecretVisitor)
}

const NOT_A_STRING: &str = "a secret must be a JSON string";

/// The longest local part (before the `@`), in bytes (RFC 5321).
pub const EMAIL_LOCAL_MAX_BYTES: usize = 64;

/// Whether `email` (surrounding whitespace ignored) is a plain address `local@domain`: the subset of
/// RFC 5322's addr-spec that mail systems agree on.
///
/// - local part: 1–64 bytes of letters, digits (any script) and ``!#$%&'*+/=?^_`{|}~-``, in
///   dot-separated non-empty pieces (no leading, trailing or double dot);
/// - domain: dot-separated labels of 1–63 bytes of letters, digits (any script) and `-` (not at
///   a label's start or end).
///
/// Refused: display names and angle brackets (`Ada <ada@example.com>`), comments, quoted local
/// parts, address literals (`[192.0.2.1]`), more than one `@`, whitespace. A server must use the
/// address exactly as validated (never re-parse it with a lenient mail parser). Check text in
/// Unicode NFC: combining marks of a decomposed letter are not letters here, so a server
/// normalises an address to NFC before checking it.
pub fn is_valid_email(email: &str) -> bool {
    let email = email.trim();
    let Some((local, domain)) = email.split_once('@') else { return false };
    if local.is_empty() || local.len() > EMAIL_LOCAL_MAX_BYTES || domain.is_empty() || email.len() > EMAIL_MAX_BYTES {
        return false;
    }
    let atext = |c: char| c.is_alphanumeric() || (c.is_ascii() && "!#$%&'*+/=?^_`{|}~-".contains(c));
    let local_ok = local.split('.').all(|piece| !piece.is_empty() && piece.chars().all(atext));
    let label_ok = |label: &str| {
        !label.is_empty() && label.len() <= 63 && !label.starts_with('-') && !label.ends_with('-') && label.chars().all(|c| c.is_alphanumeric() || c == '-')
    };
    local_ok && domain.split('.').all(label_ok)
}

fn check_email(email: &str, details: &mut ValidationDetails) {
    if !is_valid_email(email) {
        details.add("email", "is not an email address");
    }
    if let Some(problem) = text::name_problem(email) {
        details.add("email", problem);
    }
    if email.len() > EMAIL_MAX_BYTES {
        details.add("email", format!("is longer than {EMAIL_MAX_BYTES} bytes"));
    }
}

fn check_password(field: &str, password: &Password, details: &mut ValidationDetails) {
    let password = password.expose();
    if password.chars().count() < PASSWORD_MIN_CHARS {
        details.add(field, format!("is shorter than {PASSWORD_MIN_CHARS} characters"));
    }
    if password.len() > PASSWORD_MAX_BYTES {
        details.add(field, format!("is longer than {PASSWORD_MAX_BYTES} bytes"));
    }
    if password.chars().any(char::is_control) {
        details.add(field, "contains control characters");
    }
    if !password.is_empty() && password.chars().all(char::is_whitespace) {
        details.add(field, "is only whitespace");
    }
}

fn check_display_name(name: &str, details: &mut ValidationDetails) {
    if name.trim().is_empty() {
        details.add("display_name", "is empty");
    } else if name.trim() != name {
        details.add("display_name", "starts or ends with whitespace");
    }
    if name.chars().count() > DISPLAY_NAME_MAX_CHARS {
        details.add("display_name", format!("is longer than {DISPLAY_NAME_MAX_CHARS} characters"));
    }
    if let Some(problem) = text::name_problem(name) {
        details.add("display_name", problem);
    }
}

/// Create an account: `POST /v1/auth/register` → [`AuthSession`].
///
/// JSON: `{"email":"a@example.com","password":"…","display_name":"Ada"}` (`display_name` optional).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegisterRequest {
    /// The email address: a plain `local@domain` ([`is_valid_email`]; no display name, no angle
    /// brackets). The server normalises it (trim, Unicode NFC, lower-case) and proves it with a
    /// verification mail.
    pub email: String,
    /// The password ([`PASSWORD_MIN_CHARS`] characters to [`PASSWORD_MAX_BYTES`] bytes).
    pub password: Password,
    /// The public name (at most [`DISPLAY_NAME_MAX_CHARS`] characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl RegisterRequest {
    /// A registration with email and password.
    pub fn new(email: impl Into<String>, password: impl Into<Password>) -> Self {
        Self { email: email.into(), password: password.into(), display_name: None }
    }

    /// The same request with a display name.
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// The shape rules a server enforces (email shape, password length, display name); the
    /// server may add more (e.g. a breached-password check). Messages never contain the password.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        check_email(&self.email, &mut details);
        check_password("password", &self.password, &mut details);
        if let Some(name) = &self.display_name {
            check_display_name(name, &mut details);
        }
        details.into_result()
    }
}

/// Log in with email + password: `POST /v1/auth/login` → [`AuthSession`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LoginRequest {
    /// The email address.
    pub email: String,
    /// The password.
    pub password: Password,
}

impl LoginRequest {
    /// A login.
    pub fn new(email: impl Into<String>, password: impl Into<Password>) -> Self {
        Self { email: email.into(), password: password.into() }
    }
}

/// Log in with Steam: `POST /v1/auth/steam` → [`AuthSession`]. The game gets the ticket from
/// Steam's `GetAuthTicketForWebApi(identity)` and sends it hex-encoded with the same identity
/// string; the server checks it with Steam (`ISteamUserAuth/AuthenticateUserTicket`). The first
/// login creates the account.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SteamLoginRequest {
    /// The ticket bytes as hex (at most [`STEAM_TICKET_MAX_HEX`] characters).
    pub ticket_hex: Secret,
    /// The identity string the ticket was requested for (must match the server's configuration).
    pub identity: String,
}

impl SteamLoginRequest {
    /// A Steam login.
    pub fn new(ticket_hex: impl Into<Secret>, identity: impl Into<String>) -> Self {
        Self { ticket_hex: ticket_hex.into(), identity: identity.into() }
    }

    /// The shape rules: a non-empty, even-length hex ticket within the size limit, and an identity.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        let ticket = self.ticket_hex.expose();
        if ticket.is_empty() || !ticket.len().is_multiple_of(2) || !ticket.bytes().all(|b| b.is_ascii_hexdigit()) {
            details.add("ticket_hex", "is not hex-encoded bytes");
        }
        if ticket.len() > STEAM_TICKET_MAX_HEX {
            details.add("ticket_hex", format!("is longer than {STEAM_TICKET_MAX_HEX} characters"));
        }
        if self.identity.trim().is_empty() {
            details.add("identity", "is empty");
        }
        details.into_result()
    }
}

/// Get a new token pair: `POST /v1/auth/refresh` → [`TokenPair`]. The refresh token is used up.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RefreshRequest {
    /// The current refresh token.
    pub refresh_token: RefreshToken,
}

impl RefreshRequest {
    /// A refresh.
    pub fn new(refresh_token: impl Into<RefreshToken>) -> Self {
        Self { refresh_token: refresh_token.into() }
    }
}

/// Log out: `POST /v1/auth/logout` (with the access token, or with `refresh_token` in the body)
/// → [`Ack`](crate::Ack). Revokes this session's access and refresh tokens, or every session of
/// the account with `everywhere`; open WebSockets of revoked sessions are closed with 4001.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LogoutRequest {
    /// Revoke every session of the account, not just this one.
    #[serde(default)]
    pub everywhere: bool,
    /// The session's refresh token: lets a client whose access token already expired log out
    /// without refreshing first (the route accepts either a valid Bearer token or this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<RefreshToken>,
}

impl LogoutRequest {
    /// Log out this session.
    pub fn this_session() -> Self {
        Self { everywhere: false, refresh_token: None }
    }

    /// Log out every session.
    pub fn everywhere() -> Self {
        Self { everywhere: true, refresh_token: None }
    }

    /// The same request carrying the session's refresh token.
    pub fn with_refresh_token(mut self, refresh_token: impl Into<RefreshToken>) -> Self {
        self.refresh_token = Some(refresh_token.into());
        self
    }
}

/// An access token and a refresh token with their expiry times.
///
/// JSON: `{"token_type":"Bearer","access_token":"…","access_expires_at":…,"refresh_token":"…","refresh_expires_at":…}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TokenPair {
    /// Always `"Bearer"`.
    pub token_type: String,
    /// The access token.
    pub access_token: AccessToken,
    /// When the access token expires.
    pub access_expires_at: UnixMillis,
    /// The refresh token (single use).
    pub refresh_token: RefreshToken,
    /// When the refresh token expires.
    pub refresh_expires_at: UnixMillis,
}

impl TokenPair {
    /// A Bearer token pair.
    pub fn new(access_token: AccessToken, access_expires_at: UnixMillis, refresh_token: RefreshToken, refresh_expires_at: UnixMillis) -> Self {
        Self { token_type: "Bearer".into(), access_token, access_expires_at, refresh_token, refresh_expires_at }
    }

    /// The `Authorization` header value: `Bearer <access token>`. Never log it.
    pub fn authorization_header(&self) -> String {
        format!("Bearer {}", self.access_token.expose())
    }
}

/// A login provider linked to an account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LinkedIdentity {
    /// The provider ([`provider`] constants; unknown names are kept as they are).
    pub provider: String,
    /// The account's id at the provider (Steam: the SteamID64 as a string).
    pub subject: String,
}

impl LinkedIdentity {
    /// An identity.
    pub fn new(provider: impl Into<String>, subject: impl Into<String>) -> Self {
        Self { provider: provider.into(), subject: subject.into() }
    }
}

/// The caller's account: `GET /v1/account`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Account {
    /// The user id.
    pub id: UserId,
    /// The email address (`None` for an account created through a provider without one).
    #[serde(default)]
    pub email: Option<String>,
    /// Whether the email address is verified.
    #[serde(default)]
    pub email_verified: bool,
    /// The public name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The roles (e.g. `"admin"`); empty for a normal player.
    #[serde(default)]
    pub roles: Vec<String>,
    /// The linked login providers.
    #[serde(default)]
    pub identities: Vec<LinkedIdentity>,
    /// When the account was created.
    pub created_at: UnixMillis,
}

impl Account {
    /// An account with no email, roles or identities yet.
    pub fn new(id: UserId, created_at: UnixMillis) -> Self {
        Self { id, email: None, email_verified: false, display_name: None, roles: Vec::new(), identities: Vec::new(), created_at }
    }

    /// The same account with this email address.
    pub fn with_email(mut self, email: impl Into<String>, verified: bool) -> Self {
        self.email = Some(email.into());
        self.email_verified = verified;
        self
    }

    /// The same account with this display name.
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// The same account with these roles.
    pub fn with_roles(mut self, roles: Vec<String>) -> Self {
        self.roles = roles;
        self
    }

    /// The same account with these linked identities.
    pub fn with_identities(mut self, identities: Vec<LinkedIdentity>) -> Self {
        self.identities = identities;
        self
    }
}

/// The answer to a login, registration or Steam login: the account and its tokens.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AuthSession {
    /// The account.
    pub account: Account,
    /// The tokens.
    pub tokens: TokenPair,
}

impl AuthSession {
    /// A session.
    pub fn new(account: Account, tokens: TokenPair) -> Self {
        Self { account, tokens }
    }
}

/// Change the caller's account: `PATCH /v1/account` → [`Account`]. Absent fields stay unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UpdateAccountRequest {
    /// A new display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl UpdateAccountRequest {
    /// Change nothing (yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Change the display name.
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// The shape rules for the fields that are set.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if let Some(name) = &self.display_name {
            check_display_name(name, &mut details);
        }
        details.into_result()
    }
}

/// Change the password: `POST /v1/account/password` → [`Ack`](crate::Ack). Other sessions are
/// revoked.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChangePasswordRequest {
    /// The current password.
    pub current_password: Password,
    /// The new password.
    pub new_password: Password,
}

impl ChangePasswordRequest {
    /// A password change.
    pub fn new(current_password: impl Into<Password>, new_password: impl Into<Password>) -> Self {
        Self { current_password: current_password.into(), new_password: new_password.into() }
    }

    /// The length rules for the new password.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        check_password("new_password", &self.new_password, &mut details);
        details.into_result()
    }
}

/// Confirm an email address: `POST /v1/auth/email/verify` → [`Ack`](crate::Ack).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VerifyEmailRequest {
    /// The one-time token from the mail.
    pub token: Secret,
}

impl VerifyEmailRequest {
    /// A verification.
    pub fn new(token: impl Into<Secret>) -> Self {
        Self { token: token.into() }
    }
}

/// Ask for a password-reset mail: `POST /v1/auth/password/forgot` → [`Ack`](crate::Ack). The
/// answer is the same whether or not the address has an account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ForgotPasswordRequest {
    /// The email address.
    pub email: String,
}

impl ForgotPasswordRequest {
    /// A reset request.
    pub fn new(email: impl Into<String>) -> Self {
        Self { email: email.into() }
    }
}

/// Set a new password with a reset token: `POST /v1/auth/password/reset` → [`Ack`](crate::Ack).
/// Every session of the account is revoked.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ResetPasswordRequest {
    /// The one-time token from the mail.
    pub token: Secret,
    /// The new password.
    pub new_password: Password,
}

impl ResetPasswordRequest {
    /// A reset.
    pub fn new(token: impl Into<Secret>, new_password: impl Into<Password>) -> Self {
        Self { token: token.into(), new_password: new_password.into() }
    }

    /// The length rules for the new password.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        check_password("new_password", &self.new_password, &mut details);
        details.into_result()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::codes;

    #[test]
    fn register_validation() {
        assert!(RegisterRequest::new("ada@example.com", "correct horse battery").validate().is_ok());
        let error = RegisterRequest::new("not-an-email", "short").with_display_name(" ").validate().err();
        let details = error.as_ref().and_then(|e| e.details_as::<ValidationDetails>()).unwrap_or_default();
        assert!(error.is_some_and(|e| e.is(codes::VALIDATION_FAILED)));
        assert_eq!(details.fields.keys().map(String::as_str).collect::<Vec<_>>(), ["display_name", "email", "password"]);
        assert!(RegisterRequest::new("a b@example.com", "correct horse battery").validate().is_err());
        assert!(RegisterRequest::new("a@b@example.com", "correct horse battery").validate().is_err());
        assert!(RegisterRequest::new("x<victim@example.com>", "correct horse battery").validate().is_err());
        let long = "x".repeat(PASSWORD_MAX_BYTES + 1);
        assert!(RegisterRequest::new("ada@example.com", long).validate().is_err());
        assert!(RegisterRequest::new("ada@example.com", "correct horse battery").with_display_name("x".repeat(33)).validate().is_err());
    }

    #[test]
    fn plain_addresses_only() {
        for ok in ["ada@example.com", " ada@example.com ", "a.b+tag@sub.example.co", "o'neil@example.com", "zoë@exämple.de", "a@b", "x_y-z@1.example"] {
            assert!(is_valid_email(ok), "{ok}");
        }
        for bad in [
            "x<victim@example.com>",
            "<victim@example.com>",
            "Ada <ada@example.com>",
            "\"a\"@example.com",
            "ada(comment)@example.com",
            "ada@[192.0.2.1]",
            "a,b@example.com",
            "a;b@example.com",
            "a:b@example.com",
            "a\\b@example.com",
            ".ada@example.com",
            "ada.@example.com",
            "a..b@example.com",
            "ada@example..com",
            "ada@-example.com",
            "ada@example-.com",
            "ada@",
            "@example.com",
            "ada example@example.com",
            "ada@@example.com",
        ] {
            assert!(!is_valid_email(bad), "{bad}");
        }
        assert!(!is_valid_email(&format!("{}@example.com", "a".repeat(65))));
        assert!(!is_valid_email(&format!("a@{}.com", "b".repeat(64))));
    }

    #[test]
    fn steam_validation() {
        assert!(SteamLoginRequest::new("0a1B", "my-game").validate().is_ok());
        assert!(SteamLoginRequest::new("0a1", "my-game").validate().is_err());
        assert!(SteamLoginRequest::new("zz", "my-game").validate().is_err());
        assert!(SteamLoginRequest::new("", "my-game").validate().is_err());
        assert!(SteamLoginRequest::new("00", " ").validate().is_err());
        assert!(SteamLoginRequest::new("0".repeat(STEAM_TICKET_MAX_HEX + 2), "g").validate().is_err());
    }

    #[test]
    fn other_validation() {
        assert!(ChangePasswordRequest::new("old", "a new long password").validate().is_ok());
        assert!(ChangePasswordRequest::new("old", "short").validate().is_err());
        assert!(ResetPasswordRequest::new("tok", "short").validate().is_err());
        assert!(UpdateAccountRequest::new().validate().is_ok());
        assert!(UpdateAccountRequest::new().with_display_name("bad\nname").validate().is_err());
    }

    #[test]
    fn secrets_are_redacted() {
        let pair = TokenPair::new(AccessToken::new("acc-SECRET"), UnixMillis(1), RefreshToken::new("ref-SECRET"), UnixMillis(2));
        let debug = format!("{pair:?} {:?}", LoginRequest::new("a@example.com", "pw-SECRET"));
        assert!(!debug.contains("SECRET"), "{debug}");
        assert!(debug.contains("AccessToken(<redacted>)") && debug.contains("Password(<redacted>)"));
        assert_eq!(pair.authorization_header(), "Bearer acc-SECRET");
        assert_eq!(pair.access_token.clone().into_inner(), "acc-SECRET");
        assert!(!Secret::from(String::from("x")).is_empty());
    }
}
