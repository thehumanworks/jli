//! Live checks against the real backends. They spend plan usage, so they are ignored by default:
//!
//! ```sh
//! cargo test -p chatgpt-sdk --test live -- --ignored --nocapture
//! ```
//!
//! They use the Codex CLI's sign-in (`codex login`, in `$CODEX_HOME/auth.json`), and the model in
//! `CHATGPT_SDK_LIVE_MODEL`, `gpt-6-luna` by default.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use chatgpt_sdk::{Backend, Client, CodexAuth, InputItem, ReasoningEffort, ResponseRequest, StreamEvent};
use serde_json::{json, Value};

fn model() -> String {
    std::env::var("CHATGPT_SDK_LIVE_MODEL").unwrap_or_else(|_| "gpt-6-luna".into())
}

fn client() -> Client {
    let auth = CodexAuth::open_default().expect("Codex must be signed in with ChatGPT: run `codex login`");
    Client::builder(auth).originator("chatgpt_sdk_live_test").timeout(Duration::from_secs(60)).build()
}

#[test]
#[ignore = "spends ChatGPT plan usage"]
fn the_model_list_includes_the_live_model() {
    let client = client();
    assert_eq!(client.backend(), Backend::Codex);
    let models = client.models().unwrap();
    let slugs: Vec<&str> = models.iter().filter(|model| model.is_listed()).map(|model| model.slug.as_str()).collect();
    println!("listed models: {slugs:?}");
    assert!(slugs.contains(&model().as_str()), "{} is not in {slugs:?}", model());
}

#[test]
#[ignore = "spends ChatGPT plan usage"]
fn a_structured_request_completes() {
    let request = ResponseRequest::new(model())
        .instructions("Propose one shell command for the request. Reply with JSON only.")
        .input(InputItem::user("show me the newest files in this directory"))
        .reasoning_effort(ReasoningEffort::None)
        .json_schema(
            "proposal",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "risk": { "type": "string", "enum": ["safe", "modifies", "destructive"] },
                },
                "required": ["command", "risk"],
                "additionalProperties": false,
            }),
        );
    let started = Instant::now();
    let completion = client().create(&request).unwrap();
    let reply: Value = completion.json().unwrap();
    println!(
        "{} in {} ms, request {:?}: {reply} (usage {:?})",
        completion.model.as_deref().unwrap_or("?"),
        started.elapsed().as_millis(),
        completion.request_id,
        completion.usage
    );
    assert!(reply["command"].as_str().is_some_and(|command| !command.is_empty()));
    assert_eq!(reply["risk"], "safe");
}

#[test]
#[ignore = "spends ChatGPT plan usage"]
fn text_streams_as_deltas() {
    let request = ResponseRequest::new(model())
        .input(InputItem::user("Say exactly: Hello, world!"))
        .reasoning_effort(ReasoningEffort::None);
    let mut deltas = 0;
    let mut accumulator = chatgpt_sdk::Accumulator::default();
    for event in client().stream(&request).unwrap() {
        let event = event.unwrap();
        if matches!(event, StreamEvent::OutputTextDelta { .. }) {
            deltas += 1;
        }
        accumulator.push(event);
    }
    let completion = accumulator.finish().unwrap();
    println!("{deltas} deltas: {:?}", completion.text);
    assert!(deltas >= 1);
    assert!(completion.text.contains("Hello, world!"), "{}", completion.text);
}

#[test]
#[ignore = "spends ChatGPT plan usage"]
fn an_unknown_model_is_a_typed_error() {
    let request = ResponseRequest::new("gpt-does-not-exist").input(InputItem::user("hi"));
    let error = client().create(&request).unwrap_err();
    println!("{error} ({:?})", error.recovery());
    assert_eq!(error.recovery(), chatgpt_sdk::Recovery::FixRequest);
}
