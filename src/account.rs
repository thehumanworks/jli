//! The ChatGPT account jli uses, if any: `jli login`, `jli logout`, `jli models`, and which model
//! runs. Requests themselves go through `llm.rs` (ADR 25).
//!
//! Everything is kept on this host, in `~/.jli/chatgpt` (`JLI_CHATGPT_DIR`), never in the brain:
//! a sign-in is a credential, and another host may have no account at all.
//!
//!   host-id, active, accounts/<client id>.json   chatgpt-sdk's credential store
//!   jli.json                                     which sign-in jli uses, its model, and whether
//!                                                the first-sign-in message was shown

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use chatgpt_sdk::auth::{AuthorizationRequest, LoopbackListener, OAuthConfig, Registration};
use chatgpt_sdk::{Client, CodexAuth, Credential, CredentialStore, Credentials, HttpClient, HttpRequest, Model, Recovery};
use serde::{Deserialize, Serialize};

/// Where a person reviews and limits what jli uses of their plan.
pub use chatgpt_sdk::MANAGE_USAGE_URL;

use crate::llm::ChatGpt;
use crate::types::{coded, CONFIGURATION, MODEL, USAGE};
use crate::ui::Ui;

/// The name jli registers under, and sends as its `originator`.
pub const APP_NAME: &str = "jli";
/// The model the agent and the translator use on a ChatGPT plan until `jli config model` picks one.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";
/// How long `jli login` waits for the browser.
const SIGN_IN_WAIT: Duration = Duration::from_secs(300);

/// Which ChatGPT sign-in jli uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// jli's own, from `jli login`: the public Responses API.
    SignIn,
    /// The Codex CLI's, from `jli login --codex`: the Codex backend.
    Codex,
}

/// `jli.json`: what jli chose. A field this version does not know is dropped on the next write;
/// the file is this host's and holds no lesson.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether "You're using your ChatGPT plan" was shown: it is shown after the first sign-in only.
    #[serde(default)]
    pub welcomed: bool,
}

/// What `jli login` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub source: Source,
    pub email: Option<String>,
    /// Whether jli may use the plan. Without it, jli keeps using what it used before.
    pub plan_usage: bool,
    /// The first sign-in with plan usage: say what that means, once.
    pub welcome: bool,
    pub plan_type: Option<String>,
    pub model: String,
}

/// What `jli logout` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedOut {
    pub source: Option<Source>,
    pub email: Option<String>,
    /// Whether the server confirmed the session is revoked. Always true for Codex's sign-in, which
    /// jli only stops using.
    pub revoked: bool,
}

/// jli's ChatGPT sign-ins on this host.
pub struct Accounts {
    dir: PathBuf,
    codex: Option<PathBuf>,
    config: OAuthConfig,
    http: Arc<dyn HttpClient>,
    /// The loopback port tried first; tests use 0.
    port: u16,
}

impl Accounts {
    /// `JLI_CHATGPT_DIR`, or `~/.jli/chatgpt`, and Codex's sign-in in `$CODEX_HOME`.
    pub fn from_env() -> Result<Accounts> {
        let dir = match std::env::var_os("JLI_CHATGPT_DIR").filter(|dir| !dir.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => {
                let home = std::env::var_os("HOME").ok_or_else(|| coded(CONFIGURATION, "HOME is not set"))?;
                PathBuf::from(home).join(".jli").join("chatgpt")
            }
        };
        Ok(Accounts {
            dir,
            codex: CodexAuth::default_path(),
            config: OAuthConfig::default(),
            http: Arc::new(chatgpt_sdk::UreqClient::new()),
            port: LoopbackListener::DEFAULT_PORT,
        })
    }

    /// Accounts in `dir` that talk to `fake` instead of OpenAI.
    #[cfg(test)]
    pub fn fake(dir: PathBuf, fake: &Arc<chatgpt_sdk::testing::FakeChatGpt>, codex: Option<PathBuf>) -> Accounts {
        Accounts { dir, codex, config: fake.config(), http: Arc::new(fake.http()), port: 0 }
    }

    fn store(&self) -> Result<CredentialStore> {
        Ok(CredentialStore::open(&self.dir)?)
    }

    fn choice_path(&self) -> PathBuf {
        self.dir.join("jli.json")
    }

    pub fn choice(&self) -> Result<Choice> {
        match std::fs::read_to_string(self.choice_path()) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|error| anyhow!("reading {}: {error}", self.choice_path().display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Choice::default()),
            Err(error) => Err(anyhow!("reading {}: {error}", self.choice_path().display())),
        }
    }

    fn save_choice(&self, choice: &Choice) -> Result<()> {
        // Opening the store makes the directory first, readable by its owner only.
        self.store()?;
        let temporary = self.dir.join(format!(".jli.json.{}.tmp", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec_pretty(choice)?)?;
        std::fs::rename(&temporary, self.choice_path())?;
        Ok(())
    }

    /// The HTTP client every request shares, as chatgpt-sdk takes it.
    fn http(&self) -> impl HttpClient + 'static {
        let http = self.http.clone();
        move |request: HttpRequest| http.send(request)
    }

    fn client(&self, credential: impl Credential + 'static) -> Client {
        Client::builder(credential).http_client(self.http()).originator(APP_NAME).build()
    }

    fn codex_auth(&self) -> Result<CodexAuth> {
        let path = self
            .codex
            .clone()
            .ok_or_else(|| coded(CONFIGURATION, "Cannot find Codex's sign-in: set CODEX_HOME or HOME."))?;
        CodexAuth::open(&path).map_err(|error| coded(CONFIGURATION, format!("{error}")))
    }

    /// The ChatGPT plan jli is set to use, or none. A sign-in that has ended is an error, not a
    /// quiet switch to another way of paying.
    pub fn provider(&self) -> Result<Option<ChatGpt>> {
        let choice = self.choice()?;
        let model = choice.model.clone().unwrap_or_else(|| DEFAULT_MODEL.into());
        match choice.source {
            None => Ok(None),
            Some(Source::Codex) => {
                let auth = self.codex_auth().map_err(|error| {
                    coded(
                        CONFIGURATION,
                        format!(
                            "jli is set to use Codex's ChatGPT sign-in, but {error:#} Or run `jli logout` to stop using it."
                        ),
                    )
                })?;
                let email = auth.account().ok().and_then(|account| account.email);
                Ok(Some(ChatGpt { client: self.client(auth), model, email, sign_in: "codex login" }))
            }
            Some(Source::SignIn) => {
                let store = self.store()?;
                let ended = || {
                    coded(CONFIGURATION, "Your ChatGPT sign-in has ended. Run `jli login` to sign in again, or `jli logout` to stop using it.")
                };
                let active = store.active()?.filter(Credentials::is_signed_in).ok_or_else(ended)?;
                if !active.plan_usage_enabled() {
                    return Err(coded(
                        CONFIGURATION,
                        "jli may not use your ChatGPT plan. Run `jli login` to allow it, or `jli logout`.",
                    ));
                }
                let session = store.session(active.client_id.clone(), self.config.clone());
                Ok(Some(ChatGpt { client: self.client(session), model, email: active.email, sign_in: "jli login" }))
            }
        }
    }

    /// Continue with ChatGPT in the browser: the saved account again, or with `another`, a ChatGPT
    /// account this host has not used. `open` opens a URL and says whether it could.
    pub fn login(&self, ui: &Ui, open: &dyn Fn(&str) -> bool, another: bool) -> Result<SignedIn> {
        let store = self.store()?;
        let saved = if another { None } else { store.active()? };
        let registration = match &saved {
            Some(saved) => Registration::returning(saved),
            None => Registration::new(APP_NAME),
        };
        let listener = LoopbackListener::bind_port(self.port)
            .or_else(|_| LoopbackListener::bind_port(0))
            .map_err(|error| coded(CONFIGURATION, format!("{error}")))?
            .app_name(APP_NAME);
        let mut request =
            AuthorizationRequest::new(self.config.clone(), registration, store.host_id()?, listener.redirect_uri())?;
        // Asked again only for someone who declined plan usage before and now runs `jli login` to allow it.
        if saved.as_ref().is_some_and(|saved| !saved.plan_usage_enabled()) {
            request = request.prompt_consent();
        }
        let url = request.url();
        if open(&url) {
            ui.note("Continue with ChatGPT in the browser that just opened.");
        } else {
            ui.note(format!("Open this page to continue with ChatGPT:\n  {url}"));
        }
        let failed = |error: chatgpt_sdk::Error| coded(CONFIGURATION, format!("Sign-in did not complete: {error}"));
        let callback = listener.wait(request.state(), SIGN_IN_WAIT).map_err(failed)?;
        let credentials = request.complete(&self.http(), &callback).map_err(failed)?;
        store.save(&credentials)?;
        store.set_active(Some(&credentials.client_id))?;

        let mut choice = self.choice()?;
        let plan_usage = credentials.plan_usage_enabled();
        let welcome = plan_usage && !choice.welcomed;
        if plan_usage {
            choice.source = Some(Source::SignIn);
            choice.welcomed = true;
        }
        self.save_choice(&choice)?;
        Ok(SignedIn {
            source: Source::SignIn,
            email: credentials.email,
            plan_usage,
            welcome,
            plan_type: None,
            model: choice.model.unwrap_or_else(|| DEFAULT_MODEL.into()),
        })
    }

    /// Use the ChatGPT account the Codex CLI is signed in with. Codex's file is read, never
    /// changed here.
    pub fn login_codex(&self) -> Result<SignedIn> {
        let auth = self.codex_auth().map_err(|error| coded(CONFIGURATION, format!("{error:#}")))?;
        let account = auth.account()?;
        let mut choice = self.choice()?;
        let welcome = !choice.welcomed;
        choice.source = Some(Source::Codex);
        choice.welcomed = true;
        self.save_choice(&choice)?;
        Ok(SignedIn {
            source: Source::Codex,
            email: account.email,
            plan_usage: true,
            welcome,
            plan_type: account.plan_type,
            model: choice.model.unwrap_or_else(|| DEFAULT_MODEL.into()),
        })
    }

    /// Stop using the ChatGPT plan. jli's own session is revoked and its tokens cleared; the
    /// registration and host id stay for the next `jli login`. Codex's sign-in is left as it is.
    pub fn logout(&self) -> Result<SignedOut> {
        let mut choice = self.choice()?;
        let source = choice.source.take();
        let outcome = match source {
            Some(Source::Codex) => {
                let email = self.codex_auth().ok().and_then(|auth| auth.account().ok()).and_then(|account| account.email);
                SignedOut { source, email, revoked: true }
            }
            _ => {
                let store = self.store()?;
                match store.active()?.filter(Credentials::is_signed_in) {
                    Some(active) => {
                        let ended = store.sign_out(&self.http(), &self.config, &active.client_id)?;
                        SignedOut { source: Some(Source::SignIn), email: active.email, revoked: ended.revoked }
                    }
                    None => SignedOut { source, email: None, revoked: true },
                }
            }
        };
        self.save_choice(&choice)?;
        Ok(outcome)
    }

    /// The models the plan lists, and the one in use.
    pub fn models(&self) -> Result<(Vec<Model>, String)> {
        let provider = self.provider()?.ok_or_else(|| {
            coded(
                CONFIGURATION,
                "jli is not using a ChatGPT plan: run `jli login`, or `jli login --codex` to use Codex's sign-in.",
            )
        })?;
        let models = provider.client.models().map_err(|error| {
            let code = if error.recovery() == Recovery::SignInAgain { CONFIGURATION } else { MODEL };
            coded(code, format!("Could not list your ChatGPT plan's models: {error}"))
        })?;
        Ok((models.into_iter().filter(Model::is_listed).collect(), provider.model))
    }

    /// Run `slug` on the plan, once the plan's list says it has it.
    pub fn set_model(&self, slug: &str) -> Result<()> {
        let (models, _) = self.models()?;
        if !models.iter().any(|model| model.slug == slug) {
            let names: Vec<&str> = models.iter().map(|model| model.slug.as_str()).collect();
            return Err(coded(USAGE, format!("{slug} is not a model of your ChatGPT plan. It has {}.", names.join(", "))));
        }
        let mut choice = self.choice()?;
        choice.model = Some(slug.to_string());
        self.save_choice(&choice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatgpt_sdk::testing::FakeChatGpt;
    use serde_json::{json, Value};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("jli-account-{label}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn browser(fake: &Arc<FakeChatGpt>) -> impl Fn(&str) -> bool {
        let fake = fake.clone();
        move |url: &str| {
            fake.browse(url);
            true
        }
    }

    fn quiet() -> Ui {
        Ui::new(false)
    }

    #[test]
    fn a_first_sign_in_uses_the_plan_and_says_so_once() {
        let scratch = Scratch::new("login");
        let fake = FakeChatGpt::new();
        let accounts = Accounts::fake(scratch.0.join("chatgpt"), &fake, None);
        assert!(accounts.provider().unwrap().is_none(), "nothing is used before a sign-in");

        let first = accounts.login(&quiet(), &browser(&fake), false).unwrap();
        assert_eq!((first.source, first.plan_usage, first.welcome), (Source::SignIn, true, true));
        assert_eq!((first.email.as_deref(), first.model.as_str()), (Some("user@example.com"), DEFAULT_MODEL));
        let provider = accounts.provider().unwrap().unwrap();
        assert_eq!((provider.model.as_str(), provider.sign_in), (DEFAULT_MODEL, "jli login"));
        fake.reply("hello");
        let request = chatgpt_sdk::ResponseRequest::new(DEFAULT_MODEL);
        assert_eq!(provider.client.create(&request).unwrap().text, "hello");
        let sent = fake.requests().last().cloned().unwrap();
        assert_eq!(sent.header_value("originator"), Some(APP_NAME));

        let opened = Arc::new(std::sync::Mutex::new(String::new()));
        let record = opened.clone();
        let open = {
            let fake = fake.clone();
            move |url: &str| {
                *record.lock().unwrap() = url.to_string();
                fake.browse(url);
                true
            }
        };
        let again = accounts.login(&quiet(), &open, false).unwrap();
        assert!(!again.welcome, "the welcome is shown after the first sign-in only");
        let url = opened.lock().unwrap().clone();
        assert!(
            url.contains("client_id=oaiapp_fake&") && !url.contains("agent_name_hint"),
            "the saved client is reused: {url}"
        );
    }

    #[test]
    fn another_account_registers_beside_the_first_and_becomes_the_one_in_use() {
        let scratch = Scratch::new("another");
        let fake = FakeChatGpt::new();
        let accounts = Accounts::fake(scratch.0.clone(), &fake, None);
        accounts.login(&quiet(), &browser(&fake), false).unwrap();
        fake.sign_in_as("user-2", "two@example.com");
        fake.issue_client_id("oaiapp_second");
        let refused = accounts.login(&quiet(), &browser(&fake), false).unwrap_err();
        assert!(refused.to_string().contains("different ChatGPT account"), "{refused}");
        let second = accounts.login(&quiet(), &browser(&fake), true).unwrap();
        assert_eq!(second.email.as_deref(), Some("two@example.com"));
        assert_eq!(accounts.provider().unwrap().unwrap().email.as_deref(), Some("two@example.com"));
        let store = CredentialStore::open(&scratch.0).unwrap();
        let ids: Vec<String> = store.accounts().unwrap().into_iter().map(|account| account.client_id).collect();
        assert_eq!(ids, ["oaiapp_second", "oaiapp_fake"], "both registrations are kept, by email");
    }

    #[test]
    fn a_sign_in_without_plan_usage_changes_nothing_and_a_later_one_asks_again() {
        let scratch = Scratch::new("declined");
        let fake = FakeChatGpt::new();
        fake.grant("email openid profile offline_access");
        let accounts = Accounts::fake(scratch.0.clone(), &fake, None);
        let signed_in = accounts.login(&quiet(), &browser(&fake), false).unwrap();
        assert!(!signed_in.plan_usage && !signed_in.welcome);
        assert!(accounts.provider().unwrap().is_none(), "jli keeps what it used before");

        fake.grant("chatgpt.tokens.use.direct email offline_access openid profile resource.invoke");
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let record = seen.clone();
        let open = {
            let fake = fake.clone();
            move |url: &str| {
                *record.lock().unwrap() = url.to_string();
                fake.browse(url);
                true
            }
        };
        assert!(accounts.login(&quiet(), &open, false).unwrap().plan_usage);
        assert!(seen.lock().unwrap().ends_with("prompt=consent"), "consent is asked for again");
        assert!(accounts.provider().unwrap().is_some());
    }

    #[test]
    fn a_declined_or_interrupted_sign_in_is_a_configuration_error() {
        let scratch = Scratch::new("denied");
        let fake = FakeChatGpt::new();
        fake.deny_next();
        let accounts = Accounts::fake(scratch.0.clone(), &fake, None);
        let error = accounts.login(&quiet(), &browser(&fake), false).unwrap_err();
        assert_eq!(crate::types::error_code(&error), CONFIGURATION);
        assert!(error.to_string().contains("declined"), "{error}");
        assert_eq!(accounts.choice().unwrap(), Choice::default());
    }

    #[test]
    fn logout_revokes_and_an_ended_sign_in_is_never_a_quiet_switch() {
        let scratch = Scratch::new("logout");
        let fake = FakeChatGpt::new();
        let accounts = Accounts::fake(scratch.0.clone(), &fake, None);
        accounts.login(&quiet(), &browser(&fake), false).unwrap();
        let out = accounts.logout().unwrap();
        assert_eq!((out.source, out.revoked, out.email.as_deref()), (Some(Source::SignIn), true, Some("user@example.com")));
        assert_eq!(fake.revoked().len(), 1);
        assert!(accounts.provider().unwrap().is_none());
        assert!(accounts.logout().unwrap().source.is_none(), "a second logout has nothing to do");

        // A refresh that fails for good clears the session; jli then says so instead of moving on.
        accounts.login(&quiet(), &browser(&fake), false).unwrap();
        let store = CredentialStore::open(&scratch.0).unwrap();
        let active = store.active().unwrap().unwrap();
        store.save(&active.signed_out()).unwrap();
        let error = accounts.provider().err().unwrap();
        assert_eq!(crate::types::error_code(&error), CONFIGURATION);
        assert!(error.to_string().contains("jli login"), "{error}");
    }

    fn codex_file(dir: &std::path::Path) -> PathBuf {
        let fake = FakeChatGpt::new();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let exp = now + 3600;
        let access = fake.id_token(&json!({ "exp": exp, "client_id": "app_codex" }));
        let id = fake.id_token(
            &json!({ "email": "codex@example.com", "https://api.openai.com/auth": { "chatgpt_plan_type": "pro" } }),
        );
        let path = dir.join("auth.json");
        let file = json!({ "auth_mode": "chatgpt", "tokens": { "id_token": id, "access_token": access, "refresh_token": "rt", "account_id": "acct" } });
        std::fs::write(&path, file.to_string()).unwrap();
        path
    }

    #[test]
    fn codex_sign_in_is_used_and_let_go_of_without_being_changed() {
        let scratch = Scratch::new("codex");
        let path = codex_file(&scratch.0);
        let before = std::fs::read_to_string(&path).unwrap();
        let fake = FakeChatGpt::new();
        let accounts = Accounts::fake(scratch.0.join("chatgpt"), &fake, Some(path.clone()));
        let signed_in = accounts.login_codex().unwrap();
        assert_eq!((signed_in.source, signed_in.welcome), (Source::Codex, true));
        assert_eq!((signed_in.email.as_deref(), signed_in.plan_type.as_deref()), (Some("codex@example.com"), Some("pro")));
        let provider = accounts.provider().unwrap().unwrap();
        assert_eq!((provider.client.backend(), provider.sign_in), (chatgpt_sdk::Backend::Codex, "codex login"));
        let out = accounts.logout().unwrap();
        assert_eq!((out.source, out.revoked), (Some(Source::Codex), true));
        assert!(accounts.provider().unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "Codex's file is never written by logout");

        let missing = Accounts::fake(scratch.0.join("other"), &fake, Some(scratch.0.join("absent.json")));
        let error = missing.login_codex().unwrap_err();
        assert!(error.to_string().contains("codex login"), "{error}");
    }

    #[test]
    fn a_model_is_picked_from_the_plan_list() {
        let scratch = Scratch::new("model");
        let fake = FakeChatGpt::new();
        let accounts = Accounts::fake(scratch.0.clone(), &fake, None);
        assert!(accounts.models().is_err(), "no list without a sign-in");
        accounts.login(&quiet(), &browser(&fake), false).unwrap();
        let (models, current) = accounts.models().unwrap();
        let slugs: Vec<&str> = models.iter().map(|model| model.slug.as_str()).collect();
        assert_eq!(
            (slugs, current.as_str()),
            (vec!["gpt-6-luna", "gpt-6.1-sol"], DEFAULT_MODEL),
            "hidden models are left out"
        );
        accounts.set_model("gpt-6.1-sol").unwrap();
        assert_eq!(accounts.provider().unwrap().unwrap().model, "gpt-6.1-sol");
        let error = accounts.set_model("hidden-model").unwrap_err();
        assert_eq!(crate::types::error_code(&error), USAGE);
        let written: Value = serde_json::from_str(&std::fs::read_to_string(scratch.0.join("jli.json")).unwrap()).unwrap();
        assert_eq!(written, json!({ "source": "signin", "model": "gpt-6.1-sol", "welcomed": true }));
    }
}
