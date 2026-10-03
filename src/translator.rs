use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::llm::{
    nullable_string, object, risk_schema, slot_values_schema, slots_schema, JsonRequest, Llm, Reasoning, SlotDraft,
    SlotValue, TRANSLATOR_MODEL,
};
use crate::types::{Risk, Skill};

/// What the translator makes of "this phrase means this command".
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    #[serde(default)]
    pub existing_label: Option<String>,
    pub label: String,
    pub steps: Vec<String>,
    pub scenario: String,
    #[serde(default)]
    pub slots: Vec<SlotDraft>,
    #[serde(default)]
    pub slot_values: Vec<SlotValue>,
    pub risk: Risk,
}

fn draft_schema() -> Value {
    object(json!({
        "existingLabel": nullable_string(
            "label of a known skill whose steps produce exactly this command once slot values are filled in, else null",
        ),
        "label": { "type": "string", "description": "short kebab-case name for the skill, e.g. git-diff-summary" },
        "steps": {
            "type": "array",
            "items": { "type": "string" },
            "description": "the command, with instance-specific values replaced by {slot} placeholders",
        },
        "scenario": { "type": "string", "description": "one sentence starting with 'the user wants'" },
        "slots": slots_schema(),
        "slotValues": slot_values_schema(),
        "risk": risk_schema(),
    }))
}

const INSTRUCTIONS: &str = r#"You write one row of a decision matrix.

A fast classifier reads a user's plain-language request and picks the row whose "scenario" fits it best.
A person has just told us that a phrase means a command. Turn that single instance into a row that
matches every way someone might ask for the SAME outcome, and nothing else.

scenario
- One sentence starting with "the user wants". Describe the outcome the command produces, not the
  wording of the phrase and not the name of the tool.
- Be exactly as broad as the command. Everything the scenario covers must be something this command
  actually does. If you are unsure whether a neighbouring intent is covered, leave it out.
- Name the distinguishing detail: the form of the output, the scope, the ordering, the target.
  "a per-file summary of uncommitted changes" beats "see changes".
- Read knownSkills. If one is close, your scenario must say what differs from it, so the classifier can
  tell them apart. Never write a scenario that would also be true of a known skill.
- Do not copy details that only belong to this instance (a specific file name, a specific number).

steps and slots
- Replace values that belong only to this instance (a file, a branch, a count, a message) with {slot}
  placeholders and put the instance's value in slotValues. Never quote placeholders.
- Substituting slotValues into steps must reproduce the original command exactly.
- Flags, subcommands and fixed arguments are part of the skill. Do not turn them into slots.
- If nothing is instance-specific, return the command unchanged with no slots.
- Name each slot with the plain, full word for its value ("directory", not "d"). In aliases, list up to
  four other names a person might type for it as name=value ("dir", "path"). An alias must not be able
  to mean another slot of this command.

existingLabel
- Set it only when a known skill's steps, with some slot values, are exactly this command.

risk
- "safe" only reads. "modifies" changes local state and can be undone. "destructive" deletes data,
  rewrites history or changes remote systems."#;

pub struct TranslateInput<'a> {
    pub phrase: &'a str,
    pub steps: &'a [String],
    pub skills: &'a [Skill],
    /// What went wrong with the previous draft, when retrying.
    pub feedback: Option<&'a str>,
}

/// Turn one taught instance into a generalised row for the decision matrix.
pub fn translate(llm: &Llm, input: &TranslateInput) -> Result<(Draft, u64)> {
    let known: Vec<Value> = input
        .skills
        .iter()
        .map(|skill| json!({ "label": skill.label, "steps": skill.steps, "scenario": skill.scenario }))
        .collect();
    llm.complete_json(&JsonRequest {
        model: &TRANSLATOR_MODEL,
        system: INSTRUCTIONS,
        name: "matrix_row",
        timeout: Duration::from_secs(8),
        reasoning: Reasoning::Off,
        schema: draft_schema(),
        input: json!({
            "phrase": input.phrase,
            "command": input.steps,
            "previousAttemptProblem": input.feedback,
            "knownSkills": known,
        }),
    })
}
