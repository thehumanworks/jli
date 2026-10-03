//! Errors, and what to do about each one.
//!
//! Every failure keeps what the server said: the HTTP status, the error code when there is one, and
//! the request id. [`Error::recovery`] maps a failure to the action the Sign in with ChatGPT
//! documentation prescribes for it, so an app can decide between retrying, asking the user to sign
//! in again, and sending them to their usage settings without matching on strings.

use std::fmt;

use serde_json::Value;

/// The page where a user reviews and manages what apps use of their ChatGPT plan.
pub const MANAGE_USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// A `Result` whose error is this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Anything that can go wrong signing in, refreshing, or running a request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The request did not get an HTTP reply: a connection failure, a timeout, or a broken stream.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The server replied with a status outside 2xx.
    #[error(transparent)]
    Api(Box<ApiError>),
    /// A response stream ended in failure, or without saying it was complete.
    #[error(transparent)]
    Response(Box<ResponseError>),
    /// The token endpoint refused a code exchange, a refresh, or a revocation.
    #[error(transparent)]
    OAuth(Box<OAuthError>),
    /// The browser sign-in did not complete.
    #[error(transparent)]
    SignIn(#[from] SignInError),
    /// An ID token failed validation.
    #[error(transparent)]
    IdToken(#[from] IdTokenError),
    /// The saved session can no longer be used: the user has to sign in again.
    #[error("sign in again: {reason}")]
    SignInRequired {
        /// Why the session cannot be used.
        reason: String,
    },
    /// The user signed in, but did not allow this app to use their ChatGPT plan.
    #[error("ChatGPT plan usage is not enabled for this sign-in")]
    PlanUsageNotEnabled,
    /// A local file could not be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done, such as the path being written.
        context: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A reply or a saved file was not the JSON expected.
    #[error("{context}: {source}")]
    Json {
        /// What was being read.
        context: String,
        /// The underlying error.
        source: serde_json::Error,
    },
    /// The server or a saved file said something this crate cannot use.
    #[error("{0}")]
    Protocol(String),
}

impl From<ApiError> for Error {
    fn from(error: ApiError) -> Error {
        Error::Api(Box::new(error))
    }
}

impl From<ResponseError> for Error {
    fn from(error: ResponseError) -> Error {
        Error::Response(Box::new(error))
    }
}

impl From<OAuthError> for Error {
    fn from(error: OAuthError) -> Error {
        Error::OAuth(Box::new(error))
    }
}

impl Error {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io { context: context.into(), source }
    }

    pub(crate) fn json(context: impl Into<String>, source: serde_json::Error) -> Error {
        Error::Json { context: context.into(), source }
    }

    /// What the app should do about this error.
    pub fn recovery(&self) -> Recovery {
        match self {
            Error::Transport(_) => Recovery::RetryLater,
            Error::Api(error) => error.recovery(),
            Error::Response(error) => error.recovery(),
            Error::OAuth(error) => error.recovery(),
            Error::SignIn(_) | Error::IdToken(_) | Error::SignInRequired { .. } => Recovery::SignInAgain,
            Error::PlanUsageNotEnabled => Recovery::EnablePlanUsage,
            Error::Io { .. } | Error::Json { .. } | Error::Protocol(_) => Recovery::None,
        }
    }

    /// The request id the server gave the failed request, when it gave one.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Error::Api(error) => error.request_id.as_deref(),
            Error::Response(error) => error.request_id(),
            Error::OAuth(error) => error.request_id.as_deref(),
            _ => None,
        }
    }
}

/// The action the documentation prescribes for an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Recovery {
    /// A temporary failure. Keep the credentials and retry later, with bounded backoff.
    RetryLater,
    /// The session is gone: ask the user to sign in again.
    SignInAgain,
    /// A usage limit was reached. Pause requests on the plan and send the user to
    /// [`MANAGE_USAGE_URL`]. Do not infer when the limit resets.
    ManageUsage,
    /// The user, workspace or policy cannot use a ChatGPT plan here. Explain it; do not retry or
    /// loop through sign-in.
    NotEligible,
    /// The user did not allow plan usage. Offer to sign in again with it, or another way to pay.
    EnablePlanUsage,
    /// The request asked for something the route does not support. Change it; do not resend it.
    FixRequest,
    /// The client or grant is misconfigured. Retrying will not help.
    CheckConfiguration,
    /// Nothing specific: report the error.
    None,
}

/// What kind of transport failure happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TransportErrorKind {
    /// The request or the stream took longer than its timeout.
    Timeout,
    /// No connection could be made.
    Connect,
    /// Anything else, such as a stream that broke off.
    Other,
}

/// A request that got no HTTP reply.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    /// What kind of failure it was.
    pub kind: TransportErrorKind,
    /// A description from the HTTP client.
    pub message: String,
}

impl TransportError {
    /// A transport error of `kind`.
    pub fn new(kind: TransportErrorKind, message: impl Into<String>) -> TransportError {
        TransportError { kind, message: message.into() }
    }
}

/// A reply with a status outside 2xx, with everything the server said about it.
///
/// The ChatGPT plan routes answer in more than one shape: the Responses API's
/// `{"error": {"code", "message", "param", "type"}}`, and an admission layer's `{"detail": "…"}`,
/// which has no code. Both are read into the same fields; the body is kept as it was.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ApiError {
    /// The HTTP status.
    pub status: u16,
    /// The machine-readable `error.code`, when the reply had one.
    pub code: Option<String>,
    /// The `error.type`, when the reply had one.
    pub kind: Option<String>,
    /// The `error.param`, when the reply had one.
    pub param: Option<String>,
    /// The human-readable message: `error.message`, `detail`, or the body itself.
    pub message: String,
    /// The request id from the reply's headers.
    pub request_id: Option<String>,
    /// The reply body, as received.
    pub body: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {}", self.status)?;
        if let Some(code) = self.code.as_deref().or(self.kind.as_deref()) {
            write!(f, " {code}")?;
        }
        write!(f, ": {}", self.message)?;
        if let Some(id) = &self.request_id {
            write!(f, " (request {id})")?;
        }
        Ok(())
    }
}

fn text_at(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(Value::as_str).map(str::to_string)
}

impl ApiError {
    /// Read an error reply. Any body is accepted: one that is not JSON becomes the message.
    pub fn from_reply(status: u16, body: impl Into<String>, request_id: Option<String>) -> ApiError {
        let body = body.into();
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let message = text_at(&parsed, "/error/message")
            .or_else(|| text_at(&parsed, "/detail"))
            .or_else(|| text_at(&parsed, "/error"))
            .or_else(|| text_at(&parsed, "/message"))
            .unwrap_or_else(|| body.trim().chars().take(500).collect());
        ApiError {
            status,
            code: text_at(&parsed, "/error/code").or_else(|| text_at(&parsed, "/code")),
            kind: text_at(&parsed, "/error/type"),
            param: text_at(&parsed, "/error/param"),
            message,
            request_id,
            body,
        }
    }

    /// What to do about this reply, from its code first and its status second.
    pub fn recovery(&self) -> Recovery {
        let by_code = self.code.as_deref().and_then(recovery_for_code);
        let by_kind = self.kind.as_deref().and_then(recovery_for_code);
        by_code.or(by_kind).unwrap_or(match self.status {
            401 => Recovery::SignInAgain,
            403 => Recovery::CheckConfiguration,
            408 | 409 | 429 | 500 | 502 | 503 | 504 => Recovery::RetryLater,
            400 | 404 | 413 | 422 => Recovery::FixRequest,
            _ => Recovery::None,
        })
    }

    /// Whether a usage limit was reached: the user's plan, or this app's share of it.
    pub fn is_usage_limit(&self) -> bool {
        self.recovery() == Recovery::ManageUsage
    }
}

/// The documented recovery for an error code, from either ChatGPT plan route.
pub(crate) fn recovery_for_code(code: &str) -> Option<Recovery> {
    Some(match code {
        "subscription_sharing_usage_limit_exceeded" | "usage_limit_reached" => Recovery::ManageUsage,
        "subscription_sharing_user_not_eligible" | "usage_not_included" => Recovery::NotEligible,
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => Recovery::RetryLater,
        "subscription_sharing_unsupported_capability" | "subscription_sharing_route_not_supported" => Recovery::FixRequest,
        "subscription_sharing_invalid_user" => Recovery::SignInAgain,
        "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => Recovery::CheckConfiguration,
        "rate_limit_exceeded" | "server_error" => Recovery::RetryLater,
        _ => return None,
    })
}

/// A response stream that did not end in `response.completed`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ResponseError {
    /// The stream ended with `response.failed`.
    #[error("the response failed{}: {message}", code.as_ref().map(|code| format!(" ({code})")).unwrap_or_default())]
    Failed {
        /// The response's id.
        response_id: String,
        /// `response.error.code`, such as `subscription_sharing_usage_limit_exceeded`.
        code: Option<String>,
        /// `response.error.message`.
        message: String,
        /// The request id from the reply's headers.
        request_id: Option<String>,
    },
    /// The stream ended with `response.incomplete`, for example at an output limit.
    #[error("the response is incomplete: {}", reason.as_deref().unwrap_or("no reason given"))]
    Incomplete {
        /// The response's id.
        response_id: String,
        /// `response.incomplete_details.reason`.
        reason: Option<String>,
        /// The text generated before the response stopped.
        partial_text: String,
        /// The request id from the reply's headers.
        request_id: Option<String>,
    },
    /// The stream carried an `error` event.
    #[error("the stream reported an error{}: {message}", code.as_ref().map(|code| format!(" ({code})")).unwrap_or_default())]
    Stream {
        /// The event's `code`.
        code: Option<String>,
        /// The event's `message`.
        message: String,
        /// The event's `param`.
        param: Option<String>,
        /// The request id from the reply's headers.
        request_id: Option<String>,
    },
    /// The stream closed before a terminal event. Inference is not complete.
    #[error("the stream ended before response.completed")]
    EndedEarly {
        /// The text received before the stream closed.
        partial_text: String,
        /// The request id from the reply's headers.
        request_id: Option<String>,
    },
}

impl ResponseError {
    /// What to do about this failure.
    pub fn recovery(&self) -> Recovery {
        match self {
            ResponseError::Failed { code, .. } | ResponseError::Stream { code, .. } => {
                code.as_deref().and_then(recovery_for_code).unwrap_or(Recovery::None)
            }
            ResponseError::Incomplete { .. } => Recovery::FixRequest,
            ResponseError::EndedEarly { .. } => Recovery::RetryLater,
        }
    }

    /// The request id from the reply's headers.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            ResponseError::Failed { request_id, .. }
            | ResponseError::Incomplete { request_id, .. }
            | ResponseError::Stream { request_id, .. }
            | ResponseError::EndedEarly { request_id, .. } => request_id.as_deref(),
        }
    }
}

/// An OAuth error from the token or revocation endpoint (RFC 6749, section 5.2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{error}{}{}", description.as_ref().map(|text| format!(": {text}")).unwrap_or_default(), format!(" (HTTP {status})"))]
pub struct OAuthError {
    /// The HTTP status.
    pub status: u16,
    /// The `error` code, such as `invalid_grant`.
    pub error: String,
    /// The `error_description`, when there is one.
    pub description: Option<String>,
    /// The request id from the reply's headers.
    pub request_id: Option<String>,
}

/// Refresh error codes that mean the refresh token can no longer be used.
const UNUSABLE_REFRESH: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

impl OAuthError {
    /// Read an error reply from the token endpoint. A body without an `error` field is kept as the
    /// description, with the error `http_<status>`.
    pub fn from_reply(status: u16, body: &str, request_id: Option<String>) -> OAuthError {
        let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        // Some replies nest the code: {"error": {"code": "refresh_token_expired", "message": "…"}}.
        let error = text_at(&parsed, "/error")
            .or_else(|| text_at(&parsed, "/error/code"))
            .or_else(|| text_at(&parsed, "/code"))
            .unwrap_or_else(|| format!("http_{status}"));
        let description = text_at(&parsed, "/error_description")
            .or_else(|| text_at(&parsed, "/error/message"))
            .or_else(|| text_at(&parsed, "/detail"))
            .or_else(|| (!body.trim().is_empty() && parsed.is_null()).then(|| body.trim().chars().take(300).collect()));
        OAuthError { status, error, description, request_id }
    }

    /// Whether this refresh error means the refresh token is unusable and the user must sign in.
    pub fn refresh_token_unusable(&self) -> bool {
        UNUSABLE_REFRESH.contains(&self.error.as_str())
    }

    /// What to do about this error.
    pub fn recovery(&self) -> Recovery {
        if self.refresh_token_unusable() {
            Recovery::SignInAgain
        } else if self.error == "invalid_client" || self.error == "unauthorized_client" {
            Recovery::CheckConfiguration
        } else if self.status >= 500 || self.status == 429 {
            Recovery::RetryLater
        } else {
            Recovery::None
        }
    }
}

/// A browser sign-in that did not complete.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SignInError {
    /// The user declined: the callback carried `error=access_denied`.
    #[error("the sign-in was declined")]
    Denied,
    /// The callback carried another OAuth error.
    #[error("the sign-in failed: {error}{}", description.as_ref().map(|text| format!(": {text}")).unwrap_or_default())]
    Authorization {
        /// The `error` code.
        error: String,
        /// The `error_description`, when there is one.
        description: Option<String>,
    },
    /// The callback's `state` is not the one this attempt sent.
    #[error("the sign-in callback does not belong to this attempt (state mismatch)")]
    StateMismatch,
    /// The callback had no authorization code.
    #[error("the sign-in callback has no authorization code")]
    MissingCode,
    /// A new registration's callback did not carry the issued client id.
    #[error("the registration did not complete: the callback has no issued client_id")]
    RegistrationIncomplete,
    /// A reauthorization's callback named a client other than the saved one.
    #[error("the callback names client {returned}, not the saved client {expected}")]
    ClientMismatch {
        /// The saved client id.
        expected: String,
        /// The client id in the callback.
        returned: String,
    },
    /// The token response had no ID token.
    #[error("the token response has no ID token")]
    MissingIdToken,
    /// The token response had no access token.
    #[error("the token response has no access token")]
    MissingAccessToken,
    /// A returning sign-in came back as a different ChatGPT account.
    #[error("signed in as a different ChatGPT account than the one selected")]
    AccountMismatch,
    /// No callback arrived before the deadline.
    #[error("no sign-in callback arrived in time")]
    TimedOut,
}

/// Why an ID token was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdTokenError {
    /// It is not a three-part JWT with JSON header and claims.
    #[error("the ID token is malformed: {0}")]
    Malformed(String),
    /// It is signed with an algorithm other than RS256.
    #[error("the ID token uses the unsupported algorithm {0}")]
    UnsupportedAlgorithm(String),
    /// No key in the JWKS matches its `kid`.
    #[error("no published key matches the ID token's key id {}", .0.as_deref().unwrap_or("(none)"))]
    UnknownKey(Option<String>),
    /// The signature does not verify.
    #[error("the ID token's signature does not verify")]
    BadSignature,
    /// The issuer is not the expected one.
    #[error("the ID token was issued by {found}, not {expected}")]
    Issuer {
        /// The expected issuer.
        expected: String,
        /// The `iss` claim.
        found: String,
    },
    /// The audience does not include the client id.
    #[error("the ID token is not for client {expected}")]
    Audience {
        /// The client id that should be in `aud`.
        expected: String,
    },
    /// It has expired.
    #[error("the ID token has expired")]
    Expired,
    /// Its nonce is not the one this sign-in sent.
    #[error("the ID token's nonce does not match this sign-in")]
    Nonce,
    /// A required claim is missing or empty.
    #[error("the ID token has no {0} claim")]
    MissingClaim(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_responses_error_keeps_its_code_and_request_id() {
        let body = r#"{"error":{"message":"Limit reached","type":"usage_limit","param":null,"code":"subscription_sharing_usage_limit_exceeded"}}"#;
        let error = ApiError::from_reply(429, body, Some("req_1".into()));
        assert_eq!(error.code.as_deref(), Some("subscription_sharing_usage_limit_exceeded"));
        assert_eq!(error.message, "Limit reached");
        assert_eq!(error.recovery(), Recovery::ManageUsage, "a usage limit is not a rate limit");
        assert!(error.is_usage_limit());
        assert_eq!(error.to_string(), "HTTP 429 subscription_sharing_usage_limit_exceeded: Limit reached (request req_1)");
    }

    #[test]
    fn an_admission_error_has_detail_and_no_code() {
        let error = ApiError::from_reply(400, r#"{"detail":"Stream must be set to true"}"#, None);
        assert_eq!((error.code.as_deref(), error.message.as_str()), (None, "Stream must be set to true"));
        assert_eq!(error.recovery(), Recovery::FixRequest);
        assert_eq!(ApiError::from_reply(401, r#"{"detail":"Unauthorized"}"#, None).recovery(), Recovery::SignInAgain);
        assert_eq!(ApiError::from_reply(503, "upstream busy", None).message, "upstream busy");
        assert_eq!(ApiError::from_reply(503, "", None).recovery(), Recovery::RetryLater);
    }

    #[test]
    fn documented_codes_map_to_their_recovery() {
        for (code, status, recovery) in [
            ("subscription_sharing_user_not_eligible", 403, Recovery::NotEligible),
            ("subscription_sharing_usage_unavailable", 503, Recovery::RetryLater),
            ("subscription_sharing_unsupported_capability", 400, Recovery::FixRequest),
            ("subscription_sharing_route_not_supported", 403, Recovery::FixRequest),
            ("subscription_sharing_invalid_user", 401, Recovery::SignInAgain),
            ("chatpass_v2_scope_not_authorized", 403, Recovery::CheckConfiguration),
            ("chatpass_v2_invalid_authorization_context", 403, Recovery::CheckConfiguration),
            ("subscription_sharing_user_unavailable", 503, Recovery::RetryLater),
        ] {
            let body = format!(r#"{{"error":{{"code":"{code}","message":"m"}}}}"#);
            assert_eq!(ApiError::from_reply(status, body, None).recovery(), recovery, "{code}");
        }
        let codex_limit = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached"}}"#;
        assert_eq!(ApiError::from_reply(429, codex_limit, None).recovery(), Recovery::ManageUsage);
    }

    #[test]
    fn unusable_refresh_tokens_need_a_new_sign_in() {
        let reused = OAuthError::from_reply(400, r#"{"error":"refresh_token_reused","error_description":"used"}"#, None);
        assert!(reused.refresh_token_unusable());
        assert_eq!(Error::from(reused.clone()).recovery(), Recovery::SignInAgain);
        assert_eq!(reused.to_string(), "refresh_token_reused: used (HTTP 400)");
        let nested = OAuthError::from_reply(401, r#"{"error":{"code":"refresh_token_expired","message":"old"}}"#, None);
        assert_eq!((nested.error.as_str(), nested.description.as_deref()), ("refresh_token_expired", Some("old")));
        let client = OAuthError::from_reply(401, r#"{"error":"invalid_client"}"#, None);
        assert_eq!(client.recovery(), Recovery::CheckConfiguration);
        let plain = OAuthError::from_reply(502, "Bad gateway", None);
        assert_eq!((plain.error.as_str(), plain.recovery()), ("http_502", Recovery::RetryLater));
    }

    #[test]
    fn a_failed_response_maps_by_its_code() {
        let failed = ResponseError::Failed {
            response_id: "resp_1".into(),
            code: Some("subscription_sharing_usage_limit_exceeded".into()),
            message: "limit".into(),
            request_id: Some("req_9".into()),
        };
        assert_eq!(failed.recovery(), Recovery::ManageUsage);
        assert_eq!(Error::from(failed).request_id(), Some("req_9"));
        let ended = ResponseError::EndedEarly { partial_text: String::new(), request_id: None };
        assert_eq!(ended.recovery(), Recovery::RetryLater);
    }
}
