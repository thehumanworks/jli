//! One account registration's saved credentials, in the documented record format.

use std::fmt;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::auth::{refresh_tokens, verify_id_token, Jwks, OAuthConfig, Validation, PLAN_USAGE_SCOPE};
use crate::error::{Error, Result, SignInError};
use crate::http::HttpClient;

/// An access token's lifetime when the token response does not give one.
pub(crate) const DEFAULT_LIFETIME: u64 = 3600;

/// How long before expiry an access token is refreshed.
pub const REFRESH_MARGIN: Duration = Duration::minutes(5);

/// The saved sign-in of one ChatGPT account registration on this host.
///
/// Serialised, it is the record the documentation describes, with tokens left out once signed
/// out. Unknown fields are kept, so a record written by a newer version survives a rewrite.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    /// The email from the validated ID token, for display and as a login hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The ID token's issuer.
    pub issuer: String,
    /// The validated ID token's `sub`: the account's identity.
    pub subject: String,
    /// The client id issued for this registration.
    pub client_id: String,
    /// The host id sent when signing in.
    pub ext_agent_host_id: String,
    /// The ID token from the last sign-in, kept as a hint for the next. Cleared at sign-out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// The access token. Cleared at sign-out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// The refresh token. Cleared at sign-out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// `Bearer`.
    #[serde(default = "bearer")]
    pub token_type: String,
    /// The access token's lifetime, in seconds from `saved_at`.
    #[serde(default)]
    pub expires_in: u64,
    /// The granted scopes.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// When the token response was received.
    pub saved_at: DateTime<Utc>,
    /// Fields this version does not know, kept as they were.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn bearer() -> String {
    "Bearer".into()
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hidden = |token: &Option<String>| token.as_ref().map(|_| "<redacted>");
        f.debug_struct("Credentials")
            .field("email", &self.email)
            .field("subject", &self.subject)
            .field("client_id", &self.client_id)
            .field("ext_agent_host_id", &self.ext_agent_host_id)
            .field("id_token", &hidden(&self.id_token))
            .field("access_token", &hidden(&self.access_token))
            .field("refresh_token", &hidden(&self.refresh_token))
            .field("expires_in", &self.expires_in)
            .field("scopes", &self.scopes)
            .field("saved_at", &self.saved_at)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Whether the user allowed this app to use their ChatGPT plan. Without it, the sign-in may
    /// not be used for inference.
    pub fn plan_usage_enabled(&self) -> bool {
        self.scopes.iter().any(|scope| scope == PLAN_USAGE_SCOPE)
    }

    /// Whether there is a session to use or renew.
    pub fn is_signed_in(&self) -> bool {
        self.access_token.is_some() || self.refresh_token.is_some()
    }

    /// When the access token expires.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.saved_at + Duration::seconds(i64::try_from(self.expires_in).unwrap_or(i64::MAX / 2).min(i64::MAX / 2))
    }

    /// Whether the access token is missing, or expires within [`REFRESH_MARGIN`] of `now`.
    pub fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        self.access_token.is_none() || now + REFRESH_MARGIN >= self.expires_at()
    }

    /// The same registration with every token cleared, as kept after sign-out: the client id and
    /// host id stay for the next sign-in.
    pub fn signed_out(&self) -> Credentials {
        Credentials { id_token: None, access_token: None, refresh_token: None, expires_in: 0, ..self.clone() }
    }

    /// Refresh the access token. The access token, its expiry, the scopes and the rotating
    /// refresh token are replaced together. An unusable refresh token is
    /// [`Error::SignInRequired`].
    pub fn refreshed(&self, http: &dyn HttpClient, config: &OAuthConfig) -> Result<Credentials> {
        let refresh_token = self
            .refresh_token
            .as_deref()
            .ok_or_else(|| Error::SignInRequired { reason: "no refresh token is saved; sign in again".into() })?;
        let tokens = refresh_tokens(http, config, &self.client_id, refresh_token).map_err(|error| match error {
            Error::OAuth(oauth) if oauth.refresh_token_unusable() => Error::SignInRequired { reason: oauth.to_string() },
            other => other,
        })?;
        let access_token = tokens.access_token.ok_or(SignInError::MissingAccessToken)?;
        // The server has already spent the old refresh token, so nothing after this point may stop
        // the new ones from being returned. A refreshed ID token only serves as the next sign-in's
        // hint: it replaces the old one when it verifies as the same account, and is dropped
        // otherwise, as when the key set cannot be fetched.
        let id_token = match tokens.id_token {
            Some(id_token) if self.is_same_account(http, config, &id_token) => Some(id_token),
            _ => self.id_token.clone(),
        };
        Ok(Credentials {
            id_token,
            access_token: Some(access_token),
            refresh_token: tokens.refresh_token.or_else(|| self.refresh_token.clone()),
            token_type: tokens.token_type,
            expires_in: tokens.expires_in.unwrap_or(DEFAULT_LIFETIME),
            scopes: tokens.scope.as_deref().map(crate::auth::split_scopes).unwrap_or_else(|| self.scopes.clone()),
            saved_at: Utc::now(),
            ..self.clone()
        })
    }

    /// Whether `id_token` verifies, without a nonce, as this registration's account.
    fn is_same_account(&self, http: &dyn HttpClient, config: &OAuthConfig, id_token: &str) -> bool {
        Jwks::fetch(http, &config.jwks_uri)
            .ok()
            .and_then(|jwks| verify_id_token(id_token, &jwks, &Validation::new(&config.issuer, &self.client_id)).ok())
            .is_some_and(|claims| claims.sub == self.subject)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> Value {
        json!({
            "email": "user@example.com",
            "issuer": "https://auth.openai.com",
            "subject": "user-1",
            "client_id": "oaiapp_1",
            "ext_agent_host_id": "urn:uuid:6f1c2d9e-8d4b-4c7a-9e1f-2b3c4d5e6f70",
            "id_token": "id.token.x",
            "access_token": "access.token.x",
            "refresh_token": "rt-1",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scopes": ["chatgpt.tokens.use.direct", "email", "offline_access", "openid", "profile", "resource.invoke"],
            "saved_at": "2026-10-03T12:00:00Z",
            "written_by": "a newer version",
        })
    }

    #[test]
    fn the_documented_record_round_trips_with_unknown_fields() {
        let credentials: Credentials = serde_json::from_value(record()).unwrap();
        assert!(credentials.plan_usage_enabled() && credentials.is_signed_in());
        assert_eq!(credentials.expires_at().to_rfc3339(), "2026-10-03T13:00:00+00:00");
        assert_eq!(serde_json::to_value(&credentials).unwrap(), record());
        let shown = format!("{credentials:?}");
        assert!(!shown.contains("access.token.x") && !shown.contains("rt-1") && shown.contains("oaiapp_1"));
    }

    #[test]
    fn refresh_is_due_five_minutes_before_expiry() {
        let credentials: Credentials = serde_json::from_value(record()).unwrap();
        let at = |text: &str| DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc);
        assert!(!credentials.needs_refresh(at("2026-10-03T12:54:59Z")));
        assert!(credentials.needs_refresh(at("2026-10-03T12:55:00Z")));
        let signed_out = credentials.signed_out();
        assert!(!signed_out.is_signed_in() && signed_out.needs_refresh(at("2026-10-03T12:00:00Z")));
        assert_eq!((signed_out.client_id.as_str(), signed_out.id_token.as_ref()), ("oaiapp_1", None));
        let written = serde_json::to_value(&signed_out).unwrap();
        assert!(written.get("access_token").is_none() && written.get("refresh_token").is_none());
    }

    #[test]
    fn a_refresh_rotates_the_refresh_token_and_keeps_the_rest() {
        let credentials: Credentials = serde_json::from_value(record()).unwrap();
        let http = |_: crate::HttpRequest| {
            Ok(crate::HttpResponse::new(
                200,
                r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":1800,"token_type":"Bearer"}"#,
            ))
        };
        let refreshed = credentials.refreshed(&http, &OAuthConfig::default()).unwrap();
        assert_eq!((refreshed.access_token.as_deref(), refreshed.refresh_token.as_deref()), (Some("at-2"), Some("rt-2")));
        assert_eq!((refreshed.expires_in, refreshed.scopes.len()), (1800, 6), "scopes stay when none are returned");
        assert_eq!(refreshed.id_token.as_deref(), Some("id.token.x"));
        assert!(refreshed.saved_at > credentials.saved_at);

        // A new ID token that cannot be checked never costs the rotated tokens.
        let unverifiable = |request: crate::HttpRequest| {
            Ok(match request.url.ends_with("/jwks.json") {
                true => crate::HttpResponse::new(503, "down"),
                false => {
                    crate::HttpResponse::new(200, r#"{"access_token":"at-3","refresh_token":"rt-3","id_token":"a.b.c"}"#)
                }
            })
        };
        let kept = credentials.refreshed(&unverifiable, &OAuthConfig::default()).unwrap();
        assert_eq!((kept.refresh_token.as_deref(), kept.id_token.as_deref()), (Some("rt-3"), Some("id.token.x")));
        let key = crate::auth::id_token::tests::TestKey::load();
        let now = Utc::now().timestamp();
        let same = key.token(
            &json!({ "iss": "https://auth.openai.com", "sub": "user-1", "aud": "oaiapp_1", "iat": now, "exp": now + 60 }),
        );
        let reply = json!({ "access_token": "at-4", "refresh_token": "rt-4", "id_token": same }).to_string();
        let jwks = key.jwks_value().to_string();
        let verifiable = move |request: crate::HttpRequest| {
            Ok(crate::HttpResponse::new(200, if request.url.ends_with("/jwks.json") { jwks.clone() } else { reply.clone() }))
        };
        assert_eq!(credentials.refreshed(&verifiable, &OAuthConfig::default()).unwrap().id_token, Some(same));

        let reused = |_: crate::HttpRequest| Ok(crate::HttpResponse::new(400, r#"{"error":"refresh_token_reused"}"#));
        let error = credentials.refreshed(&reused, &OAuthConfig::default()).unwrap_err();
        assert!(matches!(error, Error::SignInRequired { .. }), "{error}");
        let busy = |_: crate::HttpRequest| Ok(crate::HttpResponse::new(503, "busy"));
        assert!(matches!(credentials.refreshed(&busy, &OAuthConfig::default()).unwrap_err(), Error::OAuth(_)));
        assert!(matches!(
            credentials.signed_out().refreshed(&busy, &OAuthConfig::default()).unwrap_err(),
            Error::SignInRequired { .. }
        ));
    }
}
