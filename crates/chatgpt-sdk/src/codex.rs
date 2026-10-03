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
//!
//! An app can also keep a sign-in of the same kind in a file of its own: [`CodexAuth::create`]
//! saves the tokens a [`crate::DeviceCode`] sign-in returns, in the same format.

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{unverified_claims, TokenResponse};
use crate::client::{Backend, Bearer, Credential};
use crate::error::{Error, OAuthError, Result, SignInError};
use crate::files::{lock_existing, private_dir, read_optional, write_private};
use crate::http::{HttpClient, HttpRequest};

/// Where Codex refreshes its tokens. `CODEX_REFRESH_TOKEN_URL_OVERRIDE` overrides it, as it does
/// for Codex.
pub const CODEX_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
/// Where Codex revokes its refresh token when it signs out. `CODEX_REVOKE_TOKEN_URL_OVERRIDE`
/// overrides it, as it does for Codex.
pub const CODEX_REVOKE_URL: &str = "https://auth.openai.com/oauth/revoke";
/// The Codex CLI's public client id, used when the access token does not name its client.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// A ChatGPT sign-in in Codex's `auth.json` format: the Codex CLI's own, or an app's from
/// [`CodexAuth::create`].
#[derive(Debug)]
pub struct CodexAuth {
    path: PathBuf,
    refresh_url: String,
    revoke_url: String,
    sign_in_command: String,
}

/// Who a sign-in in Codex's format is for.
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

impl CodexAccount {
    /// Who the tokens of a [`crate::DeviceCode`] sign-in are for, read from their claims without
    /// verifying them, before they are saved.
    pub fn from_tokens(tokens: &TokenResponse) -> CodexAccount {
        CodexAccount::from_claims(tokens.id_token.as_deref(), tokens.access_token.as_deref().unwrap_or_default(), None)
    }

    /// The ID token's claims first, then the access token's. `account_id` is the one the file
    /// names, when it names one.
    fn from_claims(id_token: Option<&str>, access_token: &str, account_id: Option<String>) -> CodexAccount {
        let id: IdClaims = id_token.and_then(|token| unverified_claims(token).ok()).unwrap_or_default();
        let access: AccessClaims = unverified_claims(access_token).unwrap_or_default();
        let (id_auth, access_auth) = (id.auth.unwrap_or_default(), access.auth.unwrap_or_default());
        CodexAccount {
            email: id.email,
            plan_type: id_auth.chatgpt_plan_type.or(access_auth.chatgpt_plan_type),
            account_id: account_id.or(id_auth.chatgpt_account_id).or(access_auth.chatgpt_account_id),
            expires_at: access.exp.and_then(|exp| DateTime::from_timestamp(exp, 0)),
        }
    }
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

    /// The sign-in in `path`. Fails unless the file holds a ChatGPT sign-in.
    pub fn open(path: impl Into<PathBuf>) -> Result<CodexAuth> {
        let from_env = |name: &str, default: &str| {
            std::env::var(name).ok().filter(|url| !url.trim().is_empty()).unwrap_or_else(|| default.into())
        };
        let auth = CodexAuth {
            path: path.into(),
            refresh_url: from_env("CODEX_REFRESH_TOKEN_URL_OVERRIDE", CODEX_REFRESH_URL),
            revoke_url: from_env("CODEX_REVOKE_TOKEN_URL_OVERRIDE", CODEX_REVOKE_URL),
            sign_in_command: "codex login".into(),
        };
        auth.read()?;
        Ok(auth)
    }

    /// Save the tokens of a [`crate::DeviceCode`] sign-in to `path`, in Codex's `auth.json` format
    /// with mode 0600, and open them. The file must be the app's own: given Codex's `auth.json`,
    /// this would replace Codex's sign-in.
    pub fn create(path: impl Into<PathBuf>, tokens: &TokenResponse) -> Result<CodexAuth> {
        let path = path.into();
        let access_token =
            tokens.access_token.as_deref().filter(|token| !token.is_empty()).ok_or(SignInError::MissingAccessToken)?;
        let account = CodexAccount::from_tokens(tokens);
        let file = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": tokens.id_token,
                "access_token": access_token,
                "refresh_token": tokens.refresh_token,
                "account_id": account.account_id,
            },
            "last_refresh": Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
        });
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            private_dir(dir)?;
        }
        let pretty = serde_json::to_vec_pretty(&file).map_err(|error| Error::json("serialising the sign-in", error))?;
        write_private(&path, &pretty)?;
        CodexAuth::open(path)
    }

    /// The same sign-in, refreshed at `url` instead.
    pub fn refresh_url(mut self, url: impl Into<String>) -> CodexAuth {
        self.refresh_url = url.into();
        self
    }

    /// The same sign-in, revoked at `url` instead.
    pub fn revoke_url(mut self, url: impl Into<String>) -> CodexAuth {
        self.revoke_url = url.into();
        self
    }

    /// The same sign-in, telling the person to run `command` when it can no longer be used,
    /// instead of `codex login`.
    pub fn sign_in_command(mut self, command: impl Into<String>) -> CodexAuth {
        self.sign_in_command = command.into();
        self
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Who is signed in, read from the file's tokens without verifying them.
    pub fn account(&self) -> Result<CodexAccount> {
        let snapshot = self.read()?;
        Ok(CodexAccount::from_claims(snapshot.id_token.as_deref(), &snapshot.access_token, snapshot.account_id))
    }

    /// End the sign-in's renewable session: revoke its refresh token as `codex logout` does, with a
    /// JSON body. A refresh token the server no longer accepts counts as revoked. The file is left
    /// for the app to remove. Never call it on Codex's own file: it would sign Codex out.
    pub fn revoke(&self, http: &dyn HttpClient) -> Result<()> {
        let current = self.read()?;
        let Some(refresh_token) = current.refresh_token.as_deref() else { return Ok(()) };
        let client_id = current.client_id.as_deref().unwrap_or(CODEX_CLIENT_ID);
        let body = json!({ "token": refresh_token, "token_type_hint": "refresh_token", "client_id": client_id });
        let request = HttpRequest::post(&self.revoke_url, body.to_string().into_bytes())
            .header("content-type", "application/json")
            .header("accept", "application/json");
        let reply = http.send(request)?;
        let (status, request_id) = (reply.status, reply.request_id());
        let text = reply.text()?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        let error = OAuthError::from_reply(status, &text, request_id);
        if error.refresh_token_unusable() {
            Ok(())
        } else {
            Err(error.into())
        }
    }

    fn not_signed_in(&self, why: &str) -> Error {
        Error::SignInRequired { reason: format!("{why} in {}: run `{}`", self.path.display(), self.sign_in_command) }
    }

    fn read_value(&self) -> Result<Value> {
        let text = read_optional(&self.path)?.ok_or_else(|| self.not_signed_in("there is no ChatGPT sign-in"))?;
        serde_json::from_str(&text).map_err(|error| Error::json(format!("reading {}", self.path.display()), error))
    }

    fn read(&self) -> Result<Snapshot> {
        let file: AuthFile = serde_json::from_value(self.read_value()?)
            .map_err(|error| Error::json(format!("reading {}", self.path.display()), error))?;
        let tokens = file.tokens.ok_or_else(|| self.not_signed_in("there is no ChatGPT sign-in"))?;
        let access_token = tokens
            .access_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| self.not_signed_in("the sign-in has no access token"))?;
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
            current.refresh_token.as_deref().ok_or_else(|| self.not_signed_in("the ChatGPT sign-in has expired"))?;
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
                self.not_signed_in(&format!("the ChatGPT sign-in can no longer be refreshed ({})", error.error))
            } else {
                error.into()
            });
        }
        let fresh: Value = serde_json::from_str(&text).map_err(|error| Error::json("the Codex token refresh", error))?;
        let mut file = self.read_value()?;
        let tokens = file
            .get_mut("tokens")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| self.not_signed_in("there is no ChatGPT sign-in"))?;
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
        // Through a symlink to the file it names, so a link kept by a dotfile manager stays a link.
        let target = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        write_private(&target, &pretty).map_err(|error| {
            // The server has already spent the old refresh token, so the file on disk is no longer usable.
            self.not_signed_in(&format!("the refreshed ChatGPT sign-in could not be saved ({error})"))
        })?;
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
        Err(self.not_signed_in("the ChatGPT sign-in was rejected"))
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
    fn a_device_sign_in_is_saved_in_codex_format_and_revoked_as_codex_does() {
        let dir = scratch_dir("codex-create");
        let key = TestKey::load();
        let id = key.token(&json!({ "email": "d@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-d", "chatgpt_plan_type": "plus" } }));
        let access = access_token(&key, Utc::now().timestamp() + 3600);
        let tokens: TokenResponse =
            serde_json::from_value(json!({ "access_token": access, "refresh_token": "rt-d", "id_token": id })).unwrap();
        assert_eq!(CodexAccount::from_tokens(&tokens).account_id.as_deref(), Some("acct-d"), "the ID token's account first");

        let path = dir.join("app").join("device.json");
        let auth = CodexAuth::create(&path, &tokens)
            .unwrap()
            .sign_in_command("my-app login")
            .revoke_url("https://revoke.test/oauth/revoke");
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((written["auth_mode"].as_str(), written["OPENAI_API_KEY"].is_null()), (Some("chatgpt"), true));
        assert_eq!(written["tokens"]["account_id"], "acct-d");
        assert_eq!(written["tokens"]["refresh_token"], "rt-d");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let account = auth.account().unwrap();
        assert_eq!((account.email.as_deref(), account.plan_type.as_deref()), (Some("d@example.com"), Some("plus")));
        assert_eq!(auth.bearer(&never()).unwrap().account_id.as_deref(), Some("acct-d"));

        let revoked = |request: HttpRequest| {
            assert_eq!(request.url, "https://revoke.test/oauth/revoke");
            assert_eq!(request.header_value("content-type"), Some("application/json"));
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body, json!({ "token": "rt-d", "token_type_hint": "refresh_token", "client_id": "app_codex" }));
            Ok(HttpResponse::new(200, ""))
        };
        auth.revoke(&revoked).unwrap();
        let unknown = |_: HttpRequest| {
            Ok(HttpResponse::new(
                401,
                r#"{"error":{"message":"Could not validate your refresh token.","code":"invalid_refresh_token"}}"#,
            ))
        };
        auth.revoke(&unknown).expect("a token the server no longer accepts has nothing left to end");
        assert!(auth.revoke(&|_: HttpRequest| Ok(HttpResponse::new(503, ""))).is_err());

        std::fs::remove_file(&path).unwrap();
        let error = auth.bearer(&never()).unwrap_err();
        assert!(matches!(&error, Error::SignInRequired { reason } if reason.ends_with("run `my-app login`")), "{error}");
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

    #[cfg(unix)]
    #[test]
    fn a_refresh_through_a_symlink_writes_the_file_it_names() {
        let dir = scratch_dir("codex-symlink");
        let key = TestKey::load();
        let real = write_auth(&dir, &access_token(&key, Utc::now().timestamp() - 10));
        let link = dir.join("linked-auth.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let fresh = access_token(&key, Utc::now().timestamp() + 3600);
        let reply = json!({ "access_token": fresh, "refresh_token": "rt-2" }).to_string();
        let http = move |_: HttpRequest| Ok(HttpResponse::new(200, reply.clone()));
        assert_eq!(CodexAuth::open(&link).unwrap().bearer(&http).unwrap().access_token, fresh);
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link stays a link");
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(written["tokens"]["refresh_token"], "rt-2");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
