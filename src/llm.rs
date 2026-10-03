//! The only way this app talks to a language model: one POST to an
//! OpenAI-compatible `/chat/completions` endpoint with a JSON Schema for the
//! reply. No SDK.
//!
//!   JLI_LLM_API_KEY + JLI_LLM_BASE_URL           any other compatible endpoint
//!   OPENROUTER_API_KEY (+ OPENROUTER_BASE_URL)   OpenRouter, the default
//!   AI_GATEWAY_API_KEY                           Vercel AI Gateway, the fallback

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::types::SlotKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: String,
}

fn clean(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

/// Which endpoint to call, from the environment `get` reads.
pub fn endpoint_from(get: impl Fn(&str) -> Option<String>) -> Result<Endpoint> {
    let var = |name: &str| clean(get(name));
    if let (Some(api_key), Some(base_url)) = (var("JLI_LLM_API_KEY"), var("JLI_LLM_BASE_URL")) {
        return Ok(Endpoint { base_url, api_key });
    }
    if let Some(api_key) = var("OPENROUTER_API_KEY") {
        let base_url = var("OPENROUTER_BASE_URL").unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
        return Ok(Endpoint { base_url, api_key });
    }
    if let Some(api_key) = var("AI_GATEWAY_API_KEY") {
        return Ok(Endpoint { base_url: "https://ai-gateway.vercel.sh/v1".into(), api_key });
    }
    bail!("No language model key found. Set OPENROUTER_API_KEY or AI_GATEWAY_API_KEY.")
}

pub fn endpoint() -> Result<Endpoint> {
    endpoint_from(|name| std::env::var(name).ok())
}

/// Fast model: rewrites what was taught and suggests alternatives.
pub static TRANSLATOR_MODEL: LazyLock<String> =
    LazyLock::new(|| clean(std::env::var("JLI_TRANSLATOR_MODEL").ok()).unwrap_or_else(|| "inception/mercury-2.5".into()));
/// Reasoning agent: proposes commands for requests nothing in the matrix fits.
pub static AGENT_MODEL: LazyLock<String> =
    LazyLock::new(|| clean(std::env::var("JLI_AGENT_MODEL").ok()).unwrap_or_else(|| "inception/mercury-2.5".into()));

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

impl Llm {
    pub fn from_env() -> Llm {
        Llm { fake: None }
    }

    /// A client that sends every request to `transport` instead of the network.
    pub fn with_transport(
        transport: impl Fn(&HttpCall) -> std::result::Result<(u16, String), String> + Send + Sync + 'static,
    ) -> Llm {
        Llm { fake: Some(Box::new(transport)) }
    }

    /// Ask for a reply that conforms to `schema`. The caller owns the type `T`.
    pub fn complete_json<T: DeserializeOwned>(&self, request: &JsonRequest) -> Result<(T, u64)> {
        let Endpoint { base_url, api_key } = match self.fake {
            Some(_) => Endpoint { base_url: "http://fake.invalid/v1".into(), api_key: "fake".into() },
            None => endpoint()?,
        };
        let started = Instant::now();
        let user = match &request.input {
            Value::String(text) => text.clone(),
            other => one_space_json(other),
        };
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
        Err(anyhow!(last_error))
    }
}

fn read_reply<T: DeserializeOwned>(text: &str) -> Result<T> {
    let reply: Value = serde_json::from_str(text)?;
    let content =
        reply.pointer("/choices/0/message/content").and_then(Value::as_str).ok_or_else(|| anyhow!("no message content"))?;
    Ok(serde_json::from_str(strip_fence(content))?)
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
}
