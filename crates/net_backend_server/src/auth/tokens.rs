//! Opaque tokens: 32 random bytes from the OS as 64 hex characters behind a kind prefix
//! (`nbsa_` access, `nbsr_` refresh, `nbse_` one-time email tokens). The database stores only the
//! SHA-256 of a token (high-entropy secrets need no slow hash), so a database leak reveals no
//! usable token.
//!
//! **Refresh rotation:** the pair that replaces a refresh token is DERIVED from it with
//! HMAC-SHA-256 under a server key and a random per-rotation nonce ([`derive`]). Presenting the same
//! refresh token again within the grace window recomputes exactly the same pair, so the server
//! answers a retry with the same tokens without ever storing a token in plain text. The nonce is
//! forgotten once the grace window has passed: then even the key plus an old token cannot compute
//! the current tokens.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::AppError;

/// The prefix of access tokens.
pub(crate) const ACCESS_PREFIX: &str = "nbsa_";
/// The prefix of refresh tokens.
pub(crate) const REFRESH_PREFIX: &str = "nbsr_";
/// The prefix of one-time email tokens (verification, reset).
pub(crate) const EMAIL_PREFIX: &str = "nbse_";

const RANDOM_BYTES: usize = 32;
const HEX_LEN: usize = RANDOM_BYTES * 2;

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Random bytes from the OS (an error, never a panic, if the OS refuses).
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N], AppError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| AppError::internal(std::io::Error::other(format!("the system random generator failed: {e}"))))?;
    Ok(bytes)
}

/// A new random token with this prefix.
pub(crate) fn random_token(prefix: &str) -> Result<String, AppError> {
    let bytes = random_bytes::<RANDOM_BYTES>()?;
    Ok(format!("{prefix}{}", hex(&bytes)))
}

/// Whether `token` has the shape of a token of this kind (prefix + 64 lowercase hex characters).
pub(crate) fn has_shape(token: &str, prefix: &str) -> bool {
    token.strip_prefix(prefix).is_some_and(|rest| rest.len() == HEX_LEN && rest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// The SHA-256 of a token, hex: what the database stores and looks up.
pub(crate) fn hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// A token of kind `prefix` derived from `from` with the server key and the rotation's nonce:
/// HMAC-SHA-256(key, purpose ‖ 0x00 ‖ nonce ‖ 0x00 ‖ from).
pub(crate) fn derive(key: &[u8], nonce: &str, from: &str, purpose: &str, prefix: &str) -> Result<String, AppError> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|_| AppError::internal(std::io::Error::other("invalid derivation key length")))?;
    mac.update(purpose.as_bytes());
    mac.update(&[0]);
    mac.update(nonce.as_bytes());
    mac.update(&[0]);
    mac.update(from.as_bytes());
    Ok(format!("{prefix}{}", hex(&mac.finalize().into_bytes())))
}

/// A new random derivation key as hex (stored in `auth_secrets`).
pub(crate) fn new_key_hex() -> Result<String, AppError> {
    Ok(hex(&random_bytes::<32>()?))
}

/// Hex text to bytes (`None` if not hex).
pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| text.get(i..i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_and_hashes() {
        let token = random_token(ACCESS_PREFIX).unwrap_or_default();
        assert!(has_shape(&token, ACCESS_PREFIX), "{token}");
        assert!(!has_shape(&token, REFRESH_PREFIX));
        assert!(!has_shape("nbsa_xyz", ACCESS_PREFIX));
        assert!(!has_shape(&token.to_uppercase(), ACCESS_PREFIX));
        assert_ne!(token, random_token(ACCESS_PREFIX).unwrap_or_default());
        // SHA-256("abc"), FIPS 180-2 test vector.
        assert_eq!(hash("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hash(&token).len(), 64);
    }

    #[test]
    fn hmac_matches_rfc_4231_and_derivation_is_stable() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(b"Jefe").ok();
        if let Some(mac) = mac.as_mut() {
            mac.update(b"what do ya want for nothing?");
        }
        let out = mac.map(|m| hex(&m.finalize().into_bytes())).unwrap_or_default();
        assert_eq!(out, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");

        let key = [7u8; 32];
        let a = derive(&key, "n1", "nbsr_old", "access", ACCESS_PREFIX).unwrap_or_default();
        assert_eq!(a, derive(&key, "n1", "nbsr_old", "access", ACCESS_PREFIX).unwrap_or_default());
        assert_ne!(a, derive(&key, "n2", "nbsr_old", "access", ACCESS_PREFIX).unwrap_or_default(), "another rotation nonce");
        assert!(has_shape(&a, ACCESS_PREFIX));
        assert_ne!(a, derive(&key, "n1", "nbsr_old", "refresh", ACCESS_PREFIX).unwrap_or_default());
        assert_ne!(a, derive(&[8u8; 32], "n1", "nbsr_old", "access", ACCESS_PREFIX).unwrap_or_default());
        assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex(&new_key_hex().unwrap_or_default()).map(|k| k.len()), Some(32));
    }
}
