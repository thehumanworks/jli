use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::context::Context;
use crate::llm::{object, JsonRequest, Llm, Reasoning, TRANSLATOR_MODEL};
use crate::types::Skill;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    WrongCommand,
    Environment,
    WrongValue,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedOption {
    pub command: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    pub verdict: Verdict,
    #[serde(default)]
    pub explanation: String,
    #[serde(default)]
    pub options: Vec<SuggestedOption>,
}

fn schema() -> Value {
    object(json!({
        "verdict": {
            "type": "string",
            "enum": ["wrong_command", "environment", "wrong_value", "not_applicable"],
            "description": concat!(
                "wrong_command: the command does not do what the request asked. ",
                "environment: the command fits the request, but this machine or directory is not in a state where it can work. ",
                "wrong_value: the right command with a wrong argument value. ",
                "not_applicable: there was no failure to judge.",
            ),
        },
        "explanation": { "type": "string", "description": "one short sentence for the user, in plain words" },
        "options": {
            "type": "array",
            "description": "up to three alternative commands, most likely first",
            "items": object(json!({
                "command": { "type": "string", "description": "a complete, non-interactive shell command" },
                "description": { "type": "string", "description": "what the user would get, in at most twelve plain words" },
            })),
        },
    }))
}

const INSTRUCTIONS: &str = r#"A command-line assistant mapped a user's plain-language request to a command.
The user declined it, or it ran and failed. Work out what the user actually expected.

verdict
- Judge it only when "failure" is given; otherwise return "not_applicable".
- "environment" when the command is a sensible answer to the request and failed because of where it ran:
  not a git repository, a missing file or tool, no network, no permission. This is the most common case.
- "wrong_value" when the command is right but an argument (a file, branch, number) is wrong.
- "wrong_command" only when a different command is what the request called for.

options
- When there is no failure, the user looked at the command and said no. They wanted something else,
  so always return three options: nearby intents, other flags, another scope or level of detail.
- Options are commands the user more likely wanted, most likely first, each meaningfully different
  from the others and from the commands already tried.
- Complete and non-interactive, for the user's platform. No placeholders: use real values from the
  request and the directory listing.
- Describe each by its outcome, the way the user would say what they expected.
- For "environment", return no options.
- Prefer commands that known skills already run when one fits."#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub exit_code: i32,
    pub stderr: String,
}

pub struct SuggestInput<'a> {
    pub query: &'a str,
    pub context: &'a Context,
    pub entries: &'a [String],
    pub skills: &'a [Skill],
    /// Commands already offered or run for this request.
    pub tried: &'a [String],
    pub failure: Option<Failure>,
}

/// Diagnose a declined or failed command and offer what the user more likely expected.
pub fn suggest(llm: &Llm, input: &SuggestInput) -> Result<(Suggestion, u64)> {
    let known: Vec<Value> =
        input.skills.iter().map(|skill| json!({ "steps": skill.steps, "scenario": skill.scenario })).collect();
    let (mut data, ms) = llm.complete_json::<Suggestion>(&JsonRequest {
        model: &TRANSLATOR_MODEL,
        system: INSTRUCTIONS,
        name: "suggestion",
        schema: schema(),
        timeout: Duration::from_secs(10),
        reasoning: Reasoning::Off,
        input: json!({
            "request": input.query,
            "platform": input.context.platform,
            "insideGitRepository": input.context.inside_git_repository,
            "directoryEntries": input.entries.iter().take(60).collect::<Vec<_>>(),
            "commandsTried": input.tried,
            "failure": input.failure,
            "knownSkills": known,
        }),
    })?;
    if data.verdict == Verdict::Environment {
        data.options.clear();
    }
    data.options.retain(|option| !option.command.trim().is_empty() && !input.tried.contains(&option.command));
    data.options.truncate(3);
    Ok((data, ms))
}
