//! The authorization request, the code exchange, refresh and revocation.

use std::fmt;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::id_token::{verify_id_token, Jwks, Validation};
use super::{
    form_encode, random_token, Callback, HostId, OAuthConfig, Pkce, DYNAMIC_CLIENT_ID, IDENTITY_SCOPES, PLAN_USAGE_SCOPES,
};
use crate::credentials::Credentials;
use crate::error::{Error, OAuthError, Result, SignInError};
use crate::http::{HttpClient, HttpRequest};

/// Which client the sign-in is for.
#[derive(Clone, PartialEq, Eq)]
pub enum Registration {
    /// A ChatGPT account this app has not registered yet. The user names and authorizes a new
    /// client, and the callback returns its id.
    New {
        /// The app's name, the same on every install. The user may edit it before approving.
        agent_name: String,
    },
    /// A saved registration: sign in again with its issued client id.
    Returning {
        /// The client id issued when the account was registered.
        client_id: String,
        /// The ID token from the last sign-in, which lets the user skip the account picker. It
        /// may have expired.
        id_token_hint: Option<String>,
        /// The account's email, to preselect it.
        login_hint: Option<String>,
        /// The account's verified subject. When set, a sign-in as another account is refused.
        subject: Option<String>,
    },
}

impl Registration {
    /// A first-time registration for the app named `agent_name`.
    pub fn new(agent_name: impl Into<String>) -> Registration {
        Registration::New { agent_name: agent_name.into() }
    }

    /// Sign in again to a saved account. A signed-out account has no ID token left to hint with.
    pub fn returning(saved: &Credentials) -> Registration {
        Registration::Returning {
            client_id: saved.client_id.clone(),
            id_token_hint: saved.id_token.clone(),
            login_hint: saved.email.clone(),
            subject: Some(saved.subject.clone()),
        }
    }

    /// The client id to send in the authorization request.
    pub fn client_id(&self) -> &str {
        match self {
            Registration::New { .. } => DYNAMIC_CLIENT_ID,
            Registration::Returning { client_id, .. } => client_id,
        }
    }
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Registration::New { agent_name } => f.debug_struct("New").field("agent_name", agent_name).finish(),
            Registration::Returning { client_id, id_token_hint, login_hint, subject } => f
                .debug_struct("Returning")
                .field("client_id", client_id)
                .field("id_token_hint", &id_token_hint.as_ref().map(|_| "<redacted>"))
                .field("login_hint", login_hint)
                .field("subject", subject)
                .finish(),
        }
    }
}

/// One sign-in attempt: its fresh state, nonce and PKCE values, and where the browser returns.
pub struct AuthorizationRequest {
    config: OAuthConfig,
    registration: Registration,
    host_id: HostId,
    redirect_uri: String,
    scopes: Vec<String>,
    state: String,
    nonce: String,
    pkce: Pkce,
    prompt: Option<String>,
}

impl fmt::Debug for AuthorizationRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationRequest")
            .field("registration", &self.registration)
            .field("host_id", &self.host_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

impl AuthorizationRequest {
    /// A request for the identity and ChatGPT plan scopes, with fresh state, nonce and PKCE.
    pub fn new(
        config: OAuthConfig,
        registration: Registration,
        host_id: HostId,
        redirect_uri: impl Into<String>,
    ) -> Result<AuthorizationRequest> {
        let scopes = IDENTITY_SCOPES.iter().chain(PLAN_USAGE_SCOPES.iter()).map(|scope| scope.to_string()).collect();
        Ok(AuthorizationRequest {
            config,
            registration,
            host_id,
            redirect_uri: redirect_uri.into(),
            scopes,
            state: random_token(32)?,
            nonce: random_token(32)?,
            pkce: Pkce::generate()?,
            prompt: None,
        })
    }

    /// Ask for other scopes, such as only [`super::IDENTITY_SCOPES`].
    pub fn scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> AuthorizationRequest {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Show the consent screen again: for a user who declined plan usage before and now chooses
    /// to enable it. Do not send it on an ordinary sign-in.
    pub fn prompt_consent(mut self) -> AuthorizationRequest {
        self.prompt = Some("consent".into());
        self
    }

    /// The state to expect back.
    pub fn state(&self) -> &str {
        &self.state
    }

    /// The nonce the ID token must carry.
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    /// Where the browser returns.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Which client this attempt is for.
    pub fn registration(&self) -> &Registration {
        &self.registration
    }

    /// The URL to open in the system browser. It may hold an ID token as a hint: do not log it.
    pub fn url(&self) -> String {
        let scope = self.scopes.join(" ");
        let mut pairs: Vec<(&str, &str)> = vec![("client_id", self.registration.client_id())];
        match &self.registration {
            Registration::New { agent_name } => pairs.push(("agent_name_hint", agent_name)),
            Registration::Returning { id_token_hint, login_hint, .. } => {
                if let Some(hint) = id_token_hint {
                    pairs.push(("id_token_hint", hint));
                }
                if let Some(hint) = login_hint {
                    pairs.push(("login_hint", hint));
                }
            }
        }
        pairs.extend([
            ("ext_agent_host_id", self.host_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", &self.redirect_uri),
            ("scope", &scope),
            ("resource", &self.config.resource),
            ("state", &self.state),
            ("nonce", &self.nonce),
            ("code_challenge", self.pkce.challenge()),
            ("code_challenge_method", self.pkce.method()),
        ]);
        if let Some(prompt) = &self.prompt {
            pairs.push(("prompt", prompt));
        }
        format!("{}?{}", self.config.authorization_endpoint, form_encode(&pairs))
    }

    /// Check the callback, exchange its code, verify the ID token, and return the credentials to
    /// save. A sign-in without plan usage succeeds: check [`Credentials::plan_usage_enabled`].
    pub fn complete(&self, http: &dyn HttpClient, callback: &Callback) -> Result<Credentials> {
        // The state is checked first, even on an error: a callback from another attempt is not ours to act on.
        if callback.state.as_deref() != Some(self.state.as_str()) {
            return Err(SignInError::StateMismatch.into());
        }
        if let Some(error) = &callback.error {
            return Err(match error.as_str() {
                "access_denied" => SignInError::Denied,
                _ => SignInError::Authorization { error: error.clone(), description: callback.error_description.clone() },
            }
            .into());
        }
        let code = callback.code.as_deref().filter(|code| !code.is_empty()).ok_or(SignInError::MissingCode)?;
        let client_id = match &self.registration {
            Registration::New { .. } => callback
                .client_id
                .clone()
                .filter(|id| !id.is_empty() && id != DYNAMIC_CLIENT_ID)
                .ok_or(SignInError::RegistrationIncomplete)?,
            Registration::Returning { client_id, .. } => match &callback.client_id {
                Some(returned) if returned != client_id => {
                    return Err(
                        SignInError::ClientMismatch { expected: client_id.clone(), returned: returned.clone() }.into()
                    )
                }
                _ => client_id.clone(),
            },
        };

        let tokens = exchange_code(http, &self.config, &client_id, code, self.pkce.verifier(), &self.redirect_uri)?;
        let id_token = tokens.id_token.clone().ok_or(SignInError::MissingIdToken)?;
        let jwks = Jwks::fetch(http, &self.config.jwks_uri)?;
        let claims =
            verify_id_token(&id_token, &jwks, &Validation::new(&self.config.issuer, &client_id).nonce(&self.nonce))?;
        if let Registration::Returning { subject: Some(subject), .. } = &self.registration {
            if &claims.sub != subject {
                return Err(SignInError::AccountMismatch.into());
            }
        }
        let scopes = tokens.scope.as_deref().or(callback.scope.as_deref()).map(split_scopes).unwrap_or_default();
        Ok(Credentials {
            email: claims.email,
            issuer: self.config.issuer.clone(),
            subject: claims.sub,
            client_id,
            ext_agent_host_id: self.host_id.to_string(),
            id_token: Some(id_token),
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            token_type: tokens.token_type,
            expires_in: tokens.expires_in.unwrap_or(crate::credentials::DEFAULT_LIFETIME),
            scopes,
            saved_at: chrono::Utc::now(),
            extra: Map::new(),
        })
    }
}

pub(crate) fn split_scopes(scope: &str) -> Vec<String> {
    let mut scopes: Vec<String> = scope.split_whitespace().map(str::to_string).collect();
    scopes.sort();
    scopes.dedup();
    scopes
}

/// A reply from the token endpoint.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct TokenResponse {
    /// The access token for inference.
    #[serde(default)]
    pub access_token: Option<String>,
    /// The refresh token, with `offline_access`. A refresh returns a replacement.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// The ID token.
    #[serde(default)]
    pub id_token: Option<String>,
    /// `Bearer`.
    #[serde(default = "bearer")]
    pub token_type: String,
    /// The access token's lifetime in seconds.
    #[serde(default)]
    pub expires_in: Option<u64>,
    /// The granted scopes, space-separated.
    #[serde(default)]
    pub scope: Option<String>,
    /// Every other field, such as `earliest_refresh_at`.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn bearer() -> String {
    "Bearer".into()
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hidden = |token: &Option<String>| token.as_ref().map(|_| "<redacted>");
        f.debug_struct("TokenResponse")
            .field("access_token", &hidden(&self.access_token))
            .field("refresh_token", &hidden(&self.refresh_token))
            .field("id_token", &hidden(&self.id_token))
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

pub(crate) fn post_form(http: &dyn HttpClient, url: &str, pairs: &[(&str, &str)]) -> Result<(u16, String, Option<String>)> {
    let request = HttpRequest::post(url, form_encode(pairs).into_bytes())
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json");
    let reply = http.send(request)?;
    let (status, request_id) = (reply.status, reply.request_id());
    Ok((status, reply.text()?, request_id))
}

pub(crate) fn token_reply(status: u16, body: &str, request_id: Option<String>) -> Result<TokenResponse> {
    if !(200..300).contains(&status) {
        return Err(OAuthError::from_reply(status, body, request_id).into());
    }
    serde_json::from_str(body).map_err(|error| Error::json("the token response", error))
}

/// Exchange an authorization code for tokens, as a public client with PKCE.
pub fn exchange_code(
    http: &dyn HttpClient,
    config: &OAuthConfig,
    client_id: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    let (status, body, request_id) = post_form(
        http,
        &config.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", code_verifier),
            ("redirect_uri", redirect_uri),
            ("resource", &config.resource),
        ],
    )?;
    token_reply(status, &body, request_id)
}

/// Exchange a refresh token for a new access token and a replacement refresh token. No scope is
/// sent, so the grant stays as it was.
pub fn refresh_tokens(
    http: &dyn HttpClient,
    config: &OAuthConfig,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let (status, body, request_id) = post_form(
        http,
        &config.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("resource", &config.resource),
        ],
    )?;
    token_reply(status, &body, request_id)
}

/// Revoke a refresh token, ending the renewable session. Revoking an already invalid token
/// succeeds too.
pub fn revoke(http: &dyn HttpClient, config: &OAuthConfig, client_id: &str, refresh_token: &str) -> Result<()> {
    let (status, body, request_id) = post_form(
        http,
        &config.revocation_endpoint,
        &[("token", refresh_token), ("token_type_hint", "refresh_token"), ("client_id", client_id)],
    )?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(OAuthError::from_reply(status, &body, request_id).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::form_decode;

    fn request(registration: Registration) -> AuthorizationRequest {
        let host = "urn:uuid:6f1c2d9e-8d4b-4c7a-9e1f-2b3c4d5e6f70".parse().unwrap();
        AuthorizationRequest::new(OAuthConfig::default(), registration, host, "http://127.0.0.1:1455/auth/callback").unwrap()
    }

    fn query(url: &str) -> Vec<(String, String)> {
        form_decode(url.split_once('?').unwrap().1)
    }

    #[test]
    fn a_new_registration_asks_for_the_documented_parameters() {
        let request = request(Registration::new("jli"));
        let url = request.url();
        assert!(url.starts_with("https://auth.openai.com/api/accounts/authorize?client_id=dynamic_agent_client&"));
        let names: Vec<String> = query(&url).into_iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            [
                "client_id",
                "agent_name_hint",
                "ext_agent_host_id",
                "response_type",
                "redirect_uri",
                "scope",
                "resource",
                "state",
                "nonce",
                "code_challenge",
                "code_challenge_method"
            ]
        );
        let value = |name: &str| query(&url).into_iter().find(|(key, _)| key == name).map(|(_, value)| value).unwrap();
        assert_eq!(value("scope"), "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct");
        assert_eq!(value("resource"), "https://api.openai.com/v1");
        assert_eq!(value("redirect_uri"), "http://127.0.0.1:1455/auth/callback");
        assert_eq!(value("state"), request.state());
        assert_eq!(value("code_challenge_method"), "S256");
        assert_ne!(request.state(), request.nonce());
    }

    #[test]
    fn a_returning_sign_in_hints_the_account_and_omits_the_name() {
        let request = request(Registration::Returning {
            client_id: "oaiapp_1".into(),
            id_token_hint: Some("old.id.token".into()),
            login_hint: Some("a@example.com".into()),
            subject: Some("user-1".into()),
        })
        .prompt_consent();
        let url = request.url();
        let pairs = query(&url);
        let names: Vec<&str> = pairs.iter().map(|(name, _)| name.as_str()).collect();
        assert!(!names.contains(&"agent_name_hint"));
        assert!(pairs.contains(&("client_id".into(), "oaiapp_1".into())));
        assert!(pairs.contains(&("login_hint".into(), "a@example.com".into())));
        assert!(pairs.contains(&("id_token_hint".into(), "old.id.token".into())));
        assert_eq!(pairs.last().unwrap(), &("prompt".into(), "consent".into()));
        assert!(!format!("{request:?}").contains("old.id.token"));
    }

    #[test]
    fn a_bad_callback_stops_before_any_exchange() {
        let no_network = |_: HttpRequest| -> std::result::Result<crate::HttpResponse, crate::TransportError> {
            panic!("no request may be sent for a bad callback")
        };
        let new = request(Registration::new("jli"));
        let state = Some(new.state().to_string());
        let check =
            |request: &AuthorizationRequest, callback: Callback| request.complete(&no_network, &callback).unwrap_err();
        let mismatch = check(
            &new,
            Callback { state: Some("other".into()), error: Some("access_denied".into()), ..Callback::default() },
        );
        assert!(matches!(mismatch, Error::SignIn(SignInError::StateMismatch)), "state is checked before the error");
        let denied =
            check(&new, Callback { state: state.clone(), error: Some("access_denied".into()), ..Callback::default() });
        assert!(matches!(denied, Error::SignIn(SignInError::Denied)));
        let other =
            check(&new, Callback { state: state.clone(), error: Some("server_error".into()), ..Callback::default() });
        assert!(matches!(other, Error::SignIn(SignInError::Authorization { .. })));
        let no_code = check(&new, Callback { state: state.clone(), ..Callback::default() });
        assert!(matches!(no_code, Error::SignIn(SignInError::MissingCode)));
        for client_id in [None, Some(DYNAMIC_CLIENT_ID.to_string())] {
            let incomplete =
                check(&new, Callback { state: state.clone(), code: Some("c".into()), client_id, ..Callback::default() });
            assert!(matches!(incomplete, Error::SignIn(SignInError::RegistrationIncomplete)));
        }
        let returning = request(Registration::Returning {
            client_id: "oaiapp_1".into(),
            id_token_hint: None,
            login_hint: None,
            subject: None,
        });
        let callback = Callback {
            state: Some(returning.state().to_string()),
            code: Some("c".into()),
            client_id: Some("oaiapp_2".into()),
            ..Callback::default()
        };
        assert!(matches!(check(&returning, callback), Error::SignIn(SignInError::ClientMismatch { .. })));
    }

    #[test]
    fn scopes_are_split_sorted_and_deduplicated() {
        assert_eq!(split_scopes("openid  email openid\tprofile"), ["email", "openid", "profile"]);
    }

    #[test]
    fn token_requests_are_form_encoded_without_a_secret() {
        let http = |request: HttpRequest| {
            assert_eq!(request.header_value("content-type"), Some("application/x-www-form-urlencoded"));
            let body = request.body_text();
            assert!(!body.contains("client_secret"));
            let reply = if request.url.ends_with("/revoke") {
                assert_eq!(body, "token=rt-1&token_type_hint=refresh_token&client_id=oaiapp_1");
                crate::HttpResponse::new(200, "")
            } else {
                assert_eq!(body, "grant_type=refresh_token&client_id=oaiapp_1&refresh_token=rt-1&resource=https%3A%2F%2Fapi.openai.com%2Fv1");
                crate::HttpResponse::new(
                    200,
                    r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":3600,"earliest_refresh_at":1}"#,
                )
            };
            Ok(reply)
        };
        let config = OAuthConfig::default();
        let tokens = refresh_tokens(&http, &config, "oaiapp_1", "rt-1").unwrap();
        assert_eq!((tokens.access_token.as_deref(), tokens.token_type.as_str()), (Some("at-2"), "Bearer"));
        assert_eq!(tokens.extra["earliest_refresh_at"], 1);
        assert!(!format!("{tokens:?}").contains("at-2"));
        revoke(&http, &config, "oaiapp_1", "rt-1").unwrap();
    }
}
