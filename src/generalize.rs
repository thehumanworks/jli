//! `jli generalize`: the agent proposes how to make project skills global, and each proposal is
//! written only once the user approves it. See docs/adr/0017-project-scope.md.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;

use crate::context::read_context;
use crate::eval::{in_parallel, verify_row, with_row};
use crate::llm::SlotDraft;
use crate::slots::{checked_slots, keep_settings, match_proposal, match_template, slots_a_flag, undeclared};
use crate::store::{slug, Brain, NewEvent, NewSkill, Promote};
use crate::system2::{generalize as propose, Generality, Generalization, GeneralizeInput};
use crate::types::{Path, Skill, Slot};
use crate::ui::{c, duration, fixed, risk_tag, Prompt, Ui};
use crate::Models;

/// How many phrasings each side of the check replays, as in `learn::verify`.
const CHECKS: usize = 8;
/// How many skills from other projects the agent is shown.
const ELSEWHERE: usize = 10;

pub struct GeneralizeOptions {
    /// One project skill, or every one in the project.
    pub label: Option<String>,
    /// Every project skill of this user, in every project.
    pub all_projects: bool,
    /// Show the proposals and write nothing.
    pub dry_run: bool,
    /// Write each proposal that passes the checks without asking. No environment variable sets it.
    pub approve_all: bool,
    pub threshold: f64,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Generalized {
    /// What was made global, as (project, project skill, global skill).
    pub written: Vec<(String, String, String)>,
    /// How many proposals were shown.
    pub proposed: usize,
    /// There was nothing to look at: no project, or no such skill.
    pub nothing: bool,
}

/// What an approved proposal would write.
enum Plan {
    Keep,
    New(NewSkill),
    Into(Skill),
}

/// Look at each project skill in turn: ask the agent, check its proposal, show it, and write it
/// only when the user approves.
pub fn generalize(
    brain: &Brain,
    models: &Models,
    options: &GeneralizeOptions,
    ui: &Ui,
    prompt: &mut dyn Prompt,
) -> Result<Generalized> {
    let mut done = Generalized::default();
    let wanted = |skill: &Skill| options.label.as_ref().is_none_or(|label| *label == skill.label);
    let targets: Vec<(String, Skill)> = if options.all_projects {
        let mut all = Vec::new();
        for project in brain.projects()? {
            all.extend(brain.skills_in(&project)?.into_iter().filter(wanted).map(|skill| (project.clone(), skill)));
        }
        all
    } else {
        let Some(project) = brain.project() else {
            ui.warn("Not inside a project: there is no .jlirc here or above. Run it in a project, name one with --project=<id>, or add --all-projects.");
            done.nothing = true;
            return Ok(done);
        };
        brain.project_skills()?.into_iter().filter(wanted).map(|skill| (project.to_string(), skill)).collect()
    };
    if targets.is_empty() {
        let global = brain.global_skills()?;
        ui.warn(match &options.label {
            Some(label) if global.iter().any(|skill| skill.label == *label) => format!("{label} is already a global skill."),
            Some(label) => format!("No project skill called {label}."),
            None => "There are no project skills to generalize.".to_string(),
        });
        done.nothing = true;
        return Ok(done);
    }
    for (project, skill) in &targets {
        consider(brain, models, options, ui, prompt, project, skill, &mut done)?;
    }
    Ok(done)
}

#[allow(clippy::too_many_arguments)]
fn consider(
    brain: &Brain,
    models: &Models,
    options: &GeneralizeOptions,
    ui: &Ui,
    prompt: &mut dyn Prompt,
    project: &str,
    skill: &Skill,
    done: &mut Generalized,
) -> Result<()> {
    // Read again for each skill: an earlier one may have just become global.
    let global = brain.global_skills()?;
    let elsewhere = same_intent(brain, project, skill)?;
    ui.line(format!(
        "\n{} {} {}",
        c::magenta("◆ generalize"),
        c::bold(&skill.label),
        c::dim(format!("· project {project} · {}", models.llm.agent_label()))
    ));
    let asked = propose(&models.llm, &GeneralizeInput { project, skill, elsewhere: &elsewhere, global: &global });
    let (proposal, ms) = match asked {
        Ok(reply) => reply,
        Err(error) => {
            ui.warn(format!("Could not ask the agent about {}: {error:#}", skill.label));
            return Ok(());
        }
    };
    let verb = match proposal.decision {
        Generality::Promote => "make it global as it is",
        Generality::Parameterize => "make it global with its project values as slots",
        Generality::Merge => "merge it into a global skill",
        Generality::Keep => "keep it in the project",
    };
    ui.line(format!("   proposes: {verb} {}", c::dim(format!("· {}", duration(ms)))));
    if !proposal.reason.is_empty() {
        ui.line(format!("   {}", c::dim(&proposal.reason)));
    }
    show(ui, &format!("before  project {project}"), skill);
    let plan = match plan(&proposal, skill, &global) {
        Ok(plan) => plan,
        Err(problem) => {
            ui.warn(format!("Not offered: {problem}. Nothing was written."));
            return Ok(());
        }
    };
    let (row, after, mut about) = match &plan {
        Plan::Keep => {
            ui.note(format!("{} stays in project {project}. Nothing was written.", skill.label));
            return Ok(());
        }
        Plan::New(new) => {
            let mut label = new.label.clone();
            while global.iter().any(|known| known.label == label) {
                label.push_str("-x");
            }
            let row = Skill {
                label,
                steps: new.steps.clone(),
                scenario: new.scenario.clone(),
                examples: new.examples.clone(),
                counter_examples: vec![],
                slots: new.slots.clone(),
                risk: new.risk,
                origin: new.origin,
                uses: 0,
                created_at: String::new(),
                last_used_at: None,
                created_host: String::new(),
            };
            let lines = shown("after   global, new skill", &row);
            let what = format!(
                "{} becomes a global skill, with the {} of {}",
                c::bold(&row.label),
                count(skill.examples.len(), "phrasing", "phrasings"),
                skill.label
            );
            (row, what, lines)
        }
        Plan::Into(into) => {
            let mut row = into.clone();
            merge_phrasings(&mut row, &skill.examples);
            let lines = shown("after   global, existing skill", &row);
            let what = format!(
                "the {} of {} join the global skill {}",
                count(skill.examples.len(), "phrasing", "phrasings"),
                skill.label,
                c::bold(&into.label)
            );
            (row, what, lines)
        }
    };
    for line in &about {
        ui.line(line);
    }
    done.proposed += 1;
    let kept = if skill.counter_examples.is_empty() {
        String::new()
    } else {
        let counters = count(skill.counter_examples.len(), "counter-example stays", "counter-examples stay");
        format!(", and its {counters} with {project} as a lesson about the global skill")
    };
    let approval = format!(
        "   {} {after}. The project skill {} is then removed from {project}{kept}.",
        c::bold("If approved:"),
        skill.label
    );
    ui.line(&approval);
    // What would be written, for the question to show when the trace did not.
    about.push(approval);

    match verify(brain, models, project, skill, &row, options)? {
        Ok(summary) => ui.note(format!("Verified with System 1: {summary}.")),
        Err(problem) => {
            ui.warn(format!("Rejected by System 1: {problem}. Nothing was written."));
            return Ok(());
        }
    }
    if options.dry_run {
        ui.note("Dry run: nothing was written.");
        return Ok(());
    }
    let approved = if options.approve_all { Some(true) } else { prompt.confirm(&ui.about(about), "Make it global?") };
    match approved {
        None => {
            ui.warn("Not written: nobody to approve it. Re-run with --approve-all to write what passes the checks.");
            return Ok(());
        }
        Some(false) => {
            ui.note(format!("Left in project {project}. Nothing was written."));
            return Ok(());
        }
        Some(true) => {}
    }
    let to = match plan {
        Plan::New(new) => Promote::New(new),
        Plan::Into(into) => Promote::Into(into.label),
        Plan::Keep => return Ok(()),
    };
    let Some(global_label) = brain.promote(project, &skill.label, &to)? else {
        ui.warn(format!("{} is no longer in project {project}, so nothing was written.", skill.label));
        return Ok(());
    };
    let learned = format!(
        "generalized: project skill {} of {project} is now global skill {global_label}, and the project row was removed",
        skill.label
    );
    brain.log_event(&NewEvent {
        cwd: options.cwd.to_string_lossy().into_owned(),
        query: format!("jli generalize {}", skill.label),
        path: Path::None,
        label: None,
        confidence: None,
        command: Some(skill.steps.join(" && ")),
        exit_code: None,
        accepted: false,
        learned: Some(learned),
        ms_system1: None,
        ms_system2: Some(ms as i64),
    })?;
    ui.learned(format!(
        "{} is global now, as {}. The project skill {} was removed from {project}.",
        skill.label,
        c::bold(&global_label),
        skill.label
    ));
    done.written.push((project.to_string(), skill.label.clone(), global_label));
    Ok(())
}

/// What the proposal would write, once checked without any model: the template must give back the
/// project's command, and a command a global skill already runs is that skill (ADR 13).
fn plan(proposal: &Generalization, skill: &Skill, global: &[Skill]) -> std::result::Result<Plan, String> {
    let command = skill.steps.join(" && ");
    let runs_it = |known: &&Skill| match_template(&known.steps, &command).is_some();
    match proposal.decision {
        Generality::Keep => return Ok(Plan::Keep),
        Generality::Merge => {
            let into = proposal.merge_into.as_deref().unwrap_or_default();
            let Some(known) = global.iter().find(|known| known.label == into) else {
                return Err(format!("there is no global skill called {into}"));
            };
            if !runs_it(&known) {
                return Err(format!("{into} does not run {command}, so the project skill cannot be merged into it"));
            }
            return Ok(Plan::Into(known.clone()));
        }
        Generality::Promote | Generality::Parameterize => {}
    }
    if proposal.steps.is_empty() {
        return Err("it proposed no command".into());
    }
    let mut slots = checked_slots(proposal.slots.iter().map(SlotDraft::to_slot).collect(), &proposal.steps);
    // How the project skill's slots are written goes with it (ADR 24). A default does only when it is one
    // of the slot's choices: otherwise it is usually a value of this project, such as its package.
    let known: Vec<Slot> = skill
        .slots
        .iter()
        .map(|slot| Slot { default: slot.default.clone().filter(|_| !slot.choices.is_empty()), ..slot.clone() })
        .collect();
    keep_settings(&mut slots, &known, &proposal.steps);
    let Some(values) = match_template(&proposal.steps, &command) else {
        if slots_a_flag(&proposal.steps, &command) {
            return Err("it made a flag a slot".into());
        }
        return Err(format!("{} does not give back the project's command, {command}", proposal.steps.join(" && ")));
    };
    let missing = undeclared(&proposal.steps, &slots);
    if !missing.is_empty() {
        return Err(format!("it uses {{{}}} without declaring it", missing.join("}, {")));
    }
    let known = global
        .iter()
        .find(|known| runs_it(known) || match_proposal(&known.steps, &known.slots, &proposal.steps, &values).is_some());
    if let Some(known) = known {
        return Ok(Plan::Into(known.clone()));
    }
    let label = slug(if proposal.label.trim().is_empty() { &skill.label } else { &proposal.label });
    let scenario = if proposal.scenario.trim().is_empty() { skill.scenario.clone() } else { proposal.scenario.clone() };
    Ok(Plan::New(NewSkill {
        label,
        steps: proposal.steps.clone(),
        scenario,
        risk: proposal.risk.max(skill.risk),
        origin: skill.origin,
        examples: skill.examples.clone(),
        slots,
    }))
}

fn show(ui: &Ui, title: &str, skill: &Skill) {
    for line in shown(title, skill) {
        ui.line(line);
    }
}

/// A skill as the trace shows it: its command, slots, scenario and phrasings.
fn shown(title: &str, skill: &Skill) -> Vec<String> {
    let mut lines = vec![format!("   {}  {}  {}", c::dim(title), c::bold(&skill.label), risk_tag(skill.risk))];
    for step in &skill.steps {
        lines.push(format!("     {} {step}", c::green("$")));
    }
    for slot in &skill.slots {
        let mut also = if slot.aliases.is_empty() { String::new() } else { format!(", also {}", slot.aliases.join(", ")) };
        let settings = slot.settings();
        if !settings.is_empty() {
            also = format!("{also}; {settings}");
        }
        lines.push(format!(
            "     {}",
            c::dim(format!("{{{}}} {}: {}{also}", slot.name, slot.kind.as_str(), slot.description))
        ));
    }
    lines.push(format!("     {}", c::dim(&skill.scenario)));
    for example in &skill.examples {
        lines.push(format!("     {} \"{example}\"", c::green("+")));
    }
    for example in &skill.counter_examples {
        lines.push(format!("     {} \"{example}\"", c::red("−")));
    }
    lines
}

/// "1 phrasing", "3 phrasings".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn same_phrase(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// A row with more phrasings, each also lifted from its counter-examples, as `promote` writes it.
fn merge_phrasings(row: &mut Skill, phrasings: &[String]) {
    for phrase in phrasings {
        if !row.examples.iter().any(|known| same_phrase(known, phrase)) {
            row.examples.push(phrase.clone());
        }
        row.counter_examples.retain(|known| !same_phrase(known, phrase));
    }
}

/// `row` as a matrix already holding a global row of its label would see it once merged: that
/// matrix's own version, with the project's phrasings added. A new row is seen as it is.
fn as_seen(view: &[Skill], row: &Skill, phrasings: &[String]) -> Skill {
    match view.iter().find(|known| known.label == row.label) {
        Some(known) => {
            let mut known = known.clone();
            merge_phrasings(&mut known, phrasings);
            known
        }
        None => row.clone(),
    }
}

/// The newest phrasings of some skills, two each, as (phrasing, label), leaving out the project skill's own.
fn newest(skills: &[Skill], mine: &[String]) -> Vec<(String, String)> {
    skills
        .iter()
        .flat_map(|known| {
            let recent = &known.examples[known.examples.len().saturating_sub(2)..];
            recent.iter().map(|example| (example.clone(), known.label.clone()))
        })
        .filter(|(example, _)| !mine.iter().any(|own| same_phrase(own, example)))
        .take(CHECKS)
        .collect()
}

/// Ask System 1 before anything is written. First the check every learned row gets
/// (`eval::verify_row`), twice: in the project, once its own row is gone and its counter-examples
/// are its lessons about the global row; and among the global skills, which every other project
/// sees. Then the project's other phrasings must reach the row there, and the newest phrasings of
/// the user's other projects' own skills must not land on it in those projects, where one
/// project's "run the tests" would otherwise take another's.
fn verify(
    brain: &Brain,
    models: &Models,
    project: &str,
    skill: &Skill,
    row: &Skill,
    options: &GeneralizeOptions,
) -> Result<std::result::Result<String, String>> {
    let (context, threshold, phrasings) = (read_context(&options.cwd), options.threshold, &skill.examples);
    let Some(phrase) = phrasings.last() else {
        return Ok(Err(format!("{} has no phrasing to check it with", skill.label)));
    };
    let global = brain.global_skills()?;

    let mut here: Vec<Skill> = brain.view_of(project)?.into_iter().filter(|known| known.label != skill.label).collect();
    if let Some(shadowed) = global.iter().find(|known| known.label == skill.label) {
        here.push(shadowed.clone());
    }
    let mut seen = as_seen(&here, row, phrasings);
    for counter in &skill.counter_examples {
        if !seen.counter_examples.iter().any(|known| same_phrase(known, counter)) {
            seen.counter_examples.push(counter.clone());
        }
    }
    let summary = match verify_row(models, &here, &seen, phrase, &context, threshold)? {
        Ok(summary) => summary,
        Err(problem) => return Ok(Err(format!("in project {project}, {problem}"))),
    };
    if let Err(problem) = verify_row(models, &global, row, phrase, &context, threshold)? {
        return Ok(Err(format!("among the global skills, {problem}")));
    }

    // (phrasing, matrix, the skill it belongs to when it is not this one's, where)
    let mut replays: Vec<(String, Vec<Skill>, Option<String>, String)> = Vec::new();
    let after = with_row(&here, &seen);
    for own in phrasings.iter().rev().skip(1).take(CHECKS - 1) {
        replays.push((own.clone(), after.clone(), None, format!("project {project}")));
    }
    let mut others = Vec::new();
    for other in brain.projects()?.into_iter().filter(|other| other != project) {
        let theirs = brain.skills_in(&other)?;
        if theirs.iter().any(|own| own.label == row.label) {
            continue; // That project's own skill of this name wins there.
        }
        let view = brain.view_of(&other)?;
        let view = with_row(&view, &as_seen(&view, row, phrasings));
        for (example, owner) in newest(&theirs, phrasings) {
            others.push((example, view.clone(), Some(owner), format!("project {other}")));
        }
    }
    let elsewhere = others.len().min(CHECKS);
    replays.extend(others.into_iter().take(CHECKS));
    let decided = in_parallel(&replays, |(example, skills, _, _)| models.system1.decide(example, &context, skills));
    for (decision, (example, _, owner, place)) in decided.into_iter().zip(&replays) {
        let decision = decision?;
        let landed = decision.skill.as_ref().is_some_and(|known| known.label == row.label);
        match owner {
            None if !landed || decision.confidence < threshold => {
                let got = decision.skill.as_ref().map_or("no skill", |known| known.label.as_str());
                return Ok(Err(format!(
                    "in {place}, \"{example}\" routed to {got} at {}, not to {}",
                    fixed(decision.confidence, 2),
                    row.label
                )));
            }
            Some(owner) if landed => {
                return Ok(Err(format!("in {place}, it would capture \"{example}\", which belongs to {owner}")));
            }
            _ => {}
        }
    }
    let own = replays.len() - elsewhere;
    let mut summary = format!("in project {project}, {summary}");
    if own > 0 {
        summary.push_str(&format!("; {own} more of its phrasings route to it"));
    }
    summary.push_str("; it holds among the global skills");
    if elsewhere > 0 {
        summary.push_str(&format!("; and {elsewhere} phrasings of your other projects' skills still route where they did"));
    }
    Ok(Ok(summary))
}

/// Words that say little about what a request is for.
const COMMON: [&str; 17] =
    ["the", "a", "an", "to", "of", "in", "on", "for", "and", "or", "is", "it", "this", "me", "my", "user", "wants"];

fn words(skill: &Skill) -> HashSet<String> {
    let text = [skill.label.replace('-', " "), skill.scenario.clone(), skill.examples.join(" ")].join(" ");
    text.to_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty() && !COMMON.contains(word))
        .map(String::from)
        .collect()
}

/// Skills in the user's other projects that look like the same request: the same label, or two
/// words or more in common. The agent judges; this only picks what it is shown.
fn same_intent(brain: &Brain, project: &str, skill: &Skill) -> Result<Vec<(String, Skill)>> {
    let mine = words(skill);
    let mut found: Vec<(usize, String, Skill)> = Vec::new();
    for other in brain.projects()?.into_iter().filter(|other| other != project) {
        for theirs in brain.skills_in(&other)? {
            let shared = words(&theirs).intersection(&mine).count();
            if shared >= 2 || theirs.label == skill.label {
                found.push((shared, other.clone(), theirs));
            }
        }
    }
    found.sort_by_key(|(shared, _, _)| std::cmp::Reverse(*shared));
    Ok(found.into_iter().take(ELSEWHERE).map(|(_, project, skill)| (project, skill)).collect())
}
