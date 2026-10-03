//! The ChatGPT sign-in the Codex CLI keeps in `$CODEX_HOME/auth.json`, as a [`Credential`].
//!
//! Codex signs in with its own client, and its tokens are refused by the public Responses API (on
//! 2026-10-03, with "Missing scopes: api.responses.write"). They are accepted by the Codex backend,
//! [`Backend::Codex`], which speaks the same Responses wire format, takes the account id as a
//! `chatgpt-account-id` header, and requires `stream: true` and `store: false`.
//!
//! The file belongs to Codex. It is read on every request, so a refresh Codex makes is picked up.
//! It is written only when the access token has actually expired: the refresh is then made the way
//! Codex makes it, and the new tokens are written back atomically with every other field kept, so
//! that Codex reads them on its next request. Codex reloads the file before it refreshes, and skips
//! its own refresh when the file has changed.

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::unverified_claims;
use crate::client::{Backend, Bearer, Credential};
use crate::error::{Error, OAuthError, Result};
use crate::files::{lock_existing, read_optional, write_private};
use crate::http::{HttpClient, HttpRequest};

/// Where Codex refreshes its tokens. `CODEX_REFRESH_TOKEN_URL_OVERRIDE` overrides it, as it does
/// for Codex.
pub const CODEX_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
/// The Codex CLI's public client id, used when the access token does not name its client.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The Codex CLI's ChatGPT sign-in.
#[derive(Debug)]
pub struct CodexAuth {
    path: PathBuf,
    refresh_url: String,
}

/// Who Codex is signed in as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAccount {
    /// The account's email.
    pub email: Option<String>,
    /// The ChatGPT plan, such as `plus` or `pro`.
    pub plan_type: Option<String>,
    /// The ChatGPT account id.
    pub account_id: Option<String>,
    /// When the access token expires.
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct AuthFile {
    #[serde(default)]
    tokens: Option<Tokens>,
}

#[derive(Deserialize)]
struct Tokens {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Deserialize, Default)]
struct AccessClaims {
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
}

#[derive(Deserialize, Default)]
struct IdClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
}

#[derive(Deserialize, Default)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
}

/// The tokens as read, with what their claims say.
struct Snapshot {
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    client_id: Option<String>,
    id_token: Option<String>,
}

impl Snapshot {
    fn expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }

    fn bearer(&self) -> Bearer {
        Bearer { access_token: self.access_token.clone(), account_id: self.account_id.clone() }
    }
}

impl CodexAuth {
    /// `$CODEX_HOME/auth.json`, or `~/.codex/auth.json`.
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("CODEX_HOME").filter(|home| !home.is_empty()).map(PathBuf::from).or_else(|| {
            std::env::var_os("HOME").filter(|home| !home.is_empty()).map(|home| PathBuf::from(home).join(".codex"))
        })?;
        Some(home.join("auth.json"))
    }

    /// Codex's sign-in at the default path.
    pub fn open_default() -> Result<CodexAuth> {
        let path = CodexAuth::default_path().ok_or_else(|| Error::Protocol("neither CODEX_HOME nor HOME is set".into()))?;
        CodexAuth::open(path)
    }

    /// Codex's sign-in in `path`. Fails unless Codex is signed in with a ChatGPT account there.
    pub fn open(path: impl Into<PathBuf>) -> Result<CodexAuth> {
        let refresh_url = std::env::var("CODEX_REFRESH_TOKEN_URL_OVERRIDE")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| CODEX_REFRESH_URL.into());
        let auth = CodexAuth { path: path.into(), refresh_url };
        auth.read()?;
        Ok(auth)
    }

    /// The same sign-in, refreshed at `url` instead.
    pub fn refresh_url(mut self, url: impl Into<String>) -> CodexAuth {
        self.refresh_url = url.into();
        self
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Who Codex is signed in as, read from the file's tokens without verifying them.
    pub fn account(&self) -> Result<CodexAccount> {
        let snapshot = self.read()?;
        let id: IdClaims = snapshot.id_token.as_deref().and_then(|token| unverified_claims(token).ok()).unwrap_or_default();
        let access: AccessClaims = unverified_claims(&snapshot.access_token).unwrap_or_default();
        let plan_type = id.auth.as_ref().and_then(|auth| auth.chatgpt_plan_type.clone());
        let plan_type = plan_type.or_else(|| access.auth.and_then(|auth| auth.chatgpt_plan_type));
        Ok(CodexAccount { email: id.email, plan_type, account_id: snapshot.account_id, expires_at: snapshot.expires_at })
    }

    fn not_signed_in(&self, why: &str) -> Error {
        Error::SignInRequired { reason: format!("{why} in {}: run `codex login`", self.path.display()) }
    }

    fn read_value(&self) -> Result<Value> {
        let text = read_optional(&self.path)?.ok_or_else(|| self.not_signed_in("Codex has no sign-in"))?;
        serde_json::from_str(&text).map_err(|error| Error::json(format!("reading {}", self.path.display()), error))
    }

    fn read(&self) -> Result<Snapshot> {
        let file: AuthFile = serde_json::from_value(self.read_value()?)
            .map_err(|error| Error::json(format!("reading {}", self.path.display()), error))?;
        let tokens = file.tokens.ok_or_else(|| self.not_signed_in("Codex is not signed in with ChatGPT"))?;
        let access_token = tokens
            .access_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| self.not_signed_in("Codex has no access token"))?;
        let claims: AccessClaims = unverified_claims(&access_token).unwrap_or_default();
        let claimed_account = claims.auth.as_ref().and_then(|auth| auth.chatgpt_account_id.clone());
        Ok(Snapshot {
            expires_at: claims.exp.and_then(|exp| DateTime::from_timestamp(exp, 0)),
            client_id: claims.client_id,
            account_id: tokens.account_id.or(claimed_account),
            refresh_token: tokens.refresh_token,
            id_token: tokens.id_token,
            access_token,
        })
    }

    /// Refresh under a lock on the file, unless the file already holds a token that has not
    /// expired, then write the new tokens back.
    fn refresh(&self, http: &dyn HttpClient) -> Result<Snapshot> {
        let _held = lock_existing(&self.path)?;
        let current = self.read()?;
        if !current.expired(Utc::now()) {
            return Ok(current);
        }
        let refresh_token =
            current.refresh_token.as_deref().ok_or_else(|| self.not_signed_in("Codex's sign-in has expired"))?;
        let client_id = current.client_id.as_deref().unwrap_or(CODEX_CLIENT_ID);
        let body = json!({ "client_id": client_id, "grant_type": "refresh_token", "refresh_token": refresh_token });
        let request = HttpRequest::post(&self.refresh_url, body.to_string().into_bytes())
            .header("content-type", "application/json")
            .header("accept", "application/json");
        let reply = http.send(request)?;
        let (status, request_id) = (reply.status, reply.request_id());
        let text = reply.text()?;
        if !(200..300).contains(&status) {
            let error = OAuthError::from_reply(status, &text, request_id);
            return Err(if error.refresh_token_unusable() || status == 401 {
                self.not_signed_in(&format!("Codex's sign-in can no longer be refreshed ({})", error.error))
            } else {
                error.into()
            });
        }
        let fresh: Value = serde_json::from_str(&text).map_err(|error| Error::json("the Codex token refresh", error))?;
        let mut file = self.read_value()?;
        let tokens = file
            .get_mut("tokens")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| self.not_signed_in("Codex is not signed in with ChatGPT"))?;
        for name in ["id_token", "access_token", "refresh_token"] {
            if let Some(token) = fresh.get(name).and_then(Value::as_str).filter(|token| !token.is_empty()) {
                tokens.insert(name.into(), Value::String(token.into()));
            }
        }
        if let Some(object) = file.as_object_mut() {
            object.insert("last_refresh".into(), Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)));
        }
        let pretty =
            serde_json::to_vec_pretty(&file).map_err(|error| Error::json("serialising the Codex sign-in", error))?;
        write_private(&self.path, &pretty)?;
        self.read()
    }
}

impl Credential for CodexAuth {
    fn backend(&self) -> Backend {
        Backend::Codex
    }

    fn bearer(&self, http: &dyn HttpClient) -> Result<Bearer> {
        let current = self.read()?;
        if current.expired(Utc::now()) {
            return Ok(self.refresh(http)?.bearer());
        }
        Ok(current.bearer())
    }

    /// Codex may have refreshed since the token was read: use the file's token if it changed. A
    /// token that has not expired is not refreshed here, which would rotate Codex's refresh token
    /// for a rejection that a refresh is unlikely to fix.
    fn after_rejection(&self, http: &dyn HttpClient, rejected: &Bearer) -> Result<Bearer> {
        let current = self.read()?;
        if current.access_token != rejected.access_token {
            return Ok(current.bearer());
        }
        if current.expired(Utc::now()) {
            return Ok(self.refresh(http)?.bearer());
        }
        Err(self.not_signed_in("Codex's ChatGPT sign-in was rejected"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::id_token::tests::TestKey;
    use crate::files::scratch_dir;
    use crate::HttpResponse;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn access_token(key: &TestKey, exp: i64) -> String {
        key.token(&json!({ "exp": exp, "client_id": "app_codex", "https://api.openai.com/auth": { "chatgpt_account_id": "acct-claim" } }))
    }

    fn write_auth(dir: &Path, access: &str) -> PathBuf {
        let key = TestKey::load();
        let id =
            key.token(&json!({ "email": "a@example.com", "https://api.openai.com/auth": { "chatgpt_plan_type": "pro" } }));
        let path = dir.join("auth.json");
        let file = json!({
            "auth_mode": "chatgpt", "OPENAI_API_KEY": null,
            "tokens": { "id_token": id, "access_token": access, "refresh_token": "rt-1", "account_id": "acct-1" },
            "last_refresh": "2026-09-29T21:23:16.123456Z", "written_by_codex": { "keep": true },
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&file).unwrap()).unwrap();
        path
    }

    fn never() -> impl HttpClient {
        |_: HttpRequest| -> std::result::Result<HttpResponse, crate::TransportError> { panic!("no request may be sent") }
    }

    #[test]
    fn a_current_token_is_used_as_it_is() {
        let dir = scratch_dir("codex-current");
        let key = TestKey::load();
        let token = access_token(&key, Utc::now().timestamp() + 3600);
        let auth = CodexAuth::open(write_auth(&dir, &token)).unwrap();
        let bearer = auth.bearer(&never()).unwrap();
        assert_eq!((bearer.access_token, bearer.account_id.as_deref()), (token, Some("acct-1")));
        assert_eq!(auth.backend(), Backend::Codex);
        let account = auth.account().unwrap();
        assert_eq!((account.email.as_deref(), account.plan_type.as_deref()), (Some("a@example.com"), Some("pro")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_expired_token_is_refreshed_as_codex_does_and_written_back_whole() {
        let dir = scratch_dir("codex-expired");
        let key = TestKey::load();
        let path = write_auth(&dir, &access_token(&key, Utc::now().timestamp() - 10));
        let fresh = access_token(&key, Utc::now().timestamp() + 864_000);
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let reply = json!({ "access_token": fresh, "refresh_token": "rt-2", "id_token": "new.id.token" }).to_string();
        let http = move |request: HttpRequest| {
            counter.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.url, "https://refresh.test/oauth/token");
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body, json!({ "client_id": "app_codex", "grant_type": "refresh_token", "refresh_token": "rt-1" }));
            Ok(HttpResponse::new(200, reply.clone()))
        };
        let auth = CodexAuth::open(&path).unwrap().refresh_url("https://refresh.test/oauth/token");
        assert_eq!(auth.bearer(&http).unwrap().access_token, fresh);
        assert_eq!(auth.bearer(&http).unwrap().access_token, fresh, "the written token is read back");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["tokens"]["refresh_token"], "rt-2");
        assert_eq!(written["tokens"]["account_id"], "acct-1");
        assert_eq!(written["written_by_codex"], json!({ "keep": true }), "fields Codex wrote are kept");
        assert_ne!(written["last_refresh"], "2026-09-29T21:23:16.123456Z");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unusable_refresh_leaves_the_file_alone() {
        let dir = scratch_dir("codex-unusable");
        let key = TestKey::load();
        let path = write_auth(&dir, &access_token(&key, Utc::now().timestamp() - 10));
        let before = std::fs::read_to_string(&path).unwrap();
        let http = |_: HttpRequest| Ok(HttpResponse::new(400, r#"{"error":"refresh_token_reused"}"#));
        let error = CodexAuth::open(&path).unwrap().bearer(&http).unwrap_err();
        assert!(matches!(&error, Error::SignInRequired { reason } if reason.contains("codex login")), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_rejection_uses_a_token_codex_refreshed_and_never_refreshes_a_current_one() {
        let dir = scratch_dir("codex-rejected");
        let key = TestKey::load();
        let first = access_token(&key, Utc::now().timestamp() + 3600);
        let path = write_auth(&dir, &first);
        let auth = CodexAuth::open(&path).unwrap();
        let rejected = auth.bearer(&never()).unwrap();
        let error = auth.after_rejection(&never(), &rejected).unwrap_err();
        assert!(matches!(error, Error::SignInRequired { .. }));
        let second = access_token(&key, Utc::now().timestamp() + 7200);
        write_auth(&dir, &second);
        assert_eq!(auth.after_rejection(&never(), &rejected).unwrap().access_token, second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_or_api_key_sign_in_is_refused() {
        let dir = scratch_dir("codex-missing");
        assert!(matches!(CodexAuth::open(dir.join("auth.json")).unwrap_err(), Error::SignInRequired { .. }));
        std::fs::write(dir.join("auth.json"), r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x","tokens":null}"#).unwrap();
        assert!(matches!(CodexAuth::open(dir.join("auth.json")).unwrap_err(), Error::SignInRequired { .. }));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
