//! OpenAPI schema of the protocol's OpenID Connect body (a mirror struct: the protocol crate has
//! no OpenAPI dependency). A test serializes the real type and compares the field names.

#![allow(dead_code)]

use serde::Serialize;
use utoipa::ToSchema;

/// `POST /v1/auth/oauth/{provider}` body.
#[derive(Serialize, ToSchema)]
pub(crate) struct OAuthToken {
    /// The provider's ID token (a compact JWT, at most 16 KiB).
    id_token: String,
    /// The nonce the game put into the authorization request (required unless the server's
    /// `require_nonce` is off).
    nonce: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use net_backend_protocol::oauth as p;
    use utoipa::openapi::schema::Schema;
    use utoipa::openapi::RefOr;
    use utoipa::PartialSchema;

    use super::*;

    #[test]
    fn mirrors_match_the_wire() {
        let properties: BTreeSet<String> = match OAuthToken::schema() {
            RefOr::T(Schema::Object(object)) => object.properties.keys().cloned().collect(),
            _ => BTreeSet::new(),
        };
        let wire = serde_json::to_value(p::OAuthToken::new("a.b.c").with_nonce("n")).unwrap_or_default();
        let keys: BTreeSet<String> = wire.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
        assert_eq!(properties, keys);
    }
}
