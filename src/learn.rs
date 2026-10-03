use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

use crate::context::{read_context, Context};
use crate::eval::{closest, verify_row};
use crate::llm::{AGENT_MODEL, TRANSLATOR_MODEL};
use crate::slots::{is_flag, is_literal, match_proposal, match_template, render, undeclared};
use crate::store::{slug, Brain, NewSkill, MAX_EXAMPLES};
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
    /// True when a literal skill that ran exactly this command now runs the translator's template
    /// in its place, with every phrasing it had (ADR 19).
    pub upgraded: bool,
}

fn comparable(command: &str) -> String {
    let unquoted: String = command.chars().filter(|ch| *ch != '\'' && *ch != '"').collect();
    unquoted.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn same_phrase(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

struct Kept {
    label: String,
    steps: Vec<String>,
    slots: Vec<Slot>,
    risk: Risk,
    /// The literal skill this row makes general in place, or None for a new skill.
    upgrades: Option<Skill>,
}

impl Kept {
    /// The row as System 1 would see it once stored. A literal skill made general keeps its label,
    /// its phrasings and its counter-examples, so it takes its own place in the matrix.
    fn as_skill(&self, scenario: String, phrase: &str) -> Skill {
        if let Some(literal) = &self.upgrades {
            let mut examples = literal.examples.clone();
            if !examples.iter().any(|known| same_phrase(known, phrase)) {
                examples.push(phrase.to_string());
            }
            examples.drain(..examples.len().saturating_sub(MAX_EXAMPLES));
            let counter_examples =
                literal.counter_examples.iter().filter(|known| !same_phrase(known, phrase)).cloned().collect();
            return Skill {
                steps: self.steps.clone(),
                scenario,
                examples,
                counter_examples,
                slots: self.slots.clone(),
                risk: self.risk,
                ..literal.clone()
            };
        }
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

/// Write the row: a literal skill made general in place, or a new skill. Another writer may have
/// changed or forgotten the literal skill first; the template is then stored as a new skill.
///
/// The translator and System 1 take a second or two, and another jli may teach the same command
/// meanwhile, under a label of its own. So the matrix is read again: a row that now runs this
/// template is this skill, as at the start. The label claim catches the rest, at the moment of writing.
fn store(brain: &Brain, kept: &Kept, scenario: String, phrase: &str, translated: bool, ui: &Ui) -> Result<Learned> {
    let row = NewSkill {
        label: kept.label.clone(),
        steps: kept.steps.clone(),
        scenario,
        risk: kept.risk,
        origin: Origin::Taught,
        examples: vec![phrase.to_string()],
        slots: kept.slots.clone(),
    };
    if let Some(literal) = &kept.upgrades {
        if brain.upgrade_skill(&literal.label, &literal.steps, &row)? {
            brain.link_phrase(&literal.label, phrase)?;
            ui.note(format!(
                "{} ran only {}. It now runs {}, and keeps its phrasings.",
                literal.label,
                literal.steps.join(" && "),
                kept.steps.join(" && ")
            ));
            return Ok(Learned { label: literal.label.clone(), created: false, translated, upgraded: true });
        }
    }
    let taught_meanwhile =
        brain.skills()?.into_iter().find(|known| match_proposal(&known.steps, &row.steps, &HashMap::new()).is_some());
    if let Some(known) = taught_meanwhile {
        brain.link_phrase(&known.label, phrase)?;
        ui.note(format!("{} was taught this command while it was being checked, so the phrase joins it.", known.label));
        return Ok(Learned { label: known.label, created: false, translated: false, upgraded: false });
    }
    let (label, created) = brain.add_new_skill(&row)?;
    Ok(Learned { label, created, translated: translated && created, upgraded: false })
}

/// The value a literal skill ran becomes the default of the slot that takes its place, where the
/// translator gave none, so a phrasing of it that names no value still runs what it ran.
fn with_defaults(slots: Vec<Slot>, values: &HashMap<String, String>) -> Vec<Slot> {
    slots
        .into_iter()
        .map(|slot| match slot.default {
            Some(_) => slot,
            None => Slot { default: values.get(&slot.name).cloned(), ..slot },
        })
        .collect()
}

/// Link a phrase to a command. In manual mode the pair is stored as given. Otherwise a fast model
/// rewrites it into a general row, and the row is kept only if the classifier confirms that it
/// matches the phrase and does not capture phrasings that belong to other skills.
///
/// A literal skill that runs exactly the command never absorbs a template (ADR 19): if the translator
/// finds values in the command, the literal skill is made general in place, keeping its phrasings.
pub fn learn_phrase(brain: &Brain, models: &Models, input: &LearnInput, ui: &Ui) -> Result<Learned> {
    let command = input.steps.join(" && ");
    let skills = brain.skills()?;
    let runs_it = |skill: &&Skill| match_template(&skill.steps, &command).is_some();
    let linked = |known: &Skill| -> Result<Learned> {
        brain.link_phrase(&known.label, &input.phrase)?;
        Ok(Learned { label: known.label.clone(), created: false, translated: false, upgraded: false })
    };
    // A template that runs this command with some values needs no model at all. A literal skill that
    // runs exactly this command is linked at once only where there is nothing to generalise: in manual
    // mode, or a seed, which is a fixed command written by hand. Any other is first shown to the translator.
    let template = skills.iter().filter(&runs_it).find(|skill| !is_literal(&skill.steps));
    let literal = skills.iter().filter(&runs_it).find(|skill| is_literal(&skill.steps));
    let fixed = literal.filter(|literal| input.mode == Mode::Manual || literal.origin == Origin::Seed);
    if let Some(known) = template.or(fixed) {
        return linked(known);
    }
    if input.mode == Mode::Manual {
        let (label, created) = brain.learn_command(&input.phrase, &input.steps, input.risk.unwrap_or(Risk::Modifies))?;
        return Ok(Learned { label, created, translated: false, upgraded: false });
    }
    // Only a row this brain owns is made general: inside a project a global skill is every project's,
    // so a template taught there is the project's own skill (ADR 17).
    let upgradable = match literal {
        Some(literal) if brain.owns(&literal.label)? => Some(literal),
        _ => None,
    };

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
        let slots_a_flag = values.values().any(|value| is_flag(value));
        let faithful = !slots_a_flag
            && undeclared(&draft.steps, &slots).is_empty()
            && comparable(&render(&draft.steps, &values).join(" && ")) == comparable(&command);
        // A template that gives back the command: more general than a literal skill that runs it.
        let general = faithful && !is_literal(&draft.steps);

        // "This is a known skill with other values": trust it only if the arithmetic checks out, and
        // never let a literal skill stand in for the template just written.
        let existing = draft.existing_label.as_ref().and_then(|label| skills.iter().find(|skill| &skill.label == label));
        if let Some(existing) = existing.filter(|existing| !(general && is_literal(&existing.steps))) {
            if comparable(&render(&existing.steps, &values).join(" && ")) == comparable(&command) {
                brain.link_phrase(&existing.label, &input.phrase)?;
                return Ok(Learned { label: existing.label.clone(), created: false, translated: true, upgraded: false });
            }
        }
        // Nothing to generalise: the literal skill already runs this command as it is.
        if let Some(literal) = literal.filter(|_| !general) {
            return linked(literal);
        }
        if !faithful && !draft.slots.is_empty() {
            ui.note("Its slots did not reproduce your command, so the command is stored literally.");
        }

        let mut row = match upgradable.filter(|_| general) {
            Some(literal) => Kept {
                label: literal.label.clone(),
                steps: draft.steps.clone(),
                slots: with_defaults(slots, &values),
                risk: input.risk.unwrap_or(draft.risk.max(literal.risk)),
                upgrades: Some(literal.clone()),
            },
            None => Kept {
                label: slug(&draft.label),
                steps: if faithful { draft.steps.clone() } else { input.steps.clone() },
                slots: if faithful { slots } else { vec![] },
                risk: input.risk.unwrap_or(draft.risk),
                upgrades: None,
            },
        };
        // The row is checked in memory and written only once it holds, so a failed check leaves nothing
        // behind. In the matrix it is checked against, a new row's label must not be another skill's;
        // a literal skill made general takes its own place.
        while row.upgrades.is_none() && skills.iter().any(|skill| skill.label == row.label) {
            row.label.push_str("-x");
        }
        let candidate = row.as_skill(draft.scenario.clone(), &input.phrase);
        let checked = verify_row(models, &skills, &candidate, &input.phrase, &read_context(&input.cwd), input.threshold);
        kept = Some(row);
        match checked {
            Ok(Ok(summary)) => {
                ui.note(format!("Verified with System 1: {summary}."));
                let row = kept.as_ref().expect("set just above");
                return store(brain, row, draft.scenario.clone(), &input.phrase, true, ui);
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

    // The model's row did not hold up. Keep what the human said, exactly as they said it. A literal
    // skill made general keeps its own scenario, which already held for its phrasings.
    if let Some(kept) = kept {
        let scenario = match &kept.upgrades {
            Some(literal) => literal.scenario.clone(),
            None => format!("the user wants: {}", input.phrase),
        };
        let learned = store(brain, &kept, scenario, &input.phrase, false, ui)?;
        ui.note(if learned.upgraded {
            "Its own scenario is kept instead."
        } else {
            "Stored with your phrase as the scenario instead."
        });
        return Ok(learned);
    }
    // The translator could not be reached: as in manual mode, a literal skill that runs it takes the phrase.
    if let Some(literal) = literal {
        return linked(literal);
    }
    let (label, created) = brain.learn_command(&input.phrase, &input.steps, input.risk.unwrap_or(Risk::Modifies))?;
    Ok(Learned { label, created, translated: false, upgraded: false })
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
