//! Sign in with ChatGPT, and run Responses API requests on the user's ChatGPT plan.
//!
//! An open-source or locally run app can let its users pay for model requests with the ChatGPT
//! plan they already have, instead of an API key. This crate implements the documented flow for
//! that ([Sign in with ChatGPT: ChatGPT plan usage](https://developers.openai.com/siwc/token-sharing-open-source)):
//!
//! - [`auth`]: the browser sign-in with dynamic client registration, PKCE, a loopback callback,
//!   and ID token verification against OpenAI's published keys.
//! - [`CredentialStore`]: the saved sign-ins on this host, written atomically with owner-only
//!   permissions, with a stable host id, refresh serialised across processes, and sign-out that
//!   revokes the renewable session.
//! - [`Client`]: model lists and streamed Responses API requests, with typed requests
//!   ([`ResponseRequest`]), events ([`StreamEvent`]) and results ([`Completion`]).
//! - [`CodexAuth`]: the sign-in the Codex CLI already keeps, for the Codex backend.
//! - [`DeviceCode`]: a sign-in with a one-time code entered on another device, for a host where no
//!   browser opens. It goes through the Codex CLI's client, so its tokens are for the Codex backend.
//! - [`Error::recovery`]: what the documentation says to do about each failure.
//!
//! Everything is blocking. HTTP goes through the [`HttpClient`] trait; the default,
//! [`UreqClient`], uses `ureq` with rustls.
//!
//! # Two backends
//!
//! | Credentials | Backend | Base URL |
//! | --- | --- | --- |
//! | [`Session`], from Sign in with ChatGPT | [`Backend::Api`] | `https://api.openai.com/v1` |
//! | [`CodexAuth`], from `codex login` or a [`DeviceCode`] | [`Backend::Codex`] | `https://chatgpt.com/backend-api/codex` |
//!
//! Each token works on its own backend only. Both take the same Responses request, streamed and
//! unstored.
//!
//! # Example
//!
//! ```no_run
//! use chatgpt_sdk::auth::OAuthConfig;
//! use chatgpt_sdk::{Client, CredentialStore, InputItem, ReasoningEffort, ResponseRequest};
//!
//! # fn main() -> chatgpt_sdk::Result<()> {
//! // After a sign-in saved with `CredentialStore::save` (see the `auth` module):
//! let store = CredentialStore::open("/home/me/.config/my-app/chatgpt")?;
//! let account = store.active()?.expect("signed in");
//! let client = Client::builder(store.session(account.client_id, OAuthConfig::default())).originator("my_app").build();
//!
//! let request = ResponseRequest::new("gpt-6-luna")
//!     .instructions("Answer in one sentence.")
//!     .input(InputItem::user("Why is the sky blue?"))
//!     .reasoning_effort(ReasoningEffort::None);
//! for event in client.stream(&request)? {
//!     if let chatgpt_sdk::StreamEvent::OutputTextDelta { delta, .. } = event? {
//!         print!("{delta}");
//!     }
//! }
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod auth;
mod client;
mod codex;
mod credentials;
mod device;
mod error;
mod files;
mod http;
mod models;
mod responses;
mod sse;
mod store;
mod stream;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use client::{AccessToken, Backend, Bearer, Client, ClientBuilder, Credential, DEFAULT_CODEX_CLIENT_VERSION};
pub use codex::{CodexAccount, CodexAuth, CODEX_CLIENT_ID, CODEX_REFRESH_URL, CODEX_REVOKE_URL};
pub use credentials::{Credentials, REFRESH_MARGIN};
pub use device::{DeviceCode, DEVICE_CODE_LIFETIME};
pub use error::{
    ApiError, Error, IdTokenError, OAuthError, Recovery, ResponseError, Result, SignInError, TransportError,
    TransportErrorKind, MANAGE_USAGE_URL,
};
#[cfg(feature = "ureq")]
pub use http::UreqClient;
pub use http::{HttpClient, HttpRequest, HttpResponse, Method};
pub use models::{Model, ReasoningLevel, Visibility};
pub use responses::{
    FunctionTool, InputContent, InputItem, InputMessage, Reasoning, ReasoningEffort, ReasoningSummary, ResponseRequest,
    Role, TextConfig, TextFormat, Tool, Verbosity,
};
pub use store::{CredentialStore, Session, SignOut};
pub use stream::{
    Accumulator, Completion, FunctionCallItem, IncompleteDetails, InputTokensDetails, MessageItem, OutputContent,
    OutputItem, OutputTokensDetails, ReasoningItem, ResponseErrorBody, ResponseObject, ResponseStatus, ResponseStream,
    StreamEvent, Usage,
};
