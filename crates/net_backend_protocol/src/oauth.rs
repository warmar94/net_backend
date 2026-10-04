//! OpenID Connect logins: a player signs in at an identity provider (Google, or any OpenID Connect
//! provider the server is configured for), the game gets the provider's **ID token** and sends it
//! to the server, which checks it and answers a normal session.
//!
//! | Route | Request → answer |
//! |---|---|
//! | `POST /v1/auth/oauth/{provider}` | [`OAuthLogin`] ([`OAuthToken`]) → [`AuthSession`] |
//!
//! - **Login or account creation:** the provider's account (`iss` + `sub` of the token) is a
//!   linked identity of an account ([`LinkedIdentity`](crate::auth::LinkedIdentity) with
//!   `provider` = the server's provider name); the first login creates the account.
//! - **Linking:** the same route with the Bearer token of a recent login links the provider's
//!   account to the caller's account instead (403 `reauthentication_required` for an older
//!   login). A provider account linked to another account answers 409 `conflict`: two existing
//!   accounts are never merged. Unlinking is `DELETE /v1/account/identities/{provider}`
//!   ([`UnlinkIdentity`](crate::auth::UnlinkIdentity)).
//! - **The nonce:** the game makes a random nonce for each sign-in, puts it into the provider's
//!   authorization request (so the provider writes it into the ID token) and sends it here too.
//!   The server compares the two and accepts each nonce once.
//! - **Errors:** 401 [`OAUTH_FAILED`](crate::codes::OAUTH_FAILED) for a token that is refused
//!   (signature, issuer, audience, expiry, nonce, a token used before), 404 for a provider the
//!   server does not know, 503 `unavailable` when the provider's keys cannot be fetched.
//!
//! The desktop flow (system browser + PKCE + a loopback redirect) is described in the server
//! README; `net_backend_client` runs it with its feature `oauth`.

use serde::{Deserialize, Serialize};

use crate::auth::{AuthSession, Secret};
use crate::error::{ApiError, ValidationDetails};

/// The largest ID token accepted (bytes of its compact form).
pub const ID_TOKEN_MAX_BYTES: usize = 16 * 1024;

/// The largest nonce accepted (bytes).
pub const NONCE_MAX_BYTES: usize = 256;

/// The body of [`OAuthLogin`]: the provider's ID token and the nonce the game made for this
/// sign-in.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OAuthToken {
    /// The ID token (a signed JWT in compact form: `header.payload.signature`).
    pub id_token: Secret,
    /// The nonce the game put into the authorization request (servers that require a nonce,
    /// the default, refuse a login without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<Secret>,
}

impl OAuthToken {
    /// An ID token without a nonce.
    pub fn new(id_token: impl Into<Secret>) -> Self {
        Self { id_token: id_token.into(), nonce: None }
    }

    /// The same token with the nonce of its sign-in.
    pub fn with_nonce(mut self, nonce: impl Into<Secret>) -> Self {
        self.nonce = Some(nonce.into());
        self
    }

    /// The shape rules: a non-empty token of three base64url parts within
    /// [`ID_TOKEN_MAX_BYTES`]; a nonce of 1 to [`NONCE_MAX_BYTES`] printable ASCII bytes.
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        let token = self.id_token.expose();
        let parts = token.split('.').count();
        let alphabet = token.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if token.is_empty() || parts != 3 || !alphabet {
            details.add("id_token", "is not a compact JWT (three base64url parts)");
        }
        if token.len() > ID_TOKEN_MAX_BYTES {
            details.add("id_token", format!("is longer than {ID_TOKEN_MAX_BYTES} bytes"));
        }
        if let Some(nonce) = &self.nonce {
            let text = nonce.expose();
            if text.is_empty() || text.len() > NONCE_MAX_BYTES || !text.bytes().all(|b| b.is_ascii_graphic()) {
                details.add("nonce", format!("must be 1 to {NONCE_MAX_BYTES} printable ASCII characters"));
            }
        }
        details.into_result()
    }
}

mod calls {
    use super::*;

    use crate::auth::is_valid_provider;
    use crate::http_call::{HttpCall, PathParams, PayloadKind};
    use crate::routes::{self, HttpMethod, Route};

    /// Log in with an ID token of `provider` (or, with the Bearer token of a recent login, link the
    /// provider's account): `POST /v1/auth/oauth/{provider}` with an [`OAuthToken`] →
    /// [`AuthSession`].
    #[derive(Clone, Debug)]
    #[non_exhaustive]
    pub struct OAuthLogin {
        /// The server's name of the provider (`google`).
        pub provider: String,
        /// The token.
        pub token: OAuthToken,
    }

    impl OAuthLogin {
        /// A login with `token` at `provider`.
        pub fn new(provider: impl Into<String>, token: OAuthToken) -> Self {
            Self { provider: provider.into(), token }
        }
    }

    impl HttpCall for OAuthLogin {
        type Payload = OAuthToken;
        type Response = AuthSession;
        const ROUTE: Route = Route::new(HttpMethod::Post, routes::auth::OAUTH, false);
        const PAYLOAD: PayloadKind = PayloadKind::Json;

        fn payload(&self) -> &OAuthToken {
            &self.token
        }

        fn path_params(&self) -> PathParams {
            PathParams::new().with("provider", &self.provider)
        }

        fn from_parts(params: &PathParams, token: OAuthToken) -> Result<Self, ApiError> {
            Ok(Self::new(params.checked("provider", is_valid_provider, "is not a provider name")?, token))
        }
    }
}

pub use calls::OAuthLogin;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_call::HttpCall;

    #[test]
    fn shapes() {
        assert!(OAuthToken::new("eyJh.eyJz.c2ln").validate().is_ok());
        assert!(OAuthToken::new("eyJh.eyJz.c2ln").with_nonce("n-0123456789").validate().is_ok());
        assert!(OAuthToken::new("eyJh.eyJz").validate().is_err());
        assert!(OAuthToken::new("").validate().is_err());
        assert!(OAuthToken::new("a.b.c+").validate().is_err());
        assert!(OAuthToken::new("a.b.c").with_nonce("").validate().is_err());
        assert!(OAuthToken::new("a.b.c").with_nonce("with space").validate().is_err());
        assert!(OAuthToken::new("a.b.c").with_nonce("n".repeat(NONCE_MAX_BYTES + 1)).validate().is_err());
        assert!(OAuthToken::new(format!("a.b.{}", "c".repeat(ID_TOKEN_MAX_BYTES))).validate().is_err());
    }

    #[test]
    fn json_and_path() {
        let token = OAuthToken::new("a.b.c").with_nonce("n1");
        let json = serde_json::to_value(&token).unwrap_or_default();
        assert_eq!(json, serde_json::json!({"id_token": "a.b.c", "nonce": "n1"}));
        let bare = serde_json::to_value(OAuthToken::new("a.b.c")).unwrap_or_default();
        assert_eq!(bare, serde_json::json!({"id_token": "a.b.c"}));
        assert!(!format!("{token:?}").contains("a.b.c"), "the token never shows in Debug");
        let call = OAuthLogin::new("google", token);
        assert_eq!(call.path().as_deref(), Some("/v1/auth/oauth/google"));
        assert!(OAuthLogin::new("Bad Name", OAuthToken::new("a.b.c")).path().is_none());
    }
}
