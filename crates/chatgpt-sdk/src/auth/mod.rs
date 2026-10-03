//! Sign in with ChatGPT for an open-source or locally run app.
//!
//! The flow, from the documentation's "Registration and sign-in":
//!
//! 1. Keep one stable [`HostId`] per install ([`crate::CredentialStore::host_id`]).
//! 2. Start a [`LoopbackListener`] on `127.0.0.1`, then build an [`AuthorizationRequest`]: a new
//!    [`Registration`] for a ChatGPT account this app has not seen, or the saved one.
//! 3. Open [`AuthorizationRequest::url`] in the system browser, and [`LoopbackListener::wait`] for
//!    the callback.
//! 4. [`AuthorizationRequest::complete`] checks the callback, exchanges the code, verifies the ID
//!    token against OpenAI's published keys, and returns the [`crate::Credentials`] to save.
//!
//! ```no_run
//! # #[cfg(feature = "ureq")]
//! # fn main() -> chatgpt_sdk::Result<()> {
//! use std::time::Duration;
//! use chatgpt_sdk::auth::{AuthorizationRequest, LoopbackListener, OAuthConfig, Registration};
//! use chatgpt_sdk::{CredentialStore, UreqClient};
//!
//! let store = CredentialStore::open("/home/me/.config/my-app/chatgpt")?;
//! let listener = LoopbackListener::bind()?.app_name("My App");
//! let registration = match store.active()? {
//!     Some(saved) => Registration::returning(&saved),
//!     None => Registration::new("My App"),
//! };
//! let request = AuthorizationRequest::new(OAuthConfig::default(), registration, store.host_id()?, listener.redirect_uri())?;
//! println!("Open {}", request.url());
//! let callback = listener.wait(request.state(), Duration::from_secs(300))?;
//! let credentials = request.complete(&UreqClient::new(), &callback)?;
//! store.save(&credentials)?;
//! store.set_active(Some(&credentials.client_id))?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "ureq"))]
//! # fn main() {}
//! ```

mod flow;
mod host_id;
pub(crate) mod id_token;
mod loopback;
mod pkce;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use ring::rand::{SecureRandom, SystemRandom};

use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpRequest};

pub(crate) use flow::split_scopes;
pub use flow::{exchange_code, refresh_tokens, revoke, AuthorizationRequest, Registration, TokenResponse};
pub use host_id::HostId;
pub use id_token::{unverified_claims, verify_id_token, IdTokenClaims, Jwk, Jwks, Validation};
pub use loopback::{Callback, LoopbackListener};
pub use pkce::Pkce;

/// The OpenID Connect issuer for ChatGPT accounts.
pub const ISSUER: &str = "https://auth.openai.com";
/// The resource ChatGPT plan tokens are for.
pub const API_RESOURCE: &str = "https://api.openai.com/v1";
/// The client id that starts a first-time, dynamic registration. It is never saved or used to
/// exchange a code: the callback carries the client id issued for the registration.
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// The scopes that identify the user.
pub const IDENTITY_SCOPES: [&str; 3] = ["openid", "profile", "email"];
/// The scopes that let the app use the user's ChatGPT plan, and refresh its access.
pub const PLAN_USAGE_SCOPES: [&str; 3] = ["offline_access", "resource.invoke", "chatgpt.tokens.use.direct"];
/// The scope without which a sign-in may not be used for inference.
pub const PLAN_USAGE_SCOPE: &str = "chatgpt.tokens.use.direct";

/// The authorization server's endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthConfig {
    /// The issuer an ID token must name exactly.
    pub issuer: String,
    /// Where the browser goes to sign in.
    pub authorization_endpoint: String,
    /// Where codes are exchanged and tokens refreshed.
    pub token_endpoint: String,
    /// Where a refresh token is revoked at sign-out.
    pub revocation_endpoint: String,
    /// Where the ID token's signing keys are published.
    pub jwks_uri: String,
    /// The resource the tokens are for.
    pub resource: String,
}

/// The production endpoints, as OpenAI's discovery document gave them on 2026-10-03.
impl Default for OAuthConfig {
    fn default() -> Self {
        OAuthConfig {
            issuer: ISSUER.into(),
            authorization_endpoint: "https://auth.openai.com/api/accounts/authorize".into(),
            token_endpoint: "https://auth.openai.com/api/accounts/oauth/token".into(),
            revocation_endpoint: "https://auth.openai.com/api/accounts/oauth/revoke".into(),
            jwks_uri: "https://auth.openai.com/.well-known/jwks.json".into(),
            resource: API_RESOURCE.into(),
        }
    }
}

impl OAuthConfig {
    /// Read the endpoints from `{issuer}/.well-known/openid-configuration`. The resource stays
    /// [`API_RESOURCE`].
    pub fn discover(http: &dyn HttpClient, issuer: &str) -> Result<OAuthConfig> {
        #[derive(serde::Deserialize)]
        struct Discovery {
            issuer: String,
            authorization_endpoint: String,
            token_endpoint: String,
            #[serde(default)]
            revocation_endpoint: Option<String>,
            jwks_uri: String,
        }
        let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
        let reply = http.send(HttpRequest::get(url).header("accept", "application/json"))?;
        let (status, request_id) = (reply.status, reply.request_id());
        let body = reply.text()?;
        if !(200..300).contains(&status) {
            return Err(crate::error::ApiError::from_reply(status, body, request_id).into());
        }
        let found: Discovery = serde_json::from_str(&body).map_err(|error| Error::json("the discovery document", error))?;
        let defaults = OAuthConfig::default();
        Ok(OAuthConfig {
            revocation_endpoint: found.revocation_endpoint.unwrap_or(defaults.revocation_endpoint),
            issuer: found.issuer,
            authorization_endpoint: found.authorization_endpoint,
            token_endpoint: found.token_endpoint,
            jwks_uri: found.jwks_uri,
            resource: defaults.resource,
        })
    }
}

/// Characters left as they are in a query or form value: RFC 3986's unreserved set.
const VALUE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// `name=value&…`, each part percent-encoded.
pub(crate) fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", utf8_percent_encode(name, VALUE), utf8_percent_encode(value, VALUE)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Decode `name=value&…`, with `+` as a space.
pub(crate) fn form_decode(query: &str) -> Vec<(String, String)> {
    let decode = |part: &str| percent_decode_str(&part.replace('+', " ")).decode_utf8_lossy().into_owned();
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (decode(name), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// `bytes` random bytes from the operating system, base64url without padding.
pub(crate) fn random_token(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    SystemRandom::new().fill(&mut buffer).map_err(|_| Error::Protocol("the system has no secure randomness".into()))?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_percent_encoded_and_decoded() {
        let encoded = form_encode(&[("scope", "openid profile"), ("redirect_uri", "http://127.0.0.1:1455/auth/callback")]);
        assert_eq!(encoded, "scope=openid%20profile&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback");
        assert_eq!(
            form_decode("scope=chatgpt.tokens.use.direct+email&state=a%2Bb&flag"),
            vec![
                ("scope".into(), "chatgpt.tokens.use.direct email".into()),
                ("state".into(), "a+b".into()),
                ("flag".into(), String::new()),
            ]
        );
    }

    #[test]
    fn random_tokens_are_fresh() {
        let (a, b) = (random_token(32).unwrap(), random_token(32).unwrap());
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
    }

    #[test]
    fn discovery_reads_the_endpoints() {
        let http = |request: HttpRequest| {
            assert_eq!(request.url, "https://issuer.test/.well-known/openid-configuration");
            Ok(crate::HttpResponse::new(
                200,
                r#"{"issuer":"https://issuer.test","authorization_endpoint":"https://issuer.test/a",
                    "token_endpoint":"https://issuer.test/t","jwks_uri":"https://issuer.test/k"}"#,
            ))
        };
        let config = OAuthConfig::discover(&http, "https://issuer.test/").unwrap();
        assert_eq!(config.token_endpoint, "https://issuer.test/t");
        assert_eq!(config.revocation_endpoint, OAuthConfig::default().revocation_endpoint);
        assert_eq!(config.resource, API_RESOURCE);
    }
}
