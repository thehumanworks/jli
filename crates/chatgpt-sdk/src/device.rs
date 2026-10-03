//! Sign in with a one-time code entered on another device, through the Codex CLI's client.
//!
//! Over SSH, or on any host where no browser opens, the loopback callback of [`crate::auth`]
//! reaches the wrong machine. OpenAI's device code endpoints serve the Codex CLI's public client,
//! [`CODEX_CLIENT_ID`], and on 2026-10-03 refused `dynamic_agent_client` with `invalid_client`.
//! This flow therefore signs in the way `codex login --device-auth` does, as other open-source
//! agents do, and its tokens are of Codex's kind: they work on [`crate::Backend::Codex`] only, as
//! a [`CodexAuth`] kept in a file of the app's own. The page the person opens names Codex, and
//! device code sign-in must be enabled in their ChatGPT security settings.
//!
//! 1. [`DeviceCode::request`] asks for a code.
//! 2. Show the person [`DeviceCode::verification_url`] and [`DeviceCode::user_code`].
//! 3. [`DeviceCode::wait`] polls until the code is entered, then exchanges it for tokens.
//! 4. [`CodexAuth::create`] saves them in Codex's `auth.json` format, owner-only.
//!
//! ```no_run
//! # #[cfg(feature = "ureq")]
//! # fn main() -> chatgpt_sdk::Result<()> {
//! use chatgpt_sdk::{auth::ISSUER, Client, CodexAuth, DeviceCode, UreqClient};
//!
//! let http = UreqClient::new();
//! let code = DeviceCode::request(&http, ISSUER)?;
//! eprintln!("Open {} on any device and enter {}", code.verification_url, code.user_code);
//! let tokens = code.wait(&http, &mut std::thread::sleep)?;
//! // The app's own file: never Codex's `~/.codex/auth.json`, which this would replace.
//! let auth = CodexAuth::create("/home/me/.config/my-app/chatgpt.json", &tokens)?.sign_in_command("my-app login");
//! let client = Client::builder(auth).originator("my_app").build();
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "ureq"))]
//! # fn main() {}
//! ```

use std::fmt;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{post_form, token_reply, TokenResponse};
use crate::codex::CODEX_CLIENT_ID;
use crate::error::{Error, OAuthError, Result, SignInError};
use crate::http::{HttpClient, HttpRequest};

/// How long a code lasts when the server does not say: 15 minutes, as long as Codex waits.
pub const DEVICE_CODE_LIFETIME: Duration = Duration::from_secs(15 * 60);
/// The pause between polls when the server does not give one (RFC 8628, section 3.2).
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// What `slow_down` adds to the pause (RFC 8628, section 3.5).
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// A one-time code waiting to be entered on another device.
pub struct DeviceCode {
    /// The code the person enters, such as `ABCD-12345`.
    pub user_code: String,
    /// The page where they enter it: `https://auth.openai.com/codex/device`.
    pub verification_url: String,
    /// The pause between polls.
    pub interval: Duration,
    /// When the code stops working.
    pub expires_at: DateTime<Utc>,
    device_auth_id: String,
    issuer: String,
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCode")
            .field("user_code", &self.user_code)
            .field("verification_url", &self.verification_url)
            .field("interval", &self.interval)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct UserCodeReply {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default)]
    interval: Option<Value>,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct Grant {
    authorization_code: String,
    code_verifier: String,
}

/// What one poll found.
enum Poll {
    Pending,
    SlowDown,
    Granted(Grant),
}

fn post_json(http: &dyn HttpClient, url: &str, body: &Value) -> Result<(u16, String, Option<String>)> {
    let request = HttpRequest::post(url, body.to_string().into_bytes())
        .header("content-type", "application/json")
        .header("accept", "application/json");
    let reply = http.send(request)?;
    let (status, request_id) = (reply.status, reply.request_id());
    Ok((status, reply.text()?, request_id))
}

/// `"5"` or `5`, in seconds.
fn seconds(value: &Value) -> Option<Duration> {
    let seconds = match value {
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => value.as_f64()?,
    };
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

impl DeviceCode {
    /// Ask `issuer` ([`crate::auth::ISSUER`]) for a code, as the Codex CLI's client.
    pub fn request(http: &dyn HttpClient, issuer: &str) -> Result<DeviceCode> {
        let issuer = issuer.trim_end_matches('/').to_string();
        let url = format!("{issuer}/api/accounts/deviceauth/usercode");
        let (status, body, request_id) = post_json(http, &url, &json!({ "client_id": CODEX_CLIENT_ID }))?;
        if !(200..300).contains(&status) {
            return Err(OAuthError::from_reply(status, &body, request_id).into());
        }
        let reply: UserCodeReply =
            serde_json::from_str(&body).map_err(|error| Error::json("the device code reply", error))?;
        let lifetime = chrono::Duration::from_std(DEVICE_CODE_LIFETIME).unwrap_or_default();
        Ok(DeviceCode {
            user_code: reply.user_code,
            verification_url: format!("{issuer}/codex/device"),
            interval: reply.interval.as_ref().and_then(seconds).unwrap_or(DEFAULT_INTERVAL),
            expires_at: reply.expires_at.unwrap_or_else(|| Utc::now() + lifetime),
            device_auth_id: reply.device_auth_id,
            issuer,
        })
    }

    /// Wait until the code is entered, then exchange it for tokens. `sleep` is called between
    /// polls with the pause to take: pass `std::thread::sleep`. The wait ends with
    /// [`SignInError::CodeExpired`] at [`DeviceCode::expires_at`], counted both by the clock and by
    /// the pauses taken, since a code that is never entered looks the same as one not entered yet.
    pub fn wait(&self, http: &dyn HttpClient, sleep: &mut dyn FnMut(Duration)) -> Result<TokenResponse> {
        let lifetime = (self.expires_at - Utc::now()).to_std().unwrap_or(Duration::ZERO);
        let started = Instant::now();
        let mut interval = self.interval;
        let mut paused = Duration::ZERO;
        loop {
            match self.poll(http)? {
                Poll::Granted(grant) => return self.exchange(http, &grant),
                Poll::SlowDown => interval += SLOW_DOWN_STEP,
                Poll::Pending => {}
            }
            let left = lifetime.saturating_sub(paused.max(started.elapsed()));
            if left.is_zero() {
                return Err(SignInError::CodeExpired.into());
            }
            let pause = interval.min(left);
            sleep(pause);
            paused += pause;
        }
    }

    fn poll(&self, http: &dyn HttpClient) -> Result<Poll> {
        let url = format!("{}/api/accounts/deviceauth/token", self.issuer);
        let body = json!({ "device_auth_id": self.device_auth_id, "user_code": self.user_code });
        let (status, body, request_id) = post_json(http, &url, &body)?;
        if (200..300).contains(&status) {
            let grant = serde_json::from_str(&body).map_err(|error| Error::json("the device code grant", error))?;
            return Ok(Poll::Granted(grant));
        }
        // Codex reads 403 and 404 as "not entered yet"; 404 is also what an unknown code gets.
        if status == 403 || status == 404 {
            return Ok(Poll::Pending);
        }
        let error = OAuthError::from_reply(status, &body, request_id);
        match error.error.as_str() {
            "authorization_pending" | "deviceauth_authorization_pending" => Ok(Poll::Pending),
            "slow_down" => Ok(Poll::SlowDown),
            _ => Err(error.into()),
        }
    }

    /// Exchange the granted code as Codex does: a public client with PKCE, the verifier coming
    /// from the grant, and the device callback as the redirect URI.
    fn exchange(&self, http: &dyn HttpClient, grant: &Grant) -> Result<TokenResponse> {
        let redirect_uri = format!("{}/deviceauth/callback", self.issuer);
        let (status, body, request_id) = post_form(
            http,
            &format!("{}/oauth/token", self.issuer),
            &[
                ("grant_type", "authorization_code"),
                ("code", &grant.authorization_code),
                ("redirect_uri", &redirect_uri),
                ("client_id", CODEX_CLIENT_ID),
                ("code_verifier", &grant.code_verifier),
            ],
        )?;
        let tokens = token_reply(status, &body, request_id)?;
        if tokens.access_token.as_deref().is_none_or(str::is_empty) {
            return Err(SignInError::MissingAccessToken.into());
        }
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeChatGpt, FakeReply, FAKE_ISSUER};

    #[test]
    fn the_interval_is_read_as_a_string_or_a_number() {
        assert_eq!(seconds(&json!("5")), Some(Duration::from_secs(5)));
        assert_eq!(seconds(&json!(2)), Some(Duration::from_secs(2)));
        assert_eq!(seconds(&json!(" 1.5 ")), Some(Duration::from_millis(1500)));
        assert_eq!(seconds(&json!("soon")), None);
        assert_eq!(seconds(&json!(-1)), None);
    }

    #[test]
    fn a_code_is_asked_for_as_codex_polled_and_exchanged() {
        let fake = FakeChatGpt::new();
        fake.script_device_poll(FakeReply::Status(403, String::new()));
        fake.script_device_poll(FakeReply::Status(400, r#"{"error":"slow_down"}"#.into()));
        let code = DeviceCode::request(&fake.http(), FAKE_ISSUER).unwrap();
        assert_eq!(code.verification_url, format!("{FAKE_ISSUER}/codex/device"));
        assert_eq!((code.user_code.as_str(), code.interval), ("FAKE-0001", Duration::from_secs(5)));
        assert!(!format!("{code:?}").contains("fake-device"), "the poll id stays out of logs");

        let mut pauses = Vec::new();
        let tokens = code.wait(&fake.http(), &mut |pause| pauses.push(pause)).unwrap();
        assert_eq!(pauses, [Duration::from_secs(5), Duration::from_secs(10)], "slow_down adds five seconds");
        assert!(tokens.access_token.is_some() && tokens.refresh_token.is_some() && tokens.id_token.is_some());

        let requests = fake.requests();
        let asked: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(asked, json!({ "client_id": CODEX_CLIENT_ID }));
        let exchange = requests.last().unwrap();
        assert_eq!(exchange.url, format!("{FAKE_ISSUER}/oauth/token"));
        let body = exchange.body_text();
        assert!(body.starts_with("grant_type=authorization_code&code="), "{body}");
        assert!(
            body.contains("redirect_uri=https%3A%2F%2Fauth.chatgpt.fake%2Fdeviceauth%2Fcallback&client_id=app_"),
            "{body}"
        );
        assert!(!body.contains("resource"), "Codex's exchange sends no resource: {body}");
    }

    #[test]
    fn a_code_never_entered_expires_and_a_refusal_stops_at_once() {
        let fake = FakeChatGpt::new();
        fake.device_code_lifetime(12);
        for _ in 0..10 {
            fake.script_device_poll(FakeReply::Status(404, r#"{"error":{"code":"deviceauth_not_found"}}"#.into()));
        }
        let code = DeviceCode::request(&fake.http(), FAKE_ISSUER).unwrap();
        let mut paused = Duration::ZERO;
        let error = code.wait(&fake.http(), &mut |pause| paused += pause).unwrap_err();
        assert!(matches!(error, Error::SignIn(SignInError::CodeExpired)), "{error}");
        assert!(paused <= Duration::from_secs(12) && paused >= Duration::from_secs(10), "{paused:?}");

        let fake = FakeChatGpt::new();
        fake.script_device_poll(FakeReply::Status(400, r#"{"error":"access_denied"}"#.into()));
        let code = DeviceCode::request(&fake.http(), FAKE_ISSUER).unwrap();
        let error = code.wait(&fake.http(), &mut |_| panic!("a refusal is not waited on")).unwrap_err();
        assert!(matches!(&error, Error::OAuth(oauth) if oauth.error == "access_denied"), "{error}");
    }

    #[test]
    fn a_client_the_server_refuses_is_an_oauth_error() {
        let http = |_: HttpRequest| {
            Ok(crate::HttpResponse::new(
                400,
                r#"{"error":{"message":"This app is unavailable.","type":"invalid_request_error","code":"invalid_client"}}"#,
            ))
        };
        let error = DeviceCode::request(&http, "https://issuer.test").unwrap_err();
        assert!(matches!(&error, Error::OAuth(oauth) if oauth.error == "invalid_client"), "{error}");
    }
}
