//! The whole sign-in, offline: `testing::FakeChatGpt` plays the browser, the authorization server
//! and the Responses API, and the real loopback listener receives the callback.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chatgpt_sdk::auth::{AuthorizationRequest, LoopbackListener, Registration};
use chatgpt_sdk::testing::{FakeChatGpt, FakeReply};
use chatgpt_sdk::{
    Client, Credential, CredentialStore, Credentials, Error, HttpRequest, HttpResponse, InputItem, Recovery,
    ResponseRequest, SignInError, TransportError,
};

fn scratch(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("chatgpt-sdk-it-{label}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Sign in through the browser, and return what the authorization URL asked for with the result.
fn sign_in(
    store: &CredentialStore,
    fake: &Arc<FakeChatGpt>,
    registration: Registration,
) -> (Vec<(String, String)>, chatgpt_sdk::Result<Credentials>) {
    let listener = LoopbackListener::bind_port(0).unwrap().app_name("test app");
    let request =
        AuthorizationRequest::new(fake.config(), registration, store.host_id().unwrap(), listener.redirect_uri()).unwrap();
    let browser = fake.browse(&request.url());
    let callback = listener.wait(request.state(), Duration::from_secs(10));
    let query = browser.join().unwrap();
    (query, callback.and_then(|callback| request.complete(&fake.http(), &callback)))
}

fn ask(client: &Client) -> chatgpt_sdk::Result<String> {
    Ok(client.create(&ResponseRequest::new("gpt-6-luna").input(InputItem::user("hi")))?.text)
}

#[test]
fn a_new_account_registers_signs_in_and_runs_a_request() {
    let dir = scratch("new");
    let store = CredentialStore::open(&dir).unwrap();
    let fake = FakeChatGpt::new();
    fake.issue_client_id("oaiapp_new");
    let (query, signed_in) = sign_in(&store, &fake, Registration::new("test app"));
    let credentials = signed_in.unwrap();
    assert!(query.contains(&("client_id".into(), "dynamic_agent_client".into())));
    assert!(query.contains(&("agent_name_hint".into(), "test app".into())));
    assert_eq!((credentials.client_id.as_str(), credentials.subject.as_str()), ("oaiapp_new", "user-fake"));
    assert_eq!(credentials.email.as_deref(), Some("user@example.com"));
    assert_eq!(credentials.ext_agent_host_id, store.host_id().unwrap().to_string());
    assert!(credentials.plan_usage_enabled());
    store.save(&credentials).unwrap();
    store.set_active(Some(&credentials.client_id)).unwrap();

    fake.reply("Hello");
    let client = Client::builder(store.session("oaiapp_new", fake.config())).http_client(fake.http()).build();
    assert_eq!(ask(&client).unwrap(), "Hello");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("accounts/oaiapp_new.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_returning_account_reuses_its_client_and_host_and_must_be_the_same_person() {
    let dir = scratch("returning");
    let store = CredentialStore::open(&dir).unwrap();
    let fake = FakeChatGpt::new();
    let first = sign_in(&store, &fake, Registration::new("test app")).1.unwrap();
    store.save(&first).unwrap();

    let (query, second) = sign_in(&store, &fake, Registration::returning(&first));
    let names: Vec<&str> = query.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"id_token_hint") && names.contains(&"login_hint") && !names.contains(&"agent_name_hint"));
    assert!(query.contains(&("client_id".into(), first.client_id.clone())));
    assert!(query.contains(&("ext_agent_host_id".into(), first.ext_agent_host_id.clone())));
    assert_eq!(second.unwrap().client_id, first.client_id, "a callback without client_id keeps the saved one");

    fake.sign_in_as("someone-else", "other@example.com");
    let error = sign_in(&store, &fake, Registration::returning(&first)).1.unwrap_err();
    assert!(matches!(error, Error::SignIn(SignInError::AccountMismatch)), "{error}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_declined_plan_keeps_the_sign_in_but_not_for_inference() {
    let dir = scratch("declined");
    let store = CredentialStore::open(&dir).unwrap();
    let fake = FakeChatGpt::new();
    fake.grant("email offline_access openid profile");
    let credentials = sign_in(&store, &fake, Registration::new("test app")).1.unwrap();
    assert!(!credentials.plan_usage_enabled());
    store.save(&credentials).unwrap();
    let never = |_: HttpRequest| -> Result<HttpResponse, TransportError> { panic!("no inference without plan usage") };
    let error = store.session(&credentials.client_id, fake.config()).bearer(&never).unwrap_err();
    assert_eq!(error.recovery(), Recovery::EnablePlanUsage);

    // Enabling it later: the saved client, the whole scope set, and the consent screen again.
    let request = AuthorizationRequest::new(
        fake.config(),
        Registration::returning(&credentials),
        store.host_id().unwrap(),
        "http://127.0.0.1:1/auth/callback",
    )
    .unwrap()
    .prompt_consent();
    let url = request.url();
    assert!(
        url.contains("client_id=oaiapp_fake")
            && url.contains("chatgpt.tokens.use.direct")
            && url.ends_with("prompt=consent")
    );

    fake.deny_next();
    let denied = sign_in(&store, &fake, Registration::returning(&credentials)).1.unwrap_err();
    assert!(matches!(denied, Error::SignIn(SignInError::Denied)), "{denied}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_rejected_token_is_refreshed_and_the_rotation_is_saved() {
    let dir = scratch("refresh");
    let store = CredentialStore::open(&dir).unwrap();
    let fake = FakeChatGpt::new();
    let credentials = sign_in(&store, &fake, Registration::new("test app")).1.unwrap();
    store.save(&credentials).unwrap();
    let client = Client::builder(store.session(&credentials.client_id, fake.config())).http_client(fake.http()).build();
    assert_eq!(ask(&client).unwrap(), "ok");

    fake.expire_access_token();
    assert_eq!(ask(&client).unwrap(), "ok", "a 401 refreshes the token and retries once");
    let saved = store.load(&credentials.client_id).unwrap().unwrap();
    assert_ne!(saved.refresh_token, credentials.refresh_token, "the rotated refresh token is saved");
    assert_ne!(saved.access_token, credentials.access_token);
    let grants: Vec<String> = fake
        .requests()
        .iter()
        .map(HttpRequest::body_text)
        .filter(|body| body.contains("grant_type=refresh_token"))
        .collect();
    assert_eq!(grants.len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn signing_out_revokes_the_session_and_ends_inference() {
    let dir = scratch("signout");
    let store = CredentialStore::open(&dir).unwrap();
    let fake = FakeChatGpt::new();
    let credentials = sign_in(&store, &fake, Registration::new("test app")).1.unwrap();
    store.save(&credentials).unwrap();
    let outcome = store.sign_out(&fake.http(), &fake.config(), &credentials.client_id).unwrap();
    assert!(outcome.revoked);
    assert_eq!(fake.revoked(), vec![credentials.refresh_token.clone().unwrap()]);
    let client = Client::builder(store.session(&credentials.client_id, fake.config())).http_client(fake.http()).build();
    assert_eq!(ask(&client).unwrap_err().recovery(), Recovery::SignInAgain);
    let kept = store.load(&credentials.client_id).unwrap().unwrap();
    assert_eq!(kept.client_id, credentials.client_id, "the registration is kept for the next sign-in");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_usage_limit_mid_stream_says_to_manage_usage() {
    let dir = scratch("limit");
    let store = CredentialStore::open(&dir).unwrap();
    let fake = FakeChatGpt::new();
    let credentials = sign_in(&store, &fake, Registration::new("test app")).1.unwrap();
    store.save(&credentials).unwrap();
    let client = Client::builder(store.session(&credentials.client_id, fake.config())).http_client(fake.http()).build();
    fake.script(FakeReply::Failed("subscription_sharing_usage_limit_exceeded".into()));
    let error = ask(&client).unwrap_err();
    assert_eq!(error.recovery(), Recovery::ManageUsage);
    assert!(error.request_id().is_some_and(|id| id.starts_with("fake-request-")));
    fake.script(FakeReply::Status(
        403,
        r#"{"error":{"code":"subscription_sharing_user_not_eligible","message":"no"}}"#.into(),
    ));
    assert_eq!(ask(&client).unwrap_err().recovery(), Recovery::NotEligible);
    std::fs::remove_dir_all(dir).unwrap();
}
