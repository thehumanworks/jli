//! The request side of the Responses API, as the ChatGPT plan routes accept it.
//!
//! Both routes take only streamed, unstored requests, so the client always sends `store: false`
//! and `stream: true`; there is no field for either. Fields the plan route rejects (`temperature`,
//! `max_output_tokens`, `previous_response_id`, `metadata` and others) are left out of the typed
//! request. [`ResponseRequest::extra`] sends anything else as written.
//!
//! ```
//! use chatgpt_sdk::{InputItem, ReasoningEffort, ResponseRequest};
//! use serde_json::json;
//!
//! let request = ResponseRequest::new("gpt-6-luna")
//!     .instructions("Answer in one word.")
//!     .input(InputItem::user("What colour is the sky?"))
//!     .reasoning_effort(ReasoningEffort::None)
//!     .json_schema("answer", json!({
//!         "type": "object",
//!         "properties": { "word": { "type": "string" } },
//!         "required": ["word"],
//!         "additionalProperties": false,
//!     }));
//! assert_eq!(request.input.len(), 1);
//! ```

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A request to create a model response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseRequest {
    /// The model's slug, such as `gpt-6-luna`. [`crate::Client::models`] lists those the account
    /// can use.
    pub model: String,
    /// System-level instructions. Use these, or a developer message, instead of a system message:
    /// the plan route rejects `system` items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The conversation so far. Over HTTP there is no stored state: send all the context needed.
    pub input: Vec<InputItem>,
    /// How much the model reasons before it answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// The shape of the text output, such as a JSON Schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextConfig>,
    /// Tools the model may call.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// How the model chooses a tool, such as `"auto"` or `{"type": "function", "name": "…"}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// Whether the model may call several tools at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Extra output to include, such as `reasoning.encrypted_content`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// A key that groups requests sharing a prompt prefix, for caching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Any other fields, sent as written after the typed ones.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ResponseRequest {
    /// An empty request for `model`.
    pub fn new(model: impl Into<String>) -> ResponseRequest {
        ResponseRequest {
            model: model.into(),
            instructions: None,
            input: vec![],
            reasoning: None,
            text: None,
            tools: vec![],
            tool_choice: None,
            parallel_tool_calls: None,
            include: vec![],
            prompt_cache_key: None,
            extra: Map::new(),
        }
    }

    /// Set the instructions.
    pub fn instructions(mut self, instructions: impl Into<String>) -> ResponseRequest {
        self.instructions = Some(instructions.into());
        self
    }

    /// Append one input item.
    pub fn input(mut self, item: InputItem) -> ResponseRequest {
        self.input.push(item);
        self
    }

    /// Set the reasoning effort.
    pub fn reasoning_effort(mut self, effort: ReasoningEffort) -> ResponseRequest {
        self.reasoning.get_or_insert_with(Reasoning::default).effort = Some(effort);
        self
    }

    /// Ask for JSON that conforms to `schema`, in strict mode.
    pub fn json_schema(mut self, name: impl Into<String>, schema: Value) -> ResponseRequest {
        let format = TextFormat::JsonSchema { name: name.into(), schema, strict: true, description: None };
        self.text.get_or_insert_with(TextConfig::default).format = format;
        self
    }

    /// Append one tool.
    pub fn tool(mut self, tool: Tool) -> ResponseRequest {
        self.tools.push(tool);
        self
    }

    /// Set any other field.
    pub fn extra(mut self, name: impl Into<String>, value: Value) -> ResponseRequest {
        self.extra.insert(name.into(), value);
        self
    }
}

/// One item of input.
#[derive(Debug, Clone, PartialEq)]
pub enum InputItem {
    /// A message from the user, the developer, or an earlier assistant turn.
    Message(InputMessage),
    /// The result of a function call the model asked for.
    FunctionCallOutput {
        /// The `call_id` of the call.
        call_id: String,
        /// What the function returned.
        output: String,
    },
    /// Any other item, sent as written: for example an output item from an earlier turn.
    Raw(Value),
}

impl InputItem {
    /// A user message of plain text.
    pub fn user(text: impl Into<String>) -> InputItem {
        InputItem::Message(InputMessage { role: Role::User, content: vec![InputContent::text(text)] })
    }

    /// A developer message of plain text.
    pub fn developer(text: impl Into<String>) -> InputItem {
        InputItem::Message(InputMessage { role: Role::Developer, content: vec![InputContent::text(text)] })
    }

    /// An earlier assistant reply, for multi-turn context.
    pub fn assistant(text: impl Into<String>) -> InputItem {
        InputItem::Message(InputMessage {
            role: Role::Assistant,
            content: vec![InputContent::OutputText { text: text.into() }],
        })
    }
}

impl Serialize for InputItem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            InputItem::Message(message) => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "message")?;
                map.serialize_entry("role", &message.role)?;
                map.serialize_entry("content", &message.content)?;
                map.end()
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "function_call_output")?;
                map.serialize_entry("call_id", call_id)?;
                map.serialize_entry("output", output)?;
                map.end()
            }
            InputItem::Raw(value) => value.serialize(serializer),
        }
    }
}

/// A message in the input.
#[derive(Debug, Clone, PartialEq)]
pub struct InputMessage {
    /// Who said it.
    pub role: Role,
    /// What was said.
    pub content: Vec<InputContent>,
}

/// Who a message is from. There is no `system`: the ChatGPT plan route rejects it; use
/// [`ResponseRequest::instructions`] or a developer message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The person using the app.
    User,
    /// The model, in an earlier turn.
    Assistant,
    /// The app.
    Developer,
}

/// One part of a message's content.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    /// Text in a user or developer message.
    InputText {
        /// The text.
        text: String,
    },
    /// An image, by URL or as a `data:` URL, when the model accepts images.
    InputImage {
        /// The image's URL.
        image_url: String,
        /// `low`, `high` or `auto`.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Text in an earlier assistant message.
    OutputText {
        /// The text.
        text: String,
    },
}

impl InputContent {
    /// Text in a user or developer message.
    pub fn text(text: impl Into<String>) -> InputContent {
        InputContent::InputText { text: text.into() }
    }
}

/// Reasoning settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Reasoning {
    /// How much effort to spend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// Whether, and how, to summarise the reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummary>,
}

/// How much a model reasons. Each model supports its own set: [`crate::Model`] lists them.
///
/// Unknown levels round-trip through [`ReasoningEffort::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReasoningEffort {
    /// No reasoning: the fastest answer.
    None,
    /// `minimal`
    Minimal,
    /// `low`
    Low,
    /// `medium`
    Medium,
    /// `high`
    High,
    /// `xhigh`
    XHigh,
    /// `max`
    Max,
    /// Any other level, as written.
    Other(String),
}

impl ReasoningEffort {
    /// The level as written on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            ReasoningEffort::None => "none",
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
            ReasoningEffort::Max => "max",
            ReasoningEffort::Other(other) => other,
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = std::convert::Infallible;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(match text {
            "none" => ReasoningEffort::None,
            "minimal" => ReasoningEffort::Minimal,
            "low" => ReasoningEffort::Low,
            "medium" => ReasoningEffort::Medium,
            "high" => ReasoningEffort::High,
            "xhigh" => ReasoningEffort::XHigh,
            "max" => ReasoningEffort::Max,
            other => ReasoningEffort::Other(other.to_string()),
        })
    }
}

impl Serialize for ReasoningEffort {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReasoningEffort {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(|never| match never {})
    }
}

/// How reasoning is summarised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSummary {
    /// Let the model decide.
    Auto,
    /// A short summary.
    Concise,
    /// A longer summary.
    Detailed,
}

/// Text output settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TextConfig {
    /// The output's format.
    pub format: TextFormat,
    /// How long the answer should be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
}

/// The format of the text output.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TextFormat {
    /// Plain text.
    #[default]
    Text,
    /// JSON that conforms to a schema.
    JsonSchema {
        /// A name for the schema.
        name: String,
        /// The JSON Schema.
        schema: Value,
        /// Whether the output must conform exactly. In strict mode every property is required and
        /// `additionalProperties` is false.
        strict: bool,
        /// What the schema is for.
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// Any JSON object.
    JsonObject,
}

/// How long an answer should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    /// Short.
    Low,
    /// The default.
    Medium,
    /// Long.
    High,
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq)]
pub enum Tool {
    /// A function the app runs.
    Function(FunctionTool),
    /// Any other tool, sent as written.
    Raw(Value),
}

/// A function the app runs when the model calls it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FunctionTool {
    /// The function's name.
    pub name: String,
    /// What it does, for the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A JSON Schema of its arguments.
    pub parameters: Value,
    /// Whether the arguments must conform exactly.
    pub strict: bool,
}

impl Serialize for Tool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Tool::Function(function) => {
                let mut value = serde_json::to_value(function).map_err(serde::ser::Error::custom)?;
                if let Value::Object(map) = &mut value {
                    let mut tagged = Map::new();
                    tagged.insert("type".into(), Value::String("function".into()));
                    tagged.append(map);
                    value = Value::Object(tagged);
                }
                value.serialize(serializer)
            }
            Tool::Raw(value) => value.serialize(serializer),
        }
    }
}

/// The body actually sent: the request, then `store: false` and `stream: true`.
#[derive(Serialize)]
pub(crate) struct WireRequest<'a> {
    #[serde(flatten)]
    pub request: &'a ResponseRequest,
    pub store: bool,
    pub stream: bool,
}

/// Deserialize a field that may be absent or `null` as its default.
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Deserialize an `aud`-style claim: one string or a list of them.
pub(crate) fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::String(one) => Ok(vec![one]),
        Value::Array(many) => many
            .into_iter()
            .map(|item| item.as_str().map(str::to_string).ok_or_else(|| de::Error::custom("expected a string")))
            .collect(),
        Value::Null => Ok(vec![]),
        _ => Err(de::Error::custom("expected a string or a list of strings")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_wire_body_is_streamed_unstored_and_typed() {
        let request = ResponseRequest::new("gpt-6-luna")
            .instructions("be brief")
            .input(InputItem::developer("context"))
            .input(InputItem::user("hi"))
            .input(InputItem::assistant("hello"))
            .input(InputItem::FunctionCallOutput { call_id: "call_1".into(), output: "42".into() })
            .reasoning_effort(ReasoningEffort::None)
            .json_schema("thing", json!({ "type": "object" }))
            .extra("service_tier", json!("auto"));
        let body = serde_json::to_value(WireRequest { request: &request, store: false, stream: true }).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "gpt-6-luna",
                "instructions": "be brief",
                "input": [
                    { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "context" }] },
                    { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hi" }] },
                    { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "hello" }] },
                    { "type": "function_call_output", "call_id": "call_1", "output": "42" },
                ],
                "reasoning": { "effort": "none" },
                "text": { "format": { "type": "json_schema", "name": "thing", "schema": { "type": "object" }, "strict": true } },
                "service_tier": "auto",
                "store": false,
                "stream": true,
            })
        );
    }

    #[test]
    fn tools_are_tagged() {
        let tool = Tool::Function(FunctionTool {
            name: "lookup".into(),
            description: None,
            parameters: json!({ "type": "object" }),
            strict: true,
        });
        assert_eq!(
            serde_json::to_value(&tool).unwrap(),
            json!({ "type": "function", "name": "lookup", "parameters": { "type": "object" }, "strict": true })
        );
        assert_eq!(
            serde_json::to_value(Tool::Raw(json!({ "type": "web_search" }))).unwrap(),
            json!({ "type": "web_search" })
        );
    }

    #[test]
    fn reasoning_levels_round_trip_even_when_unknown() {
        for text in ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"] {
            let effort: ReasoningEffort = serde_json::from_value(json!(text)).unwrap();
            assert_eq!(serde_json::to_value(&effort).unwrap(), json!(text));
        }
        assert_eq!("ultra".parse::<ReasoningEffort>(), Ok(ReasoningEffort::Other("ultra".into())));
    }
}
