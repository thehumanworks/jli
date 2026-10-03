//! Plans: a route worked out once, stored, and run later exactly as it was shown, like a prepared
//! statement. A dry run, or a run nobody was there to confirm, stores one and returns its id;
//! `jli run <planId> …` runs it with no model call and no routing. See docs/adr/0018-agent-contract.md.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::args;
use crate::router::{
    run_plan, Outcome, Ready, RouteOptions, ALREADY_RAN, DIRECTORY_MISSING, MISSING_VALUE, NOT_FOUND, NOT_RUN, PLAN_CHANGED,
    PLAN_EXPIRED,
};
use crate::slots::render;
use crate::store::{now_iso, Brain, PlanRow};
use crate::types::{coded, error_code, Mode, Path, Risk, Slot, SlotKind, USAGE};
use crate::ui::{Prompt, Ui};
use crate::Models;

/// How long a plan can be run after it was made. Long enough to review it and come back, short
/// enough that the directory and the brain it was worked out against are still the same.
pub const PLAN_TTL: Duration = Duration::from_secs(60 * 60);

/// A plan that is waiting for nothing but a go-ahead.
pub const READY: &str = "ready";
/// A plan that needs a value for a slot first, or named an argument no slot takes.
pub const INCOMPLETE: &str = "incomplete";

/// One slot's value, and where it came from: `request`, `pattern`, `system1`, `agent`, `default`,
/// or `caller` for a value given to `jli run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotValue {
    pub name: String,
    pub value: String,
    pub source: String,
}

/// A slot as an agent calling jli needs to know it, to give it a value with `name=value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlotInfo {
    pub name: String,
    pub kind: SlotKind,
    pub description: String,
    pub aliases: Vec<String>,
    pub default: Option<String>,
}

impl SlotInfo {
    pub fn of(slot: &Slot) -> SlotInfo {
        SlotInfo {
            name: slot.name.clone(),
            kind: slot.kind,
            description: slot.description.clone(),
            aliases: slot.aliases.clone(),
            default: slot.default.clone(),
        }
    }

    /// A placeholder the steps use that no slot declares: it can still be given a value by its name.
    fn undeclared(name: &str) -> SlotInfo {
        SlotInfo { name: name.to_string(), kind: SlotKind::Text, description: String::new(), aliases: vec![], default: None }
    }
}

/// System 1's runners-up, as `--json` reports them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CandidateInfo {
    pub label: String,
    pub probability: f64,
}

/// Everything needed to run a route again without thinking again. Written as JSON into the plan's
/// row; who made it, where, and until when are columns of that row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRecord {
    pub query: String,
    /// What is learned when it runs: the request with its values hidden.
    pub phrase: String,
    pub path: Path,
    pub confidence: Option<f64>,
    pub label: String,
    /// A proposal not in the brain yet: the skill below is stored if the run teaches it.
    pub is_new: bool,
    pub steps: Vec<String>,
    pub scenario: String,
    pub skill_risk: Risk,
    pub slots: Vec<Slot>,
    /// The risk the confirmation rule is applied to.
    pub risk: Risk,
    pub commands: Vec<String>,
    pub values: Vec<SlotValue>,
    /// Slots, and undeclared placeholders, with no value yet.
    pub missing: Vec<String>,
    /// `name=` arguments of the request that fit no slot. They are not in `commands`.
    pub unknown_arguments: Vec<String>,
    /// Names the request used for a slot, as (slot, name), kept as aliases when it runs.
    pub aliases: Vec<(String, String)>,
    pub ms_system1: u64,
    pub ms_system2: u64,
}

impl PlanRecord {
    pub fn state(&self) -> &'static str {
        if self.missing.is_empty() && self.unknown_arguments.is_empty() {
            READY
        } else {
            INCOMPLETE
        }
    }

    /// The slot or placeholder `key` names, if it is one of the missing ones.
    fn missing_slot(&self, key: &str) -> Option<String> {
        self.missing
            .iter()
            .find(|name| match self.slots.iter().find(|slot| &&slot.name == name) {
                Some(slot) => slot.answers_to(key),
                None => *name == key,
            })
            .cloned()
    }

    pub fn missing_info(&self) -> Vec<SlotInfo> {
        self.missing
            .iter()
            .map(|name| {
                self.slots.iter().find(|slot| &slot.name == name).map_or_else(|| SlotInfo::undeclared(name), SlotInfo::of)
            })
            .collect()
    }
}

/// A stable fingerprint of the commands, FNV-1a over each command and a separator. It is kept
/// beside the plan, and the plan is refused if the commands no longer produce it.
pub fn commands_hash(commands: &[String]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in commands.iter().flat_map(|command| command.bytes().chain([0u8])) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Store a plan for this user and return its id.
pub fn save(brain: &Brain, cwd: &std::path::Path, record: &PlanRecord) -> Result<i64> {
    brain.save_plan(
        &cwd.to_string_lossy(),
        record.state(),
        &serde_json::to_string(record)?,
        &commands_hash(&record.commands),
        PLAN_TTL,
    )
}

/// How `jli run` was asked to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunFlags {
    pub yolo: bool,
    pub capture: bool,
}

/// A `name=value` given to `jli run`.
pub fn supplied(word: &str) -> Option<(String, String)> {
    let (key, value) = word.split_once('=')?;
    let mut letters = key.bytes();
    let typable = letters.next().is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && letters.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    typable.then(|| (key.to_string(), value.to_string()))
}

fn blocked(mut outcome: Outcome, code: &str, why: String, ui: &Ui) -> Outcome {
    ui.warn(format!("{why}."));
    outcome.blocked = Some(why);
    outcome.blocked_code = Some(code.to_string());
    outcome
}

/// What a plan's outcome says before anything is decided about running it.
pub fn outcome_of(id: i64, record: &PlanRecord) -> Outcome {
    let mut outcome = Outcome::new(&record.query);
    outcome.path = record.path;
    outcome.label = Some(record.label.clone());
    outcome.commands = record.commands.clone();
    outcome.confidence = record.confidence;
    outcome.risk = Some(record.risk);
    outcome.plan_id = Some(id);
    outcome.phrase = Some(record.phrase.clone());
    outcome.values = record.values.clone();
    outcome.accepts = record.slots.iter().map(SlotInfo::of).collect();
    outcome
}

fn load(brain: &Brain, id: i64) -> Result<Option<(PlanRow, PlanRecord)>> {
    let Some(row) = brain.plan(id)? else { return Ok(None) };
    let record = serde_json::from_str(&row.body)
        .map_err(|error| coded(crate::types::STORAGE, format!("plan {id} could not be read: {error}")))?;
    Ok(Some((row, record)))
}

/// Run the plans in the order given, each exactly as stored, stopping at the first that fails or
/// is refused. Every plan gets an outcome, in order: the ones after a stop say they were not run.
/// An error in one plan becomes that plan's outcome, so what already ran is never lost.
pub fn run_plans(
    brain: &Brain,
    models: &Models,
    ids: &[i64],
    values: &[(String, String)],
    ui: &Ui,
    prompt: &mut dyn Prompt,
    flags: RunFlags,
) -> Result<Vec<Outcome>> {
    // A value has to be for some plan's missing slot; anything else is a mistake to stop on before running anything.
    let records: Vec<PlanRecord> =
        ids.iter().filter_map(|id| load(brain, *id).ok().flatten().map(|(_, record)| record)).collect();
    for (key, _) in values {
        if !records.iter().any(|record| record.missing_slot(key).is_some()) {
            return Err(coded(USAGE, format!("none of these plans is missing a value for {key}=")));
        }
    }

    let mut outcomes = Vec::new();
    let mut stopped = false;
    for &id in ids {
        if stopped {
            let mut outcome =
                load(brain, id).ok().flatten().map_or_else(|| Outcome::new(""), |(_, record)| outcome_of(id, &record));
            outcome.plan_id = Some(id);
            outcome.blocked = Some("not run: an earlier plan failed or was not run".into());
            outcome.blocked_code = Some(NOT_RUN.into());
            outcomes.push(outcome);
            continue;
        }
        let outcome = match run_one(brain, models, id, values, ui, prompt, flags) {
            Ok(outcome) => outcome,
            Err(error) => {
                let mut outcome = Outcome::new("");
                outcome.plan_id = Some(id);
                outcome.error = Some(error_code(&error).into());
                outcome.error_message = Some(format!("{error:#}"));
                outcome
            }
        };
        stopped = !outcome.ran || outcome.exit_code != Some(0) || outcome.error.is_some();
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

fn run_one(
    brain: &Brain,
    models: &Models,
    id: i64,
    values: &[(String, String)],
    ui: &Ui,
    prompt: &mut dyn Prompt,
    flags: RunFlags,
) -> Result<Outcome> {
    let Some((row, record)) = load(brain, id)? else {
        let mut outcome = Outcome::new("");
        outcome.plan_id = Some(id);
        return Ok(blocked(outcome, NOT_FOUND, format!("there is no plan {id}"), ui));
    };
    let outcome = outcome_of(id, &record);
    ui.note(format!("plan {id}: {}", record.query));
    if row.state != READY && row.state != INCOMPLETE {
        return Ok(blocked(outcome, ALREADY_RAN, format!("plan {id} has already run, and a plan runs once"), ui));
    }
    if row.expires_at <= now_iso() {
        return Ok(blocked(outcome, PLAN_EXPIRED, format!("plan {id} expired at {}", row.expires_at), ui));
    }
    if commands_hash(&record.commands) != row.commands_hash {
        return Ok(blocked(outcome, PLAN_CHANGED, format!("plan {id} no longer holds the commands it was made with"), ui));
    }
    let cwd = PathBuf::from(&row.cwd);
    if !cwd.is_dir() {
        return Ok(blocked(outcome, DIRECTORY_MISSING, format!("the directory plan {id} runs in is gone: {}", row.cwd), ui));
    }

    // The caller's values fill what is missing, as written (ADR 11). A pinned value never changes.
    let mut pinned: HashMap<String, String> =
        record.values.iter().map(|value| (value.name.clone(), value.value.clone())).collect();
    let mut given = record.values.clone();
    let mut missing = record.missing.clone();
    for (key, value) in values {
        let Some(name) = record.missing_slot(key) else { continue };
        let is_number = record.slots.iter().any(|slot| slot.name == name && slot.kind == SlotKind::Number);
        let value = if is_number { args::as_number(value) } else { value.clone() };
        missing.retain(|open| *open != name);
        pinned.insert(name.clone(), value.clone());
        given.push(SlotValue { name, value, source: "caller".into() });
    }
    let commands = if record.missing.is_empty() { record.commands.clone() } else { render(&record.steps, &pinned) };
    let mut outcome = Outcome { commands: commands.clone(), values: given.clone(), ..outcome };
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|name| format!("{name}=")).collect();
        let still = PlanRecord { missing, ..record.clone() };
        outcome.missing = still.missing_info();
        let why = format!("plan {id} still needs a value: give it as jli run {id} {}", names.join(" "));
        return Ok(blocked(outcome, MISSING_VALUE, why, ui));
    }
    outcome.unknown_arguments = record.unknown_arguments.clone();

    let options = RouteOptions {
        yes: true,
        yolo: flags.yolo,
        capture: flags.capture,
        ..RouteOptions::new(cwd.clone(), crate::router::DEFAULT_THRESHOLD, Mode::Semi)
    };
    let ready = Ready { id, state: &row.state, record: &record, commands, cwd };
    run_plan(brain, models, &ready, outcome, ui, prompt, &options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_stable_and_sees_where_commands_split() {
        let one = |list: &[&str]| commands_hash(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(one(&["ls -ltr"]), one(&["ls -ltr"]));
        assert_eq!(one(&[]), "cbf29ce484222325", "FNV-1a's offset basis: the hash does not move between builds");
        assert_ne!(one(&["ls", "-l"]), one(&["ls -l"]));
        assert_ne!(one(&["ab", "c"]), one(&["a", "bc"]));
    }

    #[test]
    fn a_value_for_jli_run_is_a_typable_name() {
        assert_eq!(supplied("message=fix the parser"), Some(("message".into(), "fix the parser".into())));
        assert_eq!(supplied("n="), Some(("n".into(), String::new())));
        assert_eq!(supplied("12"), None);
        assert_eq!(supplied("-m=x"), None);
        assert_eq!(supplied("=x"), None);
    }
}
