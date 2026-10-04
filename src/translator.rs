use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::llm::{
    nullable_string, object, risk_schema, slot_values_schema, slots_schema, JsonRequest, Llm, Reasoning, SlotDraft,
    SlotValue,
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
- description, when present, is the person's own account of when and why to run this command. Keep its
  meaning and never contradict it. Where it is broader than the command, describe what the command does.
- name, when present, is what the person calls this skill. Read it as a hint to what they mean.

steps and slots
- Replace values that belong only to this instance (a file, a branch, a count, a message) with {slot}
  placeholders and put the instance's value in slotValues. Never quote placeholders.
- Substituting slotValues into steps must reproduce the original command exactly.
- Flags, subcommands and fixed arguments are part of the skill. Do not turn them into slots.
- A value given with a flag is optional when the command does the same job without the flag: `git log`
  still lists commits without `-n 3`, and `cargo build` still builds without `--profile release`. Then
  set optional to true and flag to the flag as it is written ("-n", "--profile", or "--profile=" for
  `--profile=release`), keep the flag in steps right before its placeholder, and give no default. A
  request that does not name the value leaves out both.
- A value the command cannot do its job without is not optional, and its flag is null: the file of
  `wc -l notes.txt`, the path of `cargo install --path .`.
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
    /// What the person calls the skill, from `jli teach --name`.
    pub name: Option<&'a str>,
    /// When and why to run it, in the person's words, from `jli teach --description`.
    pub description: Option<&'a str>,
}

fn known_skills(skills: &[Skill]) -> Vec<Value> {
    skills.iter().map(|skill| json!({ "label": skill.label, "steps": skill.steps, "scenario": skill.scenario })).collect()
}

/// Turn one taught instance into a generalised row for the decision matrix.
pub fn translate(llm: &Llm, input: &TranslateInput) -> Result<(Draft, u64)> {
    llm.complete_json(&JsonRequest {
        model: &llm.translator_model(),
        system: INSTRUCTIONS,
        name: "matrix_row",
        timeout: Duration::from_secs(8),
        reasoning: Reasoning::Off,
        schema: draft_schema(),
        input: json!({
            "phrase": input.phrase,
            "command": input.steps,
            "name": input.name,
            "description": input.description,
            "previousAttemptProblem": input.feedback,
            "knownSkills": known_skills(input.skills),
        }),
    })
}

/// What the translator makes of a phrase and a command that is kept exactly as taught (ADR 30).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Description {
    pub label: String,
    pub scenario: String,
    pub risk: Risk,
}

fn description_schema() -> Value {
    object(json!({
        "label": { "type": "string", "description": "short kebab-case name for the skill, e.g. git-pull-origin" },
        "scenario": { "type": "string", "description": "one sentence starting with 'the user wants'" },
        "risk": risk_schema(),
    }))
}

const DESCRIBE: &str = r#"You write the scenario of one row of a decision matrix.

A fast classifier reads a user's plain-language request and picks the row whose "scenario" fits it best.
A person has just told us that a phrase means a command. The command is kept exactly as they wrote it
and runs as written every time. Write the scenario that makes every way of asking for what this command
does land on this row, and nothing else.

scenario
- One sentence starting with "the user wants". Describe the outcome of running the command as written,
  not the wording of the phrase and not the name of the tool.
- Every value in the command is fixed: a remote, a branch, a file, a task, a number. Name the ones that
  set this command apart, so that a request for another value does not fit: "the three most recent
  commits", not "recent commits".
- Be exactly as broad as the command. If you are unsure whether a neighbouring intent is covered, leave
  it out.
- description, when present, is the person's own account of when and why to run this command. Keep its
  meaning and never contradict it. Where it is broader than the command, describe what the command does.
- name, when present, is what the person calls this skill. Read it as a hint to what they mean.
- Read knownSkills. If one is close, your scenario must say what differs from it, so the classifier can
  tell them apart. Never write a scenario that would also be true of a known skill.

label
- A short kebab-case name for what the command does.

risk
- "safe" only reads. "modifies" changes local state and can be undone. "destructive" deletes data,
  rewrites history or changes remote systems."#;

/// Write the scenario for a command that is stored as taught: the default of `jli teach` (ADR 30).
/// The steps are not the model's to change, so it is given no way to change them.
pub fn describe(llm: &Llm, input: &TranslateInput) -> Result<(Description, u64)> {
    llm.complete_json(&JsonRequest {
        model: &llm.translator_model(),
        system: DESCRIBE,
        name: "skill_scenario",
        timeout: Duration::from_secs(8),
        reasoning: Reasoning::Off,
        schema: description_schema(),
        input: json!({
            "phrase": input.phrase,
            "command": input.steps,
            "name": input.name,
            "description": input.description,
            "previousAttemptProblem": input.feedback,
            "knownSkills": known_skills(input.skills),
        }),
    })
}
