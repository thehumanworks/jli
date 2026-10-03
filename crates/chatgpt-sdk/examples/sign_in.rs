//! Sign in with ChatGPT in the browser, save the credentials, then ask a model one question.
//!
//! ```sh
//! cargo run -p chatgpt-sdk --example sign_in -- ~/.config/my-app/chatgpt "Why is the sky blue?"
//! ```

use std::time::Duration;

use chatgpt_sdk::auth::{AuthorizationRequest, LoopbackListener, OAuthConfig, Registration};
use chatgpt_sdk::{Client, CredentialStore, InputItem, ReasoningEffort, ResponseRequest, UreqClient, MANAGE_USAGE_URL};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().ok_or("usage: sign_in <credentials dir> [question]")?;
    let question = args.next().unwrap_or_else(|| "Say hello in five words.".into());

    let store = CredentialStore::open(dir)?;
    let http = UreqClient::new();
    let saved = store.active()?;
    let account = match saved.filter(|saved| saved.is_signed_in() && saved.plan_usage_enabled()) {
        Some(account) => account,
        None => {
            let listener = LoopbackListener::bind()?.app_name("chatgpt-sdk example");
            let registration = match store.active()? {
                Some(saved) => Registration::returning(&saved),
                None => Registration::new("chatgpt-sdk example"),
            };
            let request =
                AuthorizationRequest::new(OAuthConfig::default(), registration, store.host_id()?, listener.redirect_uri())?;
            println!("Continue with ChatGPT: open this page in your browser\n\n  {}\n", request.url());
            let callback = listener.wait(request.state(), Duration::from_secs(300))?;
            let credentials = request.complete(&http, &callback)?;
            store.save(&credentials)?;
            store.set_active(Some(&credentials.client_id))?;
            if !credentials.plan_usage_enabled() {
                return Err("signed in, but ChatGPT plan usage was not allowed".into());
            }
            println!("You're using your ChatGPT plan. Manage usage at {MANAGE_USAGE_URL}\n");
            credentials
        }
    };

    let client =
        Client::builder(store.session(account.client_id, OAuthConfig::default())).originator("chatgpt_sdk_example").build();
    let models = client.models()?;
    let model = models.iter().find(|model| model.is_listed()).ok_or("the account lists no models")?;
    println!("Using ChatGPT plan · {}", model.name());
    let request = ResponseRequest::new(&model.slug).input(InputItem::user(question)).reasoning_effort(ReasoningEffort::Low);
    println!("{}", client.create(&request)?.text);
    Ok(())
}
