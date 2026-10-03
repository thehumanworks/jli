use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::args;
use crate::context::{list_entries, read_context, Context};
use crate::eval::verify_row;
use crate::exec::{run, try_builtins, RunOptions, RunResult};
use crate::learn::{learn_phrase, settle_proposed, LearnInput, Learned, Settled};
use crate::llm::SlotDraft;
use crate::plan::{self, CandidateInfo, PlanRecord, SlotInfo, SlotValue};
use crate::shell::{first_caller_step, hand_off_text, safe_to_try, HandOff, Shell};
use crate::slots::{
    checked_slots, expand_path, fill_slots, is_flag, match_proposal, match_skill, render, render_fish, undeclared, Filled,
    Source, Template,
};
use crate::store::{slug, Brain, NewEvent, NewSkill};
use crate::suggest::{suggest, Failure, SuggestInput, Suggestion, Verdict};
use crate::system1::Decision;
use crate::system2::{propose, Proposal, ProposalKind, ProposeInput, Rejected};
use crate::types::error_code;
use crate::types::{add_alias, Mode, Origin, Path, Risk, Skill, Slot, SlotKind};
use crate::ui::{c, command_lines, fixed, Prompt, Ui};
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
/// A step such as `cd` must run in the calling shell, and no shell function is there to take it.
pub const NEEDS_CALLER_SHELL: &str = "must run in the calling shell";

/// Why nothing ran, as `--json`'s `blockedCode` says it. The set is closed: a caller can match on it.
pub const DRY_RUN_CODE: &str = "dry_run";
pub const NOT_A_COMMAND_CODE: &str = "not_a_command";
pub const NEEDS_CONFIRMATION: &str = "needs_confirmation";
pub const DESTRUCTIVE: &str = "destructive";
pub const DECLINED: &str = "declined";
pub const UNKNOWN_ARGUMENT: &str = "unknown_argument";
pub const MISSING_VALUE: &str = "missing_value";
pub const PLAN_EXPIRED: &str = "plan_expired";
pub const NOT_FOUND: &str = "plan_not_found";
pub const ALREADY_RAN: &str = "plan_already_run";
pub const PLAN_CHANGED: &str = "plan_changed";
pub const DIRECTORY_MISSING: &str = "directory_missing";
/// A later plan of `jli run`, after one before it failed or was not run.
pub const NOT_RUN: &str = "not_run";
pub const NEEDS_CALLER_SHELL_CODE: &str = "needs_caller_shell";
/// The agent proposed again a skill just rejected for this request (ADR 20).
pub const ALREADY_REJECTED: &str = "already_rejected";

#[derive(Debug, Clone, PartialEq)]
pub struct RouteOptions {
    pub cwd: PathBuf,
    /// System 1's pick is used at or above this confidence; below it the agent is asked.
    pub threshold: f64,
    /// Who does the teaching: see `Mode`.
    pub mode: Mode,
    /// Approve confirmations automatically. Never covers destructive commands.
    pub yes: bool,
    /// Approve every confirmation automatically, destructive commands included.
    pub yolo: bool,
    pub dry_run: bool,
    /// Capture command output instead of streaming it.
    pub capture: bool,
    /// Skip System 1 and go straight to the agent.
    pub agent_only: bool,
    /// Labels to leave out, used when correcting a wrong route.
    pub rejected: Vec<String>,
    /// The user's own words about what they expected, passed to the agent.
    pub clarification: Option<String>,
    /// Where steps such as `cd` go to run in the calling shell, when the shell function started jli.
    pub hand_off: Option<HandOff>,
}

impl RouteOptions {
    pub fn new(cwd: PathBuf, threshold: f64, mode: Mode) -> RouteOptions {
        RouteOptions {
            cwd,
            threshold,
            mode,
            yes: false,
            yolo: false,
            dry_run: false,
            capture: false,
            agent_only: false,
            rejected: vec![],
            clarification: None,
            hand_off: None,
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
    /// The steps that have to run in the calling shell, as they were or would be handed to it.
    pub caller_shell: Vec<String>,
    /// Whether they were handed to the shell function, to run once jli has exited.
    pub handed_off: bool,
    /// The event this was logged as. A dry run logs none.
    pub event_id: Option<i64>,
    /// The stored plan that `jli run <planId>` runs exactly as shown here.
    pub plan_id: Option<i64>,
    /// What was learned, or would be: the request with its values hidden.
    pub phrase: Option<String>,
    /// Each slot's value and where it came from.
    pub values: Vec<SlotValue>,
    /// `blocked` as one of a closed set of codes.
    pub blocked_code: Option<String>,
    /// The slots that still need a value, when that is why nothing ran.
    pub missing: Vec<SlotInfo>,
    /// `name=` arguments of the request that fit no slot.
    pub unknown_arguments: Vec<String>,
    /// Every slot the chosen skill takes.
    pub accepts: Vec<SlotInfo>,
    /// System 1's labels by probability, highest first.
    pub candidates: Vec<CandidateInfo>,
    /// The command's stdout and stderr apart, when output was captured. `output` is both.
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// Auto mode's reading of a failure, with the commands it offers instead.
    pub diagnosis: Option<Suggestion>,
    /// An error, and what it said. With `ran` true, the command ran and what failed came after it.
    pub error: Option<String>,
    pub error_message: Option<String>,
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
    pub fn new(query: &str) -> Outcome {
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
            caller_shell: vec![],
            handed_off: false,
            event_id: None,
            plan_id: None,
            phrase: None,
            values: vec![],
            blocked_code: None,
            missing: vec![],
            unknown_arguments: vec![],
            accepts: vec![],
            candidates: vec![],
            stdout: None,
            stderr: None,
            diagnosis: None,
            error: None,
            error_message: None,
        }
    }

    fn block(&mut self, why: impl Into<String>, code: &str) {
        self.blocked = Some(why.into());
        self.blocked_code = Some(code.to_string());
    }

    /// Something failed after the command ran. The outcome still says it ran, and says this beside it.
    pub fn failed_after_running(&mut self, error: &anyhow::Error) {
        let message = format!("{error:#}");
        self.error_message = Some(match self.error_message.take() {
            Some(earlier) => format!("{earlier}; {message}"),
            None => message,
        });
        self.error.get_or_insert_with(|| error_code(error).to_string());
    }

    fn took_output(&mut self, result: &RunResult) {
        self.output = result.output.clone();
        self.stdout = result.captured_stdout.clone();
        self.stderr = result.captured_stderr.clone();
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

/// What happened when the commands ran, as the write-back needs to know it.
struct Ran {
    result: RunResult,
    diagnosed: Option<Verdict>,
    /// A person was asked "Run it?" and said yes.
    asked: bool,
    /// stderr was captured or watched, so an empty one means it was empty.
    stderr_seen: bool,
    /// System 1's check of a new proposal, run while the command ran (ADR 16), and the row it checked.
    checked: Option<Result<std::result::Result<String, String>>>,
    proposed: Option<Skill>,
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
    /// The request with its stated values hidden. System 1 routes on this and it is what gets
    /// learned, so the words of a value never steer the route.
    phrase: String,
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
        phrase: args::phrase(query, &[]),
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

    fn finish(&mut self) -> Result<Outcome> {
        let outcome = &self.outcome;
        if !self.options.dry_run {
            let nonzero = |ms: u64| (ms > 0).then_some(ms as i64);
            let logged = self.brain.log_event(&NewEvent {
                cwd: self.options.cwd.to_string_lossy().into_owned(),
                query: self.query.to_string(),
                path: outcome.path,
                label: outcome.label.clone(),
                confidence: outcome.confidence,
                command: Some(outcome.commands.join(" && ")).filter(|command| !command.is_empty()),
                exit_code: outcome.exit_code.map(i64::from),
                accepted: outcome.ran,
                // A declined route is marked as `jli fix` marks one it undid, so no reader counts it as a success.
                learned: if outcome.blocked.as_deref() == Some("declined") {
                    Some("corrected".into())
                } else {
                    Some(outcome.learned.join("; ")).filter(|learned| !learned.is_empty())
                },
                ms_system1: nonzero(outcome.ms_system1),
                ms_system2: nonzero(outcome.ms_system2),
            });
            match logged {
                Ok(id) => self.outcome.event_id = Some(id),
                // The command ran whatever the log says: report the failure beside it rather than lose it.
                Err(error) if self.outcome.ran => self.outcome.failed_after_running(&error),
                Err(error) => return Err(error),
            }
        }
        Ok(self.outcome.clone())
    }

    /// Keep a plan of the route as it stands, so `jli run <planId>` can run exactly this later.
    fn prepare(&mut self, plan: &Plan, skill: &Skill, filled: &Filled) -> Result<()> {
        // An optional slot with no value is left out of the plan's commands, not waiting for one (ADR 24).
        let mut missing: Vec<String> = skill
            .slots
            .iter()
            .filter(|slot| !slot.optional && filled.values.get(&slot.name).is_none_or(|value| value.is_empty()))
            .map(|slot| slot.name.clone())
            .collect();
        missing.extend(undeclared(&skill.steps, &skill.slots));
        let record = PlanRecord { missing, ..self.record(plan, skill, filled) };
        self.outcome.missing = record.missing_info();
        let id = plan::save(self.brain, &self.options.cwd, &record)?;
        self.outcome.plan_id = Some(id);
        self.ui.note(format!("Stored as plan {id}: jli run {id} runs exactly this, with no model call."));
        Ok(())
    }

    fn record(&self, plan: &Plan, skill: &Skill, filled: &Filled) -> PlanRecord {
        PlanRecord {
            query: self.query.to_string(),
            phrase: self.phrase.clone(),
            path: plan.path,
            confidence: plan.confidence,
            label: skill.label.clone(),
            is_new: plan.is_new,
            steps: skill.steps.clone(),
            scenario: skill.scenario.clone(),
            skill_risk: skill.risk,
            slots: skill.slots.clone(),
            risk: plan.risk,
            commands: self.outcome.commands.clone(),
            values: self.outcome.values.clone(),
            missing: vec![],
            unknown_arguments: filled.unmatched.clone(),
            aliases: filled.aliases.clone(),
            ms_system1: self.outcome.ms_system1,
            ms_system2: self.outcome.ms_system2,
        }
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
            let decided = self.models.system1.decide(&self.phrase, &self.context, &self.skills())?;
            self.outcome.ms_system1 += decided.ms;
            ui.decision(&decided);
            self.outcome.candidates = decided
                .candidates
                .iter()
                .map(|candidate| CandidateInfo { label: candidate.label.clone(), probability: candidate.probability })
                .collect();

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
                    self.outcome.block(NOT_A_COMMAND, NOT_A_COMMAND_CODE);
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
                ui.escalate(&escalation, &self.models.llm.agent_label());
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
                        arguments: args::stated(self.query),
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
                // A flag is never a slot value (ADR 19): given one, the proposal is the command with the flag
                // written in, which no known skill's slot stands for. Looked up among every skill, so a
                // rejected template named with a flag is not taken for the rejected command below.
                let (proposal, provided) = match flags_written_in(&proposal, &provided, &self.all_skills, &self.phrase) {
                    Some((written, provided, why)) => {
                        ui.note(why);
                        (written, provided)
                    }
                    None => (proposal, provided),
                };
                let existing = match proposal.kind {
                    ProposalKind::Existing => {
                        skills.iter().find(|skill| Some(&skill.label) == proposal.existing_label.as_ref()).cloned()
                    }
                    _ => None,
                };
                // A "new" command that a known skill already produces is that skill, as in `learn_phrase`:
                // a second row would compete with it for the same phrasings.
                let (existing, provided) = match existing {
                    None if proposal.kind != ProposalKind::NotACommand => skills
                        .iter()
                        .find_map(|skill| {
                            match_proposal(&skill.steps, &skill.slots, &proposal.steps, &provided)
                                .map(|values| (skill.clone(), values))
                        })
                        .map_or((None, provided), |(skill, values)| {
                            ui.note(format!("{} already runs this command, so that skill is used.", c::bold(&skill.label)));
                            (Some(skill), values)
                        }),
                    existing => (existing, provided),
                };
                // A skill just rejected for this request stays off the list above, so it is never routed to
                // again; nor does its command come back under another label (ADR 20).
                if existing.is_none() && proposal.kind != ProposalKind::NotACommand {
                    let named = |skill: &&Skill| {
                        proposal.kind == ProposalKind::Existing && proposal.existing_label.as_ref() == Some(&skill.label)
                    };
                    // The command as it would run: a file value expanded, as `fill_slots` expands it.
                    let as_run = |files: Vec<&str>| -> HashMap<String, String> {
                        let expand = |value: &str| expand_path(value, &options.cwd, |name| std::env::var(name).ok()).value;
                        let value = |name: &str, value: &str| {
                            if files.contains(&name) {
                                expand(value)
                            } else {
                                value.to_string()
                            }
                        };
                        provided.iter().map(|(name, given)| (name.clone(), value(name, given))).collect()
                    };
                    let files = proposal.slots.iter().filter(|slot| slot.kind == SlotKind::File);
                    let proposed = as_run(files.map(|slot| slot.name.as_str()).collect());
                    let proposed_slots =
                        checked_slots(proposal.slots.iter().map(SlotDraft::to_slot).collect(), &proposal.steps);
                    let again = self
                        .all_skills
                        .iter()
                        .filter(|skill| options.rejected.contains(&skill.label))
                        .find(|skill| named(skill) || produces(skill, &proposal.steps, &proposed_slots, &proposed));
                    let again = again.map(|rejected| {
                        let commands = if named(&rejected) {
                            let files = rejected.slots.iter().filter(|slot| slot.kind == SlotKind::File);
                            render(&rejected.steps, &rejected.slots, &as_run(files.map(|slot| slot.name.as_str()).collect()))
                        } else {
                            render(&proposal.steps, &proposed_slots, &proposed)
                        };
                        (rejected.label.clone(), commands)
                    });
                    if let Some((label, commands)) = again {
                        return self.refuse_rejected(label, commands);
                    }
                }
                let confidence = decision.as_ref().map(|decided| decided.confidence);

                if proposal.kind == ProposalKind::NotACommand || (existing.is_none() && proposal.steps.is_empty()) {
                    self.outcome.path = Path::System2;
                    self.outcome.answer = proposal.answer.clone();
                    self.outcome.block(NOT_A_COMMAND, NOT_A_COMMAND_CODE);
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
                        let base =
                            proposal.label.as_deref().or(proposal.steps.first().map(String::as_str)).unwrap_or("skill");
                        let mut label = slug(base);
                        while self.all_skills.iter().any(|skill| skill.label == label) {
                            label.push_str("-x");
                        }
                        let slots = checked_slots(proposal.slots.iter().map(SlotDraft::to_slot).collect(), &proposal.steps);
                        // A proposal nobody has vetted yet: the request's own risk reading gets a vote too.
                        let intent = decision.as_ref().map_or(Risk::Safe, |decided| decided.intent_risk);
                        Plan {
                            skill: draft_skill(
                                label,
                                proposal.steps.clone(),
                                proposal.scenario.clone(),
                                proposal.risk,
                                slots,
                            ),
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
        let mut skill = plan.skill.clone();
        self.outcome.path = plan.path;
        self.outcome.label = Some(skill.label.clone());
        self.outcome.confidence = plan.confidence;
        self.outcome.risk = Some(plan.risk);

        let template = Template { steps: &skill.steps, scenario: &skill.scenario, slots: &skill.slots };
        let filled = fill_slots(self.models, self.query, &template, &self.context, &plan.provided)?;
        self.phrase = filled.phrase.clone();
        self.outcome.ms_system1 += filled.ms_system1;
        self.outcome.ms_system2 += filled.ms_system2;
        let mut unknown = filled.missing.clone();
        unknown.extend(undeclared(&skill.steps, &skill.slots));
        self.outcome.commands = render(&skill.steps, &skill.slots, &filled.values);
        self.outcome.phrase = Some(self.phrase.clone());
        self.outcome.values = filled
            .sources
            .iter()
            .map(|(name, source)| SlotValue {
                name: name.clone(),
                value: filled.values[name].clone(),
                source: source.to_string(),
            })
            .collect();
        self.outcome.accepts = skill.slots.iter().map(SlotInfo::of).collect();
        let caller_at = first_caller_step(&self.outcome.commands);
        if let Some(at) = caller_at {
            // Read by the calling shell, so quoted for it.
            let fish = options.hand_off.as_ref().is_some_and(|hand_off| hand_off.shell == Shell::Fish);
            let for_shell =
                if fish { render_fish(&skill.steps, &skill.slots, &filled.values) } else { self.outcome.commands.clone() };
            self.outcome.caller_shell = for_shell[at..].to_vec();
        }
        ui.command(&self.outcome.commands, plan.risk);
        for (name, source) in &filled.sources {
            let written = filled.expanded.iter().find(|(slot, _)| slot == name).map(|(_, written)| written);
            if *source != Source::Agent || !plan.is_new || written.is_some() {
                let read_as = filled.aliases.iter().find(|(slot, _)| slot == name);
                let read_as = read_as.map(|(_, alias)| format!(", reading {alias}= as {name}=")).unwrap_or_default();
                let written = written.map(|written| format!(", {written} expanded")).unwrap_or_default();
                let value = &filled.values[name];
                // A choice says what it put in the command, which is not the word that chose it.
                let chose = skill.slots.iter().find(|slot| &slot.name == name).and_then(|slot| slot.choice(value));
                let chose = chose.map(|choice| match choice.text.trim() {
                    "" => ", which writes nothing".to_string(),
                    text => format!(", written {text}"),
                });
                ui.note(format!("{{{name}}} = {value}{} (from {source}{read_as}{written})", chose.unwrap_or_default()));
            }
        }
        for name in &filled.omitted {
            let flag = skill.slots.iter().find(|slot| &slot.name == name).and_then(|slot| slot.flag.as_deref());
            let flag = flag.map(|flag| format!(" with {flag}")).unwrap_or_default();
            ui.note(format!("{{{name}}} is left out{flag}: it is optional, and nothing gave it a value."));
        }
        for (name, variable) in &filled.not_set {
            ui.note(format!("${variable} is not set, so {{{name}}} is passed as written."));
        }
        if !filled.unmatched.is_empty() {
            let given: Vec<String> = filled.unmatched.iter().map(|name| format!("{name}=")).collect();
            let takes: Vec<String> = skill
                .slots
                .iter()
                .map(|slot| match slot.aliases.as_slice() {
                    [] => slot.name.clone(),
                    aliases => format!("{} (or {})", slot.name, aliases.join(", ")),
                })
                .collect();
            let blocked = format!("{} does not take {}", skill.label, given.join(", "));
            ui.warn(if takes.is_empty() {
                format!("{blocked}: it takes no arguments.")
            } else {
                format!("{blocked}. Its arguments are: {}.", takes.join(", "))
            });
            self.outcome.block(blocked, UNKNOWN_ARGUMENT);
            self.outcome.unknown_arguments = filled.unmatched.clone();
            self.prepare(&plan, &skill, &filled)?;
            return self.finish();
        }
        if !filled.not_a_choice.is_empty() {
            let each: Vec<String> = filled
                .not_a_choice
                .iter()
                .filter_map(|(name, value)| {
                    let slot = skill.slots.iter().find(|slot| &slot.name == name)?;
                    Some(format!("{{{name}}} takes {}, not {value}", slot.choice_list()))
                })
                .collect();
            let blocked = each.join("; ");
            ui.warn(format!("{blocked}."));
            self.outcome.block(blocked, MISSING_VALUE);
            self.prepare(&plan, &skill, &filled)?;
            return self.finish();
        }
        if !unknown.is_empty() {
            let names: Vec<String> = unknown
                .iter()
                .map(|name| {
                    match skill.slots.iter().find(|slot| &slot.name == name).filter(|slot| !slot.choices.is_empty()) {
                        Some(slot) => format!("{{{name}}} ({})", slot.choice_list()),
                        None => format!("{{{name}}}"),
                    }
                })
                .collect();
            let blocked = format!("could not work out a value for {}", names.join(", "));
            ui.warn(format!("{blocked}. Say it in the request, in quotes if it is text."));
            self.outcome.block(blocked, MISSING_VALUE);
            self.prepare(&plan, &skill, &filled)?;
            return self.finish();
        }
        if options.dry_run {
            self.outcome.block(DRY_RUN, DRY_RUN_CODE);
            if caller_at.is_some() {
                let those = self.outcome.caller_shell.join(" && ");
                ui.note(if options.hand_off.is_some() {
                    format!("{those} would run in your shell, once jli has exited.")
                } else {
                    format!("{those} would have to run in your shell, which needs the shell function (jli shell-init).")
                });
            }
            ui.note("Dry run: nothing was executed and nothing was learned.");
            self.prepare(&plan, &skill, &filled)?;
            return Ok(self.outcome.clone());
        }
        if caller_at.is_some() && options.hand_off.is_none() {
            self.refuse_without_function();
            return self.finish();
        }

        // ── Permission: one question, every time, unless --yes or --yolo ───────────────────
        let destructive = plan.risk == Risk::Destructive;
        let asked = !(options.yolo || (options.yes && !destructive));
        let about = ui.about(command_lines(&self.outcome.commands, plan.risk));
        let approved = if asked { prompt.confirm(&about, "Run it?") } else { Some(true) };
        let Some(approved) = approved else {
            self.outcome.block(
                if destructive { "destructive commands need an interactive yes or --yolo" } else { "needs confirmation" },
                if destructive { DESTRUCTIVE } else { NEEDS_CONFIRMATION },
            );
            ui.warn(if destructive {
                "Not run: it is destructive, which needs an interactive yes or --yolo."
            } else {
                "Not run: nobody to confirm. Re-run with --yes to approve."
            });
            self.prepare(&plan, &skill, &filled)?;
            return self.finish();
        };
        if !approved {
            // A "no" is a chance to learn what the right answer was.
            if let Some(corrected) = self.correct(prompt, &plan, None, true)? {
                return Ok(corrected);
            }
            self.outcome.block("declined", DECLINED);
            return self.finish();
        }

        // ── Execute ───────────────────────────────────────────────────────────────
        // A new proposal's stderr is watched too, so a failure with nothing to say can be told apart.
        let watch_stderr = options.mode == Mode::Auto || plan.is_new;
        let (steps, for_shell) = (self.outcome.commands.clone(), self.outcome.caller_shell.clone());
        // A skill the agent proposed is checked with System 1 while its command runs, so the check
        // never holds up the command's output.
        let proposed = plan.is_new.then(|| Skill { examples: vec![self.phrase.clone()], ..skill.clone() });
        let (models, known, context, phrase) =
            (self.models, self.all_skills.clone(), self.context.clone(), self.phrase.clone());
        let (result, checked) = std::thread::scope(|scope| {
            let check = proposed.as_ref().map(|row| {
                let (known, context, phrase) = (&known, &context, &phrase);
                scope.spawn(move || verify_row(models, known, row, phrase, context, options.threshold))
            });
            let result = self.execute(&steps, &for_shell, watch_stderr);
            (result, check.map(|check| check.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))))
        });
        let result = result?;
        let stderr_seen = options.capture || watch_stderr;
        self.outcome.ran = true;
        self.outcome.exit_code = Some(result.exit_code);
        self.outcome.took_output(&result);
        if result.exit_code != 0 {
            ui.warn(format!("exited with code {}", result.exit_code));
        }

        // ── Auto mode: a failure is feedback, but only a diagnosis says what kind ──
        let mut diagnosed = None;
        if options.mode == Mode::Auto && result.exit_code != 0 {
            let failure = Failure { exit_code: result.exit_code, stderr: result.stderr.clone() };
            if let Some(diagnosis) = self.diagnose(Some(failure)) {
                self.outcome.diagnosis = Some(diagnosis.clone());
                for line in diagnosis_lines(&diagnosis) {
                    ui.line(line);
                }
                // An environment problem says nothing about the route, so nothing is unlearned or offered.
                let verdict = diagnosis.verdict;
                diagnosed = Some(verdict);
                if matches!(verdict, Verdict::WrongCommand | Verdict::WrongValue) {
                    match self.correct(prompt, &plan, Some(diagnosis), verdict == Verdict::WrongCommand) {
                        Ok(Some(corrected)) => return Ok(corrected),
                        Ok(None) => {}
                        // The first command ran: keep saying so.
                        Err(error) => {
                            self.outcome.failed_after_running(&error);
                            return self.finish();
                        }
                    }
                }
                if verdict == Verdict::WrongCommand {
                    ui.note("Nothing was learned from this run.");
                    return self.finish();
                }
            }
        }

        // ── Write back: this is where the software learns ─────────────────────────
        let ran = Ran { result, diagnosed, asked, stderr_seen, checked, proposed };
        if let Err(error) = self.write_back(&plan, &mut skill, &filled, ran) {
            self.outcome.failed_after_running(&error);
        }
        self.finish()
    }

    /// What an approved run teaches: a new skill, a phrasing, a slot's other name, and a use. The
    /// same for a route run now and a stored plan run later.
    fn write_back(&mut self, plan: &Plan, skill: &mut Skill, filled: &Filled, ran: Ran) -> Result<()> {
        let options = self.options;
        let ui = self.ui;
        let Ran { result, diagnosed, asked, stderr_seen, checked, proposed } = ran;
        // A human yes vouches for the route even if the command itself failed; --yes and --yolo need a clean exit.
        let vouched = plan.path == Path::System2 || !(options.yes || options.yolo) || result.exit_code == 0;
        // A name the request used for a slot is kept on the same terms as a phrasing.
        let mut renamed = Vec::new();
        for (slot, alias) in &filled.aliases {
            if add_alias(&mut skill.slots, slot, alias) {
                renamed.push((slot, alias));
            }
        }
        if plan.is_new {
            // A failure that is not the command's fault still teaches the route: a diagnosis that says
            // so, or exit 1 with nothing on stderr after a person said yes (grep finding no match,
            // diff finding a difference). Under --yes or --yolo only a clean exit counts.
            let blameless = if diagnosed == Some(Verdict::Environment) {
                Some("the diagnosis put the failure on where it ran")
            } else if result.exit_code == 1 && asked && stderr_seen && result.stderr.trim().is_empty() {
                Some("it exited 1 with nothing on stderr, which is an answer and not an error")
            } else {
                None
            };
            if let Some(reason) = blameless {
                ui.note(format!("The command did not exit 0, but {reason}, so the proposal is learned."));
            }
            if result.exit_code == 0 || blameless.is_some() {
                let (scenario, settled) = match (checked, &proposed) {
                    (Some(checked), Some(row)) => settle_proposed(
                        self.models,
                        &self.all_skills,
                        row,
                        &self.phrase,
                        &self.context,
                        options.threshold,
                        checked,
                        ui,
                    ),
                    _ => (skill.scenario.clone(), Settled::Unchecked),
                };
                // Another writer may have taken the label since it was shown; the skill then gets the next free
                // one, or, if that writer stored this same command, the phrasing joins their skill.
                let (label, created) = self.brain.add_new_skill(&NewSkill {
                    label: skill.label.clone(),
                    steps: skill.steps.clone(),
                    scenario,
                    risk: skill.risk,
                    origin: Origin::Agent,
                    examples: vec![self.phrase.clone()],
                    slots: skill.slots.clone(),
                })?;
                self.outcome.label = Some(label.clone());
                self.brain.record_use(&label)?;
                if created {
                    self.outcome.learned.push(format!("new skill {label}"));
                    self.outcome.learned.extend(settled.note(&label));
                    ui.learned(format!("new skill {}. Next time System 1 handles this without the agent.", c::bold(&label)));
                } else {
                    self.outcome.learned.push(format!("\"{}\" means {label}", self.phrase));
                    ui.learned(format!(
                        "\"{}\" means {}, which another jli stored for this command while it ran",
                        self.phrase,
                        c::bold(&label)
                    ));
                }
            } else {
                ui.note("The command failed, so the proposal was not learned.");
            }
        } else {
            self.brain.record_use(&skill.label)?;
            for (slot, alias) in renamed {
                if vouched && self.brain.add_alias(&skill.label, slot, alias)? {
                    self.outcome.learned.push(format!("{alias}= means {{{slot}}} in {}", skill.label));
                    ui.learned(format!("{alias}= means {{{slot}}} in {}", c::bold(&skill.label)));
                }
            }
            let taught_something =
                vouched && (plan.path == Path::System2 || plan.confidence.unwrap_or(1.0) < ALREADY_KNOWN_AT);
            // Linked as teaching links it: a phrasing that now means this skill stops being a counter-example of it.
            if taught_something && self.brain.link_phrase(&skill.label, &self.phrase)? {
                self.outcome.learned.push(format!("\"{}\" means {}", self.phrase, skill.label));
                ui.learned(format!("\"{}\" means {}", self.phrase, c::bold(&skill.label)));
            }
        }
        Ok(())
    }

    // ── The calling shell: steps such as cd that only work there (ADR 14) ──────

    /// Run the steps. Those before the first that needs the calling shell run in jli's own; that
    /// one and every one after it, `for_shell`, go to the shell function, so the later steps see
    /// what it changed. Builtins that touch nothing else are tried here first, so a `cd` to a
    /// missing directory fails with its own message and exit code, and nothing is handed over.
    fn execute(&mut self, steps: &[String], for_shell: &[String], watch_stderr: bool) -> Result<RunResult> {
        let options = self.options;
        let at = steps.len() - for_shell.len();
        let ours = RunOptions { capture: options.capture, watch_stderr };
        if for_shell.is_empty() {
            return run(steps, &options.cwd, ours);
        }
        let hand_off = options.hand_off.as_ref().ok_or_else(|| anyhow!("no shell function to hand {} to", for_shell[0]))?;
        let mut result = run(&steps[..at], &options.cwd, ours)?;
        if result.exit_code != 0 {
            return Ok(result);
        }
        let theirs = &steps[at..];
        if safe_to_try(theirs) {
            let tried = try_builtins(&hand_off_text(theirs, Shell::Bash), &options.cwd, options.capture)?;
            if tried.exit_code != 0 {
                let output = result.output.take().map(|mut output| {
                    output.push_str(tried.output.as_deref().unwrap_or_default());
                    output
                });
                let joined = |ours: Option<String>, theirs: Option<String>| {
                    ours.map(|mut ours| {
                        ours.push_str(theirs.as_deref().unwrap_or_default());
                        ours
                    })
                };
                return Ok(RunResult {
                    exit_code: tried.exit_code,
                    output,
                    stderr: result.stderr + &tried.stderr,
                    captured_stdout: joined(result.captured_stdout, tried.captured_stdout),
                    captured_stderr: joined(result.captured_stderr, tried.captured_stderr),
                });
            }
        }
        hand_off.write(for_shell)?;
        self.outcome.handed_off = true;
        self.ui.note(format!("{} runs in your shell as soon as jli exits.", for_shell.join(" && ")));
        Ok(result)
    }

    /// Without the shell function, a step such as `cd` would run in a shell of jli's own and
    /// change nothing. Say so, and how to install it, instead of reporting a success.
    fn refuse_without_function(&mut self) {
        let ui = self.ui;
        self.outcome.block(NEEDS_CALLER_SHELL, NEEDS_CALLER_SHELL_CODE);
        ui.warn(format!(
            "Not run: {} has to run in your own shell, and jli can only run commands in a shell of its own, where this would change nothing.",
            self.outcome.caller_shell.join(" && ")
        ));
        let running = Shell::running(|name| std::env::var(name).ok(), crate::exec::process_table);
        let shells = running.map_or_else(|| Shell::ALL.to_vec(), |shell| vec![shell]);
        for shell in shells {
            let (line, file) = shell.install_hint();
            ui.note(format!("To let jli hand it to your shell, add this line to {file} and open a new terminal:"));
            ui.note(format!("    {line}"));
        }
        ui.note("Nothing was learned.");
    }

    /// The agent proposed again what was just rejected for this request. Running it would repeat
    /// the route the person turned down, and storing it would bring the skill back as a second row.
    fn refuse_rejected(&mut self, label: String, commands: Vec<String>) -> Result<Outcome> {
        let ui = self.ui;
        self.outcome.path = Path::System2;
        self.outcome.label = Some(label.clone());
        self.outcome.commands = commands;
        ui.warn(format!(
            "Not run: the agent proposed {} again, which {} runs and was just rejected for this request.",
            self.outcome.commands.join(" && "),
            c::bold(&label)
        ));
        ui.note(format!("Nothing was learned. To teach the right command: jli teach \"{}\" -- <command>", self.query));
        self.outcome
            .block(format!("{label} was rejected for this request, and the agent proposed it again"), ALREADY_REJECTED);
        self.finish()
    }

    // ── Corrections: how a "no" or a failed run turns into a lesson ────────────

    /// The route the person turned down, logged as its own event before the correction is: not
    /// accepted, and marked `corrected`. The row the correction then writes carries its own label.
    fn log_declined(&mut self) -> Result<()> {
        let attempt = self.outcome.clone();
        self.outcome.ran = false;
        self.outcome.blocked = Some("declined".into());
        self.finish()?;
        self.outcome = attempt;
        Ok(())
    }

    fn diagnose(&mut self, failure: Option<Failure>) -> Option<Suggestion> {
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
            Ok((suggestion, ms)) => {
                // The suggester is a language model call like the agent's, so its time is counted with it.
                self.outcome.ms_system2 += ms;
                Some(suggestion)
            }
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

        // A failure's diagnosis was in the trace; when the trace is hidden, the question shows it.
        let mut about = given.as_ref().map(diagnosis_lines).unwrap_or_default();
        let suggestion = match given {
            Some(given) if options.mode == Mode::Auto => Some(given),
            None if options.mode == Mode::Auto => self.diagnose(None),
            _ => None,
        };
        let choice = if let Some(suggestion) = suggestion {
            for (index, option) in suggestion.options.iter().enumerate() {
                let line = format!("   {}  {}  {}", c::bold(index + 1), option.command, c::dim(&option.description));
                ui.line(&line);
                about.push(line);
            }
            let hint = c::dim("· \"?\" asks the agent · enter skips");
            let question = if suggestion.options.is_empty() {
                format!("Say what you expected {hint}")
            } else {
                format!("Pick a number, or say what you expected {hint}")
            };
            let answer = prompt.ask(&ui.about(about), &question);
            let Some(answer) = answer.map(|answer| answer.trim().to_string()) else { return Ok(None) };
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
            let question =
                format!("What should it run instead? {}", c::dim("a command, \"?\" to ask the agent, or enter to skip"));
            // Only a "no" to "Run it?" leads here, and that question showed the command.
            let Some(answer) = prompt.ask(&[], &question).map(|answer| answer.trim().to_string()) else { return Ok(None) };
            if answer.is_empty() {
                return Ok(None);
            }
            if answer == "?" {
                Correction::Clarification(None)
            } else {
                Correction::Steps(split_steps(&answer))
            }
        };

        self.log_declined()?;
        let was_wrong = match &choice {
            Correction::Steps(steps) => is_another_command(skill, steps),
            Correction::Clarification(_) => unlearn,
        };
        if was_wrong && !plan.is_new {
            unlearn_phrase(self.brain, ui, skill, &self.phrase)?;
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
        // A typed `cd` is handed to the calling shell like any other, and without the shell
        // function it can neither run nor be learned.
        let for_shell = first_caller_step(&steps).map_or_else(Vec::new, |at| steps[at..].to_vec());
        if !for_shell.is_empty() && options.hand_off.is_none() {
            self.outcome.commands = steps;
            self.outcome.caller_shell = for_shell;
            self.refuse_without_function();
            return self.finish().map(Some);
        }
        let taught = teach_instead(self.brain, self.models, ui, &self.phrase, &steps, options)?;
        let risk = self.brain.skill(&taught.label)?.map_or(Risk::Modifies, |skill| skill.risk);
        ui.command(&steps, risk);
        self.outcome.caller_shell = for_shell.clone();
        let rerun = self.execute(&steps, &for_shell, false)?;
        if let Err(error) = self.brain.record_use(&taught.label) {
            self.outcome.failed_after_running(&error);
        }
        if rerun.exit_code != 0 {
            ui.warn(format!("exited with code {}", rerun.exit_code));
        }
        self.outcome.path = Path::Taught;
        self.outcome.label = Some(taught.label.clone());
        // System 1's confidence was in the label just turned down, not in this one.
        self.outcome.confidence = None;
        self.outcome.commands = steps;
        self.outcome.ran = true;
        self.outcome.exit_code = Some(rerun.exit_code);
        self.outcome.blocked = None;
        self.outcome.blocked_code = None;
        self.outcome.took_output(&rerun);
        self.outcome.learned = vec![format!("\"{}\" means {}", self.phrase, taught.label)];
        self.finish().map(Some)
    }
}

/// Auto mode's reading of a failure, as the trace shows it.
fn diagnosis_lines(diagnosis: &Suggestion) -> Vec<String> {
    let meaning = match diagnosis.verdict {
        Verdict::WrongCommand => "this was the wrong command for the request",
        Verdict::Environment => "the command fits the request; the problem is where it ran",
        Verdict::WrongValue => "right command, wrong value",
        Verdict::NotApplicable => "no verdict",
    };
    vec![format!("{} {}", c::yellow("◇ diagnosis"), c::dim(format!("· {meaning}"))), format!("   {}", diagnosis.explanation)]
}

// ── What a correction teaches, shared by a "no" at the prompt and `jli fix` ──

/// Choosing a different command is itself the verdict on the old one. The same command
/// with another slot value, or without an optional one, is not: that skill was right.
pub fn is_another_command(wrong: &Skill, steps: &[String]) -> bool {
    match_skill(&wrong.steps, &wrong.slots, &steps.join(" && ")).is_none()
}

/// A proposal that gives a flag for a slot's value, of the known skill it names or of its own template, as
/// the command with the flag written into its steps: `cd {directory}` with `-` is `cd -`, and `ls {options}
/// {directory}` with `-la` is `ls -la {directory}`. A translator's draft is refused the same way when a
/// command is taught (ADR 19). Returns the new proposal, the values left and what to tell the person;
/// None when no value the steps use is a flag.
fn flags_written_in(
    proposal: &Proposal,
    provided: &HashMap<String, String>,
    skills: &[Skill],
    phrase: &str,
) -> Option<(Proposal, HashMap<String, String>, String)> {
    let named = match proposal.kind {
        ProposalKind::Existing => Some(skills.iter().find(|skill| Some(&skill.label) == proposal.existing_label.as_ref())?),
        ProposalKind::New => None,
        ProposalKind::NotACommand => return None,
    };
    let (steps, slots): (Vec<String>, Vec<SlotDraft>) = match named {
        Some(skill) => (skill.steps.clone(), skill.slots.iter().map(SlotDraft::of).collect()),
        None => (proposal.steps.clone(), proposal.slots.clone()),
    };
    let used = undeclared(&steps, &[]);
    let flags: HashMap<String, String> = provided
        .iter()
        .filter(|(name, value)| used.contains(name) && is_flag(value))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let (name, value) = flags.iter().min_by_key(|(name, _)| used.iter().position(|seen| seen == *name))?;
    let steps = render(&steps, &[], &flags);
    let whose = named.map(|skill| format!(" of {}", c::bold(&skill.label))).unwrap_or_default();
    let why =
        format!("{value} is a flag, not a value for {{{name}}}{whose}, so {} is a command of its own.", steps.join(" && "));
    let written = Proposal {
        kind: ProposalKind::New,
        existing_label: None,
        label: if named.is_some() { None } else { proposal.label.clone() },
        steps,
        scenario: if proposal.scenario.trim().is_empty() {
            format!("the user wants: {phrase}")
        } else {
            proposal.scenario.clone()
        },
        slots: slots.into_iter().filter(|slot| !flags.contains_key(&slot.name)).collect(),
        slot_values: proposal.slot_values.iter().filter(|slot| !flags.contains_key(&slot.name)).cloned().collect(),
        risk: named.map_or(proposal.risk, |skill| proposal.risk.max(skill.risk)),
        ..proposal.clone()
    };
    let left = provided.iter().filter(|(name, _)| !flags.contains_key(*name));
    Some((written, left.map(|(name, value)| (name.clone(), value.clone())).collect(), why))
}

/// Whether a skill's steps run the proposed command: the same template (`match_proposal`), or, once
/// the agent's values are in, the command a literal skill runs (`wc -l {file}` with `notes.txt` for
/// `wc -l notes.txt`). The second is too broad to route by (ADR 13), but right for refusing a repeat.
fn produces(skill: &Skill, proposed: &[String], slots: &[Slot], values: &HashMap<String, String>) -> bool {
    !proposed.is_empty()
        && (match_proposal(&skill.steps, &skill.slots, proposed, values).is_some()
            || match_skill(&skill.steps, &skill.slots, &render(proposed, slots, values).join(" && ")).is_some())
}

/// `phrase` does not mean `skill`: it stops being one of its examples and becomes a counter-example.
pub fn unlearn_phrase(brain: &Brain, ui: &Ui, skill: &Skill, phrase: &str) -> Result<()> {
    brain.remove_example(&skill.label, phrase)?;
    if brain.add_counter_example(&skill.label, phrase)? {
        ui.learned(format!("\"{}\" does {} mean {}", phrase, c::bold("not"), skill.label));
    }
    Ok(())
}

/// `phrase` means `steps`: it joins the skill that runs them, or a new one, as `jli teach` would.
pub fn teach_instead(
    brain: &Brain,
    models: &Models,
    ui: &Ui,
    phrase: &str,
    steps: &[String],
    options: &RouteOptions,
) -> Result<Learned> {
    let taught = learn_phrase(
        brain,
        models,
        &LearnInput {
            phrase: phrase.to_string(),
            steps: steps.to_vec(),
            risk: None,
            mode: options.mode,
            threshold: options.threshold,
            cwd: options.cwd.clone(),
        },
        ui,
    )?;
    ui.learned(format!(
        "\"{}\" means {}{}",
        phrase,
        c::bold(&taught.label),
        if taught.created { " (new skill)" } else { "" }
    ));
    Ok(taught)
}

// ── Running a stored plan: no model, no routing, the same write-back ─────────

/// A stored plan with every value it needs, about to run as it stands.
pub struct Ready<'a> {
    pub id: i64,
    /// The state it was read in, which the claim checks it is still in.
    pub state: &'a str,
    pub record: &'a PlanRecord,
    pub commands: Vec<String>,
    pub cwd: PathBuf,
}

/// Run a plan exactly as stored. Running it by id approves it, as `--yes` does; a destructive plan
/// still needs `--yolo` or a yes at the terminal. It is claimed just before it runs, so it runs once.
pub fn run_plan(
    brain: &Brain,
    models: &Models,
    ready: &Ready,
    outcome: Outcome,
    ui: &Ui,
    prompt: &mut dyn Prompt,
    options: &RouteOptions,
) -> Result<Outcome> {
    let record = ready.record;
    let mut route = Route {
        brain,
        models,
        ui,
        options,
        query: &record.query,
        phrase: record.phrase.clone(),
        context: read_context(&ready.cwd),
        all_skills: brain.skills()?,
        outcome,
    };
    let mut skill = draft_skill(
        record.label.clone(),
        record.steps.clone(),
        record.scenario.clone(),
        record.skill_risk,
        record.slots.clone(),
    );
    let mut is_new = record.is_new;
    // Another plan, or another writer, may have taught this command since: then it is that skill, as in `run`.
    if is_new {
        let empty = HashMap::new();
        if let Some(known) =
            route.all_skills.iter().find(|known| match_proposal(&known.steps, &known.slots, &record.steps, &empty).is_some())
        {
            ui.note(format!("{} already runs this command, so that skill is used.", c::bold(&known.label)));
            skill = known.clone();
            is_new = false;
            route.outcome.label = Some(known.label.clone());
        }
    }
    let plan = Plan {
        skill: skill.clone(),
        path: record.path,
        confidence: record.confidence,
        is_new,
        provided: HashMap::new(),
        risk: record.risk,
    };
    route.outcome.ms_system1 = record.ms_system1;
    route.outcome.ms_system2 = record.ms_system2;
    ui.command(&ready.commands, plan.risk);
    // `jli run` hands nothing to a calling shell (ADR 14), so a plan with a `cd` in it stays unrun.
    if let Some(at) = first_caller_step(&ready.commands) {
        route.outcome.caller_shell = ready.commands[at..].to_vec();
        route.refuse_without_function();
        return Ok(route.outcome);
    }

    let destructive = plan.risk == Risk::Destructive;
    let asked = destructive && !options.yolo;
    if asked {
        match prompt.confirm(&ui.about(command_lines(&ready.commands, plan.risk)), "Run it?") {
            Some(true) => {}
            Some(false) => {
                route.outcome.block("declined", DECLINED);
                return Ok(route.outcome);
            }
            None => {
                ui.warn("Not run: it is destructive, which needs an interactive yes or --yolo.");
                route.outcome.block("destructive commands need an interactive yes or --yolo", DESTRUCTIVE);
                return Ok(route.outcome);
            }
        }
    }
    if !brain.claim_plan(ready.id, ready.state)? {
        let why = format!("plan {} has already run, and a plan runs once", ready.id);
        ui.warn(format!("{why}."));
        route.outcome.block(why, ALREADY_RAN);
        return Ok(route.outcome);
    }

    let watch_stderr = plan.is_new;
    let result = run(&ready.commands, &ready.cwd, RunOptions { capture: options.capture, watch_stderr })?;
    route.outcome.ran = true;
    route.outcome.exit_code = Some(result.exit_code);
    route.outcome.took_output(&result);
    if result.exit_code != 0 {
        ui.warn(format!("exited with code {}", result.exit_code));
    }
    let filled = Filled { aliases: record.aliases.clone(), ..Filled::default() };
    // No model is asked, so a new proposal is stored unchecked: ADR 16's check needs System 1.
    let ran =
        Ran { result, diagnosed: None, asked, stderr_seen: options.capture || watch_stderr, checked: None, proposed: None };
    if let Err(error) = route.write_back(&plan, &mut skill, &filled, ran) {
        route.outcome.failed_after_running(&error);
    }
    let mut outcome = route.finish()?;
    if let Err(error) = brain.finish_plan(ready.id, outcome.event_id) {
        outcome.failed_after_running(&error);
    }
    Ok(outcome)
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
