//! Measuring the decision matrix against the brain's own evidence, with the real System 1.
//!
//! The same machinery serves three callers: `jli eval` (leave-one-out over stored examples and a
//! replay of accepted requests), `jli eval --optimise` (a rewrite is kept only if the measurement
//! improves), and the check a new row gets before it is written (`verify_row`).
//! See docs/adr/0016-measuring-the-matrix.md.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use crate::args;
use crate::context::{platform, read_context, Context};
use crate::store::{now_iso, Brain};
use crate::system1::{Candidate, Decision, NONE};
use crate::system2::{rewrite_row, CounterExample, RewriteInput, WeakPhrasing};
use crate::types::{BrainEvent, Path, Skill};
use crate::ui::{bar, c, fixed, sparkline, Ui};
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

// ── Evidence from the events log ─────────────────────────────────────────────

/// What `learned` holds for a route the person turned down, or one `jli fix` undid.
pub const CORRECTED: &str = "corrected";
/// How many of the newest events the evidence is read from.
const EVENT_WINDOW: i64 = 2000;
/// A rewrite must raise the weighted score by at least this much to be kept. Less is within what
/// System 1 varies between two identical calls (docs/adr/0016-measuring-the-matrix.md).
pub const MIN_GAIN: f64 = 0.02;
/// A phrasing that routes home at least this confidently leaves nothing for a rewrite to gain.
const GOOD_ENOUGH: f64 = 0.9;

pub fn is_corrected(event: &BrainEvent) -> bool {
    event.learned.as_deref() == Some(CORRECTED)
}

/// Accepted, exited 0, and never turned down or undone: the person's verdict that the route was right.
pub fn deemed_correct(event: &BrainEvent) -> bool {
    event.label.is_some() && event.accepted && event.exit_code == Some(0) && !is_corrected(event)
}

fn routed(event: &BrainEvent) -> bool {
    event.label.is_some() && matches!(event.path, Path::System1 | Path::System1Confirmed | Path::System2 | Path::Taught)
}

/// The events of each request, oldest first. A request is its first attempt plus the attempts made
/// right after it was turned down: the next events from the same host with the same words.
pub fn requests(oldest_first: &[BrainEvent]) -> Vec<Vec<&BrainEvent>> {
    let mut groups: Vec<Vec<&BrainEvent>> = Vec::new();
    let mut latest: HashMap<&str, usize> = HashMap::new();
    for event in oldest_first {
        let retry = latest
            .get(event.host.as_str())
            .copied()
            .filter(|&at| groups[at].last().is_some_and(|last| is_corrected(last) && last.query == event.query));
        match retry {
            Some(at) => groups[at].push(event),
            None => {
                groups.push(vec![event]);
                latest.insert(event.host.as_str(), groups.len() - 1);
            }
        }
    }
    groups
}

/// Whether routes are right, from what the person did with them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Correctness {
    /// Requests whose first attempt routed to a skill.
    pub requests: usize,
    /// Accepted, exited 0, and not turned down or undone.
    pub first_try: usize,
    /// Turned down at "Run it?", corrected after a failure, or undone by `jli fix`.
    pub turned_down: usize,
    /// Accepted and run, with a non-zero exit.
    pub failed: usize,
    /// Never run and never judged: nobody to confirm, or a value missing.
    pub blocked: usize,
    /// Events routed by System 1, and how many of them still called a language model.
    pub system1_routes: usize,
    pub system1_with_model: usize,
}

pub fn correctness(oldest_first: &[BrainEvent]) -> Correctness {
    let mut tally = Correctness::default();
    for group in requests(oldest_first) {
        let first = group[0];
        if !routed(first) {
            continue;
        }
        tally.requests += 1;
        // A taught row with no turned-down attempt before it was written before attempts were logged.
        if first.path == Path::Taught || is_corrected(first) || group.len() > 1 {
            tally.turned_down += 1;
        } else if first.accepted && first.exit_code == Some(0) {
            tally.first_try += 1;
        } else if first.accepted {
            tally.failed += 1;
        } else {
            tally.blocked += 1;
        }
    }
    for event in oldest_first.iter().filter(|event| event.path == Path::System1 && event.label.is_some()) {
        tally.system1_routes += 1;
        if event.ms_system2.unwrap_or(0) > 0 {
            tally.system1_with_model += 1;
        }
    }
    tally
}

/// How often the person judged a skill's routes, and how often they deemed them right.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Evidence {
    pub judged: usize,
    pub correct: usize,
}

pub fn evidence(events: &[BrainEvent]) -> HashMap<String, Evidence> {
    let mut by_label: HashMap<String, Evidence> = HashMap::new();
    for event in events {
        let Some(label) = &event.label else { continue };
        let correct = deemed_correct(event);
        if correct || is_corrected(event) {
            let entry = by_label.entry(label.clone()).or_default();
            entry.judged += 1;
            entry.correct += usize::from(correct);
        }
    }
    by_label
}

/// What getting a skill right is worth: how much it is used, times how often the person deemed its
/// routes right (Laplace's rule, so a skill nobody has judged counts as a coin toss).
pub fn importance(uses: i64, evidence: Evidence) -> f64 {
    (1.0 + (uses.max(0) as f64).ln_1p()) * (evidence.correct as f64 + 1.0) / (evidence.judged as f64 + 2.0)
}

/// What a skill lacks: the mean shortfall of its home confidence below 1, plus the share of its
/// phrasings that are stolen or escalate.
pub fn deficit(routes: &[&Routed]) -> f64 {
    if routes.is_empty() {
        return 0.0;
    }
    let count = routes.len() as f64;
    let shortfall = routes.iter().map(|routed| 1.0 - routed.home.clamp(0.0, 1.0)).sum::<f64>() / count;
    let missed = routes.iter().filter(|routed| routed.verdict != Verdict::Home).count() as f64 / count;
    shortfall + missed
}

// ── Probes ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// A stored example, routed with itself held out.
    Example,
    /// An accepted request from the events log, routed against the matrix as it is.
    Replay,
}

/// One phrasing to route, and the skill it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub owner: String,
    pub phrase: String,
    pub kind: Kind,
    pub context: Context,
    /// For a replay: how many accepted requests had these words.
    pub requests: usize,
    /// True when the request's directory is gone and its context was inferred.
    pub rebuilt: bool,
}

/// The context a request was made in, read again from its directory. When the directory is gone,
/// its name still comes from the stored path; it counts as a repository if the accepted command was
/// a git command that exited 0, or if a directory above it still is one; the platform is this host's.
/// Returns the context and whether it had to be inferred.
pub fn context_at(cwd: &str, command: Option<&str>, exit_code: Option<i64>) -> (Context, bool) {
    let path = PathBuf::from(cwd);
    if path.is_dir() {
        return (read_context(&path), false);
    }
    let ran_git = command.is_some_and(|command| command.trim_start().starts_with("git ")) && exit_code == Some(0);
    let context = Context {
        directory: path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
        inside_git_repository: ran_git || path.ancestors().any(|dir| dir.join(".git").exists()),
        platform: platform(),
        cwd: path,
    };
    (context, true)
}

/// Every stored example of every skill (or of one), each to be routed with itself held out. An
/// example is routed in the context of the newest request that had its words, else in `here`.
pub fn example_probes(skills: &[Skill], oldest_first: &[BrainEvent], only: Option<&str>, here: &Context) -> Vec<Probe> {
    let mut asked_in: HashMap<String, &BrainEvent> = HashMap::new();
    for event in oldest_first {
        let owner = event.label.as_ref().and_then(|label| skills.iter().find(|skill| &skill.label == label));
        let slots = owner.map(|skill| skill.slots.as_slice()).unwrap_or_default();
        asked_in.insert(args::phrase(&event.query, slots).trim().to_lowercase(), event);
    }
    let mut probes = Vec::new();
    for skill in skills.iter().filter(|skill| only.is_none_or(|label| label == skill.label)) {
        for example in &skill.examples {
            let (context, rebuilt) = match asked_in.get(&example.trim().to_lowercase()) {
                Some(event) => context_at(&event.cwd, event.command.as_deref(), event.exit_code),
                None => (here.clone(), false),
            };
            probes.push(Probe {
                owner: skill.label.clone(),
                phrase: example.clone(),
                kind: Kind::Example,
                context,
                requests: 0,
                rebuilt,
            });
        }
    }
    probes
}

/// Accepted, uncorrected requests from the log, one probe per distinct wording and skill. Returns
/// the probes and how many accepted requests name a skill the brain no longer has.
pub fn replay_probes(skills: &[Skill], oldest_first: &[BrainEvent], only: Option<&str>) -> (Vec<Probe>, usize) {
    let mut probes: Vec<Probe> = Vec::new();
    let mut gone = 0;
    for event in oldest_first.iter().rev().filter(|event| deemed_correct(event)) {
        let Some(label) = event.label.as_deref() else { continue };
        if only.is_some_and(|only| only != label) {
            continue;
        }
        if !skills.iter().any(|skill| skill.label == label) {
            gone += 1;
            continue;
        }
        // System 1 routes a request with its stated values hidden, so the replay does too.
        let phrase = args::phrase(&event.query, &[]);
        match probes.iter_mut().find(|probe| probe.owner == label && same(&probe.phrase, &phrase)) {
            Some(known) => known.requests += 1,
            None => {
                let (context, rebuilt) = context_at(&event.cwd, event.command.as_deref(), event.exit_code);
                probes.push(Probe { owner: label.to_string(), phrase, kind: Kind::Replay, context, requests: 1, rebuilt });
            }
        }
    }
    (probes, gone)
}

/// At most `limit` probes, taken in turn from each skill so that every skill gets some.
pub fn spread(probes: Vec<Probe>, limit: Option<usize>) -> Vec<Probe> {
    let Some(limit) = limit.filter(|limit| *limit < probes.len()) else { return probes };
    let mut owners: Vec<String> = Vec::new();
    let mut queues: HashMap<String, std::collections::VecDeque<Probe>> = HashMap::new();
    for probe in probes {
        if !queues.contains_key(&probe.owner) {
            owners.push(probe.owner.clone());
        }
        queues.entry(probe.owner.clone()).or_default().push_back(probe);
    }
    let mut taken = Vec::new();
    while taken.len() < limit {
        let before = taken.len();
        for owner in &owners {
            if taken.len() == limit {
                break;
            }
            if let Some(probe) = queues.get_mut(owner).and_then(|queue| queue.pop_front()) {
                taken.push(probe);
            }
        }
        if taken.len() == before {
            break;
        }
    }
    taken
}

/// Route every probe with the real System 1, `PARALLEL` at a time, against `skills`. Returns where
/// each went, aligned with the probes, and how many calls failed (those are `None`).
pub fn measure(models: &Models, skills: &[Skill], probes: &[Probe], threshold: f64) -> Result<(Vec<Option<Routed>>, usize)> {
    let decided = in_parallel(probes, |probe| match probe.kind {
        Kind::Example => models.system1.decide(&probe.phrase, &probe.context, &held_out(skills, &probe.phrase)),
        Kind::Replay => models.system1.decide(&probe.phrase, &probe.context, skills),
    });
    let mut failed = 0;
    let mut first_error = None;
    let mut routes = Vec::with_capacity(probes.len());
    for (decision, probe) in decided.into_iter().zip(probes) {
        match decision {
            Ok(decision) => routes.push(Some(judge(&probe.owner, &decision, threshold))),
            Err(error) => {
                failed += 1;
                first_error.get_or_insert(error);
                routes.push(None);
            }
        }
    }
    if let Some(error) = first_error.filter(|_| failed == probes.len()) {
        return Err(error);
    }
    Ok((routes, failed))
}

// ── The report ───────────────────────────────────────────────────────────────

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn share(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        round(part as f64 / whole as f64)
    }
}

/// Where a set of phrasings went.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub probes: usize,
    pub home: usize,
    pub stolen: usize,
    pub escalated: usize,
    pub accuracy: f64,
    pub steal_rate: f64,
    pub escalation_rate: f64,
    /// Of the home confidence: System 1's confidence when it routed home, else its probability for the owner.
    pub mean_confidence: f64,
    pub worst_confidence: f64,
}

pub fn summarise<'a>(routes: impl IntoIterator<Item = &'a Routed>) -> Summary {
    let routes: Vec<&Routed> = routes.into_iter().collect();
    let count = |wanted: fn(&Verdict) -> bool| routes.iter().filter(|routed| wanted(&routed.verdict)).count();
    let home = count(|verdict| *verdict == Verdict::Home);
    let stolen = count(|verdict| matches!(verdict, Verdict::Stolen(_)));
    let escalated = count(|verdict| *verdict == Verdict::Escalated);
    let total = routes.len();
    let mean = if total == 0 { 0.0 } else { routes.iter().map(|routed| routed.home).sum::<f64>() / total as f64 };
    Summary {
        probes: total,
        home,
        stolen,
        escalated,
        accuracy: share(home, total),
        steal_rate: share(stolen, total),
        escalation_rate: share(escalated, total),
        mean_confidence: round(mean),
        worst_confidence: round(routes.iter().map(|routed| routed.home).fold(if total == 0 { 0.0 } else { 1.0 }, f64::min)),
    }
}

fn verdict_name(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Home => "home",
        Verdict::Stolen(_) => "stolen",
        Verdict::Escalated => "escalated",
    }
}

/// One phrasing in the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    pub phrase: String,
    pub kind: Kind,
    pub routed_to: Option<String>,
    pub confidence: f64,
    pub home: f64,
    pub verdict: &'static str,
}

fn line(probe: &Probe, routed: &Routed) -> Line {
    Line {
        phrase: probe.phrase.clone(),
        kind: probe.kind,
        routed_to: routed.chosen.clone().filter(|label| label != NONE),
        confidence: round(routed.confidence),
        home: round(routed.home),
        verdict: verdict_name(&routed.verdict),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillReport {
    pub label: String,
    pub uses: i64,
    /// Routes the person judged, and how many they deemed right.
    pub judged: usize,
    pub correct: usize,
    pub importance: f64,
    pub deficit: f64,
    /// importance × deficit: the expected gain from improving this row.
    pub weight: f64,
    pub summary: Summary,
    /// The least confident phrasings, even those that route home.
    pub lowest: Vec<Line>,
    pub steals: Vec<Line>,
    /// Every phrasing measured, least confident first.
    pub phrasings: Vec<Line>,
}

/// The rank `--optimise` works down.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ranked {
    pub label: String,
    pub weight: f64,
    pub importance: f64,
    pub deficit: f64,
}

/// One rewrite the optimiser tried, kept or not. A kept one is stored in `meta` under
/// `optimised:<ts>:<label>`, which is also what `jli eval undo` reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub ts: String,
    pub label: String,
    pub attempt: usize,
    pub kept: bool,
    pub scenario_before: String,
    pub scenario_after: String,
    pub dropped: Vec<String>,
    pub counter_examples: Vec<CounterExample>,
    /// The weighted score of every phrasing in scope: this skill's and its competitors'.
    pub score_before: f64,
    pub score_after: f64,
    pub home_before: usize,
    pub home_after: usize,
    pub probes: usize,
    pub problem: Option<String>,
    pub explanation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone_at: Option<String>,
    /// The project a rewrite of a global skill was measured in. Its scenario is kept as that project's
    /// own and the global row is not written, so no other project sees it (ADR 22).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// With `project`: whether the project already kept a scenario of its own, which `undo` puts back.
    /// Without one, `undo` lets the project see the global scenario again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project_had_scenario: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Optimisation {
    pub ranking: Vec<Ranked>,
    pub changes: Vec<Change>,
    pub system1_calls: usize,
    pub system2_calls: usize,
    /// False in a dry run: the changes marked kept were measured and not written.
    pub applied: bool,
    /// Why the loop stopped before its last candidate, when it did.
    pub stopped: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub ts: String,
    pub threshold: f64,
    pub dry_run: bool,
    pub only: Option<String>,
    /// The System 1 calls the measurement was priced at before it ran, and that price.
    pub estimated_calls: usize,
    pub estimated_cost: f64,
    pub failed_calls: usize,
    pub overall: Summary,
    pub leave_one_out: Summary,
    pub replay: Summary,
    /// Replays whose directory is gone, routed in an inferred context.
    pub rebuilt_contexts: usize,
    /// Accepted requests left out because their skill was forgotten.
    pub forgotten: usize,
    pub skills: Vec<SkillReport>,
    pub optimisation: Option<Optimisation>,
    /// The `meta` key the summary was stored under; none in a dry run.
    pub stored: Option<String>,
    /// System 1's own count and cost estimate for everything this run asked.
    pub system1_calls: u64,
    pub system1_cost: f64,
}

/// What `jli stats` shows the trend from: one entry per evaluation, in `meta` under `eval:<ts>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trend {
    pub ts: String,
    pub threshold: f64,
    pub overall: Summary,
    pub leave_one_out: Summary,
    pub replay: Summary,
    #[serde(default)]
    pub only: Option<String>,
    /// True when `--limit` cut the measurement, so it is not comparable with a full one.
    #[serde(default)]
    pub limited: bool,
}

pub fn trend(brain: &Brain) -> Result<Vec<Trend>> {
    let entries = brain.meta_with_prefix("eval:")?;
    Ok(entries.into_iter().filter_map(|(_, value)| serde_json::from_str(&value).ok()).collect())
}

pub struct Bounds {
    /// Skills the optimiser tries, from the top of the ranking.
    pub max_skills: usize,
    /// Model calls the optimiser may make, System 1 and System 2 together, after the measurement.
    pub max_calls: usize,
}

pub struct EvalOptions {
    pub cwd: PathBuf,
    pub threshold: f64,
    /// Measure only this skill's phrasings.
    pub only: Option<String>,
    pub limit: Option<usize>,
    /// Measure, and propose in `--optimise`, but write nothing at all.
    pub dry_run: bool,
    pub optimise: Option<Bounds>,
}

/// `jli eval`: measure the matrix against the brain's own evidence and, with `--optimise`, let the
/// agent rewrite the weakest rows, keeping a rewrite only when the measurement says it helps.
pub fn run(brain: &Brain, models: &Models, options: &EvalOptions, ui: &Ui) -> Result<Report> {
    let skills = brain.skills()?;
    if let Some(only) = &options.only {
        if !skills.iter().any(|skill| &skill.label == only) {
            bail!("No skill called {only}.");
        }
    }
    let mut events = brain.events(EVENT_WINDOW)?;
    events.reverse();
    let here = read_context(&options.cwd);
    let mut probes = example_probes(&skills, &events, options.only.as_deref(), &here);
    let (replays, forgotten) = replay_probes(&skills, &events, options.only.as_deref());
    probes.extend(replays);
    let total = probes.len();
    let probes = spread(probes, options.limit);
    if probes.is_empty() {
        bail!("Nothing to measure yet: no skill has a stored example, and no accepted request is logged.");
    }

    let examples = probes.iter().filter(|probe| probe.kind == Kind::Example).count();
    let estimated_cost = probes.len() as f64 * COST_PER_CALL;
    ui.line(format!(
        "{} {}",
        c::cyan("◇ eval"),
        c::dim(format!(
            "· {examples} stored examples, each held out, and {} accepted requests replayed",
            probes.len() - examples
        ))
    ));
    ui.note(format!(
        "{} System 1 calls, about ${estimated_cost:.4}{}{}",
        probes.len(),
        if probes.len() < total { format!(" (limited from {total})") } else { String::new() },
        if options.dry_run { " · dry run: nothing will be written" } else { "" }
    ));
    let (routes, failed) = measure(models, &skills, &probes, options.threshold)?;
    if failed > 0 {
        ui.warn(format!("{failed} System 1 calls failed and are left out."));
    }

    let evidence = evidence(&events);
    let measured = || probes.iter().zip(&routes).filter_map(|(probe, routed)| Some((probe, routed.as_ref()?)));
    let of_kind = |kind: Kind| summarise(measured().filter(|(probe, _)| probe.kind == kind).map(|(_, routed)| routed));
    let mut skill_reports: Vec<SkillReport> = skills
        .iter()
        .filter_map(|skill| {
            let mut own: Vec<(&Probe, &Routed)> = measured().filter(|(probe, _)| probe.owner == skill.label).collect();
            if own.is_empty() {
                return None;
            }
            own.sort_by(|a, b| a.1.home.total_cmp(&b.1.home));
            let found = evidence.get(&skill.label).copied().unwrap_or_default();
            let routed: Vec<&Routed> = own.iter().map(|(_, routed)| *routed).collect();
            let (importance, deficit) = (importance(skill.uses, found), deficit(&routed));
            Some(SkillReport {
                label: skill.label.clone(),
                uses: skill.uses,
                judged: found.judged,
                correct: found.correct,
                importance: round(importance),
                deficit: round(deficit),
                weight: round(importance * deficit),
                summary: summarise(routed.iter().copied()),
                lowest: own.iter().take(3).map(|(probe, routed)| line(probe, routed)).collect(),
                steals: own
                    .iter()
                    .filter(|(_, routed)| matches!(routed.verdict, Verdict::Stolen(_)))
                    .map(|(probe, routed)| line(probe, routed))
                    .collect(),
                phrasings: own.iter().map(|(probe, routed)| line(probe, routed)).collect(),
            })
        })
        .collect();
    skill_reports.sort_by(|a, b| b.weight.total_cmp(&a.weight).then_with(|| a.label.cmp(&b.label)));

    let ts = now_iso();
    let mut report = Report {
        ts: ts.clone(),
        threshold: options.threshold,
        dry_run: options.dry_run,
        only: options.only.clone(),
        estimated_calls: probes.len(),
        estimated_cost: round(estimated_cost * 1000.0) / 1000.0,
        failed_calls: failed,
        overall: summarise(measured().map(|(_, routed)| routed)),
        leave_one_out: of_kind(Kind::Example),
        replay: of_kind(Kind::Replay),
        rebuilt_contexts: measured().filter(|(probe, _)| probe.kind == Kind::Replay && probe.rebuilt).count(),
        forgotten,
        skills: skill_reports,
        optimisation: None,
        stored: None,
        system1_calls: 0,
        system1_cost: 0.0,
    };
    if !options.dry_run {
        let key = format!("eval:{ts}");
        let entry = Trend {
            ts,
            threshold: options.threshold,
            overall: report.overall.clone(),
            leave_one_out: report.leave_one_out.clone(),
            replay: report.replay.clone(),
            only: options.only.clone(),
            limited: probes.len() < total,
        };
        brain.put_meta(&key, &serde_json::to_string(&entry)?)?;
        report.stored = Some(key);
    }
    if let Some(bounds) = &options.optimise {
        let importance: HashMap<String, f64> = skills
            .iter()
            .map(|skill| {
                (skill.label.clone(), importance(skill.uses, evidence.get(&skill.label).copied().unwrap_or_default()))
            })
            .collect();
        let ranking: Vec<Ranked> = report
            .skills
            .iter()
            .filter(|skill| skill.weight > 0.0)
            .map(|skill| Ranked {
                label: skill.label.clone(),
                weight: skill.weight,
                importance: skill.importance,
                deficit: skill.deficit,
            })
            .collect();
        // Inside a project a global skill's rewrite is kept as the project's (ADR 22), and the global
        // row's own phrasings are not the project's to drop.
        let mut global: HashMap<String, Vec<String>> = HashMap::new();
        if brain.project().is_some() {
            let own: Vec<String> = brain.project_skills()?.into_iter().map(|skill| skill.label).collect();
            for skill in brain.global_skills()?.into_iter().filter(|skill| !own.contains(&skill.label)) {
                global.insert(skill.label, skill.examples);
            }
        }
        let state =
            Optimiser { brain, models, options, bounds, probes: &probes, importance: &importance, global: &global, ui };
        report.optimisation = Some(state.run(skills, routes, ranking)?);
    }
    if let Some((calls, _, cost)) = models.system1.usage() {
        report.system1_calls = calls;
        report.system1_cost = round(cost * 10_000.0) / 10_000.0;
    }
    Ok(report)
}

// ── The optimiser ────────────────────────────────────────────────────────────

/// The weighted score of a set of phrasings: each counts by its owner's importance, and scores its
/// home confidence, or nothing when another skill took it.
pub fn score(pairs: &[(&Probe, &Routed)], importance: &HashMap<String, f64>) -> f64 {
    let weight = |owner: &str| importance.get(owner).copied().unwrap_or(1.0);
    let total: f64 = pairs.iter().map(|(probe, _)| weight(&probe.owner)).sum();
    if total == 0.0 {
        return 0.0;
    }
    let earned: f64 = pairs
        .iter()
        .map(|(probe, routed)| {
            let earned = if matches!(routed.verdict, Verdict::Stolen(_)) { 0.0 } else { routed.home };
            weight(&probe.owner) * earned
        })
        .sum();
    earned / total
}

/// The gate a rewrite of `label` must pass: the weighted score of every phrasing in scope rises by
/// at least `MIN_GAIN`, and no phrasing of another skill that routed home stops doing so.
/// Returns the score before and after, and the problem when it fails.
pub fn gate(
    label: &str,
    probes: &[&Probe],
    before: &[&Routed],
    after: &[Option<Routed>],
    importance: &HashMap<String, f64>,
) -> (f64, f64, Option<String>) {
    let mut pairs_before = Vec::new();
    let mut pairs_after = Vec::new();
    let mut problem = None;
    for ((probe, was), now) in probes.iter().zip(before).zip(after) {
        let Some(now) = now else { continue };
        pairs_before.push((*probe, *was));
        pairs_after.push((*probe, now));
        if problem.is_none() && probe.owner != label && was.verdict == Verdict::Home && now.verdict != Verdict::Home {
            let went = match &now.verdict {
                Verdict::Stolen(other) => format!("route to {other} instead"),
                _ => format!("fall to {} confidence", fixed(now.home, 2)),
            };
            problem = Some(format!("it made \"{}\", which belongs to {}, {went}", probe.phrase, probe.owner));
        }
    }
    let (was, now) = (score(&pairs_before, importance), score(&pairs_after, importance));
    if problem.is_none() && now < was + MIN_GAIN {
        problem = Some(format!(
            "the weighted score of the phrasings in scope went from {} to {}; a rewrite must gain at least {MIN_GAIN}",
            fixed(was, 3),
            fixed(now, 3)
        ));
    }
    (round(was), round(now), problem)
}

/// The skills a row competes with: those that took its phrasings or that it took from, those
/// System 1 ranked close behind either way, and the skills that share the most words with it.
pub fn competitors(label: &str, probes: &[Probe], routes: &[Option<Routed>], skills: &[Skill]) -> Vec<String> {
    const CLOSE: f64 = 0.05;
    let mut found: Vec<String> = Vec::new();
    let mut add = |other: &str| {
        if other != label && other != NONE && !found.iter().any(|known| known == other) {
            found.push(other.to_string());
        }
    };
    for (probe, routed) in probes.iter().zip(routes) {
        let Some(routed) = routed else { continue };
        let close = |name: &str| routed.candidates.iter().any(|c| c.label == name && c.probability >= CLOSE);
        if probe.owner == label {
            if let Verdict::Stolen(other) = &routed.verdict {
                add(other);
            }
            for candidate in routed.candidates.iter().filter(|candidate| candidate.probability >= CLOSE).take(3) {
                add(&candidate.label);
            }
        } else if routed.chosen.as_deref() == Some(label) || close(label) {
            add(&probe.owner);
        }
    }
    if let Some(row) = skills.iter().find(|skill| skill.label == label) {
        for skill in closest(skills, row, "", &[]).into_iter().take(3) {
            add(&skill.label);
        }
    }
    found.retain(|other| skills.iter().any(|skill| &skill.label == other));
    found
}

struct Applied {
    skills: Vec<Skill>,
    scenario: String,
    dropped: Vec<String>,
    counter_examples: Vec<CounterExample>,
}

/// The matrix with a rewrite applied, in memory. Only what the agent may change is taken: the
/// skill's scenario, up to three of its own examples to drop, and up to three counter-examples for
/// skills it competes with.
///
/// It is applied as it would be written. `global_here` holds, inside a project, the global skills it
/// sees with the global row's own examples (ADR 22): those are not the project's to drop, and a
/// counter-example for one of those skills is the project's lesson, which also hides the phrasing
/// among that skill's examples (ADR 17). Outside a project it is empty.
fn apply(
    skills: &[Skill],
    label: &str,
    rewrite: &crate::system2::Rewrite,
    rivals: &[String],
    global_here: &HashMap<String, Vec<String>>,
) -> std::result::Result<Applied, String> {
    let undroppable: &[String] = global_here.get(label).map(Vec::as_slice).unwrap_or_default();
    let Some(skill) = skills.iter().find(|skill| skill.label == label) else {
        return Err(format!("{label} is not in the matrix"));
    };
    let scenario = rewrite.scenario.trim().to_string();
    if scenario.is_empty() {
        return Err("the rewrite had no scenario".into());
    }
    let (kept, droppable): (Vec<&String>, Vec<&String>) = skill
        .examples
        .iter()
        .filter(|example| rewrite.drop_examples.iter().any(|drop| same(drop, example)))
        .partition(|example| undroppable.iter().any(|phrase| same(phrase, example)));
    let dropped: Vec<String> = droppable.into_iter().take(3).cloned().collect();
    let mut counter_examples: Vec<CounterExample> = Vec::new();
    for proposed in &rewrite.counter_examples {
        let phrase = proposed.phrase.trim();
        let known = counter_examples.iter().any(|ce| ce.label == proposed.label && same(&ce.phrase, phrase));
        if rivals.contains(&proposed.label) && !phrase.is_empty() && !known && counter_examples.len() < 3 {
            counter_examples.push(CounterExample { label: proposed.label.clone(), phrase: phrase.to_string() });
        }
    }
    if scenario == skill.scenario && dropped.is_empty() && counter_examples.is_empty() {
        return Err(match kept.first() {
            Some(phrase) => format!(
                "the rewrite changed nothing that can be kept here: \"{phrase}\" is a phrasing of the global {label}, which a project cannot drop"
            ),
            None => "the rewrite changed nothing".into(),
        });
    }
    let skills = skills
        .iter()
        .map(|other| {
            let mut other = other.clone();
            if other.label == label {
                other.scenario = scenario.clone();
                other.examples.retain(|example| !dropped.iter().any(|drop| same(drop, example)));
            }
            for ce in counter_examples.iter().filter(|ce| ce.label == other.label) {
                if !other.counter_examples.iter().any(|known| same(known, &ce.phrase)) {
                    other.counter_examples.push(ce.phrase.clone());
                }
                if global_here.contains_key(&other.label) {
                    other.examples.retain(|example| !same(example, &ce.phrase));
                }
            }
            other
        })
        .collect();
    Ok(Applied { skills, scenario, dropped, counter_examples })
}

/// Write a kept change: the record that undoes it first, then the scenario, then the examples and
/// counter-examples. Rows are updated in place, never deleted. Inside a project each goes where the
/// project keeps it (ADR 17, ADR 22): a global skill's scenario becomes the project's own, and a
/// counter-example for a global skill the project's lesson, so the global row is never written.
fn write(brain: &Brain, change: &Change) -> Result<()> {
    if brain.skill(&change.label)?.is_none() {
        bail!("{} is no longer in the brain", change.label);
    }
    brain.put_meta(&format!("optimised:{}:{}", change.ts, change.label), &serde_json::to_string(change)?)?;
    brain.set_scenario(&change.label, &change.scenario_after)?;
    for example in &change.dropped {
        brain.remove_example(&change.label, example)?;
    }
    for ce in &change.counter_examples {
        brain.add_counter_example(&ce.label, &ce.phrase)?;
    }
    Ok(())
}

struct Optimiser<'a> {
    brain: &'a Brain,
    models: &'a Models,
    options: &'a EvalOptions,
    bounds: &'a Bounds,
    probes: &'a [Probe],
    importance: &'a HashMap<String, f64>,
    /// Inside a project, the global skills it sees, with the global row's own examples. Empty outside one.
    global: &'a HashMap<String, Vec<String>>,
    ui: &'a Ui,
}

impl Optimiser<'_> {
    fn run(&self, skills: Vec<Skill>, baseline: Vec<Option<Routed>>, ranking: Vec<Ranked>) -> Result<Optimisation> {
        let ui = self.ui;
        let (threshold, probes) = (self.options.threshold, self.probes);
        let mut current = skills;
        let mut known = baseline;
        let mut changes = Vec::new();
        let (mut calls1, mut calls2) = (0usize, 0usize);
        let mut stopped = None;
        ui.line(format!("{} {}", c::magenta("◆ optimise"), c::dim("· expected gain = importance × deficit")));
        for ranked in ranking.iter().take(5) {
            ui.note(format!(
                "{:<28} {}  {} = {} × {}",
                ranked.label,
                bar(ranked.weight / ranking[0].weight.max(f64::EPSILON)),
                fixed(ranked.weight, 2),
                fixed(ranked.importance, 2),
                fixed(ranked.deficit, 2)
            ));
        }
        for ranked in ranking.iter().take(self.bounds.max_skills) {
            let label = ranked.label.as_str();
            let own_good = probes.iter().zip(&known).filter(|(probe, _)| probe.owner == label).all(|(_, routed)| {
                routed.as_ref().is_some_and(|routed| routed.verdict == Verdict::Home && routed.home >= GOOD_ENOUGH)
            });
            if own_good {
                ui.note(format!("{label}: every phrasing already routes home at {GOOD_ENOUGH} or more; nothing to gain."));
                continue;
            }
            let rivals = competitors(label, probes, &known, &current);
            let scope: Vec<usize> = (0..probes.len())
                .filter(|&index| probes[index].owner == label || rivals.contains(&probes[index].owner))
                .collect();
            let missing: Vec<usize> = scope.iter().copied().filter(|&index| known[index].is_none()).collect();
            let attempt_cost = 1 + scope.len();
            if calls1 + calls2 + missing.len() + attempt_cost > self.bounds.max_calls {
                stopped = Some(format!(
                    "{label} needs about {} more calls, beyond --max-calls={}",
                    missing.len() + attempt_cost,
                    self.bounds.max_calls
                ));
                break;
            }
            ui.note(format!(
                "{label}: {} phrasings in scope, its own and those of {}; up to {} calls, about ${:.4}",
                scope.len(),
                if rivals.is_empty() { "no other skill".to_string() } else { rivals.join(", ") },
                missing.len() + 2 * attempt_cost,
                (missing.len() + 2 * scope.len()) as f64 * COST_PER_CALL
            ));
            if !missing.is_empty() {
                let wanted: Vec<Probe> = missing.iter().map(|&index| probes[index].clone()).collect();
                let (measured, _) = measure(self.models, &current, &wanted, threshold)?;
                calls1 += wanted.len();
                for (&index, routed) in missing.iter().zip(measured) {
                    known[index] = routed;
                }
            }
            let scope: Vec<usize> = scope.into_iter().filter(|&index| known[index].is_some()).collect();
            let scoped: Vec<&Probe> = scope.iter().map(|&index| &probes[index]).collect();
            let wanted: Vec<Probe> = scoped.iter().map(|probe| (*probe).clone()).collect();
            let project = self.brain.project().filter(|_| self.global.contains_key(label)).map(String::from);
            let project_had_scenario = project.is_some() && self.brain.project_scenario(label)?.is_some();

            let mut feedback: Option<String> = None;
            for attempt in 1..=2 {
                if attempt == 2 && calls1 + calls2 + attempt_cost > self.bounds.max_calls {
                    stopped = Some(format!("no calls left for a second rewrite of {label} within --max-calls"));
                    break;
                }
                let skill =
                    current.iter().find(|skill| skill.label == label).cloned().expect("ranked skills are in the matrix");
                let mut weak: Vec<(&Probe, &Routed)> = probes
                    .iter()
                    .zip(&known)
                    .filter_map(|(probe, routed)| Some((probe, routed.as_ref()?)))
                    .filter(|(probe, _)| probe.owner == label)
                    .collect();
                weak.sort_by(|a, b| a.1.home.total_cmp(&b.1.home));
                let weak: Vec<WeakPhrasing> = weak
                    .iter()
                    .take(6)
                    .map(|(probe, routed)| WeakPhrasing {
                        phrase: probe.phrase.clone(),
                        routed_to: routed.chosen.clone(),
                        confidence: round(routed.home),
                    })
                    .collect();
                let rows: Vec<&Skill> =
                    rivals.iter().filter_map(|rival| current.iter().find(|s| &s.label == rival)).collect();
                calls2 += 1;
                let input = RewriteInput {
                    skill: &skill,
                    threshold,
                    weak: &weak,
                    competitors: &rows,
                    feedback: feedback.as_deref(),
                };
                let rewrite = match rewrite_row(&self.models.llm, &input) {
                    Ok((rewrite, ms)) => {
                        ui.note(format!(
                            "{label}: rewrite {attempt} in {}: {}",
                            crate::ui::duration(ms),
                            c::dim(&rewrite.scenario)
                        ));
                        rewrite
                    }
                    Err(error) => {
                        ui.warn(format!("{label}: the agent could not be reached. {error:#}"));
                        break;
                    }
                };
                let before: Vec<&Routed> = scope.iter().filter_map(|&index| known[index].as_ref()).collect();
                let mut change = Change {
                    ts: now_iso(),
                    label: label.to_string(),
                    attempt,
                    kept: false,
                    scenario_before: skill.scenario.clone(),
                    scenario_after: rewrite.scenario.trim().to_string(),
                    dropped: vec![],
                    counter_examples: vec![],
                    score_before: round(score(
                        &scoped.iter().copied().zip(before.iter().copied()).collect::<Vec<_>>(),
                        self.importance,
                    )),
                    score_after: 0.0,
                    home_before: before.iter().filter(|routed| routed.verdict == Verdict::Home).count(),
                    home_after: 0,
                    probes: scope.len(),
                    problem: None,
                    explanation: rewrite.explanation.clone(),
                    undone_at: None,
                    project: project.clone(),
                    project_had_scenario,
                };
                let applied = match apply(&current, label, &rewrite, &rivals, self.global) {
                    Ok(applied) => applied,
                    Err(problem) => {
                        ui.note(format!("{label}: discarded, {problem}."));
                        change.problem = Some(problem.clone());
                        changes.push(change);
                        feedback = Some(problem);
                        continue;
                    }
                };
                change.dropped = applied.dropped.clone();
                change.counter_examples = applied.counter_examples.clone();
                change.scenario_after = applied.scenario.clone();
                let (after, _) = measure(self.models, &applied.skills, &wanted, threshold)?;
                calls1 += wanted.len();
                let (was, now, problem) = gate(label, &scoped, &before, &after, self.importance);
                change.score_before = was;
                change.score_after = now;
                change.home_after = after.iter().flatten().filter(|routed| routed.verdict == Verdict::Home).count();
                change.problem = problem.clone();
                match problem {
                    None => {
                        change.kept = true;
                        ui.learned(format!(
                            "{label}: score {} → {}, {} → {} of {} phrasings home{}{}",
                            fixed(was, 3),
                            fixed(now, 3),
                            change.home_before,
                            change.home_after,
                            change.probes,
                            project.as_ref().map(|project| format!(" · kept for project {project}")).unwrap_or_default(),
                            if self.options.dry_run { " (dry run: not written)" } else { "" }
                        ));
                        if !self.options.dry_run {
                            write(self.brain, &change)?;
                        }
                        current = applied.skills;
                        for (&index, routed) in scope.iter().zip(after) {
                            if routed.is_some() {
                                known[index] = routed;
                            }
                        }
                        changes.push(change);
                        break;
                    }
                    Some(problem) => {
                        ui.note(format!(
                            "{label}: discarded, {problem}.{}",
                            if attempt == 1 { " Trying once more." } else { "" }
                        ));
                        changes.push(change);
                        feedback = Some(problem);
                    }
                }
            }
            if stopped.is_some() {
                break;
            }
        }
        Ok(Optimisation {
            ranking,
            changes,
            system1_calls: calls1,
            system2_calls: calls2,
            applied: !self.options.dry_run,
            stopped,
        })
    }
}

// ── Undo ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Undone {
    pub label: String,
    pub scenario: String,
    pub examples: Vec<String>,
    pub counter_examples: Vec<CounterExample>,
    pub dry_run: bool,
    /// The project the change was kept in, when it was a global skill's: only that project changes back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// Put back what the newest optimisation still in force changed, of `label` or of any skill: the
/// scenario it replaced, the examples it dropped, and without the counter-examples it added. A change
/// is on record where it was made, so one made inside a project is undone from inside it; there, a
/// global skill's scenario goes back to the project's earlier one, or to the global row's (ADR 22).
pub fn undo(brain: &Brain, label: Option<&str>, dry_run: bool) -> Result<Undone> {
    let entries = brain.meta_with_prefix("optimised:")?;
    let found = entries
        .into_iter()
        .rev()
        .filter_map(|(key, value)| Some((key, serde_json::from_str::<Change>(&value).ok()?)))
        .find(|(_, change)| change.undone_at.is_none() && label.is_none_or(|label| label == change.label));
    let Some((key, mut change)) = found else {
        return Err(anyhow!(match label {
            Some(label) => format!("No optimisation of {label} to undo."),
            None => "No optimisation to undo.".to_string(),
        }));
    };
    let Some(row) = brain.skill(&change.label)? else { bail!("{} is no longer in the brain.", change.label) };
    if row.scenario != change.scenario_after {
        bail!(
            "The scenario of {} has changed since it was optimised, so nothing was undone. It is now: {}",
            change.label,
            row.scenario
        );
    }
    // A project that kept no scenario of its own before sees the global one again, whatever it now says.
    let back_to_global = change.project.is_some() && !change.project_had_scenario;
    let global =
        if back_to_global { brain.global_skills()?.into_iter().find(|global| global.label == row.label) } else { None };
    let scenario = global.map_or_else(|| change.scenario_before.clone(), |global| global.scenario);
    if !dry_run {
        if back_to_global {
            brain.clear_project_scenario(&change.label)?;
        } else {
            brain.set_scenario(&change.label, &change.scenario_before)?;
        }
        for example in &change.dropped {
            brain.add_example(&change.label, example)?;
        }
        for ce in &change.counter_examples {
            brain.remove_counter_example(&ce.label, &ce.phrase)?;
        }
        change.undone_at = Some(now_iso());
        brain.put_meta(&key, &serde_json::to_string(&change)?)?;
    }
    Ok(Undone {
        label: change.label,
        scenario,
        examples: change.dropped,
        counter_examples: change.counter_examples,
        dry_run,
        project: change.project,
    })
}

// ── Text ─────────────────────────────────────────────────────────────────────

fn pct(value: f64) -> String {
    format!("{}%", (value * 100.0).round())
}

fn summary_line(name: &str, summary: &Summary) -> String {
    format!(
        "  {name:<34} {:>4}   home {:>4}  stolen {:>4}  escalated {:>4}   mean {}  worst {}",
        summary.probes,
        pct(summary.accuracy),
        pct(summary.steal_rate),
        pct(summary.escalation_rate),
        fixed(summary.mean_confidence, 2),
        fixed(summary.worst_confidence, 2)
    )
}

fn phrasing(line: &Line) -> String {
    let routed = match (&line.routed_to, line.verdict) {
        (_, "home") => String::new(),
        (Some(label), verdict) => format!(" → {label} at {} ({verdict})", fixed(line.confidence, 2)),
        (None, verdict) => format!(" → no skill ({verdict})"),
    };
    let kind = if line.kind == Kind::Replay { c::dim(" replayed") } else { String::new() };
    format!("{}  \"{}\"{routed}{kind}", fixed(line.home, 2), line.phrase)
}

/// The report as text, for stdout.
pub fn render(report: &Report) -> String {
    let mut out = vec![format!(
        "{} {}",
        c::bold(format!("Measured {} phrasings with System 1", report.overall.probes)),
        c::dim(format!(
            "threshold {}{}",
            report.threshold,
            report.only.as_ref().map(|only| format!(" · only {only}")).unwrap_or_default()
        ))
    )];
    out.push(summary_line("stored examples, each held out", &report.leave_one_out));
    out.push(summary_line("accepted requests, replayed", &report.replay));
    out.push(summary_line("overall", &report.overall));
    if report.rebuilt_contexts > 0 {
        out.push(c::dim(format!(
            "  {} replays ran in an inferred context: their directory is gone.",
            report.rebuilt_contexts
        )));
    }
    if report.forgotten > 0 {
        out.push(c::dim(format!(
            "  {} accepted requests name a skill that was forgotten, and were left out.",
            report.forgotten
        )));
    }
    if report.failed_calls > 0 {
        out.push(c::yellow(format!("  {} calls failed and are left out.", report.failed_calls)));
    }

    out.push(format!(
        "\n{} {}",
        c::bold("By skill"),
        c::dim("biggest expected gain first · weight = importance (use × deemed right) × deficit")
    ));
    for skill in &report.skills {
        let evidence = c::dim(format!(
            "weight {} · used {}× · {} of {} judged routes right",
            fixed(skill.weight, 2),
            skill.uses,
            skill.correct,
            skill.judged
        ));
        let summary = &skill.summary;
        out.push(format!(
            "  {}  {evidence}  {} of {} home, {} stolen, {} escalated, mean {}",
            c::bold(&skill.label),
            summary.home,
            summary.probes,
            summary.stolen,
            summary.escalated,
            fixed(summary.mean_confidence, 2)
        ));
        for line in &skill.lowest {
            out.push(format!("     {} {}", c::dim("lowest"), phrasing(line)));
        }
        for line in skill.steals.iter().filter(|steal| !skill.lowest.contains(steal)) {
            out.push(format!("     {} {}", c::red("stolen"), phrasing(line)));
        }
    }

    if let Some(optimisation) = &report.optimisation {
        out.push(format!("\n{}", c::bold("Optimisation")));
        if optimisation.changes.is_empty() {
            out.push(c::dim("  No rewrite was tried."));
        }
        for change in &optimisation.changes {
            let verdict = if change.kept { c::green("kept     ") } else { c::dim("discarded") };
            let only_here = match &change.project {
                Some(project) if change.kept => c::dim(format!(" · for project {project} only")),
                _ => String::new(),
            };
            out.push(format!(
                "  {verdict} {} #{}: score {} → {}, {} → {} of {} phrasings home{only_here}",
                c::bold(&change.label),
                change.attempt,
                fixed(change.score_before, 3),
                fixed(change.score_after, 3),
                change.home_before,
                change.home_after,
                change.probes
            ));
            out.push(format!("     {} {}", c::dim("was"), change.scenario_before));
            out.push(format!("     {} {}", c::dim("now"), change.scenario_after));
            for example in &change.dropped {
                out.push(format!("     {} \"{example}\"", c::dim("drops")));
            }
            for ce in &change.counter_examples {
                out.push(format!("     {} \"{}\" {}", c::dim("adds"), ce.phrase, c::dim(format!("as not {}", ce.label))));
            }
            if let Some(problem) = &change.problem {
                out.push(c::dim(format!("     because {problem}")));
            }
        }
        if let Some(stopped) = &optimisation.stopped {
            out.push(c::dim(format!("  Stopped: {stopped}.")));
        }
        out.push(c::dim(format!(
            "  {} System 1 and {} System 2 calls. {}",
            optimisation.system1_calls,
            optimisation.system2_calls,
            if optimisation.applied {
                "Kept changes are written; jli eval undo puts the last one back."
            } else {
                "Dry run: nothing was applied."
            }
        )));
    }

    out.push(String::new());
    out.push(c::dim(format!(
        "System 1: {} calls, about ${:.4}. {}",
        report.system1_calls,
        report.system1_cost,
        match &report.stored {
            Some(key) => format!("Stored as {key}; jli stats shows the trend."),
            None => "Dry run: no summary was stored.".to_string(),
        }
    )));
    out.join("\n")
}

/// The trend lines `jli stats` shows: full evaluations only, so that the numbers compare.
pub fn render_trend(trend: &[Trend]) -> Vec<String> {
    let full: Vec<&Trend> = trend.iter().filter(|entry| entry.only.is_none() && !entry.limited).collect();
    let Some(last) = full.last() else {
        return vec![c::dim("  not measured yet: jli eval routes every stored example with itself held out")];
    };
    let accuracy: Vec<f64> = full.iter().rev().take(12).rev().map(|entry| entry.overall.accuracy).collect();
    vec![
        format!(
            "  {}  {} home now, over {} evaluations",
            c::cyan(sparkline(&accuracy)),
            pct(last.overall.accuracy),
            full.len()
        ),
        c::dim(format!(
            "  last on {}: {} phrasings, {} stolen, {} escalated, mean confidence {}, worst {}",
            last.ts.chars().take(10).collect::<String>(),
            last.overall.probes,
            pct(last.overall.steal_rate),
            pct(last.overall.escalation_rate),
            fixed(last.overall.mean_confidence, 2),
            fixed(last.overall.worst_confidence, 2)
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed(home: f64, verdict: Verdict) -> Routed {
        Routed { chosen: None, confidence: home, home, verdict, candidates: vec![] }
    }

    fn skill(label: &str, step: &str) -> Skill {
        Skill {
            label: label.into(),
            steps: vec![step.into()],
            scenario: format!("the user wants {label}"),
            examples: vec![],
            counter_examples: vec![],
            slots: vec![],
            risk: crate::types::Risk::Safe,
            origin: crate::types::Origin::Seed,
            uses: 0,
            created_at: String::new(),
            last_used_at: None,
            created_host: String::new(),
        }
    }

    fn probe(owner: &str) -> Probe {
        Probe {
            owner: owner.into(),
            phrase: format!("{owner} phrasing"),
            kind: Kind::Example,
            context: read_context(std::path::Path::new("/")),
            requests: 0,
            rebuilt: false,
        }
    }

    #[test]
    fn a_used_and_trusted_skill_outweighs_an_unused_weaker_one() {
        let trusted = Evidence { judged: 20, correct: 19 };
        let unknown = Evidence::default();
        assert_eq!(importance(0, unknown), 0.5, "nobody has judged it: a coin toss, and no use");
        assert!((importance(40, trusted) - (1.0 + 41f64.ln()) * 20.0 / 22.0).abs() < 1e-12);

        // Routed home at 0.55, against routed at 0.30 and so escalating.
        let busy = importance(40, trusted) * deficit(&[&routed(0.55, Verdict::Home)]);
        let idle = importance(0, unknown) * deficit(&[&routed(0.30, Verdict::Escalated)]);
        assert!((deficit(&[&routed(0.30, Verdict::Escalated)]) - 1.7).abs() < 1e-12, "shortfall 0.7 plus one miss in one");
        assert!(busy > idle, "{busy} > {idle}");
        assert_eq!(deficit(&[]), 0.0);
        let often_wrong = importance(40, Evidence { judged: 20, correct: 2 });
        assert!(often_wrong < importance(40, trusted), "a skill often turned down gains less from more confidence");
    }

    #[test]
    fn the_gate_weighs_by_owner_and_refuses_a_regression_elsewhere() {
        let importance = HashMap::from([("a".to_string(), 3.0), ("b".to_string(), 1.0)]);
        let probes = [probe("a"), probe("b")];
        let scoped: Vec<&Probe> = probes.iter().collect();
        let before = [routed(0.4, Verdict::Escalated), routed(0.9, Verdict::Home)];
        let before: Vec<&Routed> = before.iter().collect();

        let better = vec![Some(routed(0.8, Verdict::Home)), Some(routed(0.9, Verdict::Home))];
        let (was, now, problem) = gate("a", &scoped, &before, &better, &importance);
        assert_eq!((was, now, problem), (0.525, 0.825, None), "(3×0.4 + 0.9) / 4, then (3×0.8 + 0.9) / 4");

        let stolen = vec![Some(routed(0.95, Verdict::Home)), Some(routed(0.1, Verdict::Stolen("a".into())))];
        let (_, _, problem) = gate("a", &scoped, &before, &stolen, &importance);
        assert_eq!(problem.as_deref(), Some("it made \"b phrasing\", which belongs to b, route to a instead"));

        let flat = vec![Some(routed(0.41, Verdict::Escalated)), Some(routed(0.9, Verdict::Home))];
        let (_, _, problem) = gate("a", &scoped, &before, &flat, &importance);
        assert!(problem.unwrap().contains("must gain at least"), "noise is not an improvement");
    }

    #[test]
    fn a_held_out_phrase_leaves_every_row() {
        let mut one = skill("one", "true");
        one.examples = vec!["Newest Files".into(), "keep".into()];
        let mut two = skill("two", "true");
        two.counter_examples = vec!["newest files".into()];
        let held = held_out(&[one, two], " newest files ");
        assert_eq!(held[0].examples, ["keep"]);
        assert!(held[1].counter_examples.is_empty());
    }

    #[test]
    fn a_new_row_takes_its_place_in_the_matrix() {
        let make = |label: &str, uses: i64| Skill { uses, ..skill(label, "true") };
        let skills = [make("busy", 5), make("alpha", 0), make("omega", 0)];
        let labels = |all: Vec<Skill>| all.into_iter().map(|skill| skill.label).collect::<Vec<_>>();
        assert_eq!(labels(with_row(&skills, &make("middle", 0))), ["busy", "alpha", "middle", "omega"]);
        assert_eq!(labels(with_row(&skills, &make("alpha", 9))), ["alpha", "busy", "omega"], "a label is listed once");
    }

    #[test]
    fn a_rewrite_is_measured_as_a_project_would_keep_it() {
        let with_examples = |label: &str, examples: &[&str]| Skill {
            examples: examples.iter().map(|example| example.to_string()).collect(),
            ..skill(label, "true")
        };
        let skills = [with_examples("disk", &["size here", "taught here"]), with_examples("files", &["size here"])];
        let rewrite = crate::system2::Rewrite {
            scenario: "the user wants the total size".into(),
            drop_examples: vec!["size here".into(), "taught here".into()],
            counter_examples: vec![CounterExample { label: "files".into(), phrase: "size here".into() }],
            explanation: String::new(),
        };
        let rivals = ["files".to_string()];

        // Outside a project the rows themselves change.
        let applied = apply(&skills, "disk", &rewrite, &rivals, &HashMap::new()).unwrap();
        assert_eq!(applied.dropped, ["size here", "taught here"]);
        assert_eq!((applied.skills[1].examples.len(), applied.skills[1].counter_examples.len()), (1, 1));

        // Inside one, both skills global: only the phrasing the project learned can go, and the
        // counter-example is a lesson, which hides the phrasing among the competitor's examples.
        let global = HashMap::from([("disk".to_string(), vec!["size here".to_string()]), ("files".into(), vec![])]);
        let applied = apply(&skills, "disk", &rewrite, &rivals, &global).unwrap();
        assert_eq!(applied.dropped, ["taught here"]);
        assert_eq!(applied.skills[0].examples, ["size here"]);
        assert!(applied.skills[1].examples.is_empty() && applied.skills[1].counter_examples == ["size here"]);

        let drop_only = crate::system2::Rewrite {
            scenario: skills[0].scenario.clone(),
            drop_examples: vec!["size here".into()],
            counter_examples: vec![],
            explanation: String::new(),
        };
        let problem = apply(&skills, "disk", &drop_only, &rivals, &global).err().unwrap_or_default();
        assert!(problem.contains("\"size here\" is a phrasing of the global disk"), "{problem}");
    }
}
