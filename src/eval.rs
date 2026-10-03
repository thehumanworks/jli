//! Measuring the decision matrix against the brain's own evidence, with the real System 1.
//!
//! The same machinery serves three callers: `jli eval` (leave-one-out over stored examples and a
//! replay of accepted requests), `jli eval --optimise` (a rewrite is kept only if the measurement
//! improves), and the check a new row gets before it is written (`verify_row`).
//! See docs/adr/0016-measuring-the-matrix.md.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::Result;

use crate::context::Context;
use crate::system1::{Candidate, Decision};
use crate::types::Skill;
use crate::ui::fixed;
use crate::Models;

/// What one System 1 call costs, from the README's $0.012 for about 200 requests.
pub const COST_PER_CALL: f64 = 0.00006;
/// System 1 calls in flight at once.
const PARALLEL: usize = 8;
/// How many phrasings of other skills are replayed to check a new row does not disturb them.
pub const NEIGHBOUR_CHECKS: usize = 8;

/// Run `work` on every item, at most `PARALLEL` at a time, and return the results in order.
/// A panic in a worker is re-raised here, so a fake that panics still fails its test.
pub fn in_parallel<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..PARALLEL.min(items.len()))
            .map(|_| {
                scope.spawn(|| loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else { break };
                    let result = work(item);
                    results.lock().unwrap_or_else(|poisoned| poisoned.into_inner())[index] = Some(result);
                })
            })
            .collect();
        for worker in workers {
            if let Err(panic) = worker.join() {
                std::panic::resume_unwind(panic);
            }
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .into_iter()
        .map(|result| result.expect("every item is taken by exactly one worker"))
        .collect()
}

fn same(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// The matrix with `row` in it, at the place the brain would list it (most used first, then by label).
pub fn with_row(skills: &[Skill], row: &Skill) -> Vec<Skill> {
    let mut all: Vec<Skill> = skills.iter().filter(|skill| skill.label != row.label).cloned().collect();
    let at = all.iter().position(|skill| (skill.uses, &row.label) < (row.uses, &skill.label)).unwrap_or(all.len());
    all.insert(at, row.clone());
    all
}

/// The matrix with `phrase` held out: gone from every row's examples and counter-examples, so a
/// probe measures what the rest of the row teaches and not a stored copy of the answer.
pub fn held_out(skills: &[Skill], phrase: &str) -> Vec<Skill> {
    skills
        .iter()
        .map(|skill| {
            let mut skill = skill.clone();
            skill.examples.retain(|example| !same(example, phrase));
            skill.counter_examples.retain(|example| !same(example, phrase));
            skill
        })
        .collect()
}

/// Where a phrasing went, relative to the skill it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Routed to its own skill at or above the threshold.
    Home,
    /// Routed to another skill at or above the threshold.
    Stolen(String),
    /// Below the threshold, or to no skill: the agent would be asked.
    Escalated,
}

/// One routed phrasing, judged against its owner.
#[derive(Debug, Clone, PartialEq)]
pub struct Routed {
    pub chosen: Option<String>,
    pub confidence: f64,
    /// System 1's confidence when it routed home; otherwise its probability for the owner.
    pub home: f64,
    pub verdict: Verdict,
    pub candidates: Vec<Candidate>,
}

pub fn judge(owner: &str, decision: &Decision, threshold: f64) -> Routed {
    let chosen = decision.skill.as_ref().map(|skill| skill.label.clone());
    let confident = decision.confidence >= threshold;
    let verdict = match chosen.as_deref() {
        Some(label) if label == owner && confident => Verdict::Home,
        Some(label) if confident => Verdict::Stolen(label.to_string()),
        _ => Verdict::Escalated,
    };
    let home = if chosen.as_deref() == Some(owner) {
        decision.confidence
    } else {
        decision.candidates.iter().find(|candidate| candidate.label == owner).map_or(0.0, |candidate| candidate.probability)
    };
    Routed { chosen, confidence: decision.confidence, home, verdict, candidates: decision.candidates.clone() }
}

const COMMON: [&str; 24] = [
    "the", "user", "wants", "and", "for", "with", "this", "that", "what", "which", "are", "was", "has", "have", "from",
    "into", "all", "its", "how", "who", "can", "you", "show", "give",
];

/// The distinctive words of a text: three letters or more, without the words every scenario shares.
fn words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|ch: char| !(ch.is_ascii_lowercase() || ch.is_ascii_digit()))
        .filter(|word| word.len() >= 3 && !COMMON.contains(word))
        .map(String::from)
        .collect()
}

fn skill_words(skill: &Skill) -> HashSet<String> {
    let mut text = vec![skill.label.as_str(), skill.scenario.as_str()];
    text.extend(skill.steps.iter().map(String::as_str));
    text.extend(skill.examples.iter().map(String::as_str));
    words(&text.join(" "))
}

/// The skills a new row is most likely to disturb, closest first: the runner-ups System 1 itself
/// named for the taught phrase, then the skills that share the most words with the row.
pub fn closest<'a>(skills: &'a [Skill], row: &Skill, phrase: &str, candidates: &[Candidate]) -> Vec<&'a Skill> {
    let mut order: Vec<&Skill> = Vec::new();
    for candidate in candidates.iter().filter(|candidate| candidate.probability > 0.0) {
        if let Some(skill) = skills.iter().find(|skill| skill.label == candidate.label && skill.label != row.label) {
            if !order.iter().any(|known| known.label == skill.label) {
                order.push(skill);
            }
        }
    }
    let mut text = vec![phrase, row.scenario.as_str()];
    text.extend(row.steps.iter().map(String::as_str));
    let wanted = words(&text.join(" "));
    let mut rest: Vec<(usize, &Skill)> = skills
        .iter()
        .filter(|skill| skill.label != row.label && !order.iter().any(|known| known.label == skill.label))
        .map(|skill| (skill_words(skill).intersection(&wanted).count(), skill))
        .filter(|(overlap, _)| *overlap > 0)
        .collect();
    rest.sort_by_key(|(overlap, _)| std::cmp::Reverse(*overlap));
    order.extend(rest.into_iter().map(|(_, skill)| skill));
    order
}

/// Check a new row against the real classifier before it is written. Nothing is stored here:
/// the row lives only in the matrix that is sent.
///
/// The phrase must route to the row at or above the threshold. Then up to eight phrasings of the
/// closest skills (`closest`) are routed with the row in the matrix: none may land on the row, and
/// none that routed home without the row may stop doing so, by falling below the threshold or by
/// moving to a third skill. Returns what is wrong with it, or a summary when it holds.
pub fn verify_row(
    models: &Models,
    skills: &[Skill],
    row: &Skill,
    phrase: &str,
    context: &Context,
    threshold: f64,
) -> Result<std::result::Result<String, String>> {
    let after = with_row(skills, row);
    let own = models.system1.decide(phrase, context, &after)?;
    if own.skill.as_ref().map(|skill| skill.label.as_str()) != Some(row.label.as_str()) || own.confidence < threshold {
        let got = own.skill.as_ref().map_or("no skill", |skill| skill.label.as_str());
        return Ok(Err(format!(
            "the phrase itself routed to {got} at {}, so the scenario does not describe it well enough",
            fixed(own.confidence, 2)
        )));
    }

    let neighbours: Vec<(&str, &str)> = closest(skills, row, phrase, &own.candidates)
        .into_iter()
        .flat_map(|skill| {
            let recent = &skill.examples[skill.examples.len().saturating_sub(2)..];
            recent.iter().map(move |example| (example.as_str(), skill.label.as_str()))
        })
        .filter(|(example, _)| !same(example, phrase))
        .take(NEIGHBOUR_CHECKS)
        .collect();
    let decided = in_parallel(&neighbours, |(example, _)| models.system1.decide(example, context, &after));
    let mut moved = Vec::new();
    for (decision, (example, owner)) in decided.into_iter().zip(&neighbours) {
        let routed = judge(owner, &decision?, threshold);
        if routed.chosen.as_deref() == Some(row.label.as_str()) {
            return Ok(Err(format!("the scenario is too broad: it captured \"{example}\", which belongs to {owner}")));
        }
        if routed.verdict != Verdict::Home {
            moved.push((*example, *owner, routed));
        }
    }
    // A neighbour that was already weak says nothing about this row; one it knocked off does.
    let before = in_parallel(&moved, |(example, _, _)| models.system1.decide(example, context, skills));
    for (decision, (example, owner, routed)) in before.into_iter().zip(&moved) {
        if judge(owner, &decision?, threshold).verdict != Verdict::Home {
            continue;
        }
        let now = match &routed.verdict {
            Verdict::Stolen(label) => format!("routes to {label} instead"),
            _ => format!("falls to {} confidence", fixed(routed.home, 2)),
        };
        return Ok(Err(format!("with it in the matrix, \"{example}\", which belongs to {owner}, {now}")));
    }

    let mut summary = format!("the phrase routes to it at {}", fixed(own.confidence, 2));
    if !neighbours.is_empty() {
        summary.push_str(&format!(", and {} phrasings of the closest skills still route where they did", neighbours.len()));
    }
    Ok(Ok(summary))
}
