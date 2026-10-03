//! Stream an answer on the ChatGPT plan the Codex CLI is signed in with.
//!
//! ```sh
//! codex login        # once
//! cargo run -p chatgpt-sdk --example codex -- gpt-6-luna "Write a haiku about Rust."
//! ```

use std::io::Write;

use chatgpt_sdk::{
    Accumulator, Client, CodexAuth, InputItem, ReasoningEffort, Recovery, ResponseRequest, StreamEvent, MANAGE_USAGE_URL,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let model = args.next().unwrap_or_else(|| "gpt-6-luna".into());
    let prompt = args.next().unwrap_or_else(|| "Say hello.".into());

    let auth = CodexAuth::open_default()?;
    let account = auth.account()?;
    eprintln!(
        "Using ChatGPT plan · {} · {}",
        account.email.as_deref().unwrap_or("?"),
        account.plan_type.as_deref().unwrap_or("?")
    );
    let client = Client::new(auth);

    let request = ResponseRequest::new(model).input(InputItem::user(prompt)).reasoning_effort(ReasoningEffort::None);
    let mut accumulator = Accumulator::default();
    let stream = match client.stream(&request) {
        Ok(stream) => stream,
        Err(error) if error.recovery() == Recovery::ManageUsage => {
            return Err(format!("usage limit reached: manage usage at {MANAGE_USAGE_URL}").into());
        }
        Err(error) => return Err(error.into()),
    };
    for event in stream {
        let event = event?;
        if let StreamEvent::OutputTextDelta { delta, .. } = &event {
            print!("{delta}");
            std::io::stdout().flush()?;
        }
        accumulator.push(event);
    }
    let completion = accumulator.finish()?;
    println!();
    if let Some(usage) = completion.usage {
        eprintln!("{} tokens in, {} out", usage.input_tokens, usage.output_tokens);
    }
    Ok(())
}
