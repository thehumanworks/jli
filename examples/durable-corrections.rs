//! Offline, self-checking demo: explicit corrections survive later learning.
//! Run `cargo run --quiet --example durable-corrections`; see examples/README.md.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Result};
use jli::llm::Llm;
use jli::router::{route, RouteOptions};
use jli::store::{Brain, BrainConfig, NewSkill};
use jli::system1::System1;
use jli::types::{Mode, Origin, Path, Risk};
use jli::ui::{Prompt, Ui};
use jli::Models;
use serde_json::json;

const PHRASE: &str = "show my greeting";

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The person says no to the wrong command, then supplies the right one.
struct Correct;

impl Prompt for Correct {
    fn read_line(&mut self) -> Option<String> {
        None
    }

    fn confirm(&mut self, _: &[String], _: &str) -> Option<bool> {
        Some(false)
    }

    fn ask(&mut self, _: &[String], _: &str) -> Option<String> {
        Some("printf corrected".into())
    }
}

fn main() -> Result<()> {
    let directory = std::env::temp_dir().join(format!(
        "jli-correction-demo-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    // Acquire an unused directory before owning its cleanup.
    std::fs::create_dir(&directory)?;
    let scratch = Scratch(directory);
    let config = BrainConfig::file(&scratch.0.join("brain.db"));
    let brain = Brain::open(config.clone())?;
    for (label, command) in [("wrong-greeting", "printf wrong"), ("right-greeting", "printf corrected")] {
        brain.add_skill(&NewSkill {
            label: label.into(),
            steps: vec![command.into()],
            scenario: "a synthetic greeting".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![],
        })?;
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let models = Models {
        system1: System1::fake(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            json!({"model": "deliberately-wrong-fixture", "answers": {
                "isCommandRequest": {"type": "noul", "noul": 1.0},
                "intentRisk": {"type": "score", "score": 0.0, "confidence": 1.0,
                    "legend": {"0": "read", "1": "modify", "2": "destroy"},
                    "probabilities": {"0": 1.0, "1": 0.0, "2": 0.0}},
                "skill": {"type": "choice", "choice": "wrong-greeting", "confidence": 1.0,
                    "probabilities": {"wrong-greeting": 1.0}}
            }})
        }),
        llm: Llm::with_transport(|_| Err("This offline demo must not ask a language model".into())),
    };
    let ui = Ui::new(false);
    let options = RouteOptions { capture: true, ..RouteOptions::new(scratch.0.clone(), 0.5, Mode::Manual) };
    let dry = RouteOptions { dry_run: true, ..options.clone() };

    println!("Offline fixture: System 1 always chooses the wrong greeting at confidence 1.0.");
    let before = route(&brain, &models, PHRASE, &ui, &mut Correct, &dry)?;
    ensure!(before.path == Path::System1 && before.commands == ["printf wrong"]);
    println!("Before correction: printf wrong (system1, dry run)");
    let corrected = route(&brain, &models, PHRASE, &ui, &mut Correct, &options)?;
    ensure!(corrected.path == Path::Taught && corrected.exit_code == Some(0));
    ensure!(corrected.stdout.as_deref() == Some("corrected"));
    let binding = brain.bindings()?.remove(0);
    ensure!(binding.rule.target.as_ref().is_some_and(|target| target.label == "right-greeting"));
    ensure!(binding.rule.rejected.iter().any(|target| target.label == "wrong-greeting"));
    ensure!(binding.rule.source == "prompt");
    println!("Reject wrong command; type printf corrected. Output: corrected");
    println!("Stored binding: right-greeting; rejected: wrong-greeting; source: prompt");

    for lesson in 1..=50 {
        brain.link_phrase("right-greeting", &format!("later lesson {lesson}"))?;
        brain.add_counter_example("wrong-greeting", &format!("later rejection {lesson}"))?;
        if [1, 10, 50].contains(&lesson) {
            let result = route(&brain, &models, PHRASE, &ui, &mut Correct, &dry)?;
            ensure!(result.path == Path::Binding && result.commands == ["printf corrected"]);
            ensure!(calls.load(Ordering::SeqCst) == 2, "a binding must precede the wrong model");
            println!("After {lesson} later lessons: printf corrected (binding, no model call)");
        }
    }
    ensure!(!brain.skill("right-greeting")?.expect("fixture skill exists").examples.contains(&PHRASE.into()));
    println!("Original bounded example has expired; the binding remains.");
    drop(brain);
    let reopened = Brain::open(config)?;
    ensure!(reopened.bindings()?.iter().any(|saved| {
        saved.id == binding.id && saved.rule.rejected.iter().any(|target| target.label == "wrong-greeting")
    }));
    let result = route(&reopened, &models, PHRASE, &ui, &mut Correct, &dry)?;
    ensure!(result.path == Path::Binding && result.commands == ["printf corrected"]);
    ensure!(calls.load(Ordering::SeqCst) == 2);
    println!("After database reopen: printf corrected (binding, no model call)");
    ensure!(reopened.revoke_binding(binding.id)?);
    let result = route(&reopened, &models, PHRASE, &ui, &mut Correct, &dry)?;
    ensure!(result.path == Path::System1 && result.commands == ["printf wrong"]);
    println!("After revocation: printf wrong (system1, dry run)");
    println!("All checks passed. This demonstrates persistence, not live model quality.");
    Ok(())
}
