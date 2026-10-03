//! The HTTP seam. Every request this crate makes goes through an [`HttpClient`].
//!
//! [`UreqClient`] (feature `ureq`, on by default) is the blocking client used unless you supply
//! your own. Implement [`HttpClient`] to use another stack, to add a proxy, or to fake the network
//! in tests. A closure `Fn(HttpRequest) -> Result<HttpResponse, TransportError>` is an
//! [`HttpClient`] too.

use std::fmt;
use std::io::{Cursor, Read};
use std::time::Duration;

use crate::error::TransportError;

/// The HTTP methods this crate uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
}

impl Method {
    /// The method as written on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// One HTTP request.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// The method.
    pub method: Method,
    /// The absolute URL, query included.
    pub url: String,
    /// Header names and values, in order.
    pub headers: Vec<(String, String)>,
    /// The body; empty for a `GET`.
    pub body: Vec<u8>,
    /// The longest the whole exchange may take, body included. `None` leaves it to the client.
    pub timeout: Option<Duration>,
}

impl HttpRequest {
    /// A `GET` of `url`.
    pub fn get(url: impl Into<String>) -> HttpRequest {
        HttpRequest { method: Method::Get, url: url.into(), headers: vec![], body: vec![], timeout: None }
    }

    /// A `POST` of `body` to `url`.
    pub fn post(url: impl Into<String>, body: Vec<u8>) -> HttpRequest {
        HttpRequest { method: Method::Post, url: url.into(), headers: vec![], body, timeout: None }
    }

    /// The same request with one more header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> HttpRequest {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The same request with a timeout.
    pub fn timeout(mut self, timeout: Option<Duration>) -> HttpRequest {
        self.timeout = timeout;
        self
    }

    /// The value of the first header called `name`, in any case.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    /// The body as UTF-8 text, for tests and logs.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Debug output never shows credentials: `authorization` and bodies with tokens are redacted.
impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(name, value)| {
                let secret = name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("cookie");
                (name.as_str(), if secret { "<redacted>" } else { value.as_str() })
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// One HTTP reply. The body is a reader, so a stream is consumed as it arrives.
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// Header names, in lowercase, and values.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Box<dyn Read + Send>,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse").field("status", &self.status).field("headers", &self.headers).finish_non_exhaustive()
    }
}

/// Header names that carry a request id, in the order they are looked for.
const REQUEST_ID_HEADERS: [&str; 3] = ["x-request-id", "openai-request-id", "x-oai-request-id"];

/// The most of an error body or a JSON reply this crate reads into memory.
const MAX_BODY: u64 = 16 * 1024 * 1024;

impl HttpResponse {
    /// A reply with `status` and a body of `text`, as a fake transport would build it.
    pub fn new(status: u16, text: impl Into<String>) -> HttpResponse {
        HttpResponse { status, headers: vec![], body: Box::new(Cursor::new(text.into().into_bytes())) }
    }

    /// The same reply with one more header. The name is stored in lowercase.
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> HttpResponse {
        self.headers.push((name.to_ascii_lowercase(), value.into()));
        self
    }

    /// The value of the first header called `name`, in any case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    /// The request id the server gave this reply: `x-request-id`, `openai-request-id` or
    /// `x-oai-request-id`.
    pub fn request_id(&self) -> Option<String> {
        REQUEST_ID_HEADERS.iter().find_map(|name| self.header(name)).map(str::to_string)
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Read the whole body as text, up to 16 MiB. Invalid UTF-8 is replaced.
    pub fn text(self) -> Result<String, TransportError> {
        let mut bytes = Vec::new();
        self.body.take(MAX_BODY).read_to_end(&mut bytes).map_err(|error| read_error(&error))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// A transport error for a body that could not be read.
pub(crate) fn read_error(error: &std::io::Error) -> TransportError {
    use crate::error::TransportErrorKind;
    let kind = match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => TransportErrorKind::Timeout,
        _ => TransportErrorKind::Other,
    };
    TransportError::new(kind, format!("reading the reply failed: {error}"))
}

/// Sends HTTP requests. Implement it to use your own HTTP stack.
///
/// A reply with any status is `Ok`; `Err` is only for a request that got no reply.
pub trait HttpClient: Send + Sync {
    /// Send `request` and return the reply, whatever its status.
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

impl<F> HttpClient for F
where
    F: Fn(HttpRequest) -> Result<HttpResponse, TransportError> + Send + Sync,
{
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self(request)
    }
}

#[cfg(feature = "ureq")]
pub use self::ureq_client::UreqClient;

#[cfg(feature = "ureq")]
mod ureq_client {
    use super::{HttpClient, HttpRequest, HttpResponse, Method};
    use crate::error::{TransportError, TransportErrorKind};

    /// The default [`HttpClient`]: blocking, rustls, with a pooled connection per host.
    #[derive(Debug, Clone)]
    pub struct UreqClient {
        agent: ureq::Agent,
    }

    impl Default for UreqClient {
        fn default() -> Self {
            UreqClient::new()
        }
    }

    impl UreqClient {
        /// A client that returns every status as a reply rather than an error.
        pub fn new() -> UreqClient {
            let agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
            UreqClient { agent }
        }

        /// A client over an agent you configured, for a proxy or custom TLS. The agent should be
        /// built with `http_status_as_error(false)`, so that error replies can be read.
        pub fn with_agent(agent: ureq::Agent) -> UreqClient {
            UreqClient { agent }
        }
    }

    fn transport_error(error: ureq::Error) -> TransportError {
        let kind = match &error {
            ureq::Error::Timeout(_) => TransportErrorKind::Timeout,
            ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => TransportErrorKind::Connect,
            ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => TransportErrorKind::Timeout,
            ureq::Error::Io(_) => TransportErrorKind::Connect,
            _ => TransportErrorKind::Other,
        };
        TransportError::new(kind, error.to_string())
    }

    impl HttpClient for UreqClient {
        fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            let reply = match request.method {
                Method::Get => {
                    let mut builder = self.agent.get(&request.url);
                    for (name, value) in &request.headers {
                        builder = builder.header(name, value);
                    }
                    builder.config().timeout_global(request.timeout).build().call()
                }
                Method::Post => {
                    let mut builder = self.agent.post(&request.url);
                    for (name, value) in &request.headers {
                        builder = builder.header(name, value);
                    }
                    builder.config().timeout_global(request.timeout).build().send(&request.body[..])
                }
            }
            .map_err(transport_error)?;
            let status = reply.status().as_u16();
            let headers = reply
                .headers()
                .iter()
                .filter_map(|(name, value)| Some((name.as_str().to_string(), value.to_str().ok()?.to_string())))
                .collect();
            Ok(HttpResponse { status, headers, body: Box::new(reply.into_body().into_reader()) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_hides_the_bearer_token() {
        let request = HttpRequest::post("https://example.test", b"secret body".to_vec())
            .header("Authorization", "Bearer abc.def.ghi")
            .header("content-type", "application/json");
        let shown = format!("{request:?}");
        assert!(!shown.contains("abc.def.ghi") && !shown.contains("secret body"), "{shown}");
        assert!(shown.contains("<redacted>") && shown.contains("application/json"));
        assert_eq!(request.header_value("AUTHORIZATION"), Some("Bearer abc.def.ghi"));
    }

    #[test]
    fn the_request_id_is_found_under_any_of_its_names() {
        let reply = HttpResponse::new(200, "").with_header("X-OAI-Request-Id", "abc");
        assert_eq!(reply.request_id().as_deref(), Some("abc"));
        let both = HttpResponse::new(200, "").with_header("openai-request-id", "b").with_header("x-request-id", "a");
        assert_eq!(both.request_id().as_deref(), Some("a"));
        assert_eq!(HttpResponse::new(500, "boom").text().unwrap(), "boom");
    }

    #[test]
    fn a_closure_is_a_client() {
        let client = |request: HttpRequest| Ok(HttpResponse::new(201, request.url));
        let reply = client.send(HttpRequest::get("https://example.test/x")).unwrap();
        assert_eq!((reply.status, reply.text().unwrap().as_str()), (201, "https://example.test/x"));
    }
}
