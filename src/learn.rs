use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::context::read_context;
use crate::llm::TRANSLATOR_MODEL;
use crate::slots::{match_template, render, undeclared};
use crate::store::{slug, Brain, NewSkill};
use crate::system1::Decision;
use crate::translator::{translate, TranslateInput};
use crate::types::{with_checked_aliases, Mode, Origin, Risk, Skill, Slot};
use crate::ui::{c, duration, fixed, Ui};
use crate::Models;

pub struct LearnInput {
    pub phrase: String,
    /// The concrete command the phrase should run.
    pub steps: Vec<String>,
    /// Overrides the translator's reading of the risk.
    pub risk: Option<Risk>,
    pub mode: Mode,
    pub threshold: f64,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Learned {
    pub label: String,
    pub created: bool,
    /// True when the stored row was written by the translator and passed verification.
    pub translated: bool,
}

/// How many phrasings of other skills are replayed to check a new row does not steal them.
const NEIGHBOUR_CHECKS: usize = 8;

fn comparable(command: &str) -> String {
    let unquoted: String = command.chars().filter(|ch| *ch != '\'' && *ch != '"').collect();
    unquoted.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Ask System 1 about each phrase at once, one thread per call.
fn decide_all(models: &Models, phrases: &[&str], skills: &[Skill], cwd: &std::path::Path) -> Result<Vec<Decision>> {
    let context = read_context(cwd);
    std::thread::scope(|scope| {
        let calls: Vec<_> = phrases
            .iter()
            .map(|phrase| {
                let context = &context;
                scope.spawn(move || models.system1.decide(phrase, context, skills))
            })
            .collect();
        calls.into_iter().map(|call| call.join().unwrap_or_else(|_| Err(anyhow!("a System 1 check panicked")))).collect()
    })
}

/// Check a freshly written row against the real classifier.
/// Returns what is wrong with it, or a summary when it holds.
fn verify(brain: &Brain, models: &Models, label: &str, input: &LearnInput) -> Result<std::result::Result<String, String>> {
    let skills = brain.skills()?;
    let phrase = input.phrase.trim().to_lowercase();
    let neighbours: Vec<(&str, &str)> = skills
        .iter()
        .filter(|skill| skill.label != label)
        .flat_map(|skill| {
            let recent = &skill.examples[skill.examples.len().saturating_sub(2)..];
            recent.iter().map(move |example| (example.as_str(), skill.label.as_str()))
        })
        .filter(|(example, _)| example.trim().to_lowercase() != phrase)
        .take(NEIGHBOUR_CHECKS)
        .collect();

    let mut phrases = vec![input.phrase.as_str()];
    phrases.extend(neighbours.iter().map(|(example, _)| *example));
    let decisions = decide_all(models, &phrases, &skills, &input.cwd)?;
    let (own, others) = decisions.split_first().expect("the phrase itself is always checked");

    if own.skill.as_ref().map(|skill| skill.label.as_str()) != Some(label) || own.confidence < input.threshold {
        let got = own.skill.as_ref().map_or("no skill", |skill| skill.label.as_str());
        return Ok(Err(format!(
            "the phrase itself routed to {got} at {}, so the scenario does not describe it well enough",
            fixed(own.confidence, 2)
        )));
    }
    for (decision, (example, owner)) in others.iter().zip(&neighbours) {
        if decision.skill.as_ref().is_some_and(|skill| skill.label == label) {
            return Ok(Err(format!("the scenario is too broad: it captured \"{example}\", which belongs to {owner}")));
        }
    }
    let mut summary = format!("the phrase routes to it at {}", fixed(own.confidence, 2));
    if !neighbours.is_empty() {
        summary.push_str(&format!(", and {} phrasings of other skills still route where they did", neighbours.len()));
    }
    Ok(Ok(summary))
}

struct Kept {
    label: String,
    steps: Vec<String>,
    slots: Vec<Slot>,
    risk: Risk,
}

fn store(brain: &Brain, kept: &Kept, scenario: String, phrase: &str) -> Result<()> {
    brain.add_skill(&NewSkill {
        label: kept.label.clone(),
        steps: kept.steps.clone(),
        scenario,
        risk: kept.risk,
        origin: Origin::Taught,
        examples: vec![phrase.to_string()],
        slots: kept.slots.clone(),
    })
}

/// Link a phrase to a command. In manual mode the pair is stored as given. Otherwise a fast model
/// rewrites it into a general row, and the row is kept only if the classifier confirms that it
/// matches the phrase and does not capture phrasings that belong to other skills.
pub fn learn_phrase(brain: &Brain, models: &Models, input: &LearnInput, ui: &Ui) -> Result<Learned> {
    let command = input.steps.join(" && ");
    let skills = brain.skills()?;
    // A command some skill already runs, exactly or with other slot values, needs no model at all.
    if let Some(known) = skills.iter().find(|skill| match_template(&skill.steps, &command).is_some()) {
        brain.link_phrase(&known.label, &input.phrase)?;
        return Ok(Learned { label: known.label.clone(), created: false, translated: false });
    }
    if input.mode == Mode::Manual {
        let (label, created) = brain.learn_command(&input.phrase, &input.steps, input.risk.unwrap_or(Risk::Modifies))?;
        return Ok(Learned { label, created, translated: false });
    }

    let mut feedback: Option<String> = None;
    let mut kept: Option<Kept> = None;
    for attempt in 0..2 {
        let translated = translate(
            &models.llm,
            &TranslateInput { phrase: &input.phrase, steps: &input.steps, skills: &skills, feedback: feedback.as_deref() },
        );
        let draft = match translated {
            Ok((draft, ms)) => {
                ui.note(format!("{} generalised it in {}: {}", *TRANSLATOR_MODEL, duration(ms), c::dim(&draft.scenario)));
                draft
            }
            Err(error) => {
                ui.warn(format!("Translator unavailable, storing the phrase as given. {error:#}"));
                break;
            }
        };
        let values: HashMap<String, String> =
            draft.slot_values.iter().map(|slot| (slot.name.clone(), slot.value.clone())).collect();

        // "This is a known skill with other values": trust it only if the arithmetic checks out.
        let existing = draft.existing_label.as_ref().and_then(|label| skills.iter().find(|skill| &skill.label == label));
        if let Some(existing) = existing {
            if comparable(&render(&existing.steps, &values).join(" && ")) == comparable(&command) {
                brain.link_phrase(&existing.label, &input.phrase)?;
                return Ok(Learned { label: existing.label.clone(), created: false, translated: true });
            }
        }

        // Slots are kept only if filling them back in reproduces the command that was taught.
        let slots: Vec<Slot> = draft
            .slots
            .iter()
            .map(|slot| Slot {
                name: slot.name.clone(),
                kind: slot.kind,
                description: slot.description.clone(),
                default: slot.default.clone(),
                aliases: slot.aliases.clone(),
            })
            .collect();
        let slots = with_checked_aliases(slots);
        // A flag is part of what the skill does, never a value. Models sometimes slot one anyway.
        let slots_a_flag = values.values().any(|value| value.trim().starts_with('-'));
        let faithful = !slots_a_flag
            && undeclared(&draft.steps, &slots).is_empty()
            && comparable(&render(&draft.steps, &values).join(" && ")) == comparable(&command);
        if !faithful && !draft.slots.is_empty() {
            ui.note("Its slots did not reproduce your command, so the command is stored literally.");
        }

        let mut label = slug(&draft.label);
        while skills.iter().any(|skill| skill.label == label) {
            label.push_str("-x");
        }
        let row = Kept {
            label: label.clone(),
            steps: if faithful { draft.steps.clone() } else { input.steps.clone() },
            slots: if faithful { slots } else { vec![] },
            risk: input.risk.unwrap_or(draft.risk),
        };
        store(brain, &row, draft.scenario.clone(), &input.phrase)?;
        kept = Some(row);

        match verify(brain, models, &label, input)? {
            Ok(summary) => {
                ui.note(format!("Verified with System 1: {summary}."));
                return Ok(Learned { label, created: true, translated: true });
            }
            Err(problem) => {
                ui.note(format!("Rejected by System 1: {problem}.{}", if attempt == 0 { " Trying once more." } else { "" }));
                brain.forget(&label)?;
                feedback = Some(problem);
            }
        }
    }

    // The model's row did not hold up. Keep what the human said, exactly as they said it.
    if let Some(kept) = kept {
        store(brain, &kept, format!("the user wants: {}", input.phrase), &input.phrase)?;
        ui.note("Stored with your phrase as the scenario instead.");
        return Ok(Learned { label: kept.label, created: true, translated: false });
    }
    let (label, created) = brain.learn_command(&input.phrase, &input.steps, input.risk.unwrap_or(Risk::Modifies))?;
    Ok(Learned { label, created, translated: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_ignores_quotes_and_spacing() {
        assert_eq!(comparable("git commit -m  'first draft' "), "git commit -m first draft");
        assert_eq!(comparable("echo \"a\"\tb"), "echo a b");
    }
}
