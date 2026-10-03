//! A fake ChatGPT for offline tests (feature `testing`).
//!
//! [`FakeChatGpt`] is an authorization server, a browser, and both Responses backends behind one
//! [`HttpClient`]. It checks PKCE and the client id, issues and rotates tokens, signs ID tokens
//! with a key that ships with this crate, and streams scripted replies. Use it to test an app's
//! sign-in, refresh, sign-out and requests without a network or an account. It also serves the
//! device code endpoints, as for Codex's client only, and plays a person who enters the code.
//!
//! The signing key is public: never trust a token it signed outside a test.
//!
//! ```
//! use std::time::Duration;
//! use chatgpt_sdk::auth::{AuthorizationRequest, LoopbackListener, Registration};
//! use chatgpt_sdk::testing::FakeChatGpt;
//! use chatgpt_sdk::{Client, CredentialStore, InputItem, ResponseRequest};
//!
//! # fn main() -> chatgpt_sdk::Result<()> {
//! let fake = FakeChatGpt::new();
//! let dir = std::env::temp_dir().join(format!("doc-{}", std::process::id()));
//! let store = CredentialStore::open(&dir)?;
//! let listener = LoopbackListener::bind_port(0)?;
//! let request = AuthorizationRequest::new(fake.config(), Registration::new("My App"), store.host_id()?, listener.redirect_uri())?;
//! let browser = fake.browse(&request.url());
//! let callback = listener.wait(request.state(), Duration::from_secs(10))?;
//! browser.join().expect("the browser returns");
//! let credentials = request.complete(&fake.http(), &callback)?;
//! store.save(&credentials)?;
//!
//! fake.reply("Hello");
//! let client = Client::builder(store.session(&credentials.client_id, fake.config())).http_client(fake.http()).build();
//! assert_eq!(client.create(&ResponseRequest::new("gpt-6-luna").input(InputItem::user("hi")))?.text, "Hello");
//! # std::fs::remove_dir_all(dir).ok();
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{KeyPair, RsaKeyPair, RSA_PKCS1_SHA256};
use serde_json::{json, Value};

use crate::auth::{form_decode, form_encode, OAuthConfig, DYNAMIC_CLIENT_ID};
use crate::client::Backend;
use crate::codex::CODEX_CLIENT_ID;
use crate::error::TransportError;
use crate::http::{HttpClient, HttpRequest, HttpResponse};

/// The fake authorization server's issuer.
pub const FAKE_ISSUER: &str = "https://auth.chatgpt.fake";

const KEY_ID: &str = "fake-key";
const ALL_SCOPES: &str = "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";

/// What a scripted request answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeReply {
    /// A streamed response with this text, then `response.completed`.
    Text(String),
    /// An HTTP error with this status and body.
    Status(u16, String),
    /// A stream that ends in `response.failed` with this code.
    Failed(String),
}

#[derive(Debug)]
struct Pending {
    challenge: String,
    /// The browser sign-in's nonce; a device code has none.
    nonce: Option<String>,
    client_id: String,
    code: String,
    /// The redirect URI the exchange must name, when it is checked.
    redirect_uri: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    issued_client_id: String,
    subject: String,
    email: String,
    granted: String,
    deny: bool,
    pending: Option<Pending>,
    serial: u32,
    access_token: Option<String>,
    refresh_token: Option<String>,
    replies: VecDeque<FakeReply>,
    models: Vec<Value>,
    requests: Vec<HttpRequest>,
    revoked: Vec<String>,
    device: Option<FakeDevice>,
    devices: u32,
    device_lifetime: i64,
    device_polls: VecDeque<FakeReply>,
}

/// A device code waiting to be entered.
#[derive(Debug)]
struct FakeDevice {
    id: String,
    user_code: String,
    verifier: String,
}

/// A fake authorization server, browser and Responses API. See the [module docs](self).
#[derive(Debug)]
pub struct FakeChatGpt {
    key: RsaKeyPair,
    state: Mutex<State>,
}

fn locked(state: &Mutex<State>) -> MutexGuard<'_, State> {
    // A test that panicked mid-request leaves the state usable for the next assertion.
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl FakeChatGpt {
    /// A fake that grants every scope, registers new clients as `oaiapp_fake`, signs users in as
    /// `user-fake` (`user@example.com`), and lists `gpt-6-luna` and `gpt-6.1-sol`.
    pub fn new() -> Arc<FakeChatGpt> {
        let key = RsaKeyPair::from_pkcs8(include_bytes!("test-key.pk8")).expect("the bundled key is a valid PKCS#8 RSA key");
        let state = State {
            issued_client_id: "oaiapp_fake".into(),
            subject: "user-fake".into(),
            email: "user@example.com".into(),
            granted: ALL_SCOPES.into(),
            models: vec![
                json!({ "slug": "gpt-6-luna", "display_name": "GPT-6-Luna", "visibility": "list" }),
                json!({ "slug": "gpt-6.1-sol", "display_name": "GPT-6.1-Sol", "visibility": "list" }),
                json!({ "slug": "hidden-model", "visibility": "hide" }),
            ],
            device_lifetime: 15 * 60,
            ..State::default()
        };
        Arc::new(FakeChatGpt { key, state: Mutex::new(state) })
    }

    /// The fake server's endpoints.
    pub fn config(&self) -> OAuthConfig {
        OAuthConfig {
            issuer: FAKE_ISSUER.into(),
            authorization_endpoint: format!("{FAKE_ISSUER}/authorize"),
            token_endpoint: format!("{FAKE_ISSUER}/token"),
            revocation_endpoint: format!("{FAKE_ISSUER}/revoke"),
            jwks_uri: format!("{FAKE_ISSUER}/jwks"),
            resource: crate::auth::API_RESOURCE.into(),
        }
    }

    /// The scopes the next sign-in grants, space-separated. Leave out `chatgpt.tokens.use.direct`
    /// to play a user who declines plan usage.
    pub fn grant(&self, scopes: &str) {
        locked(&self.state).granted = scopes.into();
    }

    /// Make the next sign-in end with `error=access_denied`.
    pub fn deny_next(&self) {
        locked(&self.state).deny = true;
    }

    /// The client id the next new registration is issued.
    pub fn issue_client_id(&self, client_id: &str) {
        locked(&self.state).issued_client_id = client_id.into();
    }

    /// Sign in as another account from now on.
    pub fn sign_in_as(&self, subject: &str, email: &str) {
        let mut state = locked(&self.state);
        state.subject = subject.into();
        state.email = email.into();
    }

    /// Queue a streamed reply with `text`.
    pub fn reply(&self, text: &str) {
        self.script(FakeReply::Text(text.into()));
    }

    /// Queue any reply. A request with nothing queued answers `ok`.
    pub fn script(&self, reply: FakeReply) {
        locked(&self.state).replies.push_back(reply);
    }

    /// Expire the current access token, so the next request is answered 401.
    pub fn expire_access_token(&self) {
        locked(&self.state).access_token = None;
    }

    /// Every request the fake has answered, in order.
    pub fn requests(&self) -> Vec<HttpRequest> {
        locked(&self.state).requests.clone()
    }

    /// Queue a reply to the next device code poll, such as `Status(403, "")` for a code not entered
    /// yet. While none is queued, a poll finds the code entered. A reply other than a status
    /// counts as not entered yet.
    pub fn script_device_poll(&self, reply: FakeReply) {
        locked(&self.state).device_polls.push_back(reply);
    }

    /// How long the next device code lasts, in seconds: 15 minutes unless set.
    pub fn device_code_lifetime(&self, seconds: i64) {
        locked(&self.state).device_lifetime = seconds;
    }

    /// The refresh tokens revoked so far.
    pub fn revoked(&self) -> Vec<String> {
        locked(&self.state).revoked.clone()
    }

    /// An ID token for `claims`, signed with the fake's key.
    pub fn id_token(&self, claims: &Value) -> String {
        let header = json!({ "alg": "RS256", "kid": KEY_ID, "typ": "JWT" });
        let input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
        let mut signature = vec![0; self.key.public().modulus_len()];
        // Signing with a valid key into a buffer of the modulus' length cannot fail.
        let _ = self.key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut signature);
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    /// The fake's key set, as `jwks_uri` serves it.
    pub fn jwks(&self) -> Value {
        let public: ring::rsa::PublicKeyComponents<Vec<u8>> = self.key.public_key().into();
        json!({ "keys": [{ "kid": KEY_ID, "kty": "RSA", "alg": "RS256", "use": "sig",
            "n": URL_SAFE_NO_PAD.encode(&public.n), "e": URL_SAFE_NO_PAD.encode(&public.e) }] })
    }

    /// The fake as an [`HttpClient`]: the authorization server's endpoints, and the Responses API
    /// and model list on both backends.
    pub fn http(self: &Arc<Self>) -> impl HttpClient + 'static {
        let fake = self.clone();
        move |request: HttpRequest| -> Result<HttpResponse, TransportError> { Ok(fake.answer(request)) }
    }

    /// Play the browser on another thread: read the authorization URL, sign the user in, and
    /// return to its loopback callback. The thread gives back the authorization request's query.
    pub fn browse(self: &Arc<Self>, authorization_url: &str) -> JoinHandle<Vec<(String, String)>> {
        let fake = self.clone();
        let url = authorization_url.to_string();
        std::thread::spawn(move || {
            let query = form_decode(url.split_once('?').map(|(_, query)| query).unwrap_or(""));
            let field =
                |name: &str| query.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()).unwrap_or_default();
            let redirect = field("redirect_uri");
            let requested = field("client_id");
            let (target, port) = {
                let mut state = locked(&fake.state);
                state.serial += 1;
                let new_registration = requested == DYNAMIC_CLIENT_ID;
                let client_id = if new_registration { state.issued_client_id.clone() } else { requested };
                let code = format!("fake-code-{}", state.serial);
                let mut pairs = vec![("state", field("state"))];
                if std::mem::take(&mut state.deny) {
                    pairs.push(("error", "access_denied".into()));
                } else {
                    pairs.push(("code", code.clone()));
                    pairs.push(("scope", state.granted.clone()));
                    if new_registration {
                        pairs.push(("client_id", client_id.clone()));
                    }
                    state.pending = Some(Pending {
                        challenge: field("code_challenge"),
                        nonce: Some(field("nonce")),
                        client_id,
                        code,
                        redirect_uri: None,
                    });
                }
                let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(name, value)| (*name, value.as_str())).collect();
                let rest = redirect.strip_prefix("http://127.0.0.1:").unwrap_or_default();
                let (port, path) = rest.split_once('/').unwrap_or((rest, ""));
                (format!("/{path}?{}", form_encode(&borrowed)), port.parse::<u16>().unwrap_or_default())
            };
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                let _ = write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
                let _ = stream.read_to_string(&mut String::new());
            }
            query
        })
    }

    fn tokens(&self, state: &mut State, client_id: &str, nonce: Option<&str>) -> Value {
        state.serial += 1;
        let access = format!("fake-access-{}", state.serial);
        let refresh = format!("fake-refresh-{}", state.serial);
        state.access_token = Some(access.clone());
        state.refresh_token = Some(refresh.clone());
        let now = chrono::Utc::now().timestamp();
        let mut claims = json!({ "iss": FAKE_ISSUER, "sub": state.subject, "aud": [client_id], "iat": now, "exp": now + 3600,
            "email": state.email, "email_verified": true,
            "https://api.openai.com/auth": { "chatgpt_account_id": format!("account-{}", state.subject), "chatgpt_plan_type": "plus" } });
        if let Some(nonce) = nonce {
            claims["nonce"] = json!(nonce);
        }
        json!({ "access_token": access, "refresh_token": refresh, "id_token": self.id_token(&claims),
            "token_type": "Bearer", "expires_in": 3600, "scope": state.granted })
    }

    fn answer(&self, request: HttpRequest) -> HttpResponse {
        let mut state = locked(&self.state);
        state.requests.push(request.clone());
        // A form, or a JSON object as Codex's endpoints take.
        let object: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let form = form_decode(&request.body_text());
        let field = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| form.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()))
                .unwrap_or_default()
        };
        let oauth_error = |code: &str| HttpResponse::new(400, json!({ "error": code }).to_string());
        let url = request.url.as_str();
        let codex = Backend::Codex.base_url();
        let api = Backend::Api.base_url();
        if url == format!("{FAKE_ISSUER}/jwks") {
            return HttpResponse::new(200, self.jwks().to_string());
        }
        if url == format!("{FAKE_ISSUER}/api/accounts/deviceauth/usercode") {
            if field("client_id") != CODEX_CLIENT_ID {
                let refused = json!({ "error": { "message": "This app is unavailable. Contact the app owner and try again later.",
                    "type": "invalid_request_error", "param": null, "code": "invalid_client" } });
                return HttpResponse::new(400, refused.to_string());
            }
            state.devices += 1;
            let device = FakeDevice {
                id: format!("fake-device-{}", state.devices),
                user_code: format!("FAKE-{:04}", state.devices),
                verifier: format!("fake-device-verifier-{}", state.devices),
            };
            let expires_at = chrono::Utc::now() + chrono::Duration::seconds(state.device_lifetime);
            let reply = json!({ "device_auth_id": device.id, "user_code": device.user_code, "interval": "5",
                "expires_at": expires_at.to_rfc3339() });
            state.device = Some(device);
            return HttpResponse::new(200, reply.to_string());
        }
        if url == format!("{FAKE_ISSUER}/api/accounts/deviceauth/token") {
            let known = state
                .device
                .as_ref()
                .is_some_and(|device| device.id == field("device_auth_id") && device.user_code == field("user_code"));
            if !known {
                return HttpResponse::new(
                    404,
                    r#"{"error":{"message":"Resource not found","code":"deviceauth_not_found"}}"#,
                );
            }
            if let Some(reply) = state.device_polls.pop_front() {
                return match reply {
                    FakeReply::Status(status, body) => HttpResponse::new(status, body),
                    _ => HttpResponse::new(403, ""),
                };
            }
            let Some(device) = state.device.take() else { return HttpResponse::new(404, "") };
            state.serial += 1;
            let code = format!("fake-device-code-{}", state.serial);
            let challenge = URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256, device.verifier.as_bytes()));
            state.pending = Some(Pending {
                challenge: challenge.clone(),
                nonce: None,
                client_id: CODEX_CLIENT_ID.into(),
                code: code.clone(),
                redirect_uri: Some(format!("{FAKE_ISSUER}/deviceauth/callback")),
            });
            let grant = json!({ "authorization_code": code, "code_challenge": challenge, "code_verifier": device.verifier });
            return HttpResponse::new(200, grant.to_string());
        }
        if url == format!("{FAKE_ISSUER}/revoke") || url == format!("{FAKE_ISSUER}/oauth/revoke") {
            state.revoked.push(field("token"));
            if state.refresh_token.as_deref() == Some(field("token").as_str()) {
                state.refresh_token = None;
                state.access_token = None;
            }
            return HttpResponse::new(200, "");
        }
        // The documented token endpoint, and the one Codex uses.
        if url == format!("{FAKE_ISSUER}/token") || url == format!("{FAKE_ISSUER}/oauth/token") {
            return match field("grant_type").as_str() {
                "authorization_code" => {
                    let Some(pending) = state.pending.take() else { return oauth_error("invalid_grant") };
                    let challenge = URL_SAFE_NO_PAD
                        .encode(ring::digest::digest(&ring::digest::SHA256, field("code_verifier").as_bytes()));
                    if field("code") != pending.code || challenge != pending.challenge {
                        return oauth_error("invalid_grant");
                    }
                    if pending.redirect_uri.as_ref().is_some_and(|expected| field("redirect_uri") != *expected) {
                        return oauth_error("invalid_grant");
                    }
                    if field("client_id") != pending.client_id {
                        return oauth_error("invalid_client");
                    }
                    let tokens = self.tokens(&mut state, &pending.client_id, pending.nonce.as_deref());
                    HttpResponse::new(200, tokens.to_string())
                }
                "refresh_token" => {
                    if state.refresh_token.as_deref() != Some(field("refresh_token").as_str()) {
                        return oauth_error("refresh_token_reused");
                    }
                    let mut tokens = self.tokens(&mut state, &field("client_id"), None);
                    if let Some(object) = tokens.as_object_mut() {
                        object.remove("id_token");
                    }
                    HttpResponse::new(200, tokens.to_string())
                }
                _ => oauth_error("unsupported_grant_type"),
            };
        }
        let authorized = request
            .header_value("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| state.access_token.as_deref() == Some(token) || token.starts_with("static-"));
        let is = |path: &str| url == format!("{api}{path}") || url.starts_with(&format!("{codex}{path}"));
        if !is("/models") && !is("/responses") {
            return HttpResponse::new(404, json!({ "detail": format!("no fake route for {url}") }).to_string());
        }
        if !authorized {
            return HttpResponse::new(401, r#"{"detail":"Unauthorized"}"#);
        }
        if is("/models") {
            return HttpResponse::new(200, json!({ "models": state.models }).to_string());
        }
        match state.replies.pop_front().unwrap_or(FakeReply::Text("ok".into())) {
            FakeReply::Status(status, body) => HttpResponse::new(status, body),
            FakeReply::Text(text) => HttpResponse::new(200, stream(&text, None)),
            FakeReply::Failed(code) => HttpResponse::new(200, stream("", Some(&code))),
        }
        .with_header("x-request-id", format!("fake-request-{}", state.requests.len()))
    }
}

/// A Responses stream that says `text`, then completes, or fails with `failure`.
fn stream(text: &str, failure: Option<&str>) -> String {
    let mut events = vec![
        json!({ "type": "response.created", "response": { "id": "resp_fake", "status": "in_progress", "output": [] } }),
    ];
    if !text.is_empty() {
        events.push(
            json!({ "type": "response.output_text.delta", "item_id": "msg_fake", "output_index": 0, "content_index": 0, "delta": text }),
        );
    }
    events.push(match failure {
        Some(code) => json!({ "type": "response.failed", "response": { "id": "resp_fake", "status": "failed",
            "error": { "code": code, "message": format!("fake failure: {code}") } } }),
        None => json!({ "type": "response.completed", "response": { "id": "resp_fake", "status": "completed", "output": [],
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 } } }),
    });
    events.iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap_or_default())).collect()
}
