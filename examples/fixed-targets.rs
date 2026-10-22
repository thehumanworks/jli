//! Offline contrastive evaluation of fixed-word targets. All commands are dry-run plans.
//! See examples/README.md: default characterises the gap; --strict fails on it; --oracle is a control.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Result};
use jli::context::read_context;
use jli::eval::{measure, Kind, Probe, Verdict};
use jli::llm::Llm;
use jli::plan::PlanRecord;
use jli::router::{route, Outcome, RouteOptions, DEFAULT_THRESHOLD, DRY_RUN_CODE, UNKNOWN_ARGUMENT};
use jli::store::{Brain, BrainConfig, NewSkill};
use jli::system1::{System1, NONE};
use jli::types::{Mode, Origin, Path, Risk, Slot, SlotKind};
use jli::ui::{Prompt, Ui};
use jli::Models;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    query: String,
    owner: String,
    command: String,
}

// Commands/scenarios and teaching phrases are independent of the held-out cases and fake replies.
const SKILLS: [(&str, &str, &str); 7] = [
    ("pull-origin", "git pull origin", "update the checkout from origin; origin is this fixture's default remote"),
    ("pull-upstream", "git pull upstream", "update from the remote literally named upstream"),
    ("pull-orbital", "git pull orbital", "update from the remote literally named orbital"),
    ("switch-main", "git switch main", "change the current branch to main"),
    ("switch-release", "git switch release", "change the current branch to release"),
    ("preview-private", "mise run deploy-preview", "publish my private preview, not shared with the team"),
    ("preview-shared", "mise run deploy-preview-shared", "publish the preview shared with the team"),
];

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Nobody;

impl Prompt for Nobody {
    fn read_line(&mut self) -> Option<String> {
        None
    }
    fn confirm(&mut self, _: &[String], _: &str) -> Option<bool> {
        None
    }
    fn ask(&mut self, _: &[String], _: &str) -> Option<String> {
        None
    }
}

fn saved_plan(brain: &Brain, outcome: &Outcome) -> Result<()> {
    ensure!(!outcome.ran && outcome.exit_code.is_none(), "the harness must never run a command");
    let id = outcome.plan_id.ok_or_else(|| anyhow::anyhow!("expected a saved plan"))?;
    let row = brain.plan(id)?.ok_or_else(|| anyhow::anyhow!("plan missing"))?;
    let plan: PlanRecord = serde_json::from_str(&row.body)?;
    ensure!(plan.commands == outcome.commands && plan.unknown_arguments == outcome.unknown_arguments);
    // Deliberately inspect only: jli run does not support dry execution of a saved plan (ADR18).
    Ok(())
}

#[derive(Default, Debug)]
struct Report {
    lines: Vec<String>,
    correct: usize,
    wrong: usize,
}

fn evaluate(oracle: bool) -> Result<Report> {
    let directory = std::env::temp_dir().join(format!(
        "jli-fixed-targets-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir(&directory)?;
    let scratch = Scratch(directory);
    let brain = Brain::open(BrainConfig::file(&scratch.0.join("brain.db")))?;
    for (label, command, scenario) in SKILLS {
        brain.add_skill(&NewSkill {
            label: label.into(),
            steps: vec![command.into()],
            scenario: scenario.into(),
            examples: vec![format!("taught {label}")],
            slots: vec![],
            risk: Risk::Modifies,
            origin: Origin::Taught,
        })?;
    }
    let cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/fixed-target-cases.json"))?;
    let choices: HashMap<String, String> = serde_json::from_str(include_str!("fixtures/fixed-target-choices.json"))?;
    let skills = brain.skills()?;
    let ids: HashSet<_> = cases.iter().map(|case| &case.id).collect();
    ensure!(ids.len() == cases.len() && choices.len() == cases.len());
    let mut replies = HashMap::new();
    for case in &cases {
        let chosen = choices.get(&case.id).ok_or_else(|| anyhow::anyhow!("missing choice: {}", case.id))?;
        ensure!(skills.iter().any(|skill| skill.label == *chosen));
        ensure!(skills.iter().any(|skill| skill.label == case.owner && skill.steps == [case.command.clone()]));
        ensure!(skills
            .iter()
            .all(|skill| !skill.examples.contains(&case.query) && !skill.counter_examples.contains(&case.query)));
        let choice = if oracle { &case.owner } else { chosen };
        ensure!(replies.insert(case.query.clone(), choice.clone()).is_none(), "duplicate query");
    }
    // A separate boundary probe: a fixed command must refuse an unknown named argument.
    replies.insert("pull remote=…".into(), "pull-origin".into());
    let unexpected = Arc::new(AtomicUsize::new(0));
    let missed = unexpected.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let models = Models {
        system1: System1::fake(move |request| {
            counted.fetch_add(1, Ordering::SeqCst);
            let query = request["state"]["request"].as_str().unwrap_or_default();
            let choice = replies.get(query).map(String::as_str).unwrap_or_else(|| {
                missed.fetch_add(1, Ordering::SeqCst);
                NONE
            });
            json!({"model":"fixed-target-fixture", "answers": {
                "isCommandRequest":{"type":"noul", "noul":1.0},
                "intentRisk":{"type":"score", "score":1.0, "confidence":1.0,
                    "legend":{"0":"read", "1":"modify", "2":"destroy"},
                    "probabilities":{"0":0.0, "1":1.0, "2":0.0}},
                "skill":{"type":"choice", "choice":choice, "confidence":1.0, "probabilities":{choice:1.0}}
            }})
        }),
        llm: Llm::with_transport(|_| Err("offline fixed-target harness: unexpected language-model call".into())),
    };
    let probes: Vec<_> = cases
        .iter()
        .map(|case| Probe {
            owner: case.owner.clone(),
            phrase: case.query.clone(),
            kind: Kind::Example,
            context: read_context(&scratch.0),
            requests: 1,
            rebuilt: false,
        })
        .collect();
    let (measured, failed) = measure(&models, &skills, &probes, DEFAULT_THRESHOLD)?;
    ensure!(failed == 0);
    let options = RouteOptions { dry_run: true, ..RouteOptions::new(scratch.0.clone(), DEFAULT_THRESHOLD, Mode::Manual) };
    let ui = Ui::of(true, true);
    let mut report = Report::default();
    for (case, judged) in cases.iter().zip(measured) {
        let judged = judged.ok_or_else(|| anyhow::anyhow!("probe failed: {}", case.id))?;
        let result = route(&brain, &models, &case.query, &ui, &mut Nobody, &options)?;
        saved_plan(&brain, &result)?;
        ensure!(result.path == Path::System1 && result.blocked_code.as_deref() == Some(DRY_RUN_CODE));
        let correct = result.commands == [case.command.clone()];
        ensure!(correct == (judged.verdict == Verdict::Home), "label and command verdicts diverged: {}", case.id);
        report.correct += usize::from(correct);
        report.wrong += usize::from(!correct);
        report.lines.push(format!(
            "{}: {} | wanted {} | planned {}",
            if correct { "OK" } else { "WRONG" },
            case.id,
            case.command,
            result.commands.join(" && ")
        ));
    }

    let stated = route(&brain, &models, "pull remote=upstream", &ui, &mut Nobody, &options)?;
    saved_plan(&brain, &stated)?;
    ensure!(stated.blocked_code.as_deref() == Some(UNKNOWN_ARGUMENT) && stated.unknown_arguments == ["remote"]);
    report.lines.push("CONTROL: literal skill refuses remote=upstream (unknown_argument)".into());
    // Explicit personal meaning outranks an evaluator's unbound natural-language interpretation.
    brain.bind_phrase("pull from upstream", "pull-origin", "teach", false)?;
    let before = calls.load(Ordering::SeqCst);
    let bound = route(&brain, &models, "pull from upstream", &ui, &mut Nobody, &options)?;
    saved_plan(&brain, &bound)?;
    ensure!(bound.path == Path::Binding && bound.commands == ["git pull origin"]);
    ensure!(calls.load(Ordering::SeqCst) == before);
    report.lines.push("CONTROL: explicit teaching keeps git pull origin (binding, no model call)".into());
    brain.add_skill(&NewSkill {
        label: "pull-remote".into(),
        steps: vec!["git pull {remote}".into()],
        scenario: "pull a named remote".into(),
        examples: vec![],
        risk: Risk::Modifies,
        origin: Origin::Taught,
        slots: vec![Slot {
            name: "remote".into(),
            kind: SlotKind::Text,
            description: "remote name".into(),
            default: None,
            aliases: vec![],
            optional: false,
            flag: None,
            choices: vec![],
        }],
    })?;
    brain.bind_phrase("pull remote=origin", "pull-remote", "teach", false)?;
    let stated = route(&brain, &models, "pull remote=upstream", &ui, &mut Nobody, &options)?;
    saved_plan(&brain, &stated)?;
    ensure!(stated.path == Path::Binding && stated.commands == ["git pull upstream"]);
    ensure!(stated.blocked_code.as_deref() == Some(DRY_RUN_CODE) && calls.load(Ordering::SeqCst) == before);
    ensure!(unexpected.load(Ordering::SeqCst) == 0);
    report.lines.push("CONTROL: stated template value keeps git pull upstream (binding, no model call)".into());
    Ok(report)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(args.iter().all(|arg| arg == "--oracle" || arg == "--strict"), "usage: fixed-targets [--oracle] [--strict]");
    let oracle = args.iter().any(|arg| arg == "--oracle");
    let report = evaluate(oracle)?;
    println!(
        "Offline synthetic {} choices; no commands executed, no live model measured.",
        if oracle { "oracle" } else { "adversarial" }
    );
    for line in report.lines {
        println!("{line}");
    }
    println!("Held-out commands: {} correct, {} wrong; 3 boundary controls passed.", report.correct, report.wrong);
    if args.iter().any(|arg| arg == "--strict") {
        ensure!(report.wrong == 0, "fixed-target acceptance failed: {} wrong commands", report.wrong);
    }
    println!("Characterisation completed; this is not a live routing-quality score.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adversarial_choices_expose_the_known_gap() {
        let report = evaluate(false).expect("offline fixture runs");
        assert_eq!((report.correct, report.wrong), (7, 5));
    }

    #[test]
    fn oracle_choices_preserve_all_contrastive_targets() {
        let report = evaluate(true).expect("offline fixture runs");
        assert_eq!((report.correct, report.wrong), (12, 0));
    }
}
