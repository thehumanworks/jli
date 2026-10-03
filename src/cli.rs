use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::args;
use crate::demo::run_demo;
use crate::learn::{learn_phrase, LearnInput};
use crate::router::{route, split_steps, Outcome, RouteOptions, DEFAULT_THRESHOLD};
use crate::store::{Brain, BrainConfig};
use crate::types::{BrainEvent, Mode, Path, Risk, DEFAULT_MODE};
use crate::ui::{bar, c, duration, risk_tag, sparkline, Prompt, Terminal, Ui};
use crate::Models;

fn help() -> String {
    format!(
        "{}  say what you want, it runs the command, and it learns how you say it

{}
  jli <request in plain language>      route, run, learn
  jli                                  interactive session

{}
  -n, --dry-run    show what would run, execute nothing, learn nothing
  -y, --yes        run without asking (never approves destructive commands)
  -Y, --yolo       run without asking, destructive commands included
      --confidence=N   use System 1's pick at or above N for this run (0 to 1)
      --mode=M         learning mode for this run: manual, semi or auto
  -q, --quiet      hide the decision trace
      --json       print the outcome as JSON (for agents); command output is captured
      --agent      skip the decision model and ask the reasoning agent directly

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
  jli config                           show settings
  jli config confidence <0..1>         confidence System 1 needs before its pick is used (default 0.5)
  jli config mode <manual|semi|auto>   who does the teaching (default semi)
  jli fix                              the last command was wrong: unlearn it and try again
  jli forget <label>                   delete a skill
  jli sync                             sync the local replica with the cloud brain
  jli demo                             watch it learn in a sandbox (about 12 API calls)

{}
  JLI_DB_URL     file:/path/brain.db (default ~/.jli/brain.db) or libsql://… for a cloud brain
  JLI_SYNC_URL   libsql://… keeps a local replica in sync with a cloud brain
  JLI_DB_TOKEN   auth token for the cloud brain
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
struct Flags {
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

fn cwd() -> Result<PathBuf> {
    std::env::current_dir().map_err(|error| anyhow!("cannot read the working directory: {error}"))
}

/// Everything a subcommand needs: the brain, the models, and the one stdin reader.
struct App {
    brain: Brain,
    models: Models,
    terminal: Terminal,
}

impl App {
    fn ask(&mut self, query: &str, flags: &Flags, rejected: Vec<String>) -> Result<Outcome> {
        let ui = Ui::new(!flags.quiet && !flags.json);
        let options = RouteOptions {
            yes: flags.yes,
            yolo: flags.yolo,
            dry_run: flags.dry_run,
            capture: flags.json,
            agent_only: flags.agent,
            rejected,
            ..RouteOptions::new(cwd()?, resolve_threshold(&self.brain, flags)?, resolve_mode(&self.brain, flags)?)
        };
        let outcome = route(&self.brain, &self.models, query, &ui, &mut self.terminal, &options)?;
        if flags.json {
            say(serde_json::to_string_pretty(&outcome)?);
        }
        Ok(outcome)
    }

    fn show_skills(&self) -> Result<()> {
        let skills = self.brain.skills()?;
        say(format!("{} {}\n", c::bold(format!("{} skills", skills.len())), c::dim(format!("in {}", self.brain.location()))));
        for skill in skills {
            let origin = match skill.origin {
                crate::types::Origin::Seed => c::dim("seed"),
                crate::types::Origin::Agent => c::magenta("learned from agent"),
                crate::types::Origin::Taught => c::cyan("taught"),
            };
            say(format!(
                "{}  {}  {origin}  {}",
                c::bold(&skill.label),
                risk_tag(skill.risk),
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
        for (name, count) in [("system 1, instant", fast.len()), ("taught by you", taught), ("system 2, agent", slow.len())] {
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
        say(format!(
            "\n{} {}",
            c::bold("Learning curve"),
            c::dim("share of requests answered instantly, oldest to newest")
        ));
        say(format!(
            "  {}  {}% now",
            c::cyan(sparkline(&curve)),
            (curve.last().copied().unwrap_or(0.0) * 100.0).round()
        ));

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

    fn teach(&self, phrase: &str, command: &[String], flags: &Flags) -> Result<()> {
        let steps = split_steps(&command.join(" "));
        let learned = learn_phrase(
            &self.brain,
            &self.models,
            &LearnInput {
                phrase: phrase.to_string(),
                steps,
                risk: flags.risk,
                mode: resolve_mode(&self.brain, flags)?,
                threshold: resolve_threshold(&self.brain, flags)?,
                cwd: cwd()?,
            },
            &Ui::new(!flags.quiet),
        )?;
        let skill = self.brain.skill(&learned.label)?;
        let runs = skill.as_ref().map(|skill| skill.steps.join(" && ")).unwrap_or_else(|| "undefined".into());
        say(format!(
            "{} \"{phrase}\" means {} {}",
            c::green("✚ learned"),
            c::bold(&learned.label),
            c::dim(format!("({runs})"))
        ));
        say(c::dim(if learned.created {
            "  new skill".to_string()
        } else {
            let count = skill.map_or(1, |skill| skill.examples.len());
            format!("  added to an existing skill, which now has {count} phrasings")
        }));
        Ok(())
    }

    fn fix(&mut self, flags: &Flags) -> Result<i32> {
        let last = self.brain.last_executed()?;
        let Some((last, label)) = last.and_then(|event| event.label.clone().map(|label| (event, label))) else {
            say("Nothing to fix: no command has been run yet.");
            return Ok(1);
        };
        if flags.dry_run {
            eprintln!("{}\n", c::dim(format!("  Dry run: would unlearn that \"{}\" means {label}.", last.query)));
        } else {
            // What was learned is the request with its stated values hidden; older rows hold it whole.
            let slots = self.brain.skill(&label)?.map(|skill| skill.slots).unwrap_or_default();
            let phrase = args::phrase(&last.query, &slots);
            self.brain.remove_example(&label, &last.query)?;
            self.brain.remove_example(&label, &phrase)?;
            self.brain.add_counter_example(&label, &phrase)?;
            self.brain.mark_rejected(last.id)?;
            eprintln!(
                "{} \"{}\" does {} mean {label}. Trying again.\n",
                c::green("✚ learned"),
                last.query,
                c::bold("not")
            );
        }
        let again = Flags { agent: true, ..flags.clone() };
        Ok(self.ask(&last.query, &again, vec![label])?.exit_code())
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
                "fix" => return self.fix(flags),
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
        if first == "forget" && others.len() == 1 {
            let label = &others[0];
            say(if self.brain.forget(label)? { format!("Forgot {label}.") } else { format!("No skill called {label}.") });
            return Ok(0);
        }
        if first == "teach" {
            if others.is_empty() || rest.is_empty() {
                say("Usage: jli teach \"<phrase>\" [--risk=safe|modifies|destructive] -- <command>");
                return Ok(1);
            }
            self.teach(&others.join(" "), rest, flags)?;
            return Ok(0);
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

fn run(argv: &[String]) -> Result<i32> {
    let Parsed { mut flags, words, rest } = parse(argv)?;
    flags.yes |= switched_on(env, "JLI_YES");
    flags.yolo |= switched_on(env, "JLI_YOLO");
    let first = words.first().map(String::as_str);
    if matches!(first, Some("help" | "--help" | "-h")) {
        say(help());
        return Ok(0);
    }
    if first == Some("demo") && words.len() == 1 {
        return run_demo();
    }

    let mut app = App { brain: Brain::open(BrainConfig::from_env())?, models: Models::from_env(), terminal: Terminal::new() };
    let code = app.dispatch(&flags, &words, &rest);
    let _ = app.brain.sync();
    code
}

/// The `jli` binary: runs and returns the process exit code.
pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(&argv) {
        Ok(code) => code,
        Err(error) => {
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
        let parsed = parse(&args(&["-n", "show", "--risk=bad", "files", "--json", "--mode=auto", "--confidence=0.7"])).unwrap();
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
        for (flag, yes, yolo) in [("-y", true, false), ("--yes", true, false), ("-Y", false, true), ("--yolo", false, true)] {
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
    fn thresholds_are_numbers_from_0_to_1() {
        assert_eq!(parse_threshold(" 0.25 ").unwrap(), 0.25);
        assert_eq!(parse_threshold("1").unwrap(), 1.0);
        assert!(parse_threshold("  ").is_err());
        assert!(parse_threshold("NaN").is_err());
        assert!(parse_threshold("-0.1").is_err());
        assert_eq!(parse_mode(" semi ").unwrap(), Mode::Semi);
    }
}
