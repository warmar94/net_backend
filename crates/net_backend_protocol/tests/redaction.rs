//! Secrets never appear in `Debug` output, alone or inside the types that carry them.

use net_backend_protocol::auth::*;
use net_backend_protocol::{AccessToken, UnixMillis, UserId, WsAuth, WsClientFrame};

const SECRET: &str = "SUPER-SECRET-VALUE";

fn assert_redacted(debug: String) {
    assert!(!debug.contains(SECRET), "a secret leaked into Debug: {debug}");
    assert!(debug.contains("<redacted>"), "no redaction marker: {debug}");
}

#[test]
fn secret_types() {
    assert_eq!(format!("{:?}", Password::new(SECRET)), "Password(<redacted>)");
    assert_eq!(format!("{:?}", AccessToken::new(SECRET)), "AccessToken(<redacted>)");
    assert_eq!(format!("{:?}", RefreshToken::new(SECRET)), "RefreshToken(<redacted>)");
    assert_eq!(format!("{:?}", Secret::new(SECRET)), "Secret(<redacted>)");
    assert_redacted(format!("{:#?}", Password::new(SECRET)));
}

#[test]
fn types_carrying_secrets() {
    let pair = TokenPair::new(AccessToken::new(SECRET), UnixMillis(1), RefreshToken::new(SECRET), UnixMillis(2));
    assert_redacted(format!("{pair:?}"));
    assert_redacted(format!("{:#?}", AuthSession::new(Account::new(UserId(1), UnixMillis(0)), pair)));
    assert_redacted(format!("{:?}", RegisterRequest::new("a@example.com", SECRET)));
    assert_redacted(format!("{:?}", LoginRequest::new("a@example.com", SECRET)));
    assert_redacted(format!("{:?}", SteamLoginRequest::new(SECRET, "my-game")));
    assert_redacted(format!("{:?}", RefreshRequest::new(SECRET)));
    assert_redacted(format!("{:?}", ChangePasswordRequest::new(SECRET, SECRET)));
    assert_redacted(format!("{:?}", VerifyEmailRequest::new(SECRET)));
    assert_redacted(format!("{:?}", ResetPasswordRequest::new(SECRET, SECRET)));
    assert_redacted(format!("{:?}", WsAuth::new(SECRET)));
    assert_redacted(format!("{:?}", WsClientFrame::Auth(WsAuth::new(SECRET))));
}

#[test]
fn validation_messages_never_quote_secrets() {
    let short = "SECRET";
    let error = RegisterRequest::new("a@example.com", short).validate().err().map(|e| format!("{e:?} {e}")).unwrap_or_default();
    assert!(!error.is_empty() && !error.contains(short), "{error}");
    let error = SteamLoginRequest::new("not-hex-SECRET", "g").validate().err().map(|e| format!("{e:?}")).unwrap_or_default();
    assert!(!error.is_empty() && !error.contains("SECRET"), "{error}");
}

#[test]
fn serialization_still_carries_the_secret() {
    // Redaction is for logs only: the wire must carry the real value.
    let json = serde_json::to_string(&LoginRequest::new("a@example.com", SECRET)).unwrap_or_default();
    assert!(json.contains(SECRET));
    assert!(WsAuth::new(SECRET).to_message().contains(SECRET));
}
