use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::context::{list_entries, read_context, Context};
use crate::exec::{run, RunOptions};
use crate::learn::{learn_phrase, LearnInput};
use crate::llm::AGENT_MODEL;
use crate::slots::{fill_slots, match_template, render, undeclared, Source, Template};
use crate::store::{slug, Brain, NewEvent, NewSkill};
use crate::suggest::{suggest, Failure, SuggestInput, Suggestion, Verdict};
use crate::system1::Decision;
use crate::system2::{propose, ProposalKind, ProposeInput, Rejected};
use crate::types::{Mode, Origin, Path, Risk, Skill, Slot};
use crate::ui::{c, fixed, Prompt, Ui};
use crate::Models;

/// Default for the confidence System 1 needs before its pick is used. Configurable per user.
pub const DEFAULT_THRESHOLD: f64 = 0.5;
/// Below this, with no skill picked, the request is treated as not a command at all.
pub const NOT_A_COMMAND_BELOW: f64 = 0.1;
/// Routes this confident teach nothing new, so their phrasing is not stored.
const ALREADY_KNOWN_AT: f64 = 0.95;

/// Why nothing ran. The first two are not failures: the exit code is 0 for them.
pub const DRY_RUN: &str = "dry run";
pub const NOT_A_COMMAND: &str = "not a command request";

#[derive(Debug, Clone, PartialEq)]
pub struct RouteOptions {
    pub cwd: PathBuf,
    /// System 1's pick is used at or above this confidence; below it the agent is asked.
    pub threshold: f64,
    /// Who does the teaching: see `Mode`.
    pub mode: Mode,
    /// Approve confirmations automatically. Never covers destructive commands.
    pub yes: bool,
    pub dry_run: bool,
    /// Capture command output instead of streaming it.
    pub capture: bool,
    /// Skip System 1 and go straight to the agent.
    pub agent_only: bool,
    /// Labels to leave out, used when correcting a wrong route.
    pub rejected: Vec<String>,
    /// The user's own words about what they expected, passed to the agent.
    pub clarification: Option<String>,
}

impl RouteOptions {
    pub fn new(cwd: PathBuf, threshold: f64, mode: Mode) -> RouteOptions {
        RouteOptions {
            cwd,
            threshold,
            mode,
            yes: false,
            dry_run: false,
            capture: false,
            agent_only: false,
            rejected: vec![],
            clarification: None,
        }
    }
}

/// What happened to one request. `--json` prints exactly this.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub query: String,
    pub path: Path,
    pub label: Option<String>,
    pub commands: Vec<String>,
    #[serde(serialize_with = "js_number")]
    pub confidence: Option<f64>,
    pub risk: Option<Risk>,
    pub ran: bool,
    pub exit_code: Option<i32>,
    /// Why nothing ran, when nothing ran.
    pub blocked: Option<String>,
    pub learned: Vec<String>,
    pub answer: Option<String>,
    pub output: Option<String>,
    pub ms_system1: u64,
    pub ms_system2: u64,
}

/// A whole number without a fraction, 1.0 as `1`, as `--json` has always written it.
fn js_number<S: serde::Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    match *value {
        Some(n) if n.is_finite() && n.fract() == 0.0 && n.abs() < 9e15 => serializer.serialize_i64(n as i64),
        Some(n) => serializer.serialize_f64(n),
        None => serializer.serialize_none(),
    }
}

impl Outcome {
    fn new(query: &str) -> Outcome {
        Outcome {
            query: query.to_string(),
            path: Path::None,
            label: None,
            commands: vec![],
            confidence: None,
            risk: None,
            ran: false,
            exit_code: None,
            blocked: None,
            learned: vec![],
            answer: None,
            output: None,
            ms_system1: 0,
            ms_system2: 0,
        }
    }

    /// The command's own code when it ran, 0 for a dry run or a non-command, 3 when blocked.
    pub fn exit_code(&self) -> i32 {
        if self.ran {
            return self.exit_code.unwrap_or(0);
        }
        match self.blocked.as_deref() {
            Some(DRY_RUN | NOT_A_COMMAND) => 0,
            _ => 3,
        }
    }
}

struct Plan {
    skill: Skill,
    path: Path,
    confidence: Option<f64>,
    is_new: bool,
    provided: HashMap<String, String>,
    risk: Risk,
}

fn draft_skill(label: String, steps: Vec<String>, scenario: String, risk: Risk, slots: Vec<Slot>) -> Skill {
    Skill {
        label,
        steps,
        scenario,
        risk,
        slots,
        examples: vec![],
        counter_examples: vec![],
        origin: Origin::Agent,
        uses: 0,
        created_at: crate::store::now_iso(),
        last_used_at: None,
        created_host: String::new(),
    }
}

/// What the person chose instead of the proposed command.
enum Correction {
    Steps(Vec<String>),
    Clarification(Option<String>),
}

/// Split a typed command on ` && ` into steps.
pub fn split_steps(command: &str) -> Vec<String> {
    let mut steps = Vec::new();
    let mut rest = command;
    loop {
        // `\s+&&\s+`: whitespace on both sides, as much as there is.
        let found = rest.match_indices("&&").find(|(at, _)| {
            let before = rest[..*at].chars().next_back().is_some_and(char::is_whitespace);
            let after = rest[at + 2..].chars().next().is_some_and(char::is_whitespace);
            before && after
        });
        match found {
            Some((at, _)) => {
                steps.push(rest[..at].trim_end().to_string());
                rest = rest[at + 2..].trim_start();
            }
            None => {
                steps.push(rest.to_string());
                break;
            }
        }
    }
    steps.retain(|step| !step.is_empty());
    steps
}

/// One request on its way through the loop.
struct Route<'a> {
    brain: &'a Brain,
    models: &'a Models,
    ui: &'a Ui,
    options: &'a RouteOptions,
    query: &'a str,
    context: Context,
    all_skills: Vec<Skill>,
    outcome: Outcome,
}

/// The whole loop: System 1 decides, System 2 is consulted only when System 1
/// cannot, and whatever gets accepted is written back so System 1 can next time.
pub fn route(
    brain: &Brain,
    models: &Models,
    query: &str,
    ui: &Ui,
    prompt: &mut dyn Prompt,
    options: &RouteOptions,
) -> Result<Outcome> {
    let mut route = Route {
        brain,
        models,
        ui,
        options,
        query,
        context: read_context(&options.cwd),
        all_skills: brain.skills()?,
        outcome: Outcome::new(query),
    };
    route.run(prompt)
}

impl Route<'_> {
    fn skills(&self) -> Vec<Skill> {
        self.all_skills.iter().filter(|skill| !self.options.rejected.contains(&skill.label)).cloned().collect()
    }

    fn finish(&self) -> Result<Outcome> {
        let outcome = &self.outcome;
        if !self.options.dry_run {
            let nonzero = |ms: u64| (ms > 0).then_some(ms as i64);
            self.brain.log_event(&NewEvent {
                cwd: self.options.cwd.to_string_lossy().into_owned(),
                query: self.query.to_string(),
                path: outcome.path,
                label: outcome.label.clone(),
                confidence: outcome.confidence,
                command: Some(outcome.commands.join(" && ")).filter(|command| !command.is_empty()),
                exit_code: outcome.exit_code.map(i64::from),
                accepted: outcome.ran,
                learned: Some(outcome.learned.join("; ")).filter(|learned| !learned.is_empty()),
                ms_system1: nonzero(outcome.ms_system1),
                ms_system2: nonzero(outcome.ms_system2),
            })?;
        }
        Ok(outcome.clone())
    }

    fn run(&mut self, prompt: &mut dyn Prompt) -> Result<Outcome> {
        let options = self.options;
        let ui = self.ui;

        // ── System 1: one fast call against the learned matrix ────────────────────
        let mut decision: Option<Decision> = None;
        let mut plan: Option<Plan> = None;
        let mut escalation = if options.clarification.as_deref().is_some_and(|text| !text.is_empty()) {
            "working from what you said you expected".to_string()
        } else {
            "asked to skip the decision model".to_string()
        };

        if !options.agent_only {
            let decided = self.models.system1.decide(self.query, &self.context, &self.skills())?;
            self.outcome.ms_system1 += decided.ms;
            ui.decision(&decided);

            match &decided.skill {
                Some(skill) if decided.confidence >= options.threshold => {
                    plan = Some(Plan {
                        skill: skill.clone(),
                        path: Path::System1,
                        confidence: Some(decided.confidence),
                        is_new: false,
                        provided: HashMap::new(),
                        risk: skill.risk,
                    });
                }
                None if decided.is_command < NOT_A_COMMAND_BELOW => {
                    ui.note(format!(
                        "Not a command request ({}% sure). Nothing to run.",
                        ((1.0 - decided.is_command) * 100.0).round()
                    ));
                    self.outcome.blocked = Some(NOT_A_COMMAND.into());
                    self.outcome.confidence = Some(decided.confidence);
                    return self.finish();
                }
                Some(skill) => {
                    escalation = format!(
                        "only {} sure about {}, below your {} threshold",
                        fixed(decided.confidence, 2),
                        skill.label,
                        options.threshold
                    );
                }
                None => escalation = "no known skill fits".to_string(),
            }
            decision = Some(decided);
        }

        // ── System 2: slow, deliberate, and only when needed ──────────────────────
        let plan = match plan {
            Some(plan) => plan,
            None => {
                ui.escalate(&escalation, &AGENT_MODEL);
                let skills = self.skills();
                let rejected: Vec<Rejected> = self
                    .all_skills
                    .iter()
                    .filter(|skill| options.rejected.contains(&skill.label))
                    .map(|skill| Rejected { label: skill.label.clone(), steps: skill.steps.clone() })
                    .collect();
                let (proposal, ms) = propose(
                    &self.models.llm,
                    &ProposeInput {
                        query: self.query,
                        context: &self.context,
                        skills: &skills,
                        entries: &list_entries(&options.cwd),
                        rejected,
                        clarification: options.clarification.as_deref(),
                    },
                )?;
                self.outcome.ms_system2 += ms;
                ui.proposal(&proposal, ms);
                let provided: HashMap<String, String> =
                    proposal.slot_values.iter().map(|slot| (slot.name.clone(), slot.value.clone())).collect();
                let existing = match proposal.kind {
                    ProposalKind::Existing => {
                        skills.iter().find(|skill| Some(&skill.label) == proposal.existing_label.as_ref()).cloned()
                    }
                    _ => None,
                };
                let confidence = decision.as_ref().map(|decided| decided.confidence);

                if proposal.kind == ProposalKind::NotACommand || (existing.is_none() && proposal.steps.is_empty()) {
                    self.outcome.path = Path::System2;
                    self.outcome.answer = proposal.answer.clone();
                    self.outcome.blocked = Some(NOT_A_COMMAND.into());
                    if let Some(answer) = proposal.answer.as_deref().filter(|answer| !answer.is_empty()) {
                        ui.line(answer);
                    }
                    return self.finish();
                }
                match existing {
                    Some(existing) => {
                        let risk = existing.risk;
                        Plan { skill: existing, path: Path::System2, confidence, is_new: false, provided, risk }
                    }
                    None => {
                        let base = proposal.label.as_deref().or(proposal.steps.first().map(String::as_str)).unwrap_or("skill");
                        let mut label = slug(base);
                        while self.all_skills.iter().any(|skill| skill.label == label) {
                            label.push_str("-x");
                        }
                        let slots = proposal
                            .slots
                            .iter()
                            .map(|slot| Slot {
                                name: slot.name.clone(),
                                kind: slot.kind,
                                description: slot.description.clone(),
                                default: slot.default.clone(),
                            })
                            .collect();
                        // A proposal nobody has vetted yet: the request's own risk reading gets a vote too.
                        let intent = decision.as_ref().map_or(Risk::Safe, |decided| decided.intent_risk);
                        Plan {
                            skill: draft_skill(label, proposal.steps.clone(), proposal.scenario.clone(), proposal.risk, slots),
                            path: Path::System2,
                            confidence,
                            is_new: true,
                            provided,
                            risk: proposal.risk.max(intent),
                        }
                    }
                }
            }
        };

        // ── Fill in the blanks ────────────────────────────────────────────────────
        let skill = plan.skill.clone();
        self.outcome.path = plan.path;
        self.outcome.label = Some(skill.label.clone());
        self.outcome.confidence = plan.confidence;
        self.outcome.risk = Some(plan.risk);

        let template = Template { steps: &skill.steps, scenario: &skill.scenario, slots: &skill.slots };
        let filled = fill_slots(self.models, self.query, &template, &self.context, &plan.provided)?;
        self.outcome.ms_system1 += filled.ms_system1;
        self.outcome.ms_system2 += filled.ms_system2;
        let mut unknown = filled.missing.clone();
        unknown.extend(undeclared(&skill.steps, &skill.slots));
        self.outcome.commands = render(&skill.steps, &filled.values);
        ui.command(&self.outcome.commands, plan.risk);
        for (name, source) in &filled.sources {
            if *source != Source::Agent || !plan.is_new {
                ui.note(format!("{{{name}}} = {} (from {source})", filled.values[name]));
            }
        }
        if !unknown.is_empty() {
            let names: Vec<String> = unknown.iter().map(|name| format!("{{{name}}}")).collect();
            let blocked = format!("could not work out a value for {}", names.join(", "));
            ui.warn(format!("{blocked}. Say it in the request, in quotes if it is text."));
            self.outcome.blocked = Some(blocked);
            return self.finish();
        }
        if options.dry_run {
            self.outcome.blocked = Some(DRY_RUN.into());
            ui.note("Dry run: nothing was executed and nothing was learned.");
            return Ok(self.outcome.clone());
        }

        // ── Permission: one question, every time, unless --yes ───────────────────
        let destructive = plan.risk == Risk::Destructive;
        let approved = if options.yes && !destructive { Some(true) } else { prompt.confirm("Run it?") };
        let Some(approved) = approved else {
            self.outcome.blocked = Some(
                if destructive { "destructive commands need an interactive yes" } else { "needs confirmation" }.into(),
            );
            ui.warn(if destructive {
                "Not run: it is destructive, which always needs an interactive yes."
            } else {
                "Not run: nobody to confirm. Re-run with --yes to approve."
            });
            return self.finish();
        };
        if !approved {
            // A "no" is a chance to learn what the right answer was.
            if let Some(corrected) = self.correct(prompt, &plan, None, true)? {
                return Ok(corrected);
            }
            self.outcome.blocked = Some("declined".into());
            return self.finish();
        }

        // ── Execute ───────────────────────────────────────────────────────────────
        let result = run(
            &self.outcome.commands,
            &options.cwd,
            RunOptions { capture: options.capture, watch_stderr: options.mode == Mode::Auto },
        )?;
        self.outcome.ran = true;
        self.outcome.exit_code = Some(result.exit_code);
        self.outcome.output = result.output.clone();
        if result.exit_code != 0 {
            ui.warn(format!("exited with code {}", result.exit_code));
        }

        // ── Auto mode: a failure is feedback, but only a diagnosis says what kind ──
        if options.mode == Mode::Auto && result.exit_code != 0 {
            let failure = Failure { exit_code: result.exit_code, stderr: result.stderr.clone() };
            if let Some(diagnosis) = self.diagnose(Some(failure)) {
                let meaning = match diagnosis.verdict {
                    Verdict::WrongCommand => "this was the wrong command for the request",
                    Verdict::Environment => "the command fits the request; the problem is where it ran",
                    Verdict::WrongValue => "right command, wrong value",
                    Verdict::NotApplicable => "no verdict",
                };
                ui.line(format!("{} {}", c::yellow("◇ diagnosis"), c::dim(format!("· {meaning}"))));
                ui.line(format!("   {}", diagnosis.explanation));
                // An environment problem says nothing about the route, so nothing is unlearned or offered.
                let verdict = diagnosis.verdict;
                if matches!(verdict, Verdict::WrongCommand | Verdict::WrongValue) {
                    if let Some(corrected) = self.correct(prompt, &plan, Some(diagnosis), verdict == Verdict::WrongCommand)? {
                        return Ok(corrected);
                    }
                }
                if verdict == Verdict::WrongCommand {
                    ui.note("Nothing was learned from this run.");
                    return self.finish();
                }
            }
        }

        // ── Write back: this is where the software learns ─────────────────────────
        if plan.is_new {
            if result.exit_code == 0 {
                self.brain.add_skill(&NewSkill {
                    label: skill.label.clone(),
                    steps: skill.steps.clone(),
                    scenario: skill.scenario.clone(),
                    risk: skill.risk,
                    origin: Origin::Agent,
                    examples: vec![self.query.to_string()],
                    slots: skill.slots.clone(),
                })?;
                self.brain.record_use(&skill.label)?;
                self.outcome.learned.push(format!("new skill {}", skill.label));
                ui.learned(format!("new skill {}. Next time System 1 handles this without the agent.", c::bold(&skill.label)));
            } else {
                ui.note("The command failed, so the proposal was not learned.");
            }
        } else {
            self.brain.record_use(&skill.label)?;
            // A human yes vouches for the route even if the command itself failed; --yes needs a clean exit.
            let vouched = plan.path == Path::System2 || !options.yes || result.exit_code == 0;
            let taught_something =
                vouched && (plan.path == Path::System2 || plan.confidence.unwrap_or(1.0) < ALREADY_KNOWN_AT);
            if taught_something && self.brain.add_example(&skill.label, self.query)? {
                self.outcome.learned.push(format!("\"{}\" means {}", self.query, skill.label));
                ui.learned(format!("\"{}\" means {}", self.query, c::bold(&skill.label)));
            }
        }
        self.finish()
    }

    // ── Corrections: how a "no" or a failed run turns into a lesson ────────────

    fn diagnose(&self, failure: Option<Failure>) -> Option<Suggestion> {
        let entries = list_entries(&self.options.cwd);
        let input = SuggestInput {
            query: self.query,
            context: &self.context,
            entries: &entries,
            skills: &self.all_skills,
            tried: &self.outcome.commands,
            failure,
        };
        match suggest(&self.models.llm, &input) {
            Ok((suggestion, _)) => Some(suggestion),
            Err(error) => {
                self.ui.note(format!("Could not get suggestions: {error:#}"));
                None
            }
        }
    }

    /// Find out what should have run, learn it, and run it. Returns the new outcome,
    /// or None when the user had nothing to add.
    ///   manual and semi: the user types the command.
    ///   auto: a fast model offers numbered options; the user picks one or says what they expected.
    fn correct(
        &mut self,
        prompt: &mut dyn Prompt,
        plan: &Plan,
        given: Option<Suggestion>,
        unlearn: bool,
    ) -> Result<Option<Outcome>> {
        let options = self.options;
        let ui = self.ui;
        let skill = &plan.skill;

        let suggestion = if options.mode == Mode::Auto { given.or_else(|| self.diagnose(None)) } else { None };
        let choice = if let Some(suggestion) = suggestion {
            for (index, option) in suggestion.options.iter().enumerate() {
                ui.line(format!("   {}  {}  {}", c::bold(index + 1), option.command, c::dim(&option.description)));
            }
            let hint = c::dim("· \"?\" asks the agent · enter skips");
            let question = if suggestion.options.is_empty() {
                format!("Say what you expected {hint}")
            } else {
                format!("Pick a number, or say what you expected {hint}")
            };
            let Some(answer) = prompt.ask(&question).map(|answer| answer.trim().to_string()) else { return Ok(None) };
            if answer.is_empty() {
                return Ok(None);
            }
            let is_number = answer.chars().all(|ch| ch.is_ascii_digit());
            let picked = if is_number {
                answer.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|index| suggestion.options.get(index))
            } else {
                None
            };
            match picked {
                Some(picked) => Correction::Steps(vec![picked.command.clone()]),
                None if is_number => {
                    ui.note(format!("There is no option {answer}."));
                    return Ok(None);
                }
                None => Correction::Clarification((answer != "?").then_some(answer)),
            }
        } else {
            let question = format!(
                "What should it run instead? {}",
                c::dim("a command, \"?\" to ask the agent, or enter to skip")
            );
            let Some(answer) = prompt.ask(&question).map(|answer| answer.trim().to_string()) else { return Ok(None) };
            if answer.is_empty() {
                return Ok(None);
            }
            if answer == "?" {
                Correction::Clarification(None)
            } else {
                Correction::Steps(split_steps(&answer))
            }
        };

        // Choosing a different command is itself the verdict on the old one. The same command
        // with another slot value is not: that skill was right.
        let was_wrong = match &choice {
            Correction::Steps(steps) => match_template(&skill.steps, &steps.join(" && ")).is_none(),
            Correction::Clarification(_) => unlearn,
        };
        if was_wrong && !plan.is_new {
            self.brain.remove_example(&skill.label, self.query)?;
            if self.brain.add_counter_example(&skill.label, self.query)? {
                ui.learned(format!("\"{}\" does {} mean {}", self.query, c::bold("not"), skill.label));
            }
        }
        let steps = match choice {
            Correction::Clarification(clarification) => {
                let mut rejected = options.rejected.clone();
                if unlearn {
                    rejected.push(skill.label.clone());
                }
                let again = RouteOptions { agent_only: true, clarification, rejected, ..options.clone() };
                return route(self.brain, self.models, self.query, ui, prompt, &again).map(Some);
            }
            Correction::Steps(steps) => steps,
        };
        let taught = learn_phrase(
            self.brain,
            self.models,
            &LearnInput {
                phrase: self.query.to_string(),
                steps: steps.clone(),
                risk: None,
                mode: options.mode,
                threshold: options.threshold,
                cwd: options.cwd.clone(),
            },
            ui,
        )?;
        ui.learned(format!(
            "\"{}\" means {}{}",
            self.query,
            c::bold(&taught.label),
            if taught.created { " (new skill)" } else { "" }
        ));
        let risk = self.brain.skill(&taught.label)?.map_or(Risk::Modifies, |skill| skill.risk);
        ui.command(&steps, risk);
        let rerun = run(&steps, &options.cwd, RunOptions { capture: options.capture, watch_stderr: false })?;
        self.brain.record_use(&taught.label)?;
        if rerun.exit_code != 0 {
            ui.warn(format!("exited with code {}", rerun.exit_code));
        }
        self.outcome.path = Path::Taught;
        self.outcome.label = Some(taught.label.clone());
        self.outcome.commands = steps;
        self.outcome.ran = true;
        self.outcome.exit_code = Some(rerun.exit_code);
        self.outcome.blocked = None;
        self.outcome.output = rerun.output;
        self.outcome.learned = vec![format!("\"{}\" means {}", self.query, taught.label)];
        self.finish().map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_commands_split_on_spaced_ampersands() {
        assert_eq!(split_steps("git add -A && git commit -m wip"), ["git add -A", "git commit -m wip"]);
        assert_eq!(split_steps("a&&b"), ["a&&b"]);
        assert_eq!(split_steps("a  &&\tb && "), ["a", "b"]);
        assert_eq!(split_steps("a && && b"), ["a", "&& b"]);
        assert_eq!(split_steps("ls -ltr"), ["ls -ltr"]);
    }

    #[test]
    fn a_whole_number_confidence_has_no_fraction() {
        let mut outcome = Outcome::new("x");
        let json = |outcome: &Outcome| serde_json::to_value(outcome).unwrap()["confidence"].to_string();
        assert_eq!(json(&outcome), "null");
        outcome.confidence = Some(1.0);
        assert_eq!(json(&outcome), "1");
        outcome.confidence = Some(0.98);
        assert_eq!(json(&outcome), "0.98");
    }

    #[test]
    fn exit_codes_follow_the_outcome() {
        let mut outcome = Outcome::new("x");
        assert_eq!(outcome.exit_code(), 3);
        outcome.blocked = Some(DRY_RUN.into());
        assert_eq!(outcome.exit_code(), 0);
        outcome.blocked = Some(NOT_A_COMMAND.into());
        assert_eq!(outcome.exit_code(), 0);
        outcome.ran = true;
        outcome.exit_code = Some(128);
        assert_eq!(outcome.exit_code(), 128);
    }
}
