//! The only way this app talks to a language model: one request with a JSON Schema for the
//! reply, to the first of these that is set up.
//!
//!   JLI_LLM_API_KEY + JLI_LLM_BASE_URL           any OpenAI-compatible /chat/completions endpoint
//!   a ChatGPT plan (jli login, jli login --codex) a streamed Responses request through chatgpt-sdk
//!   OPENROUTER_API_KEY (+ OPENROUTER_BASE_URL)   OpenRouter
//!   AI_GATEWAY_API_KEY                           Vercel AI Gateway
//!
//! `JLI_PROVIDER` (chatgpt, openrouter, gateway or custom) picks one of them instead (ADR 28).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chatgpt_sdk::{InputItem, ReasoningEffort, Recovery, ResponseError, ResponseRequest, MANAGE_USAGE_URL};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::account::Accounts;
use crate::types::{coded, Slot, SlotKind, CONFIGURATION, MODEL};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: String,
}

fn clean(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

fn openrouter(var: &impl Fn(&str) -> Option<String>) -> Option<Endpoint> {
    let api_key = var("OPENROUTER_API_KEY")?;
    let base_url = var("OPENROUTER_BASE_URL").unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
    Some(Endpoint { base_url, api_key })
}

fn gateway(var: &impl Fn(&str) -> Option<String>) -> Option<Endpoint> {
    Some(Endpoint { base_url: "https://ai-gateway.vercel.sh/v1".into(), api_key: var("AI_GATEWAY_API_KEY")? })
}

fn custom(var: &impl Fn(&str) -> Option<String>) -> Option<Endpoint> {
    Some(Endpoint { api_key: var("JLI_LLM_API_KEY")?, base_url: var("JLI_LLM_BASE_URL")? })
}

/// Which API-key endpoint to call, from the environment `get` reads: the custom one, OpenRouter,
/// then the Vercel AI Gateway.
pub fn endpoint_from(get: impl Fn(&str) -> Option<String>) -> Result<Endpoint> {
    let var = |name: &str| clean(get(name));
    custom(&var).or_else(|| openrouter(&var)).or_else(|| gateway(&var)).ok_or_else(|| {
        coded(CONFIGURATION, "No language model found. Run `jli login` to use your ChatGPT plan, or set OPENROUTER_API_KEY or AI_GATEWAY_API_KEY.")
    })
}

/// The model the agent and the translator use with an API key.
pub const MERCURY: &str = "inception/mercury-2.5";

/// The user's ChatGPT plan, as `jli login` or `jli login --codex` set it up.
pub struct ChatGpt {
    pub client: chatgpt_sdk::Client,
    /// The model `jli config model` picked, or the default.
    pub model: String,
    pub email: Option<String>,
    /// What to run when the sign-in stops working: `jli login`, or `codex login`.
    pub sign_in: &'static str,
}

/// Where language model requests go.
pub enum Provider {
    /// An OpenAI-compatible `/chat/completions` endpoint with an API key.
    Compatible(Endpoint),
    /// The user's ChatGPT plan, through chatgpt-sdk.
    ChatGpt(ChatGpt),
}

/// The provider, from the environment `get` reads and the ChatGPT plan `chatgpt` finds: what
/// `JLI_PROVIDER` names, else the custom endpoint, the ChatGPT plan, OpenRouter and the gateway,
/// in that order. `chatgpt` is asked only when it is needed.
pub fn provider_from(
    get: impl Fn(&str) -> Option<String>,
    chatgpt: impl FnOnce() -> Result<Option<ChatGpt>>,
) -> Result<Provider> {
    let var = |name: &str| clean(get(name));
    let missing = |what: &str| coded(CONFIGURATION, format!("JLI_PROVIDER is {what}, but {}", needs(what)));
    match var("JLI_PROVIDER").map(|name| name.to_lowercase()).as_deref() {
        Some("chatgpt") => return chatgpt()?.map(Provider::ChatGpt).ok_or_else(|| missing("chatgpt")),
        Some("openrouter") => return openrouter(&var).map(Provider::Compatible).ok_or_else(|| missing("openrouter")),
        Some("gateway") => return gateway(&var).map(Provider::Compatible).ok_or_else(|| missing("gateway")),
        Some("custom") => return custom(&var).map(Provider::Compatible).ok_or_else(|| missing("custom")),
        Some(other) => {
            return Err(coded(
                CONFIGURATION,
                format!("JLI_PROVIDER must be chatgpt, openrouter, gateway or custom, not {other}"),
            ))
        }
        None => {}
    }
    if let Some(endpoint) = custom(&var) {
        return Ok(Provider::Compatible(endpoint));
    }
    if let Some(chatgpt) = chatgpt()? {
        return Ok(Provider::ChatGpt(chatgpt));
    }
    endpoint_from(get).map(Provider::Compatible)
}

fn needs(provider: &str) -> &'static str {
    match provider {
        "chatgpt" => "jli is not signed in with ChatGPT: run `jli login`.",
        "openrouter" => "OPENROUTER_API_KEY is not set.",
        "gateway" => "AI_GATEWAY_API_KEY is not set.",
        _ => "JLI_LLM_API_KEY and JLI_LLM_BASE_URL are not both set.",
    }
}

/// Reasoning is off everywhere: Mercury stalls with it on, and every default model is Mercury.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reasoning {
    Off,
}

pub struct JsonRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    /// Serialised as the user message.
    pub input: Value,
    /// Name for the response schema.
    pub name: &'a str,
    pub schema: Value,
    pub timeout: Duration,
    pub reasoning: Reasoning,
}

/// One HTTP POST as the transport sees it.
pub struct HttpCall<'a> {
    pub url: &'a str,
    pub api_key: &'a str,
    pub body: &'a [u8],
    pub timeout: Duration,
}

/// Sends one POST. `Ok((status, body))` for any HTTP reply; `Err` for a connection failure or a timeout.
pub type Transport = dyn Fn(&HttpCall) -> std::result::Result<(u16, String), String> + Send + Sync;

/// The language model client. Real HTTP unless built with a fake transport.
pub struct Llm {
    fake: Option<Box<Transport>>,
    /// Found on first use, so that commands that ask no model need no key and no sign-in.
    provider: OnceLock<std::result::Result<Provider, (&'static str, String)>>,
}

impl Default for Llm {
    fn default() -> Self {
        Llm::from_env()
    }
}

const RETRY_STATUSES: [u16; 6] = [408, 429, 500, 502, 503, 504];
const ATTEMPTS: usize = 2;

fn post(call: &HttpCall) -> std::result::Result<(u16, String), String> {
    let agent: ureq::Agent =
        ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(call.timeout)).build().into();
    let mut response = agent
        .post(call.url)
        .header("authorization", &format!("Bearer {}", call.api_key))
        .header("content-type", "application/json")
        .header("x-title", "jli")
        .send(call.body)
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
    Ok((status, text))
}

/// The JSON inside a reply, without a surrounding code fence.
pub fn strip_fence(content: &str) -> &str {
    let mut text = content.trim();
    if let Some(rest) = text.strip_prefix("```") {
        text = rest.strip_prefix("json").unwrap_or(rest).trim_start();
    }
    if let Some(rest) = text.strip_suffix("```") {
        text = rest.trim_end();
    }
    text
}

/// JSON indented by one space: the shape of the user message the prompts were tuned with.
fn one_space_json(value: &Value) -> String {
    let mut out = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(b" "));
    value.serialize(&mut serializer).expect("a JSON value serializes");
    String::from_utf8(out).expect("JSON is UTF-8")
}

fn first_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

/// The user message: the input as JSON indented by one space, or the text as it is.
fn user_message(input: &Value) -> String {
    match input {
        Value::String(text) => text.clone(),
        other => one_space_json(other),
    }
}

impl Llm {
    pub fn from_env() -> Llm {
        Llm { fake: None, provider: OnceLock::new() }
    }

    /// A client that sends every request to `transport` instead of the network.
    pub fn with_transport(
        transport: impl Fn(&HttpCall) -> std::result::Result<(u16, String), String> + Send + Sync + 'static,
    ) -> Llm {
        Llm { fake: Some(Box::new(transport)), provider: OnceLock::new() }
    }

    /// A client for one provider, whatever the environment says.
    pub fn with_provider(provider: Provider) -> Llm {
        Llm { fake: None, provider: OnceLock::from(Ok(provider)) }
    }

    fn provider(&self) -> Result<&Provider> {
        let found = self.provider.get_or_init(|| {
            provider_from(|name| std::env::var(name).ok(), || Accounts::from_env()?.provider())
                .map_err(|error| (crate::types::error_code(&error), format!("{error:#}")))
        });
        found.as_ref().map_err(|(code, message)| coded(code, message.clone()))
    }

    /// The ChatGPT plan requests go to, if they go to one.
    pub fn chatgpt(&self) -> Option<&ChatGpt> {
        // A fake transport never looks for a sign-in: tests must not read this host's account.
        if self.fake.is_some() {
            return None;
        }
        match self.provider() {
            Ok(Provider::ChatGpt(chatgpt)) => Some(chatgpt),
            _ => None,
        }
    }

    fn model(&self, variable: &str) -> String {
        if let Some(model) = clean(std::env::var(variable).ok()) {
            return model;
        }
        self.chatgpt().map(|chatgpt| chatgpt.model.clone()).unwrap_or_else(|| MERCURY.into())
    }

    /// Reasoning agent: proposes commands for requests nothing in the matrix fits.
    /// `JLI_AGENT_MODEL`, else the ChatGPT plan's model, else Mercury.
    pub fn agent_model(&self) -> String {
        self.model("JLI_AGENT_MODEL")
    }

    /// Fast model: rewrites what was taught and suggests alternatives.
    /// `JLI_TRANSLATOR_MODEL`, else the ChatGPT plan's model, else Mercury.
    pub fn translator_model(&self) -> String {
        self.model("JLI_TRANSLATOR_MODEL")
    }

    /// The agent as the trace names it: on a ChatGPT plan, says so.
    pub fn agent_label(&self) -> String {
        self.label(self.agent_model())
    }

    /// The translator as the trace names it.
    pub fn translator_label(&self) -> String {
        self.label(self.translator_model())
    }

    fn label(&self, model: String) -> String {
        match self.chatgpt() {
            Some(_) => format!("{model} · using ChatGPT plan"),
            None => model,
        }
    }

    /// Where requests go, in words, for `jli config`.
    pub fn describe(&self) -> String {
        if self.fake.is_some() {
            return "a fake transport".into();
        }
        match self.provider() {
            Ok(Provider::ChatGpt(chatgpt)) => match &chatgpt.email {
                Some(email) => format!("ChatGPT plan ({email})"),
                None => "ChatGPT plan".into(),
            },
            Ok(Provider::Compatible(endpoint)) => endpoint.base_url.clone(),
            Err(error) => format!("none: {error:#}"),
        }
    }

    /// Ask for a reply that conforms to `schema`. The caller owns the type `T`.
    pub fn complete_json<T: DeserializeOwned>(&self, request: &JsonRequest) -> Result<(T, u64)> {
        let Endpoint { base_url, api_key } = match &self.fake {
            Some(_) => Endpoint { base_url: "http://fake.invalid/v1".into(), api_key: "fake".into() },
            None => match self.provider()? {
                Provider::ChatGpt(chatgpt) => return complete_on_plan(chatgpt, request),
                Provider::Compatible(endpoint) => endpoint.clone(),
            },
        };
        let started = Instant::now();
        let user = user_message(&request.input);
        let reasoning = match request.reasoning {
            Reasoning::Off => json!({ "enabled": false }),
        };
        let body = serde_json::to_vec(&json!({
            "model": request.model,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": user },
            ],
            "reasoning": reasoning,
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": request.name, "strict": true, "schema": request.schema },
            },
        }))?;
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let call = HttpCall { url: &url, api_key: &api_key, body: &body, timeout: request.timeout };

        let mut last_error = "no attempt made".to_string();
        for _ in 0..ATTEMPTS {
            let sent = match &self.fake {
                Some(fake) => fake(&call),
                None => post(&call),
            };
            // Timeouts and dropped connections get the second attempt too.
            let (status, text) = match sent {
                Ok(reply) => reply,
                Err(error) => {
                    last_error = format!("{} did not answer: {error}", request.model);
                    continue;
                }
            };
            if !(200..300).contains(&status) {
                last_error = format!("{} returned HTTP {status}: {}", request.model, first_chars(&text, 300));
                if RETRY_STATUSES.contains(&status) {
                    continue;
                }
                break;
            }
            match read_reply::<T>(&text) {
                Ok(data) => return Ok((data, started.elapsed().as_millis() as u64)),
                Err(error) => last_error = format!("{} returned an unreadable reply: {error}", request.model),
            }
        }
        Err(coded(MODEL, last_error))
    }
}

fn read_reply<T: DeserializeOwned>(text: &str) -> Result<T> {
    let reply: Value = serde_json::from_str(text)?;
    let content =
        reply.pointer("/choices/0/message/content").and_then(Value::as_str).ok_or_else(|| anyhow!("no message content"))?;
    Ok(serde_json::from_str(strip_fence(content))?)
}

/// The same request on the user's ChatGPT plan: the system prompt as `instructions`, the same user
/// message, the same strict schema, streamed and unstored. Two attempts, as for an API key, but
/// only for a failure that is temporary: a usage limit, an ineligible plan or an ended sign-in
/// stops at once, and says what to do.
fn complete_on_plan<T: DeserializeOwned>(chatgpt: &ChatGpt, request: &JsonRequest) -> Result<(T, u64)> {
    let started = Instant::now();
    let effort = match request.reasoning {
        Reasoning::Off => ReasoningEffort::None,
    };
    let body = ResponseRequest::new(request.model)
        .instructions(request.system)
        .input(InputItem::user(user_message(&request.input)))
        .reasoning_effort(effort)
        .json_schema(request.name, request.schema.clone());
    let client = chatgpt.client.with_timeout(request.timeout);
    let mut last_error = "no attempt made".to_string();
    for _ in 0..ATTEMPTS {
        match client.create(&body) {
            Ok(completion) => match serde_json::from_str::<T>(strip_fence(&completion.text)) {
                Ok(data) => return Ok((data, started.elapsed().as_millis() as u64)),
                Err(error) => last_error = format!("{} returned an unreadable reply: {error}", request.model),
            },
            Err(error) => {
                let (code, message) = plan_failure(chatgpt, request.model, &error);
                // What the plan says is final stops at once; a temporary or unknown failure gets the second attempt.
                if !matches!(error.recovery(), Recovery::RetryLater | Recovery::None) {
                    return Err(coded(code, message));
                }
                last_error = message;
            }
        }
    }
    Err(coded(MODEL, last_error))
}

/// What to tell the person when the plan refuses a request, and the error's kind.
fn plan_failure(chatgpt: &ChatGpt, model: &str, error: &chatgpt_sdk::Error) -> (&'static str, String) {
    match error.recovery() {
        Recovery::RetryLater => (MODEL, format!("{model} did not answer on your ChatGPT plan: {error}")),
        Recovery::ManageUsage => {
            (MODEL, format!("Usage limit reached on your ChatGPT plan ({error}). Manage usage at {MANAGE_USAGE_URL}"))
        }
        Recovery::NotEligible => (MODEL, format!("Your ChatGPT plan cannot be used here: {error}")),
        Recovery::SignInAgain | Recovery::EnablePlanUsage => {
            (CONFIGURATION, format!("Your ChatGPT sign-in no longer works ({error}). Run `{}`.", chatgpt.sign_in))
        }
        Recovery::FixRequest => {
            (MODEL, format!("{model} refused the request: {error}. `jli models` lists the models your plan has."))
        }
        Recovery::CheckConfiguration => (CONFIGURATION, format!("Your ChatGPT plan cannot be used from here: {error}")),
        _ => match error {
            chatgpt_sdk::Error::Response(failure) if matches!(**failure, ResponseError::Incomplete { .. }) => {
                (MODEL, format!("{model} stopped before it finished: {error}"))
            }
            _ => (MODEL, format!("{model} failed on your ChatGPT plan: {error}")),
        },
    }
}

// ── Schema fragments shared by every skill-shaped reply ─────────────────────

fn nullable(kind: &str, description: &str) -> Value {
    json!({ "type": [kind, "null"], "description": description })
}

pub fn nullable_string(description: &str) -> Value {
    nullable("string", description)
}

/// An object schema in strict mode: every property required, nothing else allowed.
pub fn object(properties: Value) -> Value {
    let required: Vec<Value> = properties
        .as_object()
        .map(|map: &Map<String, Value>| map.keys().cloned().map(Value::String).collect())
        .unwrap_or_default();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn risk_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["safe", "modifies", "destructive"],
        "description": "safe only reads. modifies changes local state and can be undone. destructive deletes data, rewrites history or changes remote systems.",
    })
}

pub fn slots_schema() -> Value {
    json!({
        "type": "array",
        "items": object(json!({
            "name": { "type": "string", "description": "placeholder name, used as {name} in steps" },
            "kind": {
                "type": "string",
                "enum": ["file", "branch", "number", "text"],
                "description": "file: a path in the working directory. branch: a git branch. number: an integer. text: free text.",
            },
            "description": { "type": "string", "description": "what the value is, e.g. 'the file to count lines in'" },
            "default": nullable("string", "value to use when the request does not give one"),
            "aliases": {
                "type": "array",
                "items": { "type": "string" },
                "description": "up to four other names a person might type for this value as name=value, such as dir and path for directory; none that could mean another slot",
            },
            "optional": {
                "type": "boolean",
                "description": "true only when the command does the same job without this value and its flag, as git log lists commits without -n {count}; false when the command needs it, as cargo install needs --path {path}",
            },
            "flag": nullable(
                "string",
                "the flag written just before the placeholder that goes with the value, such as -n in -n {count}, or --profile= in --profile={profile}; null when there is none",
            ),
        })),
    })
}

pub fn slot_values_schema() -> Value {
    json!({
        "type": "array",
        "description": "the value of every slot for THIS request, unquoted",
        "items": object(json!({ "name": { "type": "string" }, "value": { "type": "string" } })),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlotDraft {
    pub name: String,
    pub kind: SlotKind,
    pub description: String,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub flag: Option<String>,
}

impl SlotDraft {
    /// The slot as a model wrote it, before `slots::checked_slots` holds it to what a model may say.
    pub fn to_slot(&self) -> Slot {
        Slot {
            name: self.name.clone(),
            kind: self.kind,
            description: self.description.clone(),
            default: self.default.clone(),
            aliases: self.aliases.clone(),
            optional: self.optional,
            flag: self.flag.clone(),
            choices: vec![],
        }
    }

    /// A stored slot as a model would have written it: everything but its choices, which no model writes.
    pub fn of(slot: &Slot) -> SlotDraft {
        SlotDraft {
            name: slot.name.clone(),
            kind: slot.kind,
            description: slot.description.clone(),
            default: slot.default.clone(),
            aliases: slot.aliases.clone(),
            optional: slot.optional,
            flag: slot.flag.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlotValue {
    pub name: String,
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn reply(content: &str) -> std::result::Result<(u16, String), String> {
        Ok((200, json!({ "choices": [{ "message": { "content": content } }] }).to_string()))
    }

    fn request(schema: Value) -> JsonRequest<'static> {
        JsonRequest {
            model: "test/model",
            system: "be brief",
            input: json!({ "request": "hi", "list": [1, 2] }),
            name: "thing",
            schema,
            timeout: Duration::from_secs(1),
            reasoning: Reasoning::Off,
        }
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Thing {
        value: String,
    }

    #[test]
    fn endpoints_resolve_in_order() {
        let env = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            move |name: &str| map.get(name).cloned()
        };
        let explicit = endpoint_from(env(&[
            ("JLI_LLM_API_KEY", "k1"),
            ("JLI_LLM_BASE_URL", "http://local/v1"),
            ("OPENROUTER_API_KEY", "k2"),
        ]))
        .unwrap();
        assert_eq!(explicit, Endpoint { base_url: "http://local/v1".into(), api_key: "k1".into() });
        let openrouter = endpoint_from(env(&[("JLI_LLM_API_KEY", "k1"), ("OPENROUTER_API_KEY", " k2 ")])).unwrap();
        assert_eq!(openrouter, Endpoint { base_url: "https://openrouter.ai/api/v1".into(), api_key: "k2".into() });
        let gateway = endpoint_from(env(&[("AI_GATEWAY_API_KEY", "k3"), ("OPENROUTER_API_KEY", "  ")])).unwrap();
        assert_eq!(gateway.base_url, "https://ai-gateway.vercel.sh/v1");
        assert!(endpoint_from(env(&[])).is_err());
    }

    #[test]
    fn the_body_matches_the_wire_format() {
        let seen = Arc::new(Mutex::new(Value::Null));
        let recorder = seen.clone();
        let llm = Llm::with_transport(move |call| {
            assert_eq!(call.url, "http://fake.invalid/v1/chat/completions");
            *recorder.lock().unwrap() = serde_json::from_slice(call.body).unwrap();
            reply("```json\n{\"value\": \"ok\"}\n```")
        });
        let schema = object(json!({ "value": { "type": "string" } }));
        let (thing, _) = llm.complete_json::<Thing>(&request(schema.clone())).unwrap();
        assert_eq!(thing, Thing { value: "ok".into() });
        let body = seen.lock().unwrap().clone();
        assert_eq!(body["model"], "test/model");
        assert_eq!(body["messages"][0], json!({ "role": "system", "content": "be brief" }));
        assert_eq!(body["messages"][1]["content"], "{\n \"request\": \"hi\",\n \"list\": [\n  1,\n  2\n ]\n}");
        assert_eq!(body["reasoning"], json!({ "enabled": false }));
        assert_eq!(
            body["response_format"],
            json!({ "type": "json_schema", "json_schema": { "name": "thing", "strict": true, "schema": schema } })
        );
        assert_eq!(schema["required"], json!(["value"]));
        let keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["model", "messages", "reasoning", "response_format"]);
    }

    #[test]
    fn transient_failures_and_unreadable_replies_get_a_second_attempt() {
        for first in [Err("timeout".to_string()), Ok((503, "busy".to_string())), reply("not json")] {
            let calls = Arc::new(Mutex::new(0));
            let counter = calls.clone();
            let llm = Llm::with_transport(move |_| {
                let mut n = counter.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    first.clone()
                } else {
                    reply("{\"value\": \"second\"}")
                }
            });
            let (thing, _) = llm.complete_json::<Thing>(&request(json!({}))).unwrap();
            assert_eq!(thing.value, "second");
            assert_eq!(*calls.lock().unwrap(), 2);
        }
    }

    #[test]
    fn a_client_error_is_not_retried() {
        let calls = Arc::new(Mutex::new(0));
        let counter = calls.clone();
        let llm = Llm::with_transport(move |_| {
            *counter.lock().unwrap() += 1;
            Ok((400, "bad schema".to_string()))
        });
        let error = llm.complete_json::<Thing>(&request(json!({}))).unwrap_err();
        assert_eq!(error.to_string(), "test/model returned HTTP 400: bad schema");
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fence("```json\n{}\n```"), "{}");
        assert_eq!(strip_fence("  ```\n[1]```  "), "[1]");
        assert_eq!(strip_fence("{\"a\": 1}"), "{\"a\": 1}");
    }

    use chatgpt_sdk::testing::{FakeChatGpt, FakeReply};
    use chatgpt_sdk::{AccessToken, Backend, Client};

    fn on_plan(fake: &Arc<FakeChatGpt>) -> Llm {
        let client = Client::builder(AccessToken::new(Backend::Codex, "static-token").account_id("acct-1"))
            .http_client(fake.http())
            .originator("jli")
            .build();
        Llm::with_provider(Provider::ChatGpt(ChatGpt {
            client,
            model: "gpt-6-luna".into(),
            email: Some("user@example.com".into()),
            sign_in: "codex login",
        }))
    }

    #[test]
    fn on_a_chatgpt_plan_the_same_request_is_a_streamed_responses_request() {
        let fake = FakeChatGpt::new();
        fake.reply("```json\n{\"value\": \"ok\"}\n```");
        let llm = on_plan(&fake);
        let schema = object(json!({ "value": { "type": "string" } }));
        let (thing, _) = llm.complete_json::<Thing>(&request(schema.clone())).unwrap();
        assert_eq!(thing, Thing { value: "ok".into() });
        let sent = fake.requests().pop().unwrap();
        assert_eq!(sent.url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(sent.header_value("chatgpt-account-id"), Some("acct-1"));
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["model"], "test/model");
        assert_eq!(body["instructions"], "be brief", "the system prompt is the instructions, never a system message");
        assert_eq!(
            body["input"],
            json!([{ "type": "message", "role": "user", "content": [{ "type": "input_text",
                "text": "{\n \"request\": \"hi\",\n \"list\": [\n  1,\n  2\n ]\n}" }] }]),
            "the user message is the one the prompts were measured with"
        );
        assert_eq!(body["reasoning"], json!({ "effort": "none" }), "reasoning off is effort none");
        assert_eq!(
            body["text"],
            json!({ "format": { "type": "json_schema", "name": "thing", "schema": schema, "strict": true } })
        );
        assert_eq!((body["store"].clone(), body["stream"].clone()), (json!(false), json!(true)));
    }

    #[test]
    fn on_a_plan_a_temporary_failure_is_retried_and_a_limit_is_not() {
        let fake = FakeChatGpt::new();
        let llm = on_plan(&fake);
        let schema = object(json!({ "value": { "type": "string" } }));
        fake.script(FakeReply::Status(503, r#"{"detail":"busy"}"#.into()));
        fake.reply("{\"value\": \"second\"}");
        assert_eq!(llm.complete_json::<Thing>(&request(schema.clone())).unwrap().0.value, "second");
        fake.script(FakeReply::Failed("overloaded".into()));
        fake.reply("{\"value\": \"third\"}");
        let (thing, _) = llm.complete_json::<Thing>(&request(schema.clone())).unwrap();
        assert_eq!(thing.value, "third", "a failure with a code nobody documented gets the second attempt");

        let before = fake.requests().len();
        fake.script(FakeReply::Failed("subscription_sharing_usage_limit_exceeded".into()));
        let error = llm.complete_json::<Thing>(&request(schema.clone())).unwrap_err();
        assert_eq!(fake.requests().len(), before + 1, "a usage limit is not retried");
        assert_eq!(crate::types::error_code(&error), MODEL);
        assert!(error.to_string().contains("https://chatgpt.com/settings/usage"), "{error}");

        fake.script(FakeReply::Status(401, r#"{"detail":"Unauthorized"}"#.into()));
        let error = llm.complete_json::<Thing>(&request(schema.clone())).unwrap_err();
        assert_eq!(crate::types::error_code(&error), CONFIGURATION);
        assert!(error.to_string().contains("codex login"), "{error}");

        fake.script(FakeReply::Status(400, r#"{"detail":"The 'test/model' model is not supported"}"#.into()));
        let error = llm.complete_json::<Thing>(&request(schema)).unwrap_err();
        assert!(error.to_string().contains("jli models"), "{error}");
    }

    #[test]
    fn the_plan_names_the_models_and_says_it_is_in_use() {
        let llm = on_plan(&FakeChatGpt::new());
        if std::env::var("JLI_AGENT_MODEL").is_err() {
            assert_eq!(llm.agent_model(), "gpt-6-luna");
            assert_eq!(llm.agent_label(), "gpt-6-luna · using ChatGPT plan");
        }
        assert_eq!(llm.describe(), "ChatGPT plan (user@example.com)");
        let offline = Llm::with_transport(|_| Err("connection refused".into()));
        assert!(offline.chatgpt().is_none());
        assert!(!offline.agent_label().contains("ChatGPT"));
        offline.translator_model();
        assert!(offline.provider.get().is_none(), "a fake transport never looks for a sign-in on this host");
    }

    fn signed_in() -> Result<Option<ChatGpt>> {
        let client =
            Client::builder(AccessToken::new(Backend::Api, "static")).http_client(FakeChatGpt::new().http()).build();
        Ok(Some(ChatGpt { client, model: "gpt-6-luna".into(), email: None, sign_in: "jli login" }))
    }

    fn kind(provider: Result<Provider>) -> String {
        match provider {
            Ok(Provider::ChatGpt(_)) => "chatgpt".into(),
            Ok(Provider::Compatible(endpoint)) => endpoint.base_url,
            Err(error) => format!("error: {error}"),
        }
    }

    #[test]
    fn providers_resolve_in_order() {
        let env = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            move |name: &str| map.get(name).cloned()
        };
        let openrouter = [("OPENROUTER_API_KEY", "k")];
        assert_eq!(kind(provider_from(env(&openrouter), signed_in)), "chatgpt", "a sign-in wins over OpenRouter");
        assert_eq!(kind(provider_from(env(&openrouter), || Ok(None))), "https://openrouter.ai/api/v1");
        let custom = [("JLI_LLM_API_KEY", "k"), ("JLI_LLM_BASE_URL", "http://local/v1")];
        assert_eq!(kind(provider_from(env(&custom), signed_in)), "http://local/v1", "an explicit endpoint wins");
        let picked = [("JLI_PROVIDER", "OpenRouter"), ("OPENROUTER_API_KEY", "k")];
        assert_eq!(kind(provider_from(env(&picked), signed_in)), "https://openrouter.ai/api/v1");
        let never =
            || -> Result<Option<ChatGpt>> { panic!("the sign-in is not looked for when JLI_PROVIDER names another") };
        assert_eq!(kind(provider_from(env(&picked), never)), "https://openrouter.ai/api/v1");
        assert!(kind(provider_from(env(&[("JLI_PROVIDER", "chatgpt")]), || Ok(None))).contains("jli login"));
        assert!(kind(provider_from(env(&[("JLI_PROVIDER", "gateway")]), signed_in)).contains("AI_GATEWAY_API_KEY"));
        assert!(kind(provider_from(env(&[("JLI_PROVIDER", "mercury")]), signed_in)).contains("must be chatgpt"));
        let ended = || -> Result<Option<ChatGpt>> { Err(coded(CONFIGURATION, "Your ChatGPT sign-in has ended.")) };
        assert!(
            kind(provider_from(env(&openrouter), ended)).contains("has ended"),
            "an ended sign-in is not a quiet switch"
        );
        let none = provider_from(env(&[]), || Ok(None)).err().unwrap();
        assert_eq!(crate::types::error_code(&none), CONFIGURATION);
        assert!(none.to_string().contains("jli login"));
    }
}
