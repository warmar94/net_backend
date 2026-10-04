//! ID tokens: the compact JWS form, signature checks with ring (RS256 / ES256 only) and the
//! OpenID Connect claim rules. Pure functions; the keys come from [`super::jwks`].
//!
//! The algorithm is never taken on trust: the header's `alg` must be one the provider allows, the
//! key must be of that algorithm's type (an RSA key for RS256, a P-256 key for ES256) and, if the
//! key names an algorithm, the same one. `none`, HMAC and every other algorithm are refused; so
//! is a `crit` header (no extension is understood).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};
use serde_json::Value;

use super::config::Provider;

/// A signature algorithm this module checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Alg {
    /// RSASSA-PKCS1-v1_5 with SHA-256 (2048 to 8192 bit keys).
    Rs256,
    /// ECDSA on P-256 with SHA-256 (the fixed 64-byte `r || s` form JWS uses).
    Es256,
}

impl Alg {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "RS256" => Some(Alg::Rs256),
            "ES256" => Some(Alg::Es256),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Alg::Rs256 => "RS256",
            Alg::Es256 => "ES256",
        }
    }
}

/// A public key from a provider's JWKS.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum PublicKey {
    /// RSA: modulus and exponent, big-endian.
    Rsa { n: Vec<u8>, e: Vec<u8> },
    /// P-256: the uncompressed point (`0x04 || x || y`).
    P256 { point: Vec<u8> },
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublicKey::Rsa { n, .. } => write!(f, "Rsa({} bits)", n.len() * 8),
            PublicKey::P256 { .. } => f.write_str("P256"),
        }
    }
}

/// One usable key of a JWKS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Jwk {
    pub(crate) kid: Option<String>,
    /// The key's own `alg`, when it names one.
    pub(crate) alg: Option<String>,
    pub(crate) key: PublicKey,
}

impl Jwk {
    /// Whether this key may check a signature of `alg`.
    pub(crate) fn fits(&self, alg: Alg) -> bool {
        let kind = matches!((&self.key, alg), (PublicKey::Rsa { .. }, Alg::Rs256) | (PublicKey::P256 { .. }, Alg::Es256));
        kind && self.alg.as_deref().is_none_or(|a| a == alg.name())
    }
}

/// Why a token was refused (logged; the client only hears `oauth_failed`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal(pub(crate) String);

fn refuse(reason: impl Into<String>) -> Refusal {
    Refusal(reason.into())
}

/// A token split and decoded, signature not yet checked.
#[derive(Debug)]
pub(crate) struct Parsed {
    pub(crate) alg: Alg,
    pub(crate) kid: Option<String>,
    pub(crate) claims: Value,
    signing_input: String,
    signature: Vec<u8>,
}

fn part(text: &str, what: &str) -> Result<Vec<u8>, Refusal> {
    URL_SAFE_NO_PAD.decode(text).map_err(|_| refuse(format!("the {what} is not base64url")))
}

/// Split the compact form, decode header and claims, and check the header against the
/// provider's algorithms.
pub(crate) fn parse(token: &str, allowed: &[Alg]) -> Result<Parsed, Refusal> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(refuse("not a compact JWS (three parts)"));
    };
    let header_json: Value = serde_json::from_slice(&part(header, "header")?).map_err(|_| refuse("the header is not JSON"))?;
    let header_obj = header_json.as_object().ok_or_else(|| refuse("the header is not an object"))?;
    let alg_name = header_obj.get("alg").and_then(Value::as_str).ok_or_else(|| refuse("the header has no alg"))?;
    let alg = Alg::from_name(alg_name).filter(|a| allowed.contains(a)).ok_or_else(|| refuse(format!("the algorithm {alg_name:.16} is not accepted")))?;
    if header_obj.contains_key("crit") {
        return Err(refuse("the header has crit extensions"));
    }
    if let Some(typ) = header_obj.get("typ") {
        let typ = typ.as_str().unwrap_or("");
        if !typ.eq_ignore_ascii_case("JWT") {
            return Err(refuse("the header's typ is not JWT"));
        }
    }
    let kid = match header_obj.get("kid") {
        None => None,
        Some(Value::String(kid)) if kid.len() <= 256 => Some(kid.clone()),
        Some(_) => return Err(refuse("the header's kid is not a short string")),
    };
    let claims: Value = serde_json::from_slice(&part(payload, "payload")?).map_err(|_| refuse("the payload is not JSON"))?;
    if !claims.is_object() {
        return Err(refuse("the payload is not an object"));
    }
    let signature = part(signature, "signature")?;
    Ok(Parsed { alg, kid, claims, signing_input: format!("{header}.{payload}"), signature })
}

impl Parsed {
    /// Check the signature with `key` (which must fit the algorithm).
    pub(crate) fn verify(&self, key: &Jwk) -> Result<(), Refusal> {
        if !key.fits(self.alg) {
            return Err(refuse("the key does not fit the algorithm"));
        }
        let message = self.signing_input.as_bytes();
        let checked = match (&key.key, self.alg) {
            (PublicKey::Rsa { n, e }, Alg::Rs256) => {
                RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() }.verify(&signature::RSA_PKCS1_2048_8192_SHA256, message, &self.signature)
            }
            (PublicKey::P256 { point }, Alg::Es256) => UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point).verify(message, &self.signature),
            _ => return Err(refuse("the key does not fit the algorithm")),
        };
        checked.map_err(|_| refuse("the signature does not match"))
    }
}

/// The rules of one check.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rules<'a> {
    /// Now, Unix seconds.
    pub(crate) now: i64,
    pub(crate) skew: i64,
    pub(crate) max_age: i64,
    /// The nonce the client sent.
    pub(crate) nonce: Option<&'a str>,
    pub(crate) require_nonce: bool,
}

/// What a valid token says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    pub(crate) subject: String,
    pub(crate) issuer: String,
    pub(crate) expires_at: i64,
    pub(crate) nonce: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) email_verified: bool,
    pub(crate) name: Option<String>,
}

fn seconds(claims: &Value, name: &str) -> Result<Option<i64>, Refusal> {
    match claims.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let secs = value.as_i64().or_else(|| value.as_f64().filter(|f| f.is_finite() && f.abs() < 1e15).map(|f| f.floor() as i64));
            secs.map(Some).ok_or_else(|| refuse(format!("{name} is not a number")))
        }
    }
}

fn text(claims: &Value, name: &str, max: usize) -> Option<String> {
    claims.get(name).and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)).map(str::to_string)
}

/// Equal in time that does not depend on where the inputs differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The OpenID Connect claim rules: issuer, audience (+ `azp`), expiry, issue time, not-before,
/// subject and nonce.
pub(crate) fn check_claims(claims: &Value, provider: &Provider, rules: Rules<'_>) -> Result<Verified, Refusal> {
    let issuer = claims.get("iss").and_then(Value::as_str).ok_or_else(|| refuse("no iss"))?;
    if !provider.issuers.iter().any(|i| i == issuer) {
        return Err(refuse("the issuer is not this provider's"));
    }
    let audiences: Vec<&str> = match claims.get("aud") {
        Some(Value::String(aud)) => vec![aud.as_str()],
        Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).collect(),
        _ => return Err(refuse("no aud")),
    };
    if !audiences.iter().any(|a| provider.client_ids.iter().any(|c| c == a)) {
        return Err(refuse("the audience is not one of this game's client ids"));
    }
    match claims.get("azp").and_then(Value::as_str) {
        Some(azp) if !provider.client_ids.iter().any(|c| c == azp) => return Err(refuse("azp is not one of this game's client ids")),
        None if audiences.len() > 1 => return Err(refuse("several audiences without azp")),
        _ => {}
    }
    let expires_at = seconds(claims, "exp")?.ok_or_else(|| refuse("no exp"))?;
    if expires_at.saturating_add(rules.skew) <= rules.now {
        return Err(refuse("expired"));
    }
    let issued_at = seconds(claims, "iat")?.ok_or_else(|| refuse("no iat"))?;
    if issued_at > rules.now.saturating_add(rules.skew) {
        return Err(refuse("issued in the future"));
    }
    if rules.now.saturating_sub(issued_at) > rules.max_age.saturating_add(rules.skew) {
        return Err(refuse("issued too long ago"));
    }
    if let Some(not_before) = seconds(claims, "nbf")? {
        if not_before > rules.now.saturating_add(rules.skew) {
            return Err(refuse("not valid yet (nbf)"));
        }
    }
    let subject = text(claims, "sub", 255).ok_or_else(|| refuse("no usable sub"))?;
    let token_nonce = match claims.get("nonce") {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) => Some(n.clone()),
        Some(_) => return Err(refuse("the nonce is not a string")),
    };
    match (&token_nonce, rules.nonce) {
        (Some(theirs), Some(ours)) if same(theirs.as_bytes(), ours.as_bytes()) => {}
        (Some(_), Some(_)) => return Err(refuse("the nonce does not match")),
        (Some(_), None) => return Err(refuse("the token has a nonce but the login sent none")),
        (None, Some(_)) => return Err(refuse("the login sent a nonce but the token has none")),
        (None, None) if rules.require_nonce => return Err(refuse("a nonce is required")),
        (None, None) => {}
    }
    let email_verified = match claims.get("email_verified") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    };
    Ok(Verified {
        subject,
        issuer: issuer.to_string(),
        expires_at,
        nonce: token_nonce,
        email: text(claims, "email", 320),
        email_verified,
        name: text(claims, "name", 256),
    })
}

/// Decode a base64url field of a JWK.
pub(crate) fn b64(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    use serde_json::json;

    use super::*;

    fn enc(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn provider() -> Provider {
        Provider {
            name: "test".into(),
            label: "Test".into(),
            issuer: "https://issuer.example".into(),
            issuers: vec!["https://issuer.example".into()],
            jwks_uri: None,
            client_ids: vec!["game".into(), "game-2".into()],
            algorithms: vec![Alg::Rs256, Alg::Es256],
        }
    }

    fn ec_key() -> (EcdsaKeyPair, Jwk) {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).expect("pkcs8");
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).expect("pair");
        let jwk = Jwk { kid: Some("k1".into()), alg: None, key: PublicKey::P256 { point: pair.public_key().as_ref().to_vec() } };
        (pair, jwk)
    }

    fn sign(pair: &EcdsaKeyPair, header: &Value, claims: &Value) -> String {
        let input = format!("{}.{}", enc(header.to_string().as_bytes()), enc(claims.to_string().as_bytes()));
        let sig = pair.sign(&SystemRandom::new(), input.as_bytes()).expect("sign");
        format!("{input}.{}", enc(sig.as_ref()))
    }

    fn rules(nonce: Option<&str>) -> Rules<'_> {
        Rules { now: 1_000_000, skew: 60, max_age: 600, nonce, require_nonce: true }
    }

    fn claims() -> Value {
        json!({"iss": "https://issuer.example", "aud": "game", "sub": "user-1", "exp": 1_000_300, "iat": 999_900, "nonce": "n1", "email": "a@example.com", "email_verified": true})
    }

    #[test]
    fn parse_and_verify() {
        let (pair, jwk) = ec_key();
        let token = sign(&pair, &json!({"alg": "ES256", "kid": "k1", "typ": "JWT"}), &claims());
        let parsed = parse(&token, &[Alg::Es256]).expect("parsed");
        assert_eq!(parsed.kid.as_deref(), Some("k1"));
        assert!(parsed.verify(&jwk).is_ok());
        // Another key, a changed payload, a key of another type, a key naming another alg.
        let (_, other) = ec_key();
        assert!(parsed.verify(&other).is_err());
        let mut parts: Vec<&str> = token.split('.').collect();
        let changed = enc(json!({"sub": "user-2"}).to_string().as_bytes());
        parts[1] = &changed;
        assert!(parse(&parts.join("."), &[Alg::Es256]).is_ok_and(|p| p.verify(&jwk).is_err()));
        let rsa = Jwk { kid: Some("k1".into()), alg: None, key: PublicKey::Rsa { n: vec![1; 256], e: vec![1, 0, 1] } };
        assert!(parsed.verify(&rsa).is_err());
        let named = Jwk { alg: Some("RS256".into()), ..jwk.clone() };
        assert!(parsed.verify(&named).is_err());
    }

    #[test]
    fn headers_that_are_refused() {
        let (pair, _) = ec_key();
        let c = claims();
        // Not allowed for this provider; none; HMAC; crit; another typ; a long kid.
        assert!(parse(&sign(&pair, &json!({"alg": "ES256"}), &c), &[Alg::Rs256]).is_err());
        let none = format!("{}.{}.", enc(br#"{"alg":"none"}"#), enc(c.to_string().as_bytes()));
        assert!(parse(&none, &[Alg::Rs256, Alg::Es256]).is_err());
        assert!(parse(&sign(&pair, &json!({"alg": "HS256"}), &c), &[Alg::Rs256, Alg::Es256]).is_err());
        assert!(parse(&sign(&pair, &json!({"alg": "ES256", "crit": ["exp"]}), &c), &[Alg::Es256]).is_err());
        assert!(parse(&sign(&pair, &json!({"alg": "ES256", "typ": "at+jwt"}), &c), &[Alg::Es256]).is_err());
        assert!(parse(&sign(&pair, &json!({"alg": "ES256", "kid": "k".repeat(300)}), &c), &[Alg::Es256]).is_err());
        assert!(parse("a.b", &[Alg::Es256]).is_err());
        assert!(parse("a.b.c.d", &[Alg::Es256]).is_err());
        assert!(parse("!!.e30.AA", &[Alg::Es256]).is_err());
    }

    #[test]
    fn claim_rules() {
        let p = provider();
        assert!(check_claims(&claims(), &p, rules(Some("n1"))).is_ok_and(|v| v.subject == "user-1" && v.email_verified && v.expires_at == 1_000_300));
        let with = |key: &str, value: Value| {
            let mut c = claims();
            c[key] = value;
            check_claims(&c, &p, rules(Some("n1")))
        };
        assert!(with("iss", json!("https://other.example")).is_err());
        assert!(with("aud", json!("someone-else")).is_err());
        assert!(with("aud", json!(["someone-else", "game"])).is_err(), "several audiences need azp");
        assert!(with("aud", json!(["someone-else", "game"])).is_err());
        let mut c = claims();
        c["aud"] = json!(["someone-else", "game"]);
        c["azp"] = json!("game");
        assert!(check_claims(&c, &p, rules(Some("n1"))).is_ok());
        c["azp"] = json!("someone-else");
        assert!(check_claims(&c, &p, rules(Some("n1"))).is_err());
        // Expiry with skew, issue time, age, nbf.
        assert!(with("exp", json!(1_000_000 - 61)).is_err());
        assert!(with("exp", json!(1_000_000 - 59)).is_ok(), "inside the skew");
        assert!(with("exp", json!("soon")).is_err());
        assert!(with("iat", json!(1_000_000 + 120)).is_err());
        assert!(with("iat", json!(1_000_000 - 700)).is_err());
        assert!(with("nbf", json!(1_000_000 + 120)).is_err());
        assert!(with("nbf", json!(1_000_000)).is_ok());
        assert!(with("sub", json!("")).is_err());
        assert!(with("sub", json!(42)).is_err());
        // Nonces.
        assert!(check_claims(&claims(), &p, rules(Some("n2"))).is_err());
        assert!(check_claims(&claims(), &p, rules(None)).is_err());
        let mut bare = claims();
        bare.as_object_mut().map(|o| o.remove("nonce"));
        assert!(check_claims(&bare, &p, rules(None)).is_err(), "required");
        assert!(check_claims(&bare, &p, rules(Some("n1"))).is_err());
        assert!(check_claims(&bare, &p, Rules { require_nonce: false, ..rules(None) }).is_ok());
        for missing in ["exp", "iat", "iss", "aud"] {
            let mut c = claims();
            c.as_object_mut().map(|o| o.remove(missing));
            assert!(check_claims(&c, &p, rules(Some("n1"))).is_err(), "{missing}");
        }
    }
}
