//! The client: credentials, a backend, and an HTTP stack, behind two calls.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{ApiError, Error, Result};
use crate::http::{HttpClient, HttpRequest, HttpResponse};
use crate::models::{parse_models, Model};
use crate::responses::{ResponseRequest, WireRequest};
use crate::stream::{Completion, ResponseStream};

/// The `client_version` the Codex backend's model list was read with on 2026-10-03. The list
/// leaves out models newer than the version asks for, and is empty for one far behind.
pub const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.160.0";

/// Where requests go. The credentials decide which: each kind of token works on one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// The public Responses API at `https://api.openai.com/v1`, for tokens from Sign in with
    /// ChatGPT that carry `chatgpt.tokens.use.direct`. The documented route.
    Api,
    /// The Codex backend at `https://chatgpt.com/backend-api/codex`, for the tokens the Codex CLI
    /// keeps after `codex login`. Those tokens are refused by the public API.
    Codex,
}

impl Backend {
    /// The base URL requests are sent under.
    pub fn base_url(self) -> &'static str {
        match self {
            Backend::Api => "https://api.openai.com/v1",
            Backend::Codex => "https://chatgpt.com/backend-api/codex",
        }
    }
}

/// An access token, and the ChatGPT account it is for when the backend needs to know.
#[derive(Clone, PartialEq, Eq)]
pub struct Bearer {
    /// The OAuth access token.
    pub access_token: String,
    /// The ChatGPT account id, sent as `chatgpt-account-id` to the Codex backend.
    pub account_id: Option<String>,
}

impl Bearer {
    /// A bearer of `access_token`, with no account id.
    pub fn new(access_token: impl Into<String>) -> Bearer {
        Bearer { access_token: access_token.into(), account_id: None }
    }
}

impl fmt::Debug for Bearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bearer").field("access_token", &"<redacted>").field("account_id", &self.account_id).finish()
    }
}

/// A source of access tokens: a saved sign-in, the Codex CLI's sign-in, or a token you manage.
pub trait Credential: Send + Sync {
    /// The backend these credentials work on.
    fn backend(&self) -> Backend;

    /// A current access token. Refresh it first if it has expired.
    fn bearer(&self, http: &dyn HttpClient) -> Result<Bearer>;

    /// The backend refused `rejected` with HTTP 401. Return another token to retry once with,
    /// for example after a refresh, or the error that explains why there is none.
    fn after_rejection(&self, http: &dyn HttpClient, rejected: &Bearer) -> Result<Bearer>;
}

/// A token you obtained and refresh yourself.
#[derive(Debug, Clone)]
pub struct AccessToken {
    backend: Backend,
    bearer: Bearer,
}

impl AccessToken {
    /// An access token for `backend`.
    pub fn new(backend: Backend, access_token: impl Into<String>) -> AccessToken {
        AccessToken { backend, bearer: Bearer::new(access_token) }
    }

    /// The same token, with the ChatGPT account id the Codex backend wants.
    pub fn account_id(mut self, account_id: impl Into<String>) -> AccessToken {
        self.bearer.account_id = Some(account_id.into());
        self
    }
}

impl Credential for AccessToken {
    fn backend(&self) -> Backend {
        self.backend
    }

    fn bearer(&self, _: &dyn HttpClient) -> Result<Bearer> {
        Ok(self.bearer.clone())
    }

    fn after_rejection(&self, _: &dyn HttpClient, _: &Bearer) -> Result<Bearer> {
        Err(Error::SignInRequired { reason: "the access token was rejected".into() })
    }
}

/// Runs requests on the user's ChatGPT plan.
///
/// Cheap to clone: clones share the HTTP client and the credentials.
///
/// ```no_run
/// use chatgpt_sdk::{Client, CodexAuth, InputItem, ReasoningEffort, ResponseRequest};
///
/// # fn main() -> chatgpt_sdk::Result<()> {
/// let client = Client::builder(CodexAuth::open_default()?).originator("my_app").build();
/// let request = ResponseRequest::new("gpt-6-luna")
///     .input(InputItem::user("Say hello."))
///     .reasoning_effort(ReasoningEffort::None);
/// println!("{}", client.create(&request)?.text);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Client {
    http: Arc<dyn HttpClient>,
    credential: Arc<dyn Credential>,
    base_url: String,
    user_agent: String,
    originator: Option<String>,
    client_version: String,
    timeout: Option<Duration>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("backend", &self.backend())
            .field("base_url", &self.base_url)
            .field("originator", &self.originator)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client with the default HTTP stack.
    #[cfg(feature = "ureq")]
    pub fn new(credential: impl Credential + 'static) -> Client {
        Client::builder(credential).build()
    }

    /// Configure a client.
    pub fn builder(credential: impl Credential + 'static) -> ClientBuilder {
        ClientBuilder {
            credential: Arc::new(credential),
            http: None,
            base_url: None,
            user_agent: None,
            originator: None,
            client_version: DEFAULT_CODEX_CLIENT_VERSION.into(),
            timeout: None,
        }
    }

    /// The backend requests go to.
    pub fn backend(&self) -> Backend {
        self.credential.backend()
    }

    /// The same client with a different timeout for each whole request, stream included.
    pub fn with_timeout(&self, timeout: Duration) -> Client {
        Client { timeout: Some(timeout), ..self.clone() }
    }

    /// The models the account can use. Show those that are [`Model::is_listed`] in a picker.
    pub fn models(&self) -> Result<Vec<Model>> {
        let url = match self.backend() {
            Backend::Api => format!("{}/models", self.base_url),
            Backend::Codex => format!("{}/models?client_version={}", self.base_url, self.client_version),
        };
        let reply =
            self.send(|bearer| self.authorized(HttpRequest::get(&url).header("accept", "application/json"), bearer))?;
        let (status, request_id) = (reply.status, reply.request_id());
        let body = reply.text()?;
        if !(200..300).contains(&status) {
            return Err(ApiError::from_reply(status, body, request_id).into());
        }
        parse_models(&body)
    }

    /// Start a response and return its stream of events. The request is sent with
    /// `store: false` and `stream: true`, as both backends require.
    pub fn stream(&self, request: &ResponseRequest) -> Result<ResponseStream> {
        let body = serde_json::to_vec(&WireRequest { request, store: false, stream: true })
            .map_err(|error| Error::json("serialising the request", error))?;
        let url = format!("{}/responses", self.base_url);
        let reply = self.send(|bearer| {
            let request = HttpRequest::post(&url, body.clone())
                .header("content-type", "application/json")
                .header("accept", "text/event-stream");
            self.authorized(request, bearer)
        })?;
        let request_id = reply.request_id();
        if !reply.is_success() {
            let status = reply.status;
            return Err(ApiError::from_reply(status, reply.text()?, request_id).into());
        }
        Ok(ResponseStream::new(reply.body, request_id))
    }

    /// Run a request to completion and return the response.
    pub fn create(&self, request: &ResponseRequest) -> Result<Completion> {
        self.stream(request)?.into_completion()
    }

    fn authorized(&self, request: HttpRequest, bearer: &Bearer) -> HttpRequest {
        let mut request = request
            .header("authorization", format!("Bearer {}", bearer.access_token))
            .header("user-agent", &self.user_agent)
            .timeout(self.timeout);
        if let Some(originator) = &self.originator {
            request = request.header("originator", originator);
        }
        if let (Backend::Codex, Some(account)) = (self.backend(), &bearer.account_id) {
            request = request.header("chatgpt-account-id", account);
        }
        request
    }

    /// Send with the current token; after a 401, once more with whatever the credentials offer.
    fn send(&self, build: impl Fn(&Bearer) -> HttpRequest) -> Result<HttpResponse> {
        let bearer = self.credential.bearer(&*self.http)?;
        let reply = self.http.send(build(&bearer))?;
        if reply.status != 401 {
            return Ok(reply);
        }
        let fresh = self.credential.after_rejection(&*self.http, &bearer)?;
        Ok(self.http.send(build(&fresh))?)
    }
}

/// Builds a [`Client`].
pub struct ClientBuilder {
    credential: Arc<dyn Credential>,
    http: Option<Arc<dyn HttpClient>>,
    base_url: Option<String>,
    user_agent: Option<String>,
    originator: Option<String>,
    client_version: String,
    timeout: Option<Duration>,
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("base_url", &self.base_url)
            .field("originator", &self.originator)
            .finish_non_exhaustive()
    }
}

impl ClientBuilder {
    /// Send requests through `http` instead of the default stack.
    pub fn http_client(mut self, http: impl HttpClient + 'static) -> ClientBuilder {
        self.http = Some(Arc::new(http));
        self
    }

    /// Send requests under another base URL, such as a test server's.
    pub fn base_url(mut self, base_url: impl Into<String>) -> ClientBuilder {
        self.base_url = Some(base_url.into().trim_end_matches('/').to_string());
        self
    }

    /// Identify the app: a stable name such as `my_app`, the same on every install, sent as the
    /// `originator` header and in the user agent.
    pub fn originator(mut self, name: impl Into<String>) -> ClientBuilder {
        self.originator = Some(name.into());
        self
    }

    /// Replace the user agent.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> ClientBuilder {
        self.user_agent = Some(user_agent.into());
        self
    }

    /// The client version to ask the Codex backend's model list for.
    pub fn client_version(mut self, version: impl Into<String>) -> ClientBuilder {
        self.client_version = version.into();
        self
    }

    /// The longest a whole request may take, a response's stream included. No timeout by default.
    pub fn timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.timeout = Some(timeout);
        self
    }

    /// The client.
    pub fn build(self) -> Client {
        let base_url = self.base_url.unwrap_or_else(|| self.credential.backend().base_url().to_string());
        let package = concat!("chatgpt-sdk/", env!("CARGO_PKG_VERSION"));
        let user_agent = self.user_agent.unwrap_or_else(|| match &self.originator {
            Some(app) => format!("{app} {package}"),
            None => package.to_string(),
        });
        Client {
            http: self.http.unwrap_or_else(default_http),
            credential: self.credential,
            base_url,
            user_agent,
            originator: self.originator,
            client_version: self.client_version,
            timeout: self.timeout,
        }
    }
}

#[cfg(feature = "ureq")]
fn default_http() -> Arc<dyn HttpClient> {
    Arc::new(crate::http::UreqClient::new())
}

#[cfg(not(feature = "ureq"))]
fn default_http() -> Arc<dyn HttpClient> {
    use crate::error::{TransportError, TransportErrorKind};
    Arc::new(|_: HttpRequest| -> std::result::Result<HttpResponse, TransportError> {
        Err(TransportError::new(
            TransportErrorKind::Other,
            "no HTTP client: enable the `ureq` feature or call `http_client`",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{TransportError, TransportErrorKind};
    use crate::{InputItem, ReasoningEffort};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    fn sse(events: &[Value]) -> String {
        events.iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap_or(""))).collect()
    }

    fn completed_stream() -> String {
        sse(&[
            json!({ "type": "response.output_text.delta", "item_id": "m", "output_index": 0, "content_index": 0, "delta": "Hello" }),
            json!({ "type": "response.completed", "response": { "id": "resp_1", "status": "completed", "output": [] } }),
        ])
    }

    type Seen = Arc<Mutex<Vec<HttpRequest>>>;

    fn recording(replies: Vec<HttpResponse>) -> (impl HttpClient, Seen) {
        let seen: Seen = Arc::default();
        let queue = Mutex::new(replies.into_iter());
        let log = seen.clone();
        let http = move |request: HttpRequest| {
            log.lock().unwrap().push(request);
            Ok(queue.lock().unwrap().next().expect("an unexpected request"))
        };
        (http, seen)
    }

    #[test]
    fn a_codex_request_carries_the_account_and_the_wire_flags() {
        let (http, seen) =
            recording(vec![HttpResponse::new(200, completed_stream()).with_header("x-oai-request-id", "req_7")]);
        let client = Client::builder(AccessToken::new(Backend::Codex, "at-1").account_id("acct-1"))
            .http_client(http)
            .originator("jli")
            .timeout(Duration::from_secs(9))
            .build();
        let request =
            ResponseRequest::new("gpt-6-luna").input(InputItem::user("hi")).reasoning_effort(ReasoningEffort::None);
        let completion = client.create(&request).unwrap();
        assert_eq!((completion.text.as_str(), completion.request_id.as_deref()), ("Hello", Some("req_7")));
        let sent = &seen.lock().unwrap()[0];
        assert_eq!(sent.url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(sent.header_value("authorization"), Some("Bearer at-1"));
        assert_eq!(sent.header_value("chatgpt-account-id"), Some("acct-1"));
        assert_eq!(sent.header_value("originator"), Some("jli"));
        assert_eq!(sent.header_value("user-agent"), Some(concat!("jli chatgpt-sdk/", env!("CARGO_PKG_VERSION"))));
        assert_eq!(sent.timeout, Some(Duration::from_secs(9)));
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!((body["store"].clone(), body["stream"].clone()), (json!(false), json!(true)));
        assert_eq!(body["reasoning"], json!({ "effort": "none" }));
    }

    #[test]
    fn the_api_backend_gets_no_account_header_and_its_own_model_list() {
        let (http, seen) = recording(vec![HttpResponse::new(
            200,
            r#"{"models":[{"slug":"gpt-6.1-sol","display_name":"GPT-6.1-Sol","visibility":"list"}]}"#,
        )]);
        let client = Client::builder(AccessToken::new(Backend::Api, "at-1").account_id("ignored")).http_client(http).build();
        let models = client.models().unwrap();
        assert_eq!(models[0].slug, "gpt-6.1-sol");
        let sent = &seen.lock().unwrap()[0];
        assert_eq!(sent.url, "https://api.openai.com/v1/models");
        assert_eq!(sent.header_value("chatgpt-account-id"), None);
        assert_eq!(sent.header_value("originator"), None);
    }

    #[test]
    fn the_codex_model_list_names_a_client_version() {
        let (http, seen) = recording(vec![HttpResponse::new(200, r#"{"models":[]}"#)]);
        let client =
            Client::builder(AccessToken::new(Backend::Codex, "at")).http_client(http).client_version("9.9.9").build();
        assert!(client.models().unwrap().is_empty());
        assert_eq!(seen.lock().unwrap()[0].url, "https://chatgpt.com/backend-api/codex/models?client_version=9.9.9");
    }

    struct Rotating {
        tokens: Mutex<Vec<&'static str>>,
    }

    impl Credential for Rotating {
        fn backend(&self) -> Backend {
            Backend::Api
        }
        fn bearer(&self, _: &dyn HttpClient) -> Result<Bearer> {
            Ok(Bearer::new(self.tokens.lock().unwrap()[0]))
        }
        fn after_rejection(&self, _: &dyn HttpClient, rejected: &Bearer) -> Result<Bearer> {
            let mut tokens = self.tokens.lock().unwrap();
            assert_eq!(rejected.access_token, tokens[0]);
            tokens.remove(0);
            tokens.first().map(|token| Bearer::new(*token)).ok_or(Error::SignInRequired { reason: "none left".into() })
        }
    }

    #[test]
    fn a_rejected_token_is_replaced_once() {
        let (http, seen) = recording(vec![
            HttpResponse::new(401, r#"{"detail":"Unauthorized"}"#),
            HttpResponse::new(200, completed_stream()),
        ]);
        let client = Client::builder(Rotating { tokens: Mutex::new(vec!["old", "new"]) }).http_client(http).build();
        assert_eq!(client.create(&ResponseRequest::new("m")).unwrap().text, "Hello");
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.iter().map(|request| request.header_value("authorization").unwrap()).collect::<Vec<_>>(),
            ["Bearer old", "Bearer new"]
        );

        let (http, _) =
            recording(vec![HttpResponse::new(401, "{}"), HttpResponse::new(401, r#"{"detail":"Unauthorized"}"#)]);
        let client = Client::builder(Rotating { tokens: Mutex::new(vec!["a", "b"]) }).http_client(http).build();
        let error = client.create(&ResponseRequest::new("m")).unwrap_err();
        assert_eq!(error.recovery(), crate::Recovery::SignInAgain, "a second 401 is the caller's: {error}");
    }

    #[test]
    fn error_replies_keep_status_code_and_request_id() {
        let body = r#"{"error":{"code":"subscription_sharing_user_not_eligible","message":"Not eligible","param":null}}"#;
        let (http, _) = recording(vec![HttpResponse::new(403, body).with_header("x-request-id", "req_3")]);
        let client = Client::builder(AccessToken::new(Backend::Api, "at")).http_client(http).build();
        let error = client.create(&ResponseRequest::new("m")).unwrap_err();
        assert_eq!(error.recovery(), crate::Recovery::NotEligible);
        assert_eq!(error.request_id(), Some("req_3"));
        let Error::Api(api) = error else { panic!("expected an API error") };
        assert_eq!((api.status, api.code.as_deref()), (403, Some("subscription_sharing_user_not_eligible")));
    }

    #[test]
    fn a_transport_failure_is_retryable() {
        let http = |_: HttpRequest| -> std::result::Result<HttpResponse, TransportError> {
            Err(TransportError::new(TransportErrorKind::Timeout, "timed out"))
        };
        let client =
            Client::builder(AccessToken::new(Backend::Api, "at")).http_client(http).base_url("http://127.0.0.1:9/").build();
        let error = client.models().unwrap_err();
        assert_eq!(error.recovery(), crate::Recovery::RetryLater);
        assert!(format!("{client:?}").contains("http://127.0.0.1:9"));
    }
}
