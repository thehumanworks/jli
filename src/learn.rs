use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

use crate::context::{read_context, Context};
use crate::eval::{closest, verify_row};
use crate::llm::{AGENT_MODEL, TRANSLATOR_MODEL};
use crate::slots::{match_template, render, undeclared};
use crate::store::{slug, Brain, NewSkill};
use crate::system2::{rewrite_row, RewriteInput, WeakPhrasing};
use crate::translator::{translate, TranslateInput};
use crate::types::{with_checked_aliases, Mode, Origin, Risk, Skill, Slot};
use crate::ui::{c, duration, Ui};
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

fn comparable(command: &str) -> String {
    let unquoted: String = command.chars().filter(|ch| *ch != '\'' && *ch != '"').collect();
    unquoted.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct Kept {
    label: String,
    steps: Vec<String>,
    slots: Vec<Slot>,
    risk: Risk,
}

impl Kept {
    /// The row as System 1 would see it once stored.
    fn as_skill(&self, scenario: String, phrase: &str) -> Skill {
        Skill {
            label: self.label.clone(),
            steps: self.steps.clone(),
            scenario,
            examples: vec![phrase.to_string()],
            counter_examples: vec![],
            slots: self.slots.clone(),
            risk: self.risk,
            origin: Origin::Taught,
            uses: 0,
            created_at: crate::store::now_iso(),
            last_used_at: None,
            created_host: crate::store::host().to_string(),
        }
    }
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
        // The row is checked in memory and written only once it holds, so a failed check leaves nothing behind.
        let candidate = row.as_skill(draft.scenario.clone(), &input.phrase);
        let checked = verify_row(models, &skills, &candidate, &input.phrase, &read_context(&input.cwd), input.threshold);
        kept = Some(row);
        match checked {
            Ok(Ok(summary)) => {
                ui.note(format!("Verified with System 1: {summary}."));
                let row = kept.as_ref().expect("set just above");
                store(brain, row, draft.scenario.clone(), &input.phrase)?;
                return Ok(Learned { label, created: true, translated: true });
            }
            Ok(Err(problem)) => {
                ui.note(format!("Rejected by System 1: {problem}.{}", if attempt == 0 { " Trying once more." } else { "" }));
                feedback = Some(problem);
            }
            Err(error) => {
                ui.warn(format!("Could not check the row with System 1. {error:#}"));
                break;
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

/// How a skill the agent proposed came to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// System 1 confirmed the agent's scenario.
    Verified,
    /// The agent's scenario failed the check and its one rewrite passed.
    Rewritten,
    /// Both failed: the request is the scenario, as `learn_phrase` falls back.
    Phrase,
    /// System 1 could not be reached, so the agent's scenario is kept unchecked.
    Unchecked,
}

impl Settled {
    /// What goes into the outcome's `learned`, beside "new skill …", when the agent's scenario was not kept as checked.
    pub fn note(&self, label: &str) -> Option<String> {
        match self {
            Settled::Verified => None,
            Settled::Rewritten => Some(format!("the scenario of {label} was rewritten after the System 1 check")),
            Settled::Phrase => Some(format!("{label} keeps the request as its scenario")),
            Settled::Unchecked => Some(format!("{label} was not checked with System 1")),
        }
    }
}

/// A skill the agent proposed, and that ran cleanly, gets the check a taught row gets before it is
/// stored. `checked` is that check, already run (the router runs it while the command runs).
/// On a failure the agent gets one rewrite of the scenario; if that fails too, the request is kept as
/// the scenario. Returns the scenario to store.
#[allow(clippy::too_many_arguments)]
pub fn settle_proposed(
    models: &Models,
    skills: &[Skill],
    row: &Skill,
    phrase: &str,
    context: &Context,
    threshold: f64,
    checked: Result<std::result::Result<String, String>>,
    ui: &Ui,
) -> (String, Settled) {
    let problem = match checked {
        Ok(Ok(summary)) => {
            ui.note(format!("Verified with System 1: {summary}."));
            return (row.scenario.clone(), Settled::Verified);
        }
        Ok(Err(problem)) => problem,
        Err(error) => {
            ui.warn(format!("Could not check the new skill with System 1, so it is kept as proposed. {error:#}"));
            return (row.scenario.clone(), Settled::Unchecked);
        }
    };
    ui.note(format!("Rejected by System 1: {problem}. Asking {} for another scenario.", *AGENT_MODEL));
    let competitors: Vec<&Skill> = closest(skills, row, phrase, &[]).into_iter().take(4).collect();
    let weak = [WeakPhrasing { phrase: phrase.to_string(), routed_to: None, confidence: 0.0 }];
    let input = RewriteInput { skill: row, threshold, weak: &weak, competitors: &competitors, feedback: Some(&problem) };
    let fallback = || (format!("the user wants: {phrase}"), Settled::Phrase);
    let rewritten = match rewrite_row(&models.llm, &input) {
        Ok((rewrite, _)) if !rewrite.scenario.trim().is_empty() => rewrite.scenario,
        Ok(_) => return fallback(),
        Err(error) => {
            ui.note(format!("No rewrite: {error:#}. Stored with the request as its scenario."));
            return fallback();
        }
    };
    let candidate = Skill { scenario: rewritten.clone(), ..row.clone() };
    match verify_row(models, skills, &candidate, phrase, context, threshold) {
        Ok(Ok(summary)) => {
            ui.note(format!("Rewritten and verified with System 1: {summary}. {}", c::dim(&rewritten)));
            (rewritten, Settled::Rewritten)
        }
        Ok(Err(again)) => {
            ui.note(format!("Rejected again: {again}. Stored with the request as its scenario."));
            fallback()
        }
        Err(error) => {
            ui.note(format!("Could not check the rewrite: {error:#}. Stored with the request as its scenario."));
            fallback()
        }
    }
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
