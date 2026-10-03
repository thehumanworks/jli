//! Saved sign-ins on this host, one protected file per account registration.
//!
//! ```text
//! <dir>/host-id                  this host's ext_agent_host_id, made once
//! <dir>/active                   the client id of the account in use
//! <dir>/accounts/<client_id>.json  one Credentials record per registration, mode 0600
//! <dir>/accounts/<client_id>.lock  held while that registration's token is refreshed
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;

use crate::auth::{revoke, HostId, OAuthConfig};
use crate::client::{Backend, Bearer, Credential};
use crate::credentials::Credentials;
use crate::error::{Error, Recovery, Result};
use crate::files::{lock, private_dir, read_optional, write_private};
use crate::http::HttpClient;

/// The saved sign-ins of one app on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStore {
    dir: PathBuf,
}

/// What signing out did.
#[derive(Debug)]
pub struct SignOut {
    /// Whether the server confirmed the refresh token is revoked. The local tokens are cleared
    /// either way; when this is false, tell the user revocation was not confirmed and that they
    /// can disconnect the app in ChatGPT's settings.
    pub revoked: bool,
    /// The last revocation error, when it was not confirmed.
    pub error: Option<Error>,
}

impl CredentialStore {
    /// Use `dir` for this app's sign-ins, creating it (mode 0700 on Unix) if needed.
    pub fn open(dir: impl Into<PathBuf>) -> Result<CredentialStore> {
        let dir = dir.into();
        private_dir(&dir.join("accounts"))?;
        Ok(CredentialStore { dir })
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// This host's id: the saved one, or a new one saved now. Keep it for the host's life.
    pub fn host_id(&self) -> Result<HostId> {
        let path = self.dir.join("host-id");
        if let Some(text) = read_optional(&path)? {
            return text.trim().parse();
        }
        let id = HostId::generate()?;
        write_private(&path, id.as_str().as_bytes())?;
        Ok(id)
    }

    /// Replace this host's id, as when credentials are moved to a VM that has its own.
    pub fn set_host_id(&self, id: &HostId) -> Result<()> {
        write_private(&self.dir.join("host-id"), id.as_str().as_bytes())
    }

    fn record_path(&self, client_id: &str) -> Result<PathBuf> {
        let safe = !client_id.is_empty() && client_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !safe {
            return Err(Error::Protocol(format!("{client_id:?} is not a client id this store can name a file after")));
        }
        Ok(self.dir.join("accounts").join(format!("{client_id}.json")))
    }

    /// Save a registration's credentials, replacing any saved before.
    pub fn save(&self, credentials: &Credentials) -> Result<()> {
        let path = self.record_path(&credentials.client_id)?;
        let json = serde_json::to_vec_pretty(credentials).map_err(|error| Error::json("serialising credentials", error))?;
        write_private(&path, &json)
    }

    /// One registration's credentials.
    pub fn load(&self, client_id: &str) -> Result<Option<Credentials>> {
        let path = self.record_path(client_id)?;
        let Some(text) = read_optional(&path)? else { return Ok(None) };
        serde_json::from_str(&text).map(Some).map_err(|error| Error::json(format!("reading {}", path.display()), error))
    }

    /// Every saved registration, signed in or not, by email then client id.
    pub fn accounts(&self) -> Result<Vec<Credentials>> {
        let dir = self.dir.join("accounts");
        let entries = fs::read_dir(&dir).map_err(|error| Error::io(format!("listing {}", dir.display()), error))?;
        let mut accounts = vec![];
        for entry in entries {
            let path = entry.map_err(|error| Error::io(format!("listing {}", dir.display()), error))?.path();
            let client_id = path.file_stem().and_then(|stem| stem.to_str()).map(str::to_string);
            if let (Some(client_id), Some("json")) = (client_id, path.extension().and_then(|ext| ext.to_str())) {
                if let Some(credentials) = self.load(&client_id)? {
                    accounts.push(credentials);
                }
            }
        }
        accounts.sort_by(|a, b| (&a.email, &a.client_id).cmp(&(&b.email, &b.client_id)));
        Ok(accounts)
    }

    /// The client id of the account in use, if one is chosen.
    pub fn active_client_id(&self) -> Result<Option<String>> {
        Ok(read_optional(&self.dir.join("active"))?.map(|text| text.trim().to_string()).filter(|id| !id.is_empty()))
    }

    /// The account in use, if one is chosen and still saved.
    pub fn active(&self) -> Result<Option<Credentials>> {
        match self.active_client_id()? {
            Some(client_id) => self.load(&client_id),
            None => Ok(None),
        }
    }

    /// Choose the account in use, or none. Only a validated sign-in should be made active.
    pub fn set_active(&self, client_id: Option<&str>) -> Result<()> {
        let path = self.dir.join("active");
        match client_id {
            Some(client_id) => {
                self.record_path(client_id)?;
                write_private(&path, client_id.as_bytes())
            }
            None => match fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(Error::io(format!("removing {}", path.display()), error))
                }
                _ => Ok(()),
            },
        }
    }

    /// Sign out of one registration: revoke its refresh token (three tries, with backoff, on a
    /// temporary failure), then clear its tokens. The record stays, with its client id, for the
    /// next sign-in, and the host id is kept.
    pub fn sign_out(&self, http: &dyn HttpClient, config: &OAuthConfig, client_id: &str) -> Result<SignOut> {
        let _held = lock(&self.lock_path(client_id)?)?;
        let Some(saved) = self.load(client_id)? else {
            return Ok(SignOut { revoked: true, error: None });
        };
        let mut outcome = SignOut { revoked: saved.refresh_token.is_none(), error: None };
        if let Some(refresh_token) = &saved.refresh_token {
            for attempt in 0..3u32 {
                match revoke(http, config, client_id, refresh_token) {
                    Ok(()) => {
                        outcome = SignOut { revoked: true, error: None };
                        break;
                    }
                    Err(error) => {
                        let again = error.recovery() == Recovery::RetryLater;
                        outcome.error = Some(error);
                        if !again {
                            break;
                        }
                        if attempt < 2 {
                            std::thread::sleep(Duration::from_millis(250 << attempt));
                        }
                    }
                }
            }
        }
        self.save(&saved.signed_out())?;
        Ok(outcome)
    }

    fn lock_path(&self, client_id: &str) -> Result<PathBuf> {
        Ok(self.record_path(client_id)?.with_extension("lock"))
    }

    /// A [`Credential`] for one registration, refreshed and saved here as needed.
    pub fn session(&self, client_id: impl Into<String>, config: OAuthConfig) -> Session {
        Session { store: self.clone(), client_id: client_id.into(), config, cached: Mutex::new(None) }
    }
}

/// One registration's credentials, as a [`Credential`] for the public API backend.
///
/// The access token is refreshed five minutes before it expires, and after a 401. Refreshes are
/// serialised across processes with a lock file, and each process reads the saved record again
/// once it holds the lock, so a refresh token is never used twice. When the refresh token turns
/// out unusable, the saved tokens are cleared and the error is [`Error::SignInRequired`].
#[derive(Debug)]
pub struct Session {
    store: CredentialStore,
    client_id: String,
    config: OAuthConfig,
    cached: Mutex<Option<Credentials>>,
}

impl Session {
    /// The registration's current credentials.
    pub fn credentials(&self) -> Result<Credentials> {
        if let Some(cached) = self.cached.lock().map_err(|_| poisoned())?.clone() {
            return Ok(cached);
        }
        let saved = self.load()?;
        *self.cached.lock().map_err(|_| poisoned())? = Some(saved.clone());
        Ok(saved)
    }

    fn load(&self) -> Result<Credentials> {
        self.store.load(&self.client_id)?.filter(Credentials::is_signed_in).ok_or_else(|| Error::SignInRequired {
            reason: format!("no signed-in ChatGPT account with client id {}", self.client_id),
        })
    }

    /// Refresh under the lock, unless another process already did, or `rejected` is no longer the
    /// saved token.
    fn refresh(&self, http: &dyn HttpClient, rejected: Option<&str>) -> Result<Credentials> {
        let _held = lock(&self.store.lock_path(&self.client_id)?)?;
        let saved = self.load()?;
        let already_fresh = match rejected {
            Some(token) => saved.access_token.as_deref().is_some_and(|saved| saved != token),
            None => !saved.needs_refresh(Utc::now()),
        };
        let fresh = if already_fresh {
            saved
        } else {
            match saved.refreshed(http, &self.config) {
                Ok(refreshed) => {
                    self.store.save(&refreshed)?;
                    refreshed
                }
                Err(error @ Error::SignInRequired { .. }) => {
                    self.store.save(&saved.signed_out())?;
                    *self.cached.lock().map_err(|_| poisoned())? = None;
                    return Err(error);
                }
                // A temporary failure keeps the saved tokens.
                Err(error) => return Err(error),
            }
        };
        *self.cached.lock().map_err(|_| poisoned())? = Some(fresh.clone());
        Ok(fresh)
    }

    fn bearer_of(credentials: &Credentials) -> Result<Bearer> {
        credentials
            .access_token
            .clone()
            .map(Bearer::new)
            .ok_or_else(|| Error::SignInRequired { reason: "the saved sign-in has no access token".into() })
    }
}

fn poisoned() -> Error {
    Error::Protocol("a thread panicked while holding the session".into())
}

impl Credential for Session {
    fn backend(&self) -> Backend {
        Backend::Api
    }

    fn bearer(&self, http: &dyn HttpClient) -> Result<Bearer> {
        let credentials = self.credentials()?;
        if !credentials.plan_usage_enabled() {
            return Err(Error::PlanUsageNotEnabled);
        }
        let current = if credentials.needs_refresh(Utc::now()) { self.refresh(http, None)? } else { credentials };
        Session::bearer_of(&current)
    }

    fn after_rejection(&self, http: &dyn HttpClient, rejected: &Bearer) -> Result<Bearer> {
        Session::bearer_of(&self.refresh(http, Some(&rejected.access_token))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::scratch_dir;
    use crate::{HttpRequest, HttpResponse};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn credentials(client_id: &str, expires_in: u64) -> Credentials {
        serde_json::from_value(json!({
            "email": "user@example.com", "issuer": "https://auth.openai.com", "subject": "user-1", "client_id": client_id,
            "ext_agent_host_id": "urn:uuid:6f1c2d9e-8d4b-4c7a-9e1f-2b3c4d5e6f70", "id_token": "id", "access_token": "at-1",
            "refresh_token": "rt-1", "token_type": "Bearer", "expires_in": expires_in,
            "scopes": ["chatgpt.tokens.use.direct", "offline_access", "openid"], "saved_at": Utc::now().to_rfc3339(),
        }))
        .unwrap()
    }

    #[test]
    fn records_hosts_and_the_active_account_are_kept() {
        let dir = scratch_dir("store");
        let store = CredentialStore::open(&dir).unwrap();
        let host = store.host_id().unwrap();
        assert_eq!(store.host_id().unwrap(), host, "the host id is made once");
        assert!(store.active().unwrap().is_none());
        store.save(&credentials("oaiapp_b", 3600)).unwrap();
        store.save(&Credentials { email: Some("a@example.com".into()), ..credentials("oaiapp_a", 3600) }).unwrap();
        store.set_active(Some("oaiapp_b")).unwrap();
        assert_eq!(store.active().unwrap().unwrap().client_id, "oaiapp_b");
        let ids: Vec<String> = store.accounts().unwrap().into_iter().map(|account| account.client_id).collect();
        assert_eq!(ids, ["oaiapp_a", "oaiapp_b"]);
        assert!(store.save(&credentials("../escape", 1)).is_err());
        assert!(store.set_active(Some("a/b")).is_err());
        store.set_active(None).unwrap();
        store.set_active(None).unwrap();
        assert!(store.active().unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    fn token_server(calls: Arc<AtomicUsize>, reply: &'static str, status: u16) -> impl HttpClient {
        move |request: HttpRequest| {
            calls.fetch_add(1, Ordering::SeqCst);
            assert!(request.body_text().contains("grant_type=refresh_token"), "{}", request.body_text());
            Ok(HttpResponse::new(status, reply))
        }
    }

    #[test]
    fn an_expiring_token_is_refreshed_once_and_saved() {
        let dir = scratch_dir("session");
        let store = CredentialStore::open(&dir).unwrap();
        store.save(&credentials("oaiapp_1", 60)).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let http = token_server(calls.clone(), r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":3600}"#, 200);
        let session = store.session("oaiapp_1", OAuthConfig::default());
        assert_eq!(session.bearer(&http).unwrap().access_token, "at-2");
        assert_eq!(session.bearer(&http).unwrap().access_token, "at-2");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let saved = store.load("oaiapp_1").unwrap().unwrap();
        assert_eq!(saved.refresh_token.as_deref(), Some("rt-2"), "the rotated refresh token is saved");
        // Another process's session sees the saved token and does not refresh again.
        assert_eq!(store.session("oaiapp_1", OAuthConfig::default()).bearer(&http).unwrap().access_token, "at-2");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_rejected_token_is_refreshed_unless_another_process_already_did() {
        let dir = scratch_dir("rejected");
        let store = CredentialStore::open(&dir).unwrap();
        store.save(&credentials("oaiapp_1", 3600)).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let http = token_server(calls.clone(), r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":3600}"#, 200);
        let session = store.session("oaiapp_1", OAuthConfig::default());
        let first = session.bearer(&http).unwrap();
        assert_eq!(first.access_token, "at-1");
        assert_eq!(session.after_rejection(&http, &first).unwrap().access_token, "at-2");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let other = store.session("oaiapp_1", OAuthConfig::default());
        assert_eq!(other.after_rejection(&http, &first).unwrap().access_token, "at-2", "the saved token is newer");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_refreshes_spend_the_refresh_token_once() {
        let dir = scratch_dir("concurrent");
        let store = CredentialStore::open(&dir).unwrap();
        store.save(&credentials("oaiapp_1", 0)).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let tokens: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let (store, calls) = (store.clone(), calls.clone());
                    scope.spawn(move || {
                        let http =
                            token_server(calls, r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":3600}"#, 200);
                        store.session("oaiapp_1", OAuthConfig::default()).bearer(&http).unwrap().access_token
                    })
                })
                .collect();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect()
        });
        assert!(tokens.iter().all(|token| token == "at-2"));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the lock lets one refresh through");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unusable_refresh_token_signs_out_but_a_busy_server_does_not() {
        let dir = scratch_dir("unusable");
        let store = CredentialStore::open(&dir).unwrap();
        store.save(&credentials("oaiapp_1", 0)).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let busy = token_server(calls.clone(), "busy", 503);
        let session = store.session("oaiapp_1", OAuthConfig::default());
        assert_eq!(session.bearer(&busy).unwrap_err().recovery(), Recovery::RetryLater);
        assert!(store.load("oaiapp_1").unwrap().unwrap().is_signed_in(), "a temporary failure keeps the tokens");
        let expired = token_server(calls, r#"{"error":"refresh_token_expired"}"#, 400);
        assert!(matches!(session.bearer(&expired).unwrap_err(), Error::SignInRequired { .. }));
        let kept = store.load("oaiapp_1").unwrap().unwrap();
        assert!(!kept.is_signed_in() && kept.client_id == "oaiapp_1", "the registration is kept, its tokens are not");
        assert!(matches!(session.bearer(&expired).unwrap_err(), Error::SignInRequired { .. }));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_sign_in_without_plan_usage_is_not_used_for_inference() {
        let dir = scratch_dir("noplan");
        let store = CredentialStore::open(&dir).unwrap();
        store.save(&Credentials { scopes: vec!["openid".into(), "email".into()], ..credentials("oaiapp_1", 3600) }).unwrap();
        let never = |_: HttpRequest| -> std::result::Result<HttpResponse, crate::TransportError> { panic!("no request") };
        let error = store.session("oaiapp_1", OAuthConfig::default()).bearer(&never).unwrap_err();
        assert!(matches!(error, Error::PlanUsageNotEnabled));
        assert_eq!(error.recovery(), Recovery::EnablePlanUsage);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn signing_out_revokes_then_clears_and_says_when_revocation_failed() {
        let dir = scratch_dir("signout");
        let store = CredentialStore::open(&dir).unwrap();
        store.save(&credentials("oaiapp_1", 3600)).unwrap();
        let revoked = |request: HttpRequest| {
            assert!(request.url.ends_with("/oauth/revoke"));
            assert_eq!(request.body_text(), "token=rt-1&token_type_hint=refresh_token&client_id=oaiapp_1");
            Ok(HttpResponse::new(200, ""))
        };
        let outcome = store.sign_out(&revoked, &OAuthConfig::default(), "oaiapp_1").unwrap();
        assert!(outcome.revoked && outcome.error.is_none());
        assert!(!store.load("oaiapp_1").unwrap().unwrap().is_signed_in());

        store.save(&credentials("oaiapp_2", 3600)).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let down = move |_: HttpRequest| -> std::result::Result<HttpResponse, crate::TransportError> {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(HttpResponse::new(503, "down"))
        };
        let outcome = store.sign_out(&down, &OAuthConfig::default(), "oaiapp_2").unwrap();
        assert!(!outcome.revoked && outcome.error.is_some());
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "a temporary failure is retried with backoff");
        assert!(!store.load("oaiapp_2").unwrap().unwrap().is_signed_in(), "tokens are cleared either way");
        let value: Value = serde_json::from_str(&fs::read_to_string(dir.join("accounts/oaiapp_2.json")).unwrap()).unwrap();
        assert!(value.get("refresh_token").is_none());
        fs::remove_dir_all(dir).unwrap();
    }
}
