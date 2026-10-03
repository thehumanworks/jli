//! ID token validation: an RS256 signature against the issuer's published keys, then the claims.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::signature::{RsaPublicKeyComponents, RSA_PKCS1_2048_8192_SHA256};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{ApiError, Error, IdTokenError, Result};
use crate::http::{HttpClient, HttpRequest};
use crate::responses::one_or_many;

/// The issuer's signing keys (a JSON Web Key Set).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Jwks {
    /// The keys.
    pub keys: Vec<Jwk>,
}

/// One public key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Jwk {
    /// The key id an ID token's header names.
    #[serde(default)]
    pub kid: Option<String>,
    /// The key type; only `RSA` keys are used.
    pub kty: String,
    /// The algorithm, when the key names one.
    #[serde(default)]
    pub alg: Option<String>,
    /// `sig` for a signing key.
    #[serde(default, rename = "use")]
    pub key_use: Option<String>,
    /// The RSA modulus, base64url.
    #[serde(default)]
    pub n: Option<String>,
    /// The RSA exponent, base64url.
    #[serde(default)]
    pub e: Option<String>,
}

impl Jwks {
    /// Fetch the key set from `uri`.
    pub fn fetch(http: &dyn HttpClient, uri: &str) -> Result<Jwks> {
        let reply = http.send(HttpRequest::get(uri).header("accept", "application/json"))?;
        let (status, request_id) = (reply.status, reply.request_id());
        let body = reply.text()?;
        if !(200..300).contains(&status) {
            return Err(ApiError::from_reply(status, body, request_id).into());
        }
        serde_json::from_str(&body).map_err(|error| Error::json("the issuer's key set", error))
    }

    /// The signing key with id `kid`, or the only key when the token names none.
    pub fn find(&self, kid: Option<&str>) -> Option<&Jwk> {
        let signing = || self.keys.iter().filter(|key| key.kty == "RSA" && key.key_use.as_deref().unwrap_or("sig") == "sig");
        match kid {
            Some(kid) => signing().find(|key| key.kid.as_deref() == Some(kid)),
            None => {
                let mut keys = signing();
                let only = keys.next();
                if keys.next().is_some() {
                    None
                } else {
                    only
                }
            }
        }
    }
}

/// What an ID token must say to be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validation<'a> {
    /// The issuer, exactly.
    pub issuer: &'a str,
    /// The client id, which must be in `aud`.
    pub audience: &'a str,
    /// The nonce this sign-in sent. `None` skips the check, as for a token from a refresh.
    pub nonce: Option<&'a str>,
    /// The current time, in Unix seconds.
    pub now: i64,
    /// How many seconds of clock skew to allow.
    pub leeway: i64,
}

impl<'a> Validation<'a> {
    /// Checks for `issuer` and `audience` at the current time, with 30 seconds of leeway.
    pub fn new(issuer: &'a str, audience: &'a str) -> Validation<'a> {
        Validation { issuer, audience, nonce: None, now: chrono::Utc::now().timestamp(), leeway: 30 }
    }

    /// The same checks, and the nonce too.
    pub fn nonce(mut self, nonce: &'a str) -> Validation<'a> {
        self.nonce = Some(nonce);
        self
    }
}

/// An ID token's claims, once verified.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IdTokenClaims {
    /// The issuer.
    #[serde(default)]
    pub iss: String,
    /// The stable account identity.
    #[serde(default)]
    pub sub: String,
    /// The audiences: the client ids the token is for.
    #[serde(default, deserialize_with = "one_or_many")]
    pub aud: Vec<String>,
    /// When it expires, in Unix seconds.
    #[serde(default)]
    pub exp: Option<i64>,
    /// When it was issued, in Unix seconds.
    #[serde(default)]
    pub iat: Option<i64>,
    /// The nonce of the sign-in that asked for it.
    #[serde(default)]
    pub nonce: Option<String>,
    /// The email address, with the `email` scope.
    #[serde(default)]
    pub email: Option<String>,
    /// Whether the email address is verified.
    #[serde(default)]
    pub email_verified: Option<bool>,
    /// The person's name, with the `profile` scope.
    #[serde(default)]
    pub name: Option<String>,
    /// A picture URL, with the `profile` scope.
    #[serde(default)]
    pub picture: Option<String>,
    /// Every other claim, as sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

fn decode_part<T: DeserializeOwned>(part: &str, what: &str) -> std::result::Result<T, IdTokenError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part.trim_end_matches('='))
        .map_err(|error| IdTokenError::Malformed(format!("the {what} is not base64url: {error}")))?;
    serde_json::from_slice(&bytes).map_err(|error| IdTokenError::Malformed(format!("the {what} is not JSON: {error}")))
}

fn split(token: &str) -> std::result::Result<(&str, &str, &str), IdTokenError> {
    let mut parts = token.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(header), Some(payload), Some(signature), None) => Ok((header, payload, signature)),
        _ => Err(IdTokenError::Malformed("a JWT has three parts".into())),
    }
}

/// A JWT's claims, without checking its signature. Use it only to read your own token's expiry,
/// never to trust an identity.
pub fn unverified_claims<T: DeserializeOwned>(token: &str) -> std::result::Result<T, IdTokenError> {
    let (_, payload, _) = split(token)?;
    decode_part(payload, "payload")
}

/// Verify an ID token's RS256 signature against `jwks`, then its issuer, audience, expiry and
/// nonce.
pub fn verify_id_token(
    token: &str,
    jwks: &Jwks,
    validation: &Validation,
) -> std::result::Result<IdTokenClaims, IdTokenError> {
    let (header_part, payload_part, signature_part) = split(token)?;
    let header: Header = decode_part(header_part, "header")?;
    if header.alg != "RS256" {
        return Err(IdTokenError::UnsupportedAlgorithm(header.alg));
    }
    let key = jwks.find(header.kid.as_deref()).ok_or_else(|| IdTokenError::UnknownKey(header.kid.clone()))?;
    if key.alg.as_deref().is_some_and(|alg| alg != "RS256") {
        return Err(IdTokenError::UnknownKey(header.kid));
    }
    let component = |value: &Option<String>| -> std::result::Result<Vec<u8>, IdTokenError> {
        let text = value.as_deref().ok_or_else(|| IdTokenError::UnknownKey(header.kid.clone()))?;
        URL_SAFE_NO_PAD.decode(text).map_err(|_| IdTokenError::UnknownKey(header.kid.clone()))
    };
    let (n, e) = (component(&key.n)?, component(&key.e)?);
    let signature = URL_SAFE_NO_PAD
        .decode(signature_part)
        .map_err(|error| IdTokenError::Malformed(format!("the signature is not base64url: {error}")))?;
    let signed = &token[..header_part.len() + 1 + payload_part.len()];
    RsaPublicKeyComponents { n: &n, e: &e }
        .verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
        .map_err(|_| IdTokenError::BadSignature)?;

    let claims: IdTokenClaims = decode_part(payload_part, "payload")?;
    if claims.iss != validation.issuer {
        return Err(IdTokenError::Issuer { expected: validation.issuer.into(), found: claims.iss });
    }
    if !claims.aud.iter().any(|audience| audience == validation.audience) {
        return Err(IdTokenError::Audience { expected: validation.audience.into() });
    }
    let exp = claims.exp.ok_or(IdTokenError::MissingClaim("exp"))?;
    if exp + validation.leeway <= validation.now {
        return Err(IdTokenError::Expired);
    }
    claims.iat.ok_or(IdTokenError::MissingClaim("iat"))?;
    if claims.sub.is_empty() {
        return Err(IdTokenError::MissingClaim("sub"));
    }
    if let Some(nonce) = validation.nonce {
        if claims.nonce.as_deref() != Some(nonce) {
            return Err(IdTokenError::Nonce);
        }
    }
    Ok(claims)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{KeyPair, RsaKeyPair, RSA_PKCS1_SHA256};
    use serde_json::json;

    /// The fixture key, its key set, and a signer for test tokens.
    pub(crate) struct TestKey {
        pair: RsaKeyPair,
        pub kid: String,
    }

    impl TestKey {
        pub(crate) fn load() -> TestKey {
            let der = include_bytes!("../testing/test-key.pk8");
            TestKey { pair: RsaKeyPair::from_pkcs8(der).unwrap(), kid: "test-key".into() }
        }

        pub(crate) fn jwks(&self) -> Jwks {
            let public: ring::rsa::PublicKeyComponents<Vec<u8>> = self.pair.public_key().into();
            serde_json::from_value(json!({ "keys": [{
                "kid": self.kid, "kty": "RSA", "alg": "RS256", "use": "sig",
                "n": URL_SAFE_NO_PAD.encode(&public.n), "e": URL_SAFE_NO_PAD.encode(&public.e),
            }] }))
            .unwrap()
        }

        pub(crate) fn sign(&self, header: &Value, claims: &Value) -> String {
            let input =
                format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
            let mut signature = vec![0; self.pair.public().modulus_len()];
            self.pair.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut signature).unwrap();
            format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
        }

        pub(crate) fn token(&self, claims: &Value) -> String {
            self.sign(&json!({ "alg": "RS256", "kid": self.kid, "typ": "JWT" }), claims)
        }
    }

    const NOW: i64 = 1_790_000_000;

    fn claims() -> Value {
        json!({ "iss": "https://auth.openai.com", "sub": "user-1", "aud": ["oaiapp_1"], "exp": NOW + 3600, "iat": NOW,
                "nonce": "n-1", "email": "a@example.com", "email_verified": true, "https://api.openai.com/auth": { "plan": "pro" } })
    }

    fn validation() -> Validation<'static> {
        Validation { issuer: "https://auth.openai.com", audience: "oaiapp_1", nonce: Some("n-1"), now: NOW, leeway: 30 }
    }

    #[test]
    fn a_good_token_verifies_and_gives_its_claims() {
        let key = TestKey::load();
        let verified = verify_id_token(&key.token(&claims()), &key.jwks(), &validation()).unwrap();
        assert_eq!((verified.sub.as_str(), verified.email.as_deref()), ("user-1", Some("a@example.com")));
        assert_eq!(verified.extra["https://api.openai.com/auth"]["plan"], "pro");
        let single_audience = with("aud", json!("oaiapp_1"));
        assert!(verify_id_token(&key.token(&single_audience), &key.jwks(), &validation()).is_ok());
        let unverified: Value = unverified_claims(&key.token(&claims())).unwrap();
        assert_eq!(unverified["sub"], "user-1");
    }

    fn with(field: &str, value: Value) -> Value {
        let mut claims = claims();
        claims[field] = value;
        claims
    }

    #[test]
    fn every_check_rejects_its_own_failure() {
        let key = TestKey::load();
        let jwks = key.jwks();
        let check = |claims: &Value| verify_id_token(&key.token(claims), &jwks, &validation()).unwrap_err();
        assert!(matches!(check(&with("iss", json!("https://evil.test"))), IdTokenError::Issuer { .. }));
        assert!(matches!(check(&with("aud", json!(["other"]))), IdTokenError::Audience { .. }));
        assert_eq!(check(&with("exp", json!(NOW - 31))), IdTokenError::Expired);
        assert_eq!(check(&with("nonce", json!("other"))), IdTokenError::Nonce);
        assert_eq!(check(&with("sub", json!(""))), IdTokenError::MissingClaim("sub"));
        assert_eq!(check(&with("iat", Value::Null)), IdTokenError::MissingClaim("iat"));
        let within_leeway = with("exp", json!(NOW - 29));
        assert!(verify_id_token(&key.token(&within_leeway), &jwks, &validation()).is_ok());
        let no_nonce_check = Validation { nonce: None, ..validation() };
        assert!(verify_id_token(&key.token(&with("nonce", Value::Null)), &jwks, &no_nonce_check).is_ok());
    }

    #[test]
    fn forged_and_foreign_tokens_are_rejected() {
        let key = TestKey::load();
        let jwks = key.jwks();
        let token = key.token(&claims());
        let (head, rest) = token.split_once('.').unwrap();
        let (_, signature) = rest.split_once('.').unwrap();
        let forged =
            format!("{head}.{}.{signature}", URL_SAFE_NO_PAD.encode(with("sub", json!("someone-else")).to_string()));
        assert_eq!(verify_id_token(&forged, &jwks, &validation()).unwrap_err(), IdTokenError::BadSignature);
        let none = key.sign(&json!({ "alg": "none" }), &claims());
        assert_eq!(
            verify_id_token(&none, &jwks, &validation()).unwrap_err(),
            IdTokenError::UnsupportedAlgorithm("none".into())
        );
        let unknown = key.sign(&json!({ "alg": "RS256", "kid": "rotated" }), &claims());
        assert!(matches!(verify_id_token(&unknown, &jwks, &validation()).unwrap_err(), IdTokenError::UnknownKey(_)));
        assert!(matches!(verify_id_token("a.b", &jwks, &validation()).unwrap_err(), IdTokenError::Malformed(_)));
        assert!(matches!(verify_id_token("@@.@@.@@", &jwks, &validation()).unwrap_err(), IdTokenError::Malformed(_)));
    }

    #[test]
    fn a_token_without_a_key_id_uses_the_only_key() {
        let key = TestKey::load();
        let token = key.sign(&json!({ "alg": "RS256" }), &claims());
        assert!(verify_id_token(&token, &key.jwks(), &validation()).is_ok());
        let mut two = key.jwks();
        two.keys.push(Jwk { kid: Some("other".into()), ..two.keys[0].clone() });
        assert!(matches!(verify_id_token(&token, &two, &validation()).unwrap_err(), IdTokenError::UnknownKey(None)));
    }
}
