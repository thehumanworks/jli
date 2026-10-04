//! Does a translated row match better than a raw taught phrase?
//!
//! Teaches the same commands to three fresh brains: one in manual mode (phrase and command stored as
//! given), one as `jli teach` does by default (the command stored as given, a fast model writes only
//! the scenario, System 1 verifies it, ADR 30), and one in semi mode (a fast model generalises the
//! command and the scenario, System 1 verifies it). Then routes held-out paraphrases through each and
//! compares. "wrong" counts requests that reached their skill and still rendered another command: a
//! command kept as taught runs its own value for a request that asked for another.
//!
//!   cargo run --release --example bench-translator     about 250 System 1 calls and 25 translator calls
//!   cargo run --release --example bench-translator -- --forgetting
//!                                  also the forgetting rate: about 500 more System 1 calls and 25 translator calls
//!
//! The forgetting rate is the share of held-out requests that routed home after one lesson and stop
//! doing so after the next.
//!
//! Run it from the repository root: some cases name files that live there.

use std::path::{Path, PathBuf};

use anyhow::Result;
use jli::context::{read_context, Context};
use jli::learn::{learn_phrase, LearnInput, Teaching};
use jli::router::DEFAULT_THRESHOLD;
use jli::slots::{fill_slots, render, Template};
use jli::store::{Brain, BrainConfig};
use jli::types::{Risk, Slot};
use jli::ui::{fixed, Ui};
use jli::Models;

struct Case {
    phrase: &'static str,
    command: &'static str,
    /// Held-out requests, each with the command that should run.
    tests: [(&'static str, &'static str); 3],
}

const CASES: [Case; 10] = [
    Case {
        phrase: "diff of changes in this project",
        command: "git diff",
        tests: [
            ("what exactly did I modify, line by line", "git diff"),
            ("show my uncommitted edits", "git diff"),
            ("let me see the patch of my working tree", "git diff"),
        ],
    },
    Case {
        phrase: "summary of what changed per file",
        command: "git diff --stat",
        tests: [
            ("which files changed and by how much", "git diff --stat"),
            ("how many lines did I touch in each file", "git diff --stat"),
            ("short overview of my modifications by file", "git diff --stat"),
        ],
    },
    Case {
        phrase: "show the last 3 commits",
        command: "git log --oneline -n 3",
        tests: [
            ("give me the 7 most recent commits", "git log --oneline -n 7"),
            ("what were the latest 5 commits", "git log --oneline -n 5"),
            ("recent history, 12 entries", "git log --oneline -n 12"),
        ],
    },
    Case {
        phrase: "which branch am I on",
        command: "git branch --show-current",
        tests: [
            ("what's the current branch", "git branch --show-current"),
            ("name of the branch I have checked out", "git branch --show-current"),
            ("where is HEAD pointing, branch-wise", "git branch --show-current"),
        ],
    },
    Case {
        phrase: "how big is this folder",
        command: "du -sh .",
        tests: [
            ("total disk space used by this directory", "du -sh ."),
            ("how much room does everything here take up", "du -sh ."),
            ("size of the current directory overall", "du -sh ."),
        ],
    },
    Case {
        phrase: "count lines in Cargo.toml",
        command: "wc -l Cargo.toml",
        tests: [
            ("how many lines does README.md have", "wc -l README.md"),
            ("how long is Cargo.lock in lines", "wc -l Cargo.lock"),
            ("number of lines in .gitignore", "wc -l .gitignore"),
        ],
    },
    Case {
        phrase: "what's listening on port 3000",
        command: "lsof -i :3000",
        tests: [
            ("which process is using port 8080", "lsof -i :8080"),
            ("who has port 5432 open", "lsof -i :5432"),
            ("is anything bound to port 9000", "lsof -i :9000"),
        ],
    },
    Case {
        phrase: "show hidden files too",
        command: "ls -la",
        tests: [
            ("list everything including dotfiles", "ls -la"),
            ("all files, even the invisible ones, with details", "ls -la"),
            ("long listing that includes entries starting with a dot", "ls -la"),
        ],
    },
    Case {
        phrase: "who changed this repo the most",
        command: "git shortlog -sn",
        tests: [
            ("commit counts per author", "git shortlog -sn"),
            ("rank contributors by number of commits", "git shortlog -sn"),
            ("how many commits has each person made", "git shortlog -sn"),
        ],
    },
    Case {
        phrase: "what's my node version",
        command: "node --version",
        tests: [
            ("which version of node is installed", "node --version"),
            ("node release I'm running", "node --version"),
            ("print the nodejs version number", "node --version"),
        ],
    },
];

const SLOTTED: [&str; 3] = ["git log --oneline -n 3", "wc -l Cargo.toml", "lsof -i :3000"];

/// The three ways of teaching that are compared, with the name each is printed under.
const STYLES: [(&str, Teaching); 3] =
    [("manual", Teaching::AsGiven), ("taught", Teaching::Described), ("semi", Teaching::Generalised)];

fn lesson(item: &Case, teaching: Teaching, cwd: &Path) -> LearnInput {
    LearnInput {
        phrase: item.phrase.into(),
        steps: vec![item.command.into()],
        risk: Some(Risk::Safe),
        teaching,
        threshold: DEFAULT_THRESHOLD,
        cwd: cwd.to_path_buf(),
        name: None,
        description: None,
    }
}

fn teach_all(models: &Models, dir: &Path, cwd: &Path, (name, teaching): (&str, Teaching)) -> Result<(Brain, usize)> {
    let brain = Brain::open(BrainConfig::file(&dir.join(format!("{name}.db"))))?;
    let quiet = Ui::new(false);
    let mut translated = 0;
    for item in &CASES {
        let input = lesson(item, teaching, cwd);
        if learn_phrase(&brain, models, &input, &quiet)?.translated {
            translated += 1;
        }
    }
    Ok((brain, translated))
}

#[derive(Default)]
struct Score {
    routed: usize,
    correct: usize,
    confidence: f64,
    total: usize,
    misses: Vec<String>,
}

fn evaluate(models: &Models, context: &Context, brain: &Brain, cases: &[&Case]) -> Result<Score> {
    let skills = brain.skills()?;
    let mut score = Score::default();
    for item in cases {
        let wanted = skills.iter().find(|skill| skill.examples.iter().any(|example| example == item.phrase));
        for (request, expected) in item.tests {
            score.total += 1;
            let decision = models.system1.decide(request, context, &skills)?;
            let chosen = decision.skill.as_ref();
            let routed = chosen.map(|skill| &skill.label) == wanted.map(|skill| &skill.label)
                && decision.confidence >= DEFAULT_THRESHOLD;
            if routed {
                score.routed += 1;
                score.confidence += decision.confidence;
            }
            let mut command = "(nothing)".to_string();
            if let Some(skill) = chosen.filter(|_| routed) {
                // Slots are filled without the agent here: the question is what System 1 alone can do.
                // An optional slot is never asked of the agent, so it keeps no default and is left out.
                let slots: Vec<Slot> = skill
                    .slots
                    .iter()
                    .map(|slot| match slot.optional {
                        true => slot.clone(),
                        false => Slot { default: Some(slot.default.clone().unwrap_or_else(|| "?".into())), ..slot.clone() },
                    })
                    .collect();
                let values = if slots.is_empty() {
                    Default::default()
                } else {
                    let template = Template { steps: &skill.steps, scenario: &skill.scenario, slots: &slots };
                    fill_slots(models, request, &template, context, &Default::default())?.values
                };
                command = render(&skill.steps, &slots, &values).join(" && ");
            }
            if command == expected {
                score.correct += 1;
            } else {
                score.misses.push(format!(
                    "  \"{request}\" → {command}   (wanted {expected}; picked {} at {})",
                    chosen.map_or("none", |skill| skill.label.as_str()),
                    fixed(decision.confidence, 2)
                ));
            }
        }
    }
    Ok(score)
}

fn pct(n: usize, d: usize) -> String {
    format!("{:>4}", format!("{}%", (100.0 * n as f64 / d.max(1) as f64).round()))
}

fn row(score: &Score) -> String {
    format!(
        "{}  {}  {}  {}",
        pct(score.routed, score.total),
        pct(score.correct, score.total),
        pct(score.routed - score.correct, score.total),
        fixed(score.confidence / score.routed.max(1) as f64, 2)
    )
}

/// Whether a held-out request routes to the skill that holds its case's taught phrase.
fn routes_home(
    models: &Models,
    context: &Context,
    skills: &[jli::types::Skill],
    item: &Case,
    request: &str,
) -> Result<bool> {
    let wanted = skills.iter().find(|skill| skill.examples.iter().any(|example| example == item.phrase));
    let decision = models.system1.decide(request, context, skills)?;
    Ok(decision.skill.as_ref().map(|skill| &skill.label) == wanted.map(|skill| &skill.label)
        && decision.confidence >= DEFAULT_THRESHOLD)
}

/// The forgetting rate: teach the cases one at a time, and after each lesson route the held-out
/// requests of every case taught so far. A request that routed home after one lesson and does not
/// after the next was forgotten. Returns (forgotten, chances to forget, what was forgotten).
fn forgetting(
    models: &Models,
    dir: &Path,
    cwd: &Path,
    context: &Context,
    (name, teaching): (&str, Teaching),
) -> Result<(usize, usize, Vec<String>)> {
    let brain = Brain::open(BrainConfig::file(&dir.join(format!("forgetting-{name}.db"))))?;
    let quiet = Ui::new(false);
    let mut home: Vec<[bool; 3]> = Vec::new();
    let (mut forgotten, mut chances, mut lost) = (0, 0, Vec::new());
    for (taught, item) in CASES.iter().enumerate() {
        learn_phrase(&brain, models, &lesson(item, teaching, cwd), &quiet)?;
        let skills = brain.skills()?;
        let mut now = Vec::new();
        for earlier in &CASES[..=taught] {
            let mut routed = [false; 3];
            for (slot, (request, _)) in earlier.tests.iter().enumerate() {
                routed[slot] = routes_home(models, context, &skills, earlier, request)?;
            }
            now.push(routed);
        }
        for (case, was) in home.iter().enumerate() {
            for slot in 0..3 {
                if was[slot] {
                    chances += 1;
                    if !now[case][slot] {
                        forgotten += 1;
                        lost.push(format!("  \"{}\" after teaching \"{}\"", CASES[case].tests[slot].0, item.phrase));
                    }
                }
            }
        }
        home = now;
    }
    Ok((forgotten, chances, lost))
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let context = read_context(&cwd);
    let scratch = Scratch(std::env::temp_dir().join(format!("jli-bench-{}", std::process::id())));
    std::fs::create_dir_all(&scratch.0)?;
    let models = Models::from_env();

    let mut brains = Vec::new();
    for style in STYLES {
        let (brain, translated) = teach_all(&models, &scratch.0, &cwd, style)?;
        if style.1 != Teaching::AsGiven {
            println!(
                "{}: kept {translated} rows a model wrote, and its own words for {}.",
                style.0,
                CASES.len() - translated
            );
        }
        brains.push((style.0, brain));
    }
    println!("Taught {} commands to each brain.\n", CASES.len());

    let all: Vec<&Case> = CASES.iter().collect();
    let fixed_commands: Vec<&Case> = CASES.iter().filter(|item| !SLOTTED.contains(&item.command)).collect();
    let with_value: Vec<&Case> = CASES.iter().filter(|item| SLOTTED.contains(&item.command)).collect();
    let groups = [("all requests", all), ("fixed commands", fixed_commands), ("commands with a value", with_value)];

    println!("                         manual (as given)          taught (described)         semi (generalised)");
    println!("                         routed right wrong conf    routed right wrong conf    routed right wrong conf");
    let mut detail = Vec::new();
    for (group, cases) in groups {
        let mut scores = Vec::new();
        for (_, brain) in &brains {
            scores.push(evaluate(&models, &context, brain, &cases)?);
        }
        let rows: Vec<String> = scores.iter().map(row).collect();
        println!("{group:<24} {}    ({} requests)", rows.join("    "), scores[0].total);
        if group == "all requests" {
            detail = scores;
        }
    }
    for ((name, _), score) in brains.iter().zip(detail) {
        println!("\n{name} misses:");
        println!("{}", if score.misses.is_empty() { "  none".to_string() } else { score.misses.join("\n") });
    }
    if std::env::args().any(|arg| arg == "--forgetting") {
        println!("\nForgetting: requests that routed home after one lesson and stopped after the next");
        for style in STYLES {
            let (forgotten, chances, lost) = forgetting(&models, &scratch.0, &cwd, &context, style)?;
            println!("{:<8} {forgotten} of {chances} ({})", style.0, pct(forgotten, chances).trim());
            if !lost.is_empty() {
                println!("{}", lost.join("\n"));
            }
        }
    }
    if let Some((requests, tokens, cost)) = models.system1.usage() {
        println!("\nSystem 1: {requests} requests, {tokens} input tokens, about ${cost:.4}");
    }
    Ok(())
}
