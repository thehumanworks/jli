//! The response side: streamed events, the response object, and the finished [`Completion`].
//!
//! A request succeeds only once `response.completed` arrives. Read events with the iterator a
//! [`ResponseStream`] is, and fold them with an [`Accumulator`] to show text as it arrives; or call
//! [`ResponseStream::into_completion`] to wait for the end.
//!
//! The text is assembled from the stream's `response.output_text.*` and
//! `response.output_item.done` events, not taken from the final response object: on the Codex
//! backend, `response.completed` carries an empty `output` when `store` is false.

use std::collections::BTreeMap;
use std::io::{BufReader, Read};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::error::{Error, ResponseError, Result};
use crate::http::read_error;
use crate::responses::null_as_default;
use crate::sse::SseReader;

/// A response object, as sent with `response.created`, `response.completed` and the other
/// lifecycle events. Fields the server leaves out are empty.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ResponseObject {
    /// The response's id, `resp_…`.
    #[serde(default)]
    pub id: String,
    /// Its status.
    #[serde(default)]
    pub status: Option<ResponseStatus>,
    /// The model that answered.
    #[serde(default)]
    pub model: Option<String>,
    /// The output items. Empty on the Codex backend: read the stream's items instead.
    #[serde(default, deserialize_with = "null_as_default")]
    pub output: Vec<OutputItem>,
    /// Token counts, on the final event.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// Why it failed, on `response.failed`.
    #[serde(default)]
    pub error: Option<ResponseErrorBody>,
    /// Why it stopped, on `response.incomplete`.
    #[serde(default)]
    pub incomplete_details: Option<IncompleteDetails>,
    /// Every other field, as sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A response's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResponseStatus {
    /// Done.
    Completed,
    /// Failed; see the error.
    Failed,
    /// Still running.
    InProgress,
    /// Stopped early; see the incomplete details.
    Incomplete,
    /// Cancelled.
    Cancelled,
    /// Waiting to start.
    Queued,
    /// A status this crate does not know.
    #[serde(other)]
    Unknown,
}

/// The error on a failed response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ResponseErrorBody {
    /// The code, such as `subscription_sharing_usage_limit_exceeded`.
    #[serde(default)]
    pub code: Option<String>,
    /// The message.
    #[serde(default)]
    pub message: String,
}

/// Why a response is incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IncompleteDetails {
    /// The reason, such as `max_output_tokens` or `content_filter`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Token counts for one response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    /// Tokens read.
    #[serde(default)]
    pub input_tokens: u64,
    /// Tokens written, reasoning included.
    #[serde(default)]
    pub output_tokens: u64,
    /// Both.
    #[serde(default)]
    pub total_tokens: u64,
    /// How the input tokens break down.
    #[serde(default)]
    pub input_tokens_details: Option<InputTokensDetails>,
    /// How the output tokens break down.
    #[serde(default)]
    pub output_tokens_details: Option<OutputTokensDetails>,
}

/// How the input tokens break down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct InputTokensDetails {
    /// Tokens read from the prompt cache.
    #[serde(default)]
    pub cached_tokens: u64,
}

/// How the output tokens break down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct OutputTokensDetails {
    /// Tokens spent reasoning.
    #[serde(default)]
    pub reasoning_tokens: u64,
}

/// One item of output.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum OutputItem {
    /// A message from the model.
    Message(MessageItem),
    /// The model's reasoning.
    Reasoning(ReasoningItem),
    /// A call to one of the request's function tools.
    FunctionCall(FunctionCallItem),
    /// Any other item, as sent.
    Other(Value),
}

/// A message from the model.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageItem {
    /// The item's id.
    #[serde(default)]
    pub id: String,
    /// `assistant`.
    #[serde(default)]
    pub role: String,
    /// `in_progress`, `completed` or `incomplete`.
    #[serde(default)]
    pub status: Option<String>,
    /// What it said.
    #[serde(default, deserialize_with = "null_as_default")]
    pub content: Vec<OutputContent>,
}

/// One part of a message from the model.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum OutputContent {
    /// Text.
    OutputText {
        /// The text.
        text: String,
    },
    /// A refusal.
    Refusal {
        /// Why it refused.
        refusal: String,
    },
    /// Any other part, as sent.
    Other(Value),
}

/// The model's reasoning.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReasoningItem {
    /// The item's id.
    #[serde(default)]
    pub id: String,
    /// A summary of the reasoning, when one was asked for.
    #[serde(default, deserialize_with = "null_as_default")]
    pub summary: Vec<Value>,
    /// The reasoning, encrypted, when `reasoning.encrypted_content` was included.
    #[serde(default)]
    pub encrypted_content: Option<String>,
}

/// A call to a function tool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FunctionCallItem {
    /// The item's id.
    #[serde(default)]
    pub id: Option<String>,
    /// The id to answer with in [`crate::InputItem::FunctionCallOutput`].
    #[serde(default)]
    pub call_id: String,
    /// The function's name.
    #[serde(default)]
    pub name: String,
    /// The arguments, as a JSON string.
    #[serde(default)]
    pub arguments: String,
}

fn typed<T: DeserializeOwned>(value: &Value) -> Option<T> {
    serde_json::from_value(value.clone()).ok()
}

impl<'de> Deserialize<'de> for OutputItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let item = match value.get("type").and_then(Value::as_str) {
            Some("message") => typed(&value).map(OutputItem::Message),
            Some("reasoning") => typed(&value).map(OutputItem::Reasoning),
            Some("function_call") => typed(&value).map(OutputItem::FunctionCall),
            _ => None,
        };
        Ok(item.unwrap_or(OutputItem::Other(value)))
    }
}

impl<'de> Deserialize<'de> for OutputContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let text = |field: &str| value.get(field).and_then(Value::as_str).map(str::to_string);
        let part = match value.get("type").and_then(Value::as_str) {
            Some("output_text") => text("text").map(|text| OutputContent::OutputText { text }),
            Some("refusal") => text("refusal").map(|refusal| OutputContent::Refusal { refusal }),
            _ => None,
        };
        Ok(part.unwrap_or(OutputContent::Other(value)))
    }
}

/// One event of a response stream.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum StreamEvent {
    /// `response.created`
    Created(ResponseObject),
    /// `response.in_progress`
    InProgress(ResponseObject),
    /// `response.output_item.added`
    OutputItemAdded {
        /// Where the item sits in the output.
        output_index: u32,
        /// The item, as started.
        item: OutputItem,
    },
    /// `response.output_item.done`
    OutputItemDone {
        /// Where the item sits in the output.
        output_index: u32,
        /// The finished item.
        item: OutputItem,
    },
    /// `response.output_text.delta`: more text.
    OutputTextDelta {
        /// The message item it belongs to.
        item_id: String,
        /// Where the item sits in the output.
        output_index: u32,
        /// Which content part it extends.
        content_index: u32,
        /// The new text.
        delta: String,
    },
    /// `response.output_text.done`: one content part's whole text.
    OutputTextDone {
        /// The message item it belongs to.
        item_id: String,
        /// Where the item sits in the output.
        output_index: u32,
        /// Which content part it is.
        content_index: u32,
        /// The whole text.
        text: String,
    },
    /// `response.refusal.delta`
    RefusalDelta {
        /// The message item it belongs to.
        item_id: String,
        /// The new text of the refusal.
        delta: String,
    },
    /// `response.function_call_arguments.delta`
    FunctionCallArgumentsDelta {
        /// The function call item it belongs to.
        item_id: String,
        /// Where the item sits in the output.
        output_index: u32,
        /// More of the arguments' JSON.
        delta: String,
    },
    /// `response.reasoning_summary_text.delta`
    ReasoningSummaryTextDelta {
        /// The reasoning item it belongs to.
        item_id: String,
        /// More of the summary.
        delta: String,
    },
    /// `response.completed`: inference succeeded.
    Completed(ResponseObject),
    /// `response.failed`: inference failed; the object's `error` says why.
    Failed(ResponseObject),
    /// `response.incomplete`: inference stopped early.
    Incomplete(ResponseObject),
    /// `error`: the stream itself failed.
    Error {
        /// The error's code.
        code: Option<String>,
        /// What went wrong.
        message: String,
        /// The parameter at fault, if any.
        param: Option<String>,
    },
    /// Any other event, by its `type`, with its data as sent.
    Other {
        /// The event's `type`.
        kind: String,
        /// The event's data.
        data: Value,
    },
}

impl StreamEvent {
    /// Read an event from its JSON data. An event this crate does not type, or one whose fields
    /// do not have the expected shape, becomes [`StreamEvent::Other`].
    pub fn from_value(data: Value) -> StreamEvent {
        let kind = data.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
        let text = |field: &str| data.get(field).and_then(Value::as_str).map(str::to_string);
        let index = |field: &str| data.get(field).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
        let response = || data.get("response").and_then(typed::<ResponseObject>);
        let item = || data.get("item").and_then(typed::<OutputItem>);
        let event = match kind.as_str() {
            "response.created" => response().map(StreamEvent::Created),
            "response.in_progress" => response().map(StreamEvent::InProgress),
            "response.completed" => response().map(StreamEvent::Completed),
            "response.failed" => response().map(StreamEvent::Failed),
            "response.incomplete" => response().map(StreamEvent::Incomplete),
            "response.output_item.added" => {
                item().map(|item| StreamEvent::OutputItemAdded { output_index: index("output_index"), item })
            }
            "response.output_item.done" => {
                item().map(|item| StreamEvent::OutputItemDone { output_index: index("output_index"), item })
            }
            "response.output_text.delta" => text("delta").map(|delta| StreamEvent::OutputTextDelta {
                item_id: text("item_id").unwrap_or_default(),
                output_index: index("output_index"),
                content_index: index("content_index"),
                delta,
            }),
            "response.output_text.done" => text("text").map(|whole| StreamEvent::OutputTextDone {
                item_id: text("item_id").unwrap_or_default(),
                output_index: index("output_index"),
                content_index: index("content_index"),
                text: whole,
            }),
            "response.refusal.delta" => {
                text("delta").map(|delta| StreamEvent::RefusalDelta { item_id: text("item_id").unwrap_or_default(), delta })
            }
            "response.function_call_arguments.delta" => text("delta").map(|delta| StreamEvent::FunctionCallArgumentsDelta {
                item_id: text("item_id").unwrap_or_default(),
                output_index: index("output_index"),
                delta,
            }),
            "response.reasoning_summary_text.delta" => text("delta")
                .map(|delta| StreamEvent::ReasoningSummaryTextDelta { item_id: text("item_id").unwrap_or_default(), delta }),
            "error" => {
                // Some servers nest the error: {"type": "error", "error": {"code", "message"}}.
                let nested = data.get("error");
                let field = |name: &str| text(name).or_else(|| nested?.get(name)?.as_str().map(str::to_string));
                Some(StreamEvent::Error {
                    code: field("code"),
                    message: field("message").unwrap_or_else(|| "unknown stream error".into()),
                    param: field("param"),
                })
            }
            _ => None,
        };
        event.unwrap_or(StreamEvent::Other { kind, data })
    }

    /// Whether this event ends the response: completed, failed, incomplete, or an error.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StreamEvent::Completed(_) | StreamEvent::Failed(_) | StreamEvent::Incomplete(_) | StreamEvent::Error { .. }
        )
    }
}

/// A finished response.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    /// The response's id.
    pub id: String,
    /// The model that answered, when the server said.
    pub model: Option<String>,
    /// The text of every message, in output order.
    pub text: String,
    /// The refusal, if the model refused.
    pub refusal: Option<String>,
    /// The output items, from the stream.
    pub output: Vec<OutputItem>,
    /// Token counts.
    pub usage: Option<Usage>,
    /// The request id from the reply's headers.
    pub request_id: Option<String>,
    /// The `response.completed` object, as sent.
    pub response: ResponseObject,
}

impl Completion {
    /// Parse the text as JSON, as asked for with [`crate::ResponseRequest::json_schema`].
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(&self.text).map_err(|error| Error::json("the response text is not the JSON asked for", error))
    }

    /// The function calls the model asked for, in order.
    pub fn function_calls(&self) -> impl Iterator<Item = &FunctionCallItem> {
        self.output.iter().filter_map(|item| match item {
            OutputItem::FunctionCall(call) => Some(call),
            _ => None,
        })
    }
}

/// Folds stream events into a [`Completion`].
///
/// ```
/// use chatgpt_sdk::{Accumulator, StreamEvent};
/// use serde_json::json;
///
/// let mut accumulator = Accumulator::default();
/// for data in [
///     json!({ "type": "response.output_text.delta", "item_id": "m", "output_index": 0, "content_index": 0, "delta": "Hel" }),
///     json!({ "type": "response.output_text.delta", "item_id": "m", "output_index": 0, "content_index": 0, "delta": "lo" }),
///     json!({ "type": "response.completed", "response": { "id": "resp_1", "status": "completed", "output": [] } }),
/// ] {
///     accumulator.push(StreamEvent::from_value(data));
/// }
/// assert_eq!(accumulator.finish().unwrap().text, "Hello");
/// ```
#[derive(Debug, Default, Clone)]
pub struct Accumulator {
    text: BTreeMap<(u32, u32), String>,
    refusal: String,
    items: BTreeMap<u32, OutputItem>,
    end: Option<StreamEvent>,
    request_id: Option<String>,
}

impl Accumulator {
    /// An accumulator that reports `request_id` on its completion and errors.
    pub fn with_request_id(request_id: Option<String>) -> Accumulator {
        Accumulator { request_id, ..Accumulator::default() }
    }

    /// Take in one event. Events after the first terminal one are ignored.
    pub fn push(&mut self, event: StreamEvent) {
        if self.end.is_some() {
            return;
        }
        match event {
            StreamEvent::OutputTextDelta { output_index, content_index, delta, .. } => {
                self.text.entry((output_index, content_index)).or_default().push_str(&delta);
            }
            StreamEvent::OutputTextDone { output_index, content_index, text, .. } => {
                self.text.insert((output_index, content_index), text);
            }
            StreamEvent::RefusalDelta { delta, .. } => self.refusal.push_str(&delta),
            StreamEvent::OutputItemDone { output_index, item } => {
                self.items.insert(output_index, item);
            }
            event if event.is_terminal() => self.end = Some(event),
            _ => {}
        }
    }

    /// The text received so far.
    pub fn text(&self) -> String {
        // In output order. A finished message item is the authority; deltas stand in for an item
        // that is not done yet.
        let mut indexes: Vec<u32> = self.items.keys().chain(self.text.keys().map(|(index, _)| index)).copied().collect();
        indexes.sort_unstable();
        indexes.dedup();
        let mut text = String::new();
        for index in indexes {
            match self.items.get(&index) {
                Some(item @ OutputItem::Message(_)) => text.push_str(&text_in(std::slice::from_ref(item))),
                Some(_) => {}
                None => {
                    for part in self.text.range((index, 0)..=(index, u32::MAX)).map(|(_, part)| part) {
                        text.push_str(part);
                    }
                }
            }
        }
        text
    }

    /// Whether a terminal event has arrived.
    pub fn is_finished(&self) -> bool {
        self.end.is_some()
    }

    /// The completion, or why there is none.
    pub fn finish(self) -> Result<Completion> {
        let text = self.text();
        let request_id = self.request_id.clone();
        match self.end {
            Some(StreamEvent::Completed(response)) => {
                let output =
                    if self.items.is_empty() { response.output.clone() } else { self.items.into_values().collect() };
                let refusal = (!self.refusal.is_empty()).then_some(self.refusal).or_else(|| refusal_in(&output));
                // A server that sends the output only on the final object still gives its text.
                let text = if text.is_empty() { text_in(&output) } else { text };
                Ok(Completion {
                    id: response.id.clone(),
                    model: response.model.clone(),
                    text,
                    refusal,
                    output,
                    usage: response.usage,
                    request_id,
                    response,
                })
            }
            Some(StreamEvent::Failed(response)) => {
                let error = response.error.unwrap_or(ResponseErrorBody { code: None, message: "no reason given".into() });
                Err(ResponseError::Failed { response_id: response.id, code: error.code, message: error.message, request_id }
                    .into())
            }
            Some(StreamEvent::Incomplete(response)) => Err(ResponseError::Incomplete {
                response_id: response.id,
                reason: response.incomplete_details.and_then(|details| details.reason),
                partial_text: text,
                request_id,
            }
            .into()),
            Some(StreamEvent::Error { code, message, param }) => {
                Err(ResponseError::Stream { code, message, param, request_id }.into())
            }
            _ => Err(ResponseError::EndedEarly { partial_text: text, request_id }.into()),
        }
    }
}

fn text_in(output: &[OutputItem]) -> String {
    let mut text = String::new();
    for item in output {
        if let OutputItem::Message(message) = item {
            for part in &message.content {
                if let OutputContent::OutputText { text: part } = part {
                    text.push_str(part);
                }
            }
        }
    }
    text
}

fn refusal_in(output: &[OutputItem]) -> Option<String> {
    output.iter().find_map(|item| match item {
        OutputItem::Message(message) => message.content.iter().find_map(|part| match part {
            OutputContent::Refusal { refusal } => Some(refusal.clone()),
            _ => None,
        }),
        _ => None,
    })
}

/// A response arriving as server-sent events. Iterate it for [`StreamEvent`]s.
///
/// Iteration stops after the first terminal event. A stream that closes before one yields no
/// error by itself: [`Accumulator::finish`] and [`ResponseStream::into_completion`] report it.
pub struct ResponseStream {
    events: SseReader<BufReader<Box<dyn Read + Send>>>,
    request_id: Option<String>,
    done: bool,
}

impl std::fmt::Debug for ResponseStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseStream").field("request_id", &self.request_id).field("done", &self.done).finish()
    }
}

impl ResponseStream {
    /// A stream over a `text/event-stream` body.
    pub fn new(body: Box<dyn Read + Send>, request_id: Option<String>) -> ResponseStream {
        ResponseStream { events: SseReader::new(BufReader::new(body)), request_id, done: false }
    }

    /// The request id from the reply's headers.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Read the stream to its end and return the completed response.
    pub fn into_completion(self) -> Result<Completion> {
        let mut accumulator = Accumulator::with_request_id(self.request_id.clone());
        for event in self {
            accumulator.push(event?);
        }
        accumulator.finish()
    }
}

impl Iterator for ResponseStream {
    type Item = Result<StreamEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let event = match self.events.next_event() {
                Ok(Some(event)) => event,
                Ok(None) => {
                    self.done = true;
                    return None;
                }
                Err(error) => {
                    self.done = true;
                    return Some(Err(read_error(&error).into()));
                }
            };
            if event.data.trim() == "[DONE]" {
                continue;
            }
            let data: Value = match serde_json::from_str(&event.data) {
                Ok(data) => data,
                Err(error) => {
                    self.done = true;
                    return Some(Err(Error::json("a stream event is not JSON", error)));
                }
            };
            let mut event = StreamEvent::from_value(data);
            if let StreamEvent::Other { kind, .. } = &mut event {
                if kind.is_empty() {
                    continue;
                }
            }
            self.done = event.is_terminal();
            return Some(Ok(event));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn stream(lines: &[Value]) -> ResponseStream {
        let mut text = String::new();
        for line in lines {
            text.push_str(&format!("event: {}\ndata: {line}\n\n", line["type"].as_str().unwrap_or("")));
        }
        ResponseStream::new(Box::new(Cursor::new(text.into_bytes())), Some("req_1".into()))
    }

    fn delta(text: &str) -> Value {
        json!({ "type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": text })
    }

    #[test]
    fn text_is_assembled_from_deltas_when_the_final_output_is_empty() {
        let completion = stream(&[
            json!({ "type": "response.created", "response": { "id": "resp_1", "status": "in_progress", "output": [] } }),
            delta("{\"command\":"),
            delta("\"ls\"}"),
            json!({ "type": "response.completed", "response": { "id": "resp_1", "status": "completed", "model": "gpt-6-luna", "output": [],
                "usage": { "input_tokens": 60, "output_tokens": 38, "total_tokens": 98, "output_tokens_details": { "reasoning_tokens": 0 } } } }),
        ])
        .into_completion()
        .unwrap();
        assert_eq!(completion.text, "{\"command\":\"ls\"}");
        assert_eq!(completion.json::<Value>().unwrap(), json!({ "command": "ls" }));
        assert_eq!((completion.id.as_str(), completion.model.as_deref()), ("resp_1", Some("gpt-6-luna")));
        assert_eq!(completion.usage.map(|usage| usage.total_tokens), Some(98));
        assert_eq!(completion.request_id.as_deref(), Some("req_1"));
    }

    #[test]
    fn a_finished_item_wins_over_its_deltas_and_function_calls_are_kept() {
        let completion = stream(&[
            delta("partial"),
            json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "message", "id": "msg_1", "role": "assistant",
                "content": [{ "type": "output_text", "text": "Hello", "annotations": [] }] } }),
            json!({ "type": "response.output_item.done", "output_index": 1, "item": { "type": "function_call", "call_id": "call_1",
                "name": "lookup", "arguments": "{}" } }),
            json!({ "type": "response.output_item.done", "output_index": 2, "item": { "type": "web_search_call", "id": "ws" } }),
            json!({ "type": "response.completed", "response": { "id": "resp_2", "status": "completed" } }),
        ])
        .into_completion()
        .unwrap();
        assert_eq!(completion.text, "Hello");
        assert_eq!(completion.function_calls().map(|call| call.name.as_str()).collect::<Vec<_>>(), ["lookup"]);
        assert!(matches!(completion.output[2], OutputItem::Other(_)));
    }

    #[test]
    fn the_final_object_gives_the_text_when_nothing_was_streamed() {
        let completion = stream(&[json!({ "type": "response.completed", "response": { "id": "r", "output": [
            { "type": "message", "role": "assistant", "content": [{ "type": "refusal", "refusal": "no" }, { "type": "output_text", "text": "hi" }] }
        ] } })])
        .into_completion()
        .unwrap();
        assert_eq!((completion.text.as_str(), completion.refusal.as_deref()), ("hi", Some("no")));
    }

    #[test]
    fn a_failed_response_carries_its_code() {
        let error = stream(&[
            delta("Hel"),
            json!({ "type": "response.failed", "response": { "id": "resp_3", "status": "failed",
                "error": { "code": "subscription_sharing_usage_limit_exceeded", "message": "Usage limit reached" } } }),
        ])
        .into_completion()
        .unwrap_err();
        assert_eq!(error.recovery(), crate::Recovery::ManageUsage);
        assert_eq!(error.request_id(), Some("req_1"));
        assert!(error.to_string().contains("subscription_sharing_usage_limit_exceeded"), "{error}");
    }

    #[test]
    fn incomplete_error_and_early_end_are_failures() {
        let incomplete = stream(&[
            delta("Hel"),
            json!({ "type": "response.incomplete", "response": { "id": "r", "incomplete_details": { "reason": "max_output_tokens" } } }),
        ])
        .into_completion()
        .unwrap_err();
        let response = |error: &Error| match error {
            Error::Response(response) => (**response).clone(),
            other => panic!("expected a response error, got {other}"),
        };
        assert!(matches!(response(&incomplete), ResponseError::Incomplete { reason: Some(reason), partial_text, .. }
                if reason == "max_output_tokens" && partial_text == "Hel"));
        let errored =
            stream(&[json!({ "type": "error", "code": "server_error", "message": "boom" })]).into_completion().unwrap_err();
        assert!(matches!(response(&errored), ResponseError::Stream { code: Some(code), .. } if code == "server_error"));
        let ended = stream(&[delta("Hel")]).into_completion().unwrap_err();
        assert!(matches!(response(&ended), ResponseError::EndedEarly { partial_text, .. } if partial_text == "Hel"));
        assert_eq!(ended.recovery(), crate::Recovery::RetryLater);
    }

    #[test]
    fn iteration_stops_at_the_terminal_event_and_unknown_events_pass_through() {
        let events: Vec<StreamEvent> = stream(&[
            json!({ "type": "response.content_part.added", "part": {} }),
            json!({ "type": "response.completed", "response": { "id": "r" } }),
            delta("after the end"),
        ])
        .collect::<Result<_>>()
        .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Other { kind, .. } if kind == "response.content_part.added"));
        let text = "data: not json\n\n";
        let mut broken = ResponseStream::new(Box::new(Cursor::new(text.as_bytes().to_vec())), None);
        assert!(matches!(broken.next(), Some(Err(Error::Json { .. }))));
        assert!(broken.next().is_none());
    }

    #[test]
    fn a_nested_error_event_is_read() {
        let event = StreamEvent::from_value(json!({ "type": "error", "error": { "code": "x", "message": "y" } }));
        assert_eq!(event, StreamEvent::Error { code: Some("x".into()), message: "y".into(), param: None });
    }
}
