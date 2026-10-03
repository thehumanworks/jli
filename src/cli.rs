use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::args;
use crate::demo::run_demo;
use crate::eval;
use crate::generalize::{generalize, GeneralizeOptions};
use crate::hand;
use crate::learn::{learn_phrase, LearnInput};
use crate::plan::{self, PlanRecord, RunFlags};
use crate::project::{self, Found};
use crate::router::{
    is_another_command, route, split_steps, teach_instead, unlearn_phrase, Outcome, RouteOptions, DEFAULT_THRESHOLD,
    DRY_RUN, DRY_RUN_CODE,
};
use crate::shell::{init_script, HandOff, Shell};
use crate::slots::learned_phrase;
use crate::store::{user_id, Brain, BrainConfig, Correction, Shared, DEFAULT_USER};
use crate::types::{coded, error_code, BrainEvent, Mode, Path, Risk, CONFIGURATION, DEFAULT_MODE, USAGE};
use crate::ui::{bar, c, duration, risk_tag, sparkline, Prompt, Terminal, Ui};
use crate::Models;

fn help() -> String {
    format!(
        "{}  say what you want, it runs the command, and it learns how you say it

{}
  jli <request in plain language>      route, run, learn
  jli                                  interactive session

{}
  -n, --dry-run    show what would run, execute nothing, learn nothing; keep it as a plan for jli run
  -y, --yes        run without asking (never approves destructive commands)
  -Y, --yolo       run without asking, destructive commands included
      --confidence=N   use System 1's pick at or above N for this run (0 to 1)
      --mode=M         learning mode for this run: manual, semi or auto
  -q, --quiet      hide the decision trace
      --json       print the outcome as JSON (for agents); command output is captured
      --agent      skip the decision model and ask the reasoning agent directly
      --user-id=ID     whose skills, history and settings to use (default \"default\")

{}
  jli commit message=\"fix the parser\"  name=value goes straight to the command's {{message}} slot
  jli commit with message: fix it      so does the slot's name with a colon, or any \"quoted text\"
  jli go to dir=src                    another name for a slot is worked out once by the agent, then kept
  Stated values are passed as written, by no model, and never influence which command is picked.

{}
  jli skills                           what it knows, and the phrasings it learned
  jli stats                            how much it has learned
  jli history                          recent requests and how they were routed
  jli teach \"<phrase>\" -- <command>    link a phrase to a command (repeat to add more phrasings)
  jli teach --global \"<phrase>\" -- …  the same, for every project, from inside one
  jli config                           show settings
  jli config confidence <0..1>         confidence System 1 needs before its pick is used (default 0.5)
  jli config mode <manual|semi|auto>   who does the teaching (default semi)
  jli fix                              the last command run here was wrong: unlearn it and try again
  jli fix <eventId>                    the same for that event (eventId in --json), from anywhere
  jli fix <eventId> -- <command>       that was wrong, this is right: learn it, and return a plan of it
  jli fix <eventId> --expected <words> ask the agent again with what you expected, and return a plan
                                       (add --run to either to run the correction instead)
  jli run <planId> [<planId> …]        run plans (planId in --json) exactly as shown, in order, with no
                                       model call; stops at the first that fails. name=value fills a gap
  jli forget <label>                   delete a skill (inside a project, one of the project's: --global for the user's)
  jli forget-phrase <label> <phrase>   take one phrasing back from a skill; it is kept as a counter-example
  jli alias <label> <slot> <name>      give a slot another name for name=value (--remove takes one away)
  jli eval [<label>] [--limit N]       route every stored example with itself held out, and replay accepted
                                       requests: how often each skill comes home (System 1 calls, priced first)
  jli eval --optimise                  the agent rewrites the weakest rows; a rewrite is kept only if the
                                       measurement improves (--max-skills=3, --max-calls=400)
  jli eval -n [--optimise]             measure, and show the rewrites and their effect, but write nothing:
                                       no summary is stored and no rewrite is applied
  jli eval undo [<label>]              put back what the last optimisation changed
  jli share <label> --to <user>        give another user a copy of one of your skills
  jli project                          the project this directory is in, from the nearest .jlirc
  jli project init [<id>]              make this directory a project: what is learned here stays here
  jli generalize [<label>]             propose making project skills global; each is written only once you approve
      --all-projects   every project's skills · --approve-all  write what passes the checks unasked · -n  only show
  jli sync                             sync the local replica with the cloud brain
  jli shell-init [zsh|bash|fish]       the shell function that lets jli cd and export in your shell:
                                       eval \"$(jli shell-init zsh)\" in ~/.zshrc
  jli demo                             watch it learn in a sandbox (about 12 API calls)

{}
  JLI_DB_URL     file:/path/brain.db (default ~/.jli/brain.db) or libsql://… for a cloud brain
  JLI_SYNC_URL   libsql://… keeps a local replica in sync with a cloud brain
  JLI_DB_TOKEN   auth token for the cloud brain
  JLI_USER_ID    the same as --user-id= (the flag wins)
  JLI_YES        1 or true: the same as --yes
  JLI_YOLO       1 or true: the same as --yolo
  JLI_THRESHOLD  0 to 1: the same as --confidence= (JLI_CONFIDENCE also works)
",
        c::bold("jli"),
        c::bold("Usage"),
        c::bold("Options"),
        c::bold("Arguments"),
        c::bold("The brain"),
        c::bold("Storage"),
    )
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Flags {
    dry_run: bool,
    yes: bool,
    yolo: bool,
    quiet: bool,
    json: bool,
    agent: bool,
    risk: Option<Risk>,
    /// From --confidence; resolved against env, the brain's setting and the default.
    threshold: Option<f64>,
    /// From --mode.
    mode: Option<Mode>,
    /// From --user-id; resolved against JLI_USER_ID and the default user.
    user_id: Option<String>,
}

impl Flags {
    /// No flags but `--quiet`, for tests that drive a subcommand.
    #[cfg(test)]
    pub(crate) fn quiet() -> Flags {
        Flags { quiet: true, ..Flags::default() }
    }

    /// `--quiet --yes`, for tests where a correction runs what the agent proposes.
    #[cfg(test)]
    pub(crate) fn quiet_yes() -> Flags {
        Flags { yes: true, ..Flags::quiet() }
    }
}

fn parse_threshold(text: &str) -> Result<f64> {
    let value: Option<f64> = text.trim().parse().ok();
    match value {
        Some(value) if !text.trim().is_empty() && value.is_finite() && (0.0..=1.0).contains(&value) => Ok(value),
        _ => Err(anyhow!("confidence must be a number from 0 to 1, got \"{text}\"")),
    }
}

fn parse_mode(text: &str) -> Result<Mode> {
    Mode::parse(text.trim()).ok_or_else(|| {
        let modes: Vec<&str> = Mode::ALL.iter().map(|mode| mode.as_str()).collect();
        anyhow!("mode must be one of {}, got \"{text}\"", modes.join(", "))
    })
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

/// Flag, then JLI_MODE, then the brain's stored setting, then the default.
fn resolve_mode(brain: &Brain, flags: &Flags) -> Result<Mode> {
    if let Some(mode) = flags.mode {
        return Ok(mode);
    }
    if let Some(from_env) = env("JLI_MODE") {
        return parse_mode(&from_env);
    }
    match brain.setting("mode")?.filter(|stored| !stored.is_empty()) {
        Some(stored) => parse_mode(&stored),
        None => Ok(DEFAULT_MODE),
    }
}

/// Flag, then JLI_THRESHOLD (or its older name JLI_CONFIDENCE), then the brain's stored setting, then the default.
fn resolve_threshold(brain: &Brain, flags: &Flags) -> Result<f64> {
    if let Some(threshold) = flags.threshold {
        return Ok(threshold);
    }
    if let Some(from_env) = env("JLI_THRESHOLD").or_else(|| env("JLI_CONFIDENCE")) {
        return parse_threshold(&from_env);
    }
    match brain.setting("confidence")?.filter(|stored| !stored.is_empty()) {
        Some(stored) => parse_threshold(&stored),
        None => Ok(DEFAULT_THRESHOLD),
    }
}

/// Flag, then JLI_USER_ID, then the default user.
fn resolve_user(flags: &Flags, lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    match (&flags.user_id, lookup("JLI_USER_ID")) {
        (Some(user), _) => Ok(user.clone()),
        (None, Some(from_env)) => user_id(&from_env),
        (None, None) => Ok(DEFAULT_USER.to_string()),
    }
}

/// An on/off environment variable: 1 or true (any case) turns it on. Anything else is ignored.
fn switched_on(lookup: impl Fn(&str) -> Option<String>, name: &str) -> bool {
    lookup(name).is_some_and(|value| matches!(value.trim().to_lowercase().as_str(), "1" | "true"))
}

struct Parsed {
    flags: Flags,
    words: Vec<String>,
    rest: Vec<String>,
}

fn parse(argv: &[String]) -> Result<Parsed> {
    let mut flags = Flags::default();
    let mut words = Vec::new();
    let separator = argv.iter().position(|arg| arg == "--");
    let (head, rest) = match separator {
        Some(at) => (&argv[..at], argv[at + 1..].to_vec()),
        None => (argv, vec![]),
    };
    for arg in head {
        match arg.as_str() {
            "-n" | "--dry-run" => flags.dry_run = true,
            "-y" | "--yes" => flags.yes = true,
            "-Y" | "--yolo" => flags.yolo = true,
            "-q" | "--quiet" => flags.quiet = true,
            "--json" => flags.json = true,
            "--agent" => flags.agent = true,
            _ => {
                if let Some(value) = arg.strip_prefix("--confidence=") {
                    flags.threshold = Some(parse_threshold(value)?);
                } else if let Some(value) = arg.strip_prefix("--mode=") {
                    flags.mode = Some(parse_mode(value)?);
                } else if let Some(value) = arg.strip_prefix("--user-id=") {
                    flags.user_id = Some(user_id(value)?);
                } else if arg == "--user-id" {
                    return Err(anyhow!("--user-id needs a value: --user-id=<id>"));
                } else if let Some(risk) = arg.strip_prefix("--risk=").and_then(Risk::parse) {
                    flags.risk = Some(risk);
                } else {
                    words.push(arg.clone());
                }
            }
        }
    }
    Ok(Parsed { flags, words, rest })
}

fn say(text: impl std::fmt::Display) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{text}");
}

/// Say why nothing was done, as a sentence or, with `--json`, as an error object. Exit code 1.
fn refuse(flags: &Flags, code: &str, message: &str) -> i32 {
    if flags.json {
        let value = serde_json::json!({ "error": code, "errorMessage": message });
        say(serde_json::to_string_pretty(&value).unwrap_or_default());
    } else {
        say(message);
    }
    1
}

/// What `jli fix` was asked to do with the route it corrects.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FixHow {
    /// Unlearn it and ask the agent again.
    Unlearn,
    /// Unlearn it and ask the agent again, with these words about what was expected.
    Expected(String),
    /// It should have run this.
    Command(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fix {
    /// The event to correct; none for the last route run here.
    pub(crate) event: Option<i64>,
    pub(crate) how: FixHow,
    /// Run the correction instead of returning a plan of it.
    pub(crate) run: bool,
}

fn event_id(word: &str) -> Option<i64> {
    word.bytes().all(|byte| byte.is_ascii_digit()).then(|| word.parse().ok()).flatten()
}

/// `fix <eventId> [--run] [--expected <words>] [-- <command>]`, or None when the words are not that shape.
fn fix_shape(others: &[String], rest: &[String]) -> Option<Fix> {
    let (first, after) = others.split_first()?;
    let event = Some(event_id(first)?);
    let run = after.iter().any(|word| word == "--run");
    let words: Vec<&String> = after.iter().filter(|word| *word != "--run").collect();
    let expected = match words.as_slice() {
        [] => None,
        [flag, said @ ..] if *flag == "--expected" && !said.is_empty() => {
            Some(said.iter().map(|word| word.as_str()).collect::<Vec<_>>().join(" "))
        }
        [one] => Some(one.strip_prefix("--expected=").filter(|said| !said.is_empty())?.to_string()),
        _ => return None,
    };
    let how = match (expected, rest.is_empty()) {
        (None, true) => FixHow::Unlearn,
        (Some(words), true) => FixHow::Expected(words),
        (None, false) => FixHow::Command(split_steps(&rest.join(" "))),
        (Some(_), false) => return None,
    };
    Some(Fix { event, how, run })
}

/// The plans `jli run` was given, and the values for their missing slots.
struct RunArgs {
    ids: Vec<i64>,
    values: Vec<(String, String)>,
}

/// `run <planId> [<planId> …] [name=value …]`, or None when the words are not that shape.
fn run_shape(others: &[String]) -> Option<RunArgs> {
    let ids: Vec<i64> = others.iter().map_while(|word| event_id(word)).collect();
    let values: Option<Vec<(String, String)>> = others[ids.len()..].iter().map(|word| plan::supplied(word)).collect();
    (!ids.is_empty()).then_some(())?;
    Some(RunArgs { ids, values: values? })
}

fn cwd() -> Result<PathBuf> {
    std::env::current_dir().map_err(|error| anyhow!("cannot read the working directory: {error}"))
}

/// Everything a subcommand needs: the brain, the models, and the one stdin reader.
pub(crate) struct App {
    pub(crate) brain: Brain,
    pub(crate) models: Models,
    pub(crate) terminal: Terminal,
    /// The `.jlirc` the brain was opened for, if any.
    pub(crate) project: Option<Found>,
}

impl App {
    fn ask(&mut self, query: &str, flags: &Flags, rejected: Vec<String>) -> Result<Outcome> {
        self.ask_in(query, flags, rejected, None, cwd()?)
    }

    /// Route a request from `cwd`, with what the person said they expected, if they said anything.
    fn ask_in(
        &mut self,
        query: &str,
        flags: &Flags,
        rejected: Vec<String>,
        clarification: Option<String>,
        cwd: PathBuf,
    ) -> Result<Outcome> {
        let ui = Ui::new(!flags.quiet && !flags.json);
        let threshold = resolve_threshold(&self.brain, flags).map_err(configuration)?;
        let options = RouteOptions {
            yes: flags.yes,
            yolo: flags.yolo,
            dry_run: flags.dry_run,
            capture: flags.json,
            agent_only: flags.agent,
            rejected,
            clarification,
            hand_off: HandOff::from_env(env),
            ..RouteOptions::new(cwd, threshold, resolve_mode(&self.brain, flags).map_err(configuration)?)
        };
        let outcome = route(&self.brain, &self.models, query, &ui, &mut self.terminal, &options)?;
        if flags.json {
            say(serde_json::to_string_pretty(&outcome)?);
        }
        Ok(outcome)
    }

    fn show_skills(&self) -> Result<()> {
        let skills = self.brain.skills()?;
        let shared_from = self.brain.shared_from()?;
        // Inside a project each skill says whose it is (ADR 17); outside one the listing is as it was.
        let own: Vec<String> = self.brain.project_skills()?.into_iter().map(|skill| skill.label).collect();
        let lessons = self.brain.project_lessons()?;
        let rewritten = self.brain.project_scenarios()?;
        say(format!(
            "{} {}\n",
            c::bold(format!("{} skills", skills.len())),
            c::dim(format!("in {}", self.brain.location()))
        ));
        for skill in skills {
            let origin = match skill.origin {
                crate::types::Origin::Seed => c::dim("seed"),
                crate::types::Origin::Agent => c::magenta("learned from agent"),
                crate::types::Origin::Taught => c::cyan("taught"),
            };
            let shared = shared_from.get(&skill.label).map(|from| format!("  {}", c::dim(format!("shared by {from}"))));
            let scope = match self.brain.project() {
                Some(project) if own.contains(&skill.label) => format!("  {}", c::cyan(format!("project {project}"))),
                Some(_) => {
                    let learned = lessons.iter().find(|(label, _, _)| *label == skill.label);
                    let learned = learned.map(|(_, added, removed)| format!(", +{added} −{removed} learned here"));
                    let own = if rewritten.contains(&skill.label) { ", scenario rewritten here" } else { "" };
                    format!("  {}", c::dim(format!("global{}{own}", learned.unwrap_or_default())))
                }
                None => String::new(),
            };
            say(format!(
                "{}  {}  {origin}{}{scope}  {}",
                c::bold(&skill.label),
                risk_tag(skill.risk),
                shared.unwrap_or_default(),
                c::dim(format!("used {}×", skill.uses))
            ));
            for step in &skill.steps {
                say(format!("  {} {step}", c::green("$")));
            }
            for slot in skill.slots.iter().filter(|slot| !slot.aliases.is_empty()) {
                say(format!("  {}", c::dim(format!("{{{}}} is also {}", slot.name, slot.aliases.join(", ")))));
            }
            say(format!("  {}", c::dim(&skill.scenario)));
            for example in &skill.examples {
                say(format!("  {} \"{example}\"", c::green("+")));
            }
            for example in &skill.counter_examples {
                say(format!("  {} \"{example}\"", c::red("−")));
            }
            say("");
        }
        if self.brain.project().is_some() {
            let global = self.brain.global_skills()?;
            let hidden: Vec<&str> =
                global.iter().map(|skill| skill.label.as_str()).filter(|label| own.iter().any(|own| own == label)).collect();
            if !hidden.is_empty() {
                say(c::dim(format!("Hidden here by a project skill of the same name: the global {}", hidden.join(", "))));
            }
        }
        Ok(())
    }

    fn show_history(&self) -> Result<()> {
        let mut events = self.brain.events(25)?;
        events.reverse();
        if events.is_empty() {
            say("No requests yet.");
            return Ok(());
        }
        for event in events {
            let ms = event.ms_system1.unwrap_or(0) + event.ms_system2.unwrap_or(0);
            let when: String = event.ts.chars().take(16).collect::<String>().replacen('T', " ", 1);
            let learned = event.learned.as_ref().map(|learned| format!("  {}", c::green(format!("✚ {learned}"))));
            say(format!(
                "{}  {}  {:>7}  \"{}\" {} {}{}",
                c::dim(when),
                path_tag(event.path),
                duration(ms.max(0) as u64),
                event.query,
                c::dim("→"),
                event.command.clone().unwrap_or_else(|| c::dim("—")),
                learned.unwrap_or_default()
            ));
        }
        Ok(())
    }

    fn show_stats(&self) -> Result<()> {
        let skills = self.brain.skills()?;
        let mut newest_first = self.brain.events(500)?;
        newest_first.reverse();
        let events: Vec<BrainEvent> = newest_first
            .into_iter()
            .filter(|event| event.label.is_some() && event.learned.as_deref() != Some("corrected"))
            .collect();
        let learned = skills.iter().filter(|skill| skill.origin != crate::types::Origin::Seed).count();
        let phrasings: usize = skills.iter().map(|skill| skill.examples.len()).sum();
        say(format!(
            "{} {} {}",
            c::bold("Brain"),
            c::dim(self.brain.location()),
            if self.brain.is_shared() { c::green("shared") } else { c::dim("local only") }
        ));
        say(format!(
            "  {} skills ({learned} learned, {} seeded), {phrasings} learned phrasings",
            skills.len(),
            skills.len() - learned
        ));
        if events.is_empty() {
            say("\nNo routed requests yet. Try: jli show me the newest files");
            return Ok(());
        }

        let total_ms = |event: &&BrainEvent| (event.ms_system1.unwrap_or(0) + event.ms_system2.unwrap_or(0)) as f64;
        let fast: Vec<&BrainEvent> = events.iter().filter(|event| event.path == Path::System1).collect();
        let slow: Vec<&BrainEvent> = events.iter().filter(|event| event.path == Path::System2).collect();
        let taught = events.iter().filter(|event| matches!(event.path, Path::Taught | Path::System1Confirmed)).count();
        say(format!("\n{} over {} requests", c::bold("Routing"), events.len()));
        for (name, count) in [("system 1, instant", fast.len()), ("taught by you", taught), ("system 2, agent", slow.len())]
        {
            say(format!("  {name:<22} {} {count:>4}", bar(count as f64 / events.len() as f64)));
        }
        let mean = |group: &[&BrainEvent]| {
            if group.is_empty() {
                0.0
            } else {
                group.iter().map(total_ms).sum::<f64>() / group.len() as f64
            }
        };
        say(format!("\n{}", c::bold("Latency")));
        say(format!("  system 1  {} on average", duration(mean(&fast).round() as u64)));
        if !slow.is_empty() {
            say(format!("  system 2  {} on average", duration(mean(&slow).round() as u64)));
        }

        let buckets = events.len().min(12);
        let size = events.len() as f64 / buckets as f64;
        let curve: Vec<f64> = (0..buckets)
            .map(|i| {
                let chunk = &events[(i as f64 * size).floor() as usize..((i + 1) as f64 * size).floor() as usize];
                let instant = chunk.iter().filter(|event| event.path == Path::System1).count();
                instant as f64 / chunk.len().max(1) as f64
            })
            .collect();
        say(format!("\n{} {}", c::bold("Learning curve"), c::dim("share of requests answered instantly, oldest to newest")));
        say(format!("  {}  {}% now", c::cyan(sparkline(&curve)), (curve.last().copied().unwrap_or(0.0) * 100.0).round()));
        self.show_correctness()?;

        let mut seen = HashSet::new();
        let hosts: Vec<&str> = skills
            .iter()
            .map(|skill| skill.created_host.as_str())
            .chain(events.iter().map(|event| event.host.as_str()))
            .filter(|host| !host.is_empty() && seen.insert(*host))
            .collect();
        say(format!("\n{} {}", c::bold("Hosts"), hosts.join(", ")));
        Ok(())
    }

    /// Whether the routes were right, beside how fast they were: the learning curve above counts a
    /// confident wrong route as a win. Declined and corrected attempts stay in the count.
    fn show_correctness(&self) -> Result<()> {
        let mut events = self.brain.events(500)?;
        events.reverse();
        let tally = eval::correctness(&events);
        let of = |part: usize, whole: usize| format!("{}%", (100.0 * part as f64 / whole.max(1) as f64).round());
        say(format!("\n{} {}", c::bold("Correctness"), c::dim(format!("over {} routed requests", tally.requests))));
        let rows = [
            ("right the first time", tally.first_try),
            ("declined or corrected", tally.turned_down),
            ("ran and failed", tally.failed),
            ("never judged", tally.blocked),
        ];
        for (name, count) in rows {
            say(format!("  {name:<22} {} {count:>4}", bar(count as f64 / tally.requests.max(1) as f64)));
        }
        if tally.system1_routes > 0 {
            say(format!(
                "  {} of System 1's {} routes still called a language model ({})",
                tally.system1_with_model,
                tally.system1_routes,
                of(tally.system1_with_model, tally.system1_routes)
            ));
        }
        say(format!("\n{} {}", c::bold("Evaluation"), c::dim("share of stored phrasings that route home, from jli eval")));
        for line in eval::render_trend(&eval::trend(&self.brain)?) {
            say(line);
        }
        Ok(())
    }

    /// Inside a project what is taught is the project's, unless `global` says it is for every project.
    fn teach(&mut self, phrase: &str, command: &[String], flags: &Flags, global: bool) -> Result<()> {
        let steps = split_steps(&command.join(" "));
        let input = LearnInput {
            phrase: phrase.to_string(),
            steps,
            risk: flags.risk,
            mode: resolve_mode(&self.brain, flags)?,
            threshold: resolve_threshold(&self.brain, flags)?,
            cwd: cwd()?,
        };
        let (models, ui) = (&self.models, Ui::new(!flags.quiet));
        let teach = |brain: &Brain| -> Result<_> {
            let learned = learn_phrase(brain, models, &input, &ui)?;
            let skill = brain.skill(&learned.label)?;
            Ok((learned, skill))
        };
        let scope = match self.brain.project() {
            Some(_) if global => " · global, for every project".to_string(),
            Some(project) => format!(" · kept for project {project}"),
            None => String::new(),
        };
        let (learned, skill) = if global { self.brain.globally(teach)? } else { teach(&self.brain)? };
        let runs = skill.as_ref().map(|skill| skill.steps.join(" && ")).unwrap_or_else(|| "undefined".into());
        say(format!(
            "{} \"{phrase}\" means {} {}",
            c::green("✚ learned"),
            c::bold(&learned.label),
            c::dim(format!("({runs})"))
        ));
        say(c::dim(if learned.created {
            format!("  new skill{scope}")
        } else if learned.upgraded {
            let (count, takes) = skill.map_or((1, String::new()), |skill| {
                let names: Vec<String> = skill.slots.iter().map(|slot| format!("{{{}}}", slot.name)).collect();
                (skill.examples.len(), names.join(", "))
            });
            format!("  added to an existing skill, which now takes {takes} and has {count} phrasings{scope}")
        } else {
            let count = skill.map_or(1, |skill| skill.examples.len());
            format!("  added to an existing skill, which now has {count} phrasings{scope}")
        }));
        Ok(())
    }

    /// Inside a project only the project's own skills are forgotten, unless `global` says otherwise:
    /// a global skill is every project's.
    fn forget(&mut self, label: &str, global: bool) -> Result<i32> {
        let Some(project) = self.brain.project().map(String::from) else {
            say(if self.brain.forget(label)? { format!("Forgot {label}.") } else { format!("No skill called {label}.") });
            return Ok(0);
        };
        if global {
            let forgotten = self.brain.globally(|brain| brain.forget(label))?;
            say(if forgotten {
                format!("Forgot the global {label}, in every project.")
            } else {
                format!("No global skill called {label}.")
            });
            return Ok(0);
        }
        if self.brain.forget(label)? {
            let back = self.brain.global_skills()?.iter().any(|skill| skill.label == label);
            let back = if back { format!(" The global {label} is used here again.") } else { String::new() };
            say(format!("Forgot {label} in project {project}.{back}"));
            return Ok(0);
        }
        if self.brain.global_skills()?.iter().any(|skill| skill.label == label) {
            say(format!(
                "{label} is a global skill, used in every project, so it was not forgotten. To forget it everywhere: jli forget --global {label}"
            ));
            return Ok(1);
        }
        say(format!("No skill called {label}."));
        Ok(0)
    }

    fn show_project(&self) -> Result<i32> {
        let Some(found) = &self.project else {
            say("No project here: there is no .jlirc in this directory or any above it. What is learned here is global.");
            say(c::dim("Make this directory a project with: jli project init"));
            return Ok(0);
        };
        let own = self.brain.project_skills()?.len();
        let lessons = self.brain.project_lessons()?.len();
        say(format!("{} {}", c::bold("Project"), c::bold(&found.id)));
        say(format!("  from {}", found.path.display()));
        say(format!(
            "  {own} skill{} scoped to it, and lessons about {lessons} global skill{}",
            if own == 1 { "" } else { "s" },
            if lessons == 1 { "" } else { "s" }
        ));
        let rewritten = self.brain.project_scenarios()?.len();
        if rewritten > 0 {
            say(format!(
                "  its own scenario for {rewritten} global skill{}, from jli eval --optimise",
                if rewritten == 1 { "" } else { "s" }
            ));
        }
        say(c::dim("  What is learned here stays with this project; your global skills are used here too. See jli skills."));
        Ok(0)
    }

    fn init_project(&self, id: Option<&str>) -> Result<i32> {
        let here = cwd()?;
        let path = here.join(project::FILE);
        if path.exists() {
            let current = std::fs::read_to_string(&path).ok().and_then(|text| project::parse(&text).ok());
            say(match current {
                Some(id) => format!("There is already a {} here, for project {id}. Nothing was changed.", project::FILE),
                None => format!("There is already a {} here. Nothing was changed.", project::FILE),
            });
            return Ok(1);
        }
        let made = project::init(&here, id)?;
        say(format!(
            "Created {} for project {}. What jli learns here from now on stays with this project.",
            made.path.display(),
            c::bold(&made.id)
        ));
        if let Some(outer) = &self.project {
            say(c::dim(format!(
                "  This directory was in project {} ({}); from now on it is its own project.",
                outer.id,
                outer.path.display()
            )));
        }
        say(c::dim("  The file holds only the id, so it is safe to commit: every checkout is then the same project."));
        Ok(0)
    }

    fn generalize(&mut self, others: &[String], flags: &Flags) -> Result<i32> {
        let mut options = GeneralizeOptions {
            label: None,
            all_projects: false,
            dry_run: flags.dry_run,
            approve_all: false,
            threshold: resolve_threshold(&self.brain, flags)?,
            cwd: cwd()?,
        };
        for word in others {
            match word.as_str() {
                "--all-projects" => options.all_projects = true,
                "--approve-all" => options.approve_all = true,
                label if options.label.is_none() && !label.starts_with('-') => options.label = Some(label.to_string()),
                _ => {
                    say("Usage: jli generalize [<label>] [--all-projects] [--approve-all] [-n]");
                    return Ok(1);
                }
            }
        }
        let ui = Ui::new(!flags.quiet);
        let done = generalize(&self.brain, &self.models, &options, &ui, &mut self.terminal)?;
        Ok(if done.nothing { 1 } else { 0 })
    }

    fn share(&self, label: &str, to: &str) -> Result<i32> {
        let to = user_id(to)?;
        let (message, code) = match self.brain.share(label, &to)? {
            Shared::Copied => {
                // A project skill goes to the other user's copy of the same project (ADR 17).
                let into = match self.brain.project() {
                    Some(project) if self.brain.project_skills()?.iter().any(|skill| skill.label == label) => {
                        format!(", in project {project}")
                    }
                    _ => String::new(),
                };
                (format!("Shared {label} with {to}{into}. It is a copy: what either of you teaches it from now on stays yours."), 0)
            }
            Shared::NoSuchSkill => (format!("No skill called {label}."), 1),
            Shared::SameUser => (format!("{label} is already yours: you are {to}."), 1),
            Shared::LabelTaken => (
                format!("{to} already has a skill called {label}, so nothing was shared. They can drop theirs with: jli --user-id={to} forget {label}"),
                1,
            ),
            Shared::AlreadyRuns(theirs) => {
                (format!("{to} already runs the same command as {theirs}, so nothing was shared."), 1)
            }
        };
        say(message);
        Ok(code)
    }

    /// `jli alias` and `jli forget-phrase` (ADR 21): a sentence, or one JSON object; a refusal exits 1.
    fn hand_edit(&mut self, flags: &Flags, edit: impl FnOnce(&mut Brain) -> Result<hand::Edited>) -> Result<i32> {
        let done = edit(&mut self.brain)?;
        if let Some(code) = done.refused {
            return Ok(refuse(flags, code, &done.message));
        }
        say(if flags.json { serde_json::to_string_pretty(&done.json)? } else { done.message });
        Ok(0)
    }

    /// `jli fix`: bare, the last route run here; with an event id, that event, from anywhere.
    pub(crate) fn fix(&mut self, flags: &Flags, fix: Fix) -> Result<i32> {
        let here = cwd()?;
        let found = match fix.event {
            // The same text the router logs, so this directory's own events are the ones found.
            None => self.brain.last_executed(&here.to_string_lossy())?,
            Some(id) => self.brain.event(id)?,
        };
        let ran = found.as_ref().filter(|event| event.accepted);
        let Some((last, label)) = ran.and_then(|event| event.label.clone().map(|label| (event.clone(), label))) else {
            return Ok(match (fix.event, found) {
                (None, _) => refuse(
                    flags,
                    "nothing_to_fix",
                    "Nothing to fix: no command has been run in this directory on this host yet.",
                ),
                (Some(id), None) => refuse(flags, "event_not_found", &format!("Nothing to fix: you have no event {id}.")),
                (Some(id), Some(_)) => refuse(
                    flags,
                    "event_not_run",
                    &format!("Nothing to fix: event {id} ran no command, or was corrected already."),
                ),
            });
        };
        // Route again where it ran, if that directory is still there.
        let at = Some(PathBuf::from(&last.cwd)).filter(|dir| dir.is_dir()).unwrap_or(here);
        if let FixHow::Command(steps) = &fix.how {
            return self.fix_with_command(flags, &last, &label, steps, fix.run, at);
        }

        if flags.dry_run {
            eprintln!("{}\n", c::dim(format!("  Dry run: would unlearn that \"{}\" means {label}.", last.query)));
        } else {
            // What was learned is the request with its values hidden: the stated ones (ADR 11), and every
            // value the command ran with (ADR 13), read back from the command. Older rows hold less hidden.
            let (steps, slots) = self.brain.skill(&label)?.map(|skill| (skill.steps, skill.slots)).unwrap_or_default();
            let stated = args::phrase(&last.query, &slots);
            let phrase = learned_phrase(&last.query, &steps, &slots, last.command.as_deref());
            self.brain.remove_example(&label, &last.query)?;
            self.brain.remove_example(&label, &stated)?;
            self.brain.remove_example(&label, &phrase)?;
            self.brain.add_counter_example(&label, &phrase)?;
            self.brain.mark_rejected(last.id)?;
            eprintln!("{} \"{}\" does {} mean {label}. Trying again.\n", c::green("✚ learned"), last.query, c::bold("not"));
        }
        let (outcome, how, detail) = match fix.how {
            FixHow::Expected(words) => {
                // What they expected goes to the agent, and the answer is a plan unless they asked to run it.
                let again =
                    Flags { agent: true, dry_run: flags.dry_run || !fix.run, yes: flags.yes || fix.run, ..flags.clone() };
                (self.ask_in(&last.query, &again, vec![label], Some(words.clone()), at)?, "expected", Some(words))
            }
            _ => {
                let again = Flags { agent: true, ..flags.clone() };
                (self.ask_in(&last.query, &again, vec![label], None, at)?, "unlearn", None)
            }
        };
        if !flags.dry_run {
            self.log_correction(&last, how, detail, outcome.event_id)?;
        }
        Ok(outcome.exit_code())
    }

    /// `jli fix <eventId> -- <command>`: that was wrong, this is right. It teaches what a "no" followed
    /// by the command at the prompt teaches, then returns a plan of the command, or runs it with `--run`.
    fn fix_with_command(
        &mut self,
        flags: &Flags,
        last: &BrainEvent,
        label: &str,
        steps: &[String],
        run: bool,
        at: PathBuf,
    ) -> Result<i32> {
        let command = steps.join(" && ");
        if flags.dry_run {
            eprintln!("{}\n", c::dim(format!("  Dry run: would learn that \"{}\" means {command}.", last.query)));
            if flags.json {
                let mut outcome = Outcome::new(&last.query);
                outcome.path = Path::Taught;
                outcome.commands = steps.to_vec();
                outcome.blocked = Some(DRY_RUN.into());
                outcome.blocked_code = Some(DRY_RUN_CODE.into());
                say(serde_json::to_string_pretty(&outcome)?);
            }
            return Ok(0);
        }
        let ui = Ui::new(!flags.quiet && !flags.json);
        let options = RouteOptions::new(
            at.clone(),
            resolve_threshold(&self.brain, flags).map_err(configuration)?,
            resolve_mode(&self.brain, flags).map_err(configuration)?,
        );
        let wrong = self.brain.skill(label)?;
        let (wrong_steps, slots) =
            wrong.as_ref().map(|skill| (skill.steps.clone(), skill.slots.clone())).unwrap_or_default();
        let phrase = learned_phrase(&last.query, &wrong_steps, &slots, last.command.as_deref());
        if let Some(wrong) = wrong.as_ref().filter(|wrong| is_another_command(&wrong.steps, steps)) {
            unlearn_phrase(&self.brain, &ui, wrong, &phrase)?;
        }
        let taught = teach_instead(&self.brain, &self.models, &ui, &phrase, steps, &options)?;
        self.brain.mark_rejected(last.id)?;

        let skill = self.brain.skill(&taught.label)?;
        let record = PlanRecord {
            query: last.query.clone(),
            phrase,
            path: Path::Taught,
            confidence: None,
            label: taught.label.clone(),
            is_new: false,
            steps: skill.as_ref().map_or_else(|| steps.to_vec(), |skill| skill.steps.clone()),
            scenario: skill.as_ref().map(|skill| skill.scenario.clone()).unwrap_or_default(),
            skill_risk: skill.as_ref().map_or(Risk::Modifies, |skill| skill.risk),
            slots: skill.as_ref().map(|skill| skill.slots.clone()).unwrap_or_default(),
            risk: skill.as_ref().map_or(Risk::Modifies, |skill| skill.risk),
            commands: steps.to_vec(),
            values: vec![],
            missing: vec![],
            unknown_arguments: vec![],
            aliases: vec![],
            ms_system1: 0,
            ms_system2: 0,
        };
        let id = plan::save(&self.brain, &at, &record)?;
        let outcome = if run {
            let ran = plan::run_plans(
                &self.brain,
                &self.models,
                &[id],
                &[],
                &ui,
                &mut self.terminal,
                RunFlags { yolo: flags.yolo, capture: flags.json },
            )?;
            ran.into_iter().next().unwrap_or_else(|| Outcome::new(&last.query))
        } else {
            ui.note(format!("Not run. Stored as plan {id}: jli run {id} runs it, with no model call."));
            let mut outcome = plan::outcome_of(id, &record);
            outcome.blocked = Some(DRY_RUN.into());
            outcome.blocked_code = Some(DRY_RUN_CODE.into());
            outcome
        };
        let mut outcome = outcome;
        outcome.learned.insert(0, format!("\"{}\" means {}", record.phrase, taught.label));
        self.log_correction(last, "command", Some(command), outcome.event_id)?;
        if flags.json {
            say(serde_json::to_string_pretty(&outcome)?);
        }
        Ok(outcome.exit_code())
    }

    /// Keep who corrected `event`: a correction made with no terminal is an agent's or a script's.
    fn log_correction(&self, event: &BrainEvent, how: &str, detail: Option<String>, result: Option<i64>) -> Result<()> {
        self.brain.log_correction(&Correction {
            event_id: event.id,
            terminal: self.terminal.interactive(),
            how: how.to_string(),
            detail,
            result_event_id: result,
        })
    }

    /// `jli run <planId> …`: each plan as stored, in order, until one fails or is refused.
    fn run_plans(&mut self, ids: &[i64], values: &[(String, String)], flags: &Flags) -> Result<i32> {
        let ui = Ui::new(!flags.quiet && !flags.json);
        let outcomes = plan::run_plans(
            &self.brain,
            &self.models,
            ids,
            values,
            &ui,
            &mut self.terminal,
            RunFlags { yolo: flags.yolo, capture: flags.json },
        )?;
        if flags.json {
            say(serde_json::to_string_pretty(&outcomes)?);
        }
        let stop = outcomes.iter().find(|outcome| !(outcome.ran && outcome.exit_code == Some(0) && outcome.error.is_none()));
        Ok(match stop {
            None => 0,
            Some(outcome) if outcome.error.is_some() => 1,
            Some(outcome) => outcome.exit_code(),
        })
    }

    /// `jli eval …`. Returns None when the words do not fit its shape, so they are a request instead.
    fn measure(&self, args: &[String], flags: &Flags) -> Result<Option<i32>> {
        // The trace and the price go to stderr even with --json: stdout is the report alone.
        let ui = Ui::new(!flags.quiet);
        if args.first().map(String::as_str) == Some("undo") {
            let label = match args {
                [_] => None,
                [_, label] => Some(label.as_str()),
                _ => return Ok(None),
            };
            let undone = eval::undo(&self.brain, label, flags.dry_run)?;
            if flags.json {
                say(serde_json::to_string_pretty(&undone)?);
                return Ok(Some(0));
            }
            let verb = if undone.dry_run { "Would put back" } else { "Put back" };
            let here = undone.project.as_ref().map(|project| format!(" in project {project}")).unwrap_or_default();
            say(format!("{verb} the scenario of {}{here}: {}", c::bold(&undone.label), undone.scenario));
            for example in &undone.examples {
                say(format!("  {} \"{example}\" {}", c::green("+"), c::dim("as an example again")));
            }
            for ce in &undone.counter_examples {
                say(format!(
                    "  {} \"{}\" {}",
                    c::red("−"),
                    ce.phrase,
                    c::dim(format!("no longer a counter-example of {}", ce.label))
                ));
            }
            return Ok(Some(0));
        }

        let number = |text: &str| text.trim().parse::<usize>().ok().filter(|n| *n > 0);
        let (mut only, mut limit, mut optimise) = (None, None, false);
        let mut bounds = eval::Bounds { max_skills: 3, max_calls: 400 };
        let mut words = args.iter();
        while let Some(word) = words.next() {
            let value = |prefix: &str| word.strip_prefix(prefix).and_then(number);
            match word.as_str() {
                "--optimise" | "--optimize" => optimise = true,
                "--limit" => match words.next().and_then(|next| number(next)) {
                    Some(n) => limit = Some(n),
                    None => return Ok(None),
                },
                _ if value("--limit=").is_some() => limit = value("--limit="),
                _ if value("--max-skills=").is_some() => bounds.max_skills = value("--max-skills=").unwrap_or(3),
                _ if value("--max-calls=").is_some() => bounds.max_calls = value("--max-calls=").unwrap_or(400),
                label if only.is_none() && self.brain.skill(label)?.is_some() => only = Some(label.to_string()),
                _ => return Ok(None),
            }
        }
        let options = eval::EvalOptions {
            cwd: cwd()?,
            threshold: resolve_threshold(&self.brain, flags)?,
            only,
            limit,
            dry_run: flags.dry_run,
            optimise: optimise.then_some(bounds),
        };
        let report = eval::run(&self.brain, &self.models, &options, &ui)?;
        say(if flags.json { serde_json::to_string_pretty(&report)? } else { eval::render(&report) });
        Ok(Some(0))
    }

    fn repl(&mut self, flags: &Flags) -> Result<()> {
        eprintln!(
            "{} {}",
            c::bold("jli"),
            c::dim(format!(
                "· brain: {} · confident at {} · ctrl-d to leave",
                self.brain.location(),
                resolve_threshold(&self.brain, flags)?
            ))
        );
        loop {
            eprint!("{}", c::cyan("\n› "));
            let _ = std::io::stderr().flush();
            let Some(line) = self.terminal.read_line() else { break };
            let query = line.trim();
            if query == "exit" || query == "quit" {
                break;
            }
            if query.is_empty() {
                continue;
            }
            if let Err(error) = self.ask(query, flags, vec![]) {
                eprintln!("{}", c::red(format!("{error:#}")));
            }
        }
        eprintln!();
        Ok(())
    }

    fn config(&self, others: &[String], flags: &Flags) -> Result<Option<i32>> {
        let key = others.first().map(String::as_str);
        let value = others.get(1).map(String::as_str);
        match (key, value) {
            (Some("confidence"), Some(value)) => {
                self.brain.set_setting("confidence", &parse_threshold(value)?.to_string())?;
                say(format!("Confidence threshold saved in the brain: {value}"));
            }
            (Some("mode"), Some(value)) => {
                self.brain.set_setting("mode", parse_mode(value)?.as_str())?;
                say(format!("Learning mode saved in the brain: {value}"));
            }
            (None | Some("confidence" | "mode"), _) => {
                say(format!(
                    "confidence  {}  {}",
                    resolve_threshold(&self.brain, flags)?,
                    c::dim("System 1's pick is used at or above this; below it the agent is asked")
                ));
                say(format!(
                    "mode        {}  {}",
                    resolve_mode(&self.brain, flags)?,
                    c::dim("manual: stored as taught · semi: a fast model generalises what you teach · auto: also learns from a no or a failed command")
                ));
                say(c::dim("\nSet with: jli config confidence 0.7 · jli config mode auto\nOr per shell with JLI_THRESHOLD / JLI_MODE, or per run with --confidence= / --mode="));
            }
            _ => return Ok(None),
        }
        Ok(Some(0))
    }

    fn dispatch(&mut self, flags: &Flags, words: &[String], rest: &[String]) -> Result<i32> {
        let Some((first, others)) = words.split_first() else {
            if !self.terminal.interactive() {
                say(help());
                return Ok(0);
            }
            self.repl(flags)?;
            return Ok(0);
        };
        if others.is_empty() && rest.is_empty() {
            match first.as_str() {
                "skills" => return self.show_skills().map(|_| 0),
                "stats" => return self.show_stats().map(|_| 0),
                "history" => return self.show_history().map(|_| 0),
                "fix" => return self.fix(flags, Fix { event: None, how: FixHow::Unlearn, run: false }),
                "sync" => {
                    if self.brain.sync()? {
                        say(format!("Synced with {}", self.brain.config.sync_url.as_deref().unwrap_or_default()));
                    } else {
                        say("No JLI_SYNC_URL set: this brain is not a replica.");
                    }
                    return Ok(0);
                }
                _ => {}
            }
        }
        if first == "config" && rest.is_empty() && others.len() <= 2 {
            if let Some(code) = self.config(others, flags)? {
                return Ok(code);
            }
        }
        if first == "share" && rest.is_empty() && others.iter().any(|word| word == "--to" || word.starts_with("--to=")) {
            let target = match others {
                [label, to, user] if to == "--to" => Some((label, user.clone())),
                [label, to] => to.strip_prefix("--to=").map(|user| (label, user.to_string())),
                _ => None,
            };
            let Some((label, to)) = target else {
                say("Usage: jli share <label> --to <user>");
                return Ok(1);
            };
            return self.share(label, &to);
        }
        if first == "run" && rest.is_empty() {
            if let Some(given) = run_shape(others) {
                if flags.dry_run {
                    // -n promises nothing runs; a plan's preview is the outcome it was made with.
                    let why = "jli run runs plans, so -n cannot apply: the outcome that made a plan is its preview";
                    return Ok(refuse(flags, USAGE, why));
                }
                return self.run_plans(&given.ids, &given.values, flags);
            }
        }
        if first == "fix" {
            if let Some(fix) = fix_shape(others, rest) {
                return self.fix(flags, fix);
            }
        }
        // `--global` reaches the user's global skills from inside a project (ADR 17).
        let global = others.iter().any(|word| word == "--global");
        let plain: Vec<String> = others.iter().filter(|word| *word != "--global").cloned().collect();
        if first == "forget" && plain.len() == 1 {
            return self.forget(&plain[0], global);
        }
        if first == "alias" && rest.is_empty() {
            match hand::alias_shape(others, flags.dry_run) {
                Some(edit) => return self.hand_edit(flags, |brain| hand::alias(brain, &edit)),
                None if others.iter().any(|word| word == "--remove" || word == "--global") => {
                    return Ok(refuse(flags, USAGE, "Usage: jli alias [--remove] [--global] <label> <slot> <name>"))
                }
                None => {}
            }
        }
        if first == "forget-phrase" {
            return match hand::phrase_shape(others, flags.dry_run).filter(|_| rest.is_empty()) {
                Some(edit) => self.hand_edit(flags, |brain| hand::forget_phrase(brain, &edit)),
                None => Ok(refuse(flags, USAGE, "Usage: jli forget-phrase [--global] <label> \"<phrase>\"")),
            };
        }
        if first == "eval" && rest.is_empty() {
            if let Some(code) = self.measure(others, flags)? {
                return Ok(code);
            }
        }
        if first == "teach" {
            if plain.is_empty() || rest.is_empty() {
                say("Usage: jli teach [--global] \"<phrase>\" [--risk=safe|modifies|destructive] -- <command>");
                return Ok(1);
            }
            self.teach(&plain.join(" "), rest, flags, global)?;
            return Ok(0);
        }
        if first == "project" && rest.is_empty() && !global {
            match plain.as_slice() {
                [] => return self.show_project(),
                [init] if init == "init" => return self.init_project(None),
                [init, id] if init == "init" => return self.init_project(Some(id)),
                _ => {}
            }
        }
        if first == "generalize" && rest.is_empty() && !global && plain.len() <= 3 {
            return self.generalize(&plain, flags);
        }
        let query: Vec<&str> = words.iter().chain(rest).map(String::as_str).collect();
        Ok(self.ask(&args::request_from(&query), flags, vec![])?.exit_code())
    }
}

fn path_tag(path: Path) -> String {
    match path {
        Path::System1 => c::cyan("⚡ system 1"),
        Path::System1Confirmed => c::yellow("⚡ confirmed"),
        Path::System2 => c::magenta("◆ system 2"),
        Path::Taught => c::green("✎ taught  "),
        Path::None => c::dim("· nothing "),
    }
}

/// Print the shell function for the named shell, or for `$SHELL`. Needs no brain and no keys.
fn shell_init(name: Option<&str>) -> Result<i32> {
    let names = || Shell::ALL.map(Shell::as_str).join(", ");
    let shell = match name {
        Some(name) => {
            Shell::parse(name).ok_or_else(|| anyhow!("no shell function for {name}: there is one for {}", names()))?
        }
        None => Shell::of_user().ok_or_else(|| {
            anyhow!("cannot tell your shell from $SHELL; name it: jli shell-init <{}>", names().replace(", ", "|"))
        })?,
    };
    say(init_script(shell).trim_end());
    Ok(0)
}

fn run(argv: &[String]) -> Result<i32> {
    let Parsed { mut flags, words, rest } = parse(argv).map_err(|error| coded(USAGE, format!("{error:#}")))?;
    flags.yes |= switched_on(env, "JLI_YES");
    flags.yolo |= switched_on(env, "JLI_YOLO");
    let first = words.first().map(String::as_str);
    if matches!(first, Some("help" | "--help" | "-h")) {
        say(help());
        return Ok(0);
    }
    let user = resolve_user(&flags, env).map_err(configuration)?;
    if first == Some("demo") && words.len() == 1 {
        return run_demo(&user);
    }
    if first == Some("shell-init") && words.len() <= 2 && rest.is_empty() {
        return shell_init(words.get(1).map(String::as_str));
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let project = project::find(&cwd()?, home.as_deref())?;
    let mut app = App {
        brain: Brain::open_in(BrainConfig::from_env(), &user, project.as_ref().map(|found| found.id.as_str()))?,
        models: Models::from_env(),
        terminal: Terminal::new(),
        project,
    };
    let code = app.dispatch(&flags, &words, &rest);
    let _ = app.brain.sync();
    code
}

/// An error as `--json` prints it: one object, a stable code and the message.
fn error_json(error: &anyhow::Error) -> String {
    let value = serde_json::json!({ "error": error_code(error), "errorMessage": format!("{error:#}") });
    serde_json::to_string_pretty(&value).unwrap_or_default()
}

fn configuration(error: anyhow::Error) -> anyhow::Error {
    coded(CONFIGURATION, format!("{error:#}"))
}

/// The `jli` binary: runs and returns the process exit code.
pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Read before anything can fail, so that a caller who asked for JSON gets JSON even for a bad flag.
    let json = argv.iter().take_while(|arg| *arg != "--").any(|arg| arg == "--json");
    match run(&argv) {
        Ok(code) => code,
        Err(error) => {
            if json {
                say(error_json(&error));
            }
            eprintln!("{} {error:#}", c::red("jli:"));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_are_taken_out_of_the_request() {
        let parsed =
            parse(&args(&["-n", "show", "--risk=bad", "files", "--json", "--mode=auto", "--confidence=0.7"])).unwrap();
        assert_eq!(parsed.words, ["show", "--risk=bad", "files"], "an unknown risk stays in the request");
        assert!(parsed.flags.dry_run && parsed.flags.json);
        assert_eq!((parsed.flags.mode, parsed.flags.threshold), (Some(Mode::Auto), Some(0.7)));
        assert!(parse(&args(&["--confidence=2"])).is_err());
        assert!(parse(&args(&["--confidence="])).is_err());
        assert!(parse(&args(&["--mode=wild"])).is_err());

        let teach = parse(&args(&["teach", "last commits", "--risk=safe", "--", "git", "log", "-y"])).unwrap();
        assert_eq!(teach.words, ["teach", "last commits"]);
        assert_eq!(teach.rest, ["git", "log", "-y"], "flags after -- belong to the command");
        assert_eq!(teach.flags.risk, Some(Risk::Safe));
        assert!(!teach.flags.yes);
    }

    #[test]
    fn yes_comes_from_the_flag_or_the_environment() {
        for (flag, yes, yolo) in [("-y", true, false), ("--yes", true, false), ("-Y", false, true), ("--yolo", false, true)]
        {
            let parsed = parse(&args(&[flag, "stage", "everything"])).unwrap();
            assert_eq!((parsed.flags.yes, parsed.flags.yolo), (yes, yolo), "{flag}");
            assert_eq!(parsed.words, ["stage", "everything"]);
        }
        let only_yolo = |name: &str| (name == "JLI_YOLO").then(|| "1".to_string());
        assert!(switched_on(only_yolo, "JLI_YOLO") && !switched_on(only_yolo, "JLI_YES"), "each variable is its own switch");
        let with = |value: Option<&str>| switched_on(|_| value.map(String::from), "JLI_YES");
        for value in ["1", "true", " TRUE ", "True"] {
            assert!(with(Some(value)), "{value} switches it on");
        }
        for value in ["0", "false", "yes", "", "2"] {
            assert!(!with(Some(value)), "{value} does not switch it on");
        }
        assert!(!with(None));
    }

    #[test]
    fn the_user_comes_from_the_flag_then_the_environment() {
        let parsed = parse(&args(&["--user-id=agent", "teach", "x", "--", "ls", "--user-id=other"])).unwrap();
        assert_eq!(parsed.flags.user_id.as_deref(), Some("agent"));
        assert_eq!(parsed.words, ["teach", "x"]);
        assert_eq!(parsed.rest, ["ls", "--user-id=other"], "after -- it belongs to the command");
        assert!(parse(&args(&["--user-id=Agent", "skills"])).is_err());
        assert!(parse(&args(&["--user-id=", "skills"])).is_err());
        assert!(parse(&args(&["--user-id", "agent", "skills"])).is_err(), "never read as part of the request");

        let from_env = |name: &str| (name == "JLI_USER_ID").then(|| "ci".to_string());
        let nothing = |_: &str| None;
        let flagged = Flags { user_id: Some("agent".into()), ..Flags::default() };
        assert_eq!(resolve_user(&flagged, from_env).unwrap(), "agent", "the flag wins");
        assert_eq!(resolve_user(&Flags::default(), from_env).unwrap(), "ci");
        assert_eq!(resolve_user(&Flags::default(), nothing).unwrap(), DEFAULT_USER);
        assert!(resolve_user(&Flags::default(), |_| Some("two words".into())).is_err());
    }

    #[test]
    fn run_and_fix_are_subcommands_only_in_their_own_shape() {
        let run = |words: &[&str]| run_shape(&args(words)).map(|given| (given.ids, given.values));
        assert_eq!(run(&["12", "13"]), Some((vec![12, 13], vec![])));
        assert_eq!(
            run(&["12", "message=fix it", "n=3"]).unwrap().1,
            [("message".into(), "fix it".into()), ("n".into(), "3".into())]
        );
        assert_eq!(run(&["the", "tests"]), None, "jli run the tests is a request");
        assert_eq!(run(&["12", "the", "tests"]), None);
        assert_eq!(run(&["message=x"]), None);

        let fix = |words: &[&str], rest: &[&str]| fix_shape(&args(words), &args(rest));
        let by = |how: FixHow, run: bool| Some(Fix { event: Some(7), how, run });
        assert_eq!(fix(&["7"], &[]), by(FixHow::Unlearn, false));
        assert_eq!(fix(&["7"], &["ls", "-ltr", "&&", "wc", "-l"]), by(FixHow::Command(args(&["ls -ltr", "wc -l"])), false));
        assert_eq!(fix(&["7", "--run"], &["ls"]), by(FixHow::Command(args(&["ls"])), true));
        assert_eq!(fix(&["7", "--expected", "newest", "first"], &[]), by(FixHow::Expected("newest first".into()), false));
        assert_eq!(fix(&["7", "--expected=newest first"], &[]), by(FixHow::Expected("newest first".into()), false));
        assert_eq!(fix(&["7", "--expected", "x"], &["ls"]), None, "one correction at a time");
        assert_eq!(fix(&["the", "parser"], &[]), None, "jli fix the parser is a request");
        assert_eq!(fix(&["7", "please"], &[]), None);
    }

    #[test]
    fn a_dry_run_of_jli_run_is_refused_before_any_plan_is_read() {
        let dir = std::env::temp_dir().join(format!("jli-cli-dry-run-{}-{}", std::process::id(), crate::store::now_iso()));
        let mut app = App {
            brain: Brain::open(BrainConfig::file(&dir.join("brain.db"))).unwrap(),
            models: Models::default(),
            terminal: Terminal::not_a_terminal(),
            project: None,
        };
        let flags = Flags { dry_run: true, quiet: true, ..Flags::default() };
        assert_eq!(app.dispatch(&flags, &args(&["run", "1"]), &[]).unwrap(), 1, "a usage refusal, not plan_not_found's 3");
        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_error_under_json_is_one_object_with_a_kind() {
        let error = coded(USAGE, "mode must be one of manual, semi, auto, got \"wild\"");
        let value: serde_json::Value = serde_json::from_str(&error_json(&error)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "error": "usage", "errorMessage": "mode must be one of manual, semi, auto, got \"wild\"" })
        );
        let parsed = parse(&args(&["--mode=wild"])).map_err(|error| coded(USAGE, format!("{error:#}"))).err().unwrap();
        assert_eq!(error_code(&parsed), "usage");
    }

    #[test]
    fn thresholds_are_numbers_from_0_to_1() {
        assert_eq!(parse_threshold(" 0.25 ").unwrap(), 0.25);
        assert_eq!(parse_threshold("1").unwrap(), 1.0);
        assert!(parse_threshold("  ").is_err());
        assert!(parse_threshold("NaN").is_err());
        assert!(parse_threshold("-0.1").is_err());
        assert_eq!(parse_mode(" semi ").unwrap(), Mode::Semi);
    }
}
