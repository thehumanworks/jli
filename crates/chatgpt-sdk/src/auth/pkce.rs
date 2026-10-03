//! Proof Key for Code Exchange (RFC 7636), with the S256 method.

use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::digest::{digest, SHA256};

use crate::error::Result;

/// A PKCE verifier and its S256 challenge. Make a fresh one for every sign-in attempt.
#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// A fresh verifier of 64 random bytes (86 characters) and its challenge.
    pub fn generate() -> Result<Pkce> {
        Ok(Pkce::from_verifier(super::random_token(64)?))
    }

    /// The challenge for a given verifier.
    pub fn from_verifier(verifier: impl Into<String>) -> Pkce {
        let verifier = verifier.into();
        let challenge = URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()));
        Pkce { verifier, challenge }
    }

    /// The secret sent with the code exchange.
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The base64url SHA-256 of the verifier, sent with the authorization request.
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// Always `S256`.
    pub fn method(&self) -> &'static str {
        "S256"
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce").field("verifier", &"<redacted>").field("challenge", &self.challenge).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_7636_example_holds() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(pkce.challenge(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(pkce.method(), "S256");
    }

    #[test]
    fn a_generated_verifier_is_long_and_hidden() {
        let pkce = Pkce::generate().unwrap();
        assert_eq!(pkce.verifier().len(), 86, "43 to 128 characters are allowed");
        assert_ne!(pkce, Pkce::generate().unwrap());
        assert!(!format!("{pkce:?}").contains(pkce.verifier()));
    }
}
