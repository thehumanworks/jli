//! The models a signed-in account can use.
//!
//! Treat the list as a catalogue, not an entitlement check: a completed request is what proves a
//! model is available to the account.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::responses::{null_as_default, ReasoningEffort};

/// A model in the account's catalogue.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Model {
    /// The slug to send as [`crate::ResponseRequest::model`], such as `gpt-6-luna`.
    #[serde(alias = "id")]
    pub slug: String,
    /// The name to show, such as `GPT-6-Luna`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// A description, when the server gives one.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the model is meant to be shown in a picker.
    #[serde(default)]
    pub visibility: Option<Visibility>,
    /// The reasoning effort the model uses by default.
    #[serde(default)]
    pub default_reasoning_level: Option<ReasoningEffort>,
    /// The reasoning efforts the model lists. A model may accept more: `gpt-6-luna` lists `low` and
    /// up, and accepted `none` on 2026-10-03.
    #[serde(default, deserialize_with = "null_as_default")]
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
    /// The context window, in tokens.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Every other field, as sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Model {
    /// The name to show: the display name, or the slug.
    pub fn name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.slug)
    }

    /// Whether the model is meant for a picker. A model without a visibility is.
    pub fn is_listed(&self) -> bool {
        !matches!(self.visibility, Some(Visibility::Hide))
    }
}

/// Whether a model is shown in a picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Visibility {
    /// Shown.
    List,
    /// Hidden.
    Hide,
    /// Anything else.
    #[serde(other)]
    Unknown,
}

/// A reasoning effort a model supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningLevel {
    /// The effort.
    pub effort: ReasoningEffort,
    /// What it is good for, when the server says.
    pub description: Option<String>,
}

impl<'de> Deserialize<'de> for ReasoningLevel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        // Either "low" or {"effort": "low", "description": "…"}.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            Bare(ReasoningEffort),
            Described { effort: ReasoningEffort, description: Option<String> },
        }
        Ok(match Shape::deserialize(deserializer)? {
            Shape::Bare(effort) => ReasoningLevel { effort, description: None },
            Shape::Described { effort, description } => ReasoningLevel { effort, description },
        })
    }
}

/// Read a model list: `{"models": […]}`, or the API's `{"data": […]}` with `id` for the slug.
pub(crate) fn parse_models(body: &str) -> Result<Vec<Model>> {
    #[derive(Deserialize)]
    struct List {
        #[serde(default)]
        models: Option<Vec<Model>>,
        #[serde(default)]
        data: Option<Vec<Model>>,
    }
    let list: List =
        serde_json::from_str(body).map_err(|error| Error::json("the model list is not the JSON expected", error))?;
    list.models.or(list.data).ok_or_else(|| Error::Protocol("the model list has neither `models` nor `data`".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_codex_catalogue_is_read_with_its_levels() {
        let body = r#"{"models":[
            {"slug":"gpt-6-luna","display_name":"GPT-6-Luna","visibility":"list","default_reasoning_level":"medium",
             "supported_reasoning_levels":[{"effort":"low","description":"Fast"},{"effort":"max","description":"Deep"}],
             "context_window":400000,"priority":4},
            {"slug":"codex-auto-review","visibility":"hide","supported_reasoning_levels":null}
        ]}"#;
        let models = parse_models(body).unwrap();
        assert_eq!(models[0].name(), "GPT-6-Luna");
        assert_eq!(models[0].default_reasoning_level, Some(ReasoningEffort::Medium));
        assert_eq!(models[0].supported_reasoning_levels[1].effort, ReasoningEffort::Max);
        assert_eq!(models[0].extra["priority"], 4);
        assert!(models[0].is_listed() && !models[1].is_listed());
        assert_eq!(models[1].name(), "codex-auto-review");
    }

    #[test]
    fn an_api_style_list_and_bare_levels_are_read() {
        let models =
            parse_models(r#"{"object":"list","data":[{"id":"gpt-6.1-sol","supported_reasoning_levels":["low"]}]}"#).unwrap();
        assert_eq!(models[0].slug, "gpt-6.1-sol");
        assert_eq!(
            models[0].supported_reasoning_levels[0],
            ReasoningLevel { effort: ReasoningEffort::Low, description: None }
        );
        assert!(parse_models(r#"{"other":[]}"#).is_err());
        assert!(parse_models("not json").is_err());
    }
}
