//! Can one phrase mean one command in one project and another command in the next?
//!
//! Two projects, a Rust one and a Node one, are taught the same five phrases, each to its own
//! command. The Rust project learns first. The Node project then routes each phrase, and where it
//! lands on the Rust command it is corrected the way the router corrects a "no": the phrase is
//! unlearned on the wrong skill, kept as a counter-example there, and taught to the right command.
//! Then the Rust project routes its phrases again and is corrected the same way where they went
//! wrong, and the Node project is measured once more. Each step routes the phrases and three
//! held-out paraphrases of each, in each project.
//!
//! It runs three times on fresh brains: with both projects in one scope, which is what a brain
//! without a `.jlirc` does (the behaviour before ADR 17); with each project its own `.jlirc` id;
//! and with those ids but the Rust commands taught as global skills, so that the Node project's
//! corrections fall on global rows and are kept as that project's lessons.
//!
//!   cargo run --release --example bench-context     about 400 System 1 calls and 20 translator calls
//!
//! Both projects are seen from a directory called `app`, so the directory name says nothing about
//! the language.

use std::path::{Path, PathBuf};

use anyhow::Result;
use jli::context::{platform, Context};
use jli::learn::{learn_phrase, LearnInput};
use jli::router::DEFAULT_THRESHOLD;
use jli::slots::match_template;
use jli::store::{Brain, BrainConfig};
use jli::types::{Mode, Risk};
use jli::ui::Ui;
use jli::Models;

struct Case {
    phrase: &'static str,
    rust: &'static str,
    node: &'static str,
    /// Held-out requests for the same thing, in either project.
    paraphrases: [&'static str; 3],
}

const CASES: [Case; 5] = [
    Case {
        phrase: "run the tests",
        rust: "cargo test",
        node: "npm test",
        paraphrases: ["execute the test suite", "run all the tests please", "check that the tests pass"],
    },
    Case {
        phrase: "build the project",
        rust: "cargo build",
        node: "npm run build",
        paraphrases: ["compile everything", "make a build", "produce a build of this code"],
    },
    Case {
        phrase: "start the app",
        rust: "cargo run",
        node: "npm start",
        paraphrases: ["launch the application", "run the program", "boot up the app"],
    },
    Case {
        phrase: "lint the code",
        rust: "cargo clippy",
        node: "npm run lint",
        paraphrases: ["run the linter", "check the code for lint problems", "look for code smells with the linter"],
    },
    Case {
        phrase: "format the code",
        rust: "cargo fmt",
        node: "npm run format",
        paraphrases: ["reformat the source files", "apply the code formatter", "tidy up the formatting"],
    },
];

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Rust,
    Node,
}

impl Side {
    fn command(self, case: &Case) -> &'static str {
        match self {
            Side::Rust => case.rust,
            Side::Node => case.node,
        }
    }
}

struct Bench {
    models: Models,
    context: Context,
    cwd: PathBuf,
    quiet: Ui,
}

/// What one project gets right, phrases and paraphrases apart.
#[derive(Default)]
struct Score {
    phrases: usize,
    paraphrases: usize,
}

impl Bench {
    /// The command System 1 routes `request` to in `brain`, if it is confident enough to use it.
    fn routes(&self, brain: &Brain, request: &str) -> Result<Option<(String, Vec<String>)>> {
        let decision = self.models.system1.decide(request, &self.context, &brain.skills()?)?;
        Ok(decision.skill.filter(|_| decision.confidence >= DEFAULT_THRESHOLD).map(|skill| (skill.label, skill.steps)))
    }

    fn right(&self, brain: &Brain, request: &str, command: &str) -> Result<bool> {
        Ok(self.routes(brain, request)?.is_some_and(|(_, steps)| match_template(&steps, command).is_some()))
    }

    fn teach(&self, brain: &Brain, phrase: &str, command: &str) -> Result<String> {
        let input = LearnInput {
            phrase: phrase.into(),
            steps: vec![command.into()],
            risk: Some(Risk::Safe),
            mode: Mode::Semi,
            threshold: DEFAULT_THRESHOLD,
            cwd: self.cwd.clone(),
        };
        Ok(learn_phrase(brain, &self.models, &input, &self.quiet)?.label)
    }

    /// Route each phrase; where it lands on a skill that runs another command, correct it as the
    /// router does after a "no". Returns how many needed it.
    fn correct(&self, brain: &Brain, side: Side) -> Result<usize> {
        let mut corrected = 0;
        for case in &CASES {
            let command = side.command(case);
            match self.routes(brain, case.phrase)? {
                Some((_, steps)) if match_template(&steps, command).is_some() => {}
                Some((label, _)) => {
                    corrected += 1;
                    brain.remove_example(&label, case.phrase)?;
                    brain.add_counter_example(&label, case.phrase)?;
                    self.teach(brain, case.phrase, command)?;
                }
                // Nothing fits: the agent would propose it, and the result is the same lesson.
                None => {
                    self.teach(brain, case.phrase, command)?;
                }
            }
        }
        Ok(corrected)
    }

    fn score(&self, brain: &Brain, side: Side) -> Result<Score> {
        let mut score = Score::default();
        for case in &CASES {
            let command = side.command(case);
            score.phrases += usize::from(self.right(brain, case.phrase, command)?);
            for paraphrase in case.paraphrases {
                score.paraphrases += usize::from(self.right(brain, paraphrase, command)?);
            }
        }
        Ok(score)
    }
}

fn pct(n: usize, of: usize) -> String {
    format!("{n}/{of} ({}%)", (100.0 * n as f64 / of as f64).round())
}

fn line(name: &str, rust: &Score, node: &Score) {
    let (phrases, paraphrases) = (CASES.len(), CASES.len() * 3);
    println!(
        "  {name:<34} rust: phrases {:<10} paraphrases {:<12} node: phrases {:<10} paraphrases {}",
        pct(rust.phrases, phrases),
        pct(rust.paraphrases, paraphrases),
        pct(node.phrases, phrases),
        pct(node.paraphrases, paraphrases)
    );
}

#[derive(Clone, Copy, PartialEq)]
enum Arm {
    /// No `.jlirc`: what every brain did before ADR 17.
    OneScope,
    /// Each project its own id, and the Rust commands taught inside the Rust project.
    Projects,
    /// Each project its own id, and the Rust commands taught as global skills (`teach --global`),
    /// so the Node project's corrections land on global rows.
    GlobalSkills,
}

fn arm(bench: &Bench, dir: &Path, arm: Arm) -> Result<()> {
    let (file, title) = match arm {
        Arm::OneScope => ("one-scope.db", "No .jlirc: both projects in one scope"),
        Arm::Projects => ("projects.db", "Each project its own .jlirc id, the Rust commands taught in the Rust project"),
        Arm::GlobalSkills => ("global.db", "Each project its own .jlirc id, the Rust commands taught as global skills"),
    };
    let path = dir.join(file);
    let open =
        |project: &str| Brain::open_in(BrainConfig::file(&path), "default", (arm != Arm::OneScope).then_some(project));
    let (rust, node) = (open("bench-rust")?, open("bench-node")?);
    println!("\n{title}");

    let global = Brain::open(BrainConfig::file(&path))?;
    for case in &CASES {
        bench.teach(if arm == Arm::GlobalSkills { &global } else { &rust }, case.phrase, case.rust)?;
    }
    let misrouted = CASES
        .iter()
        .map(|case| bench.routes(&node, case.phrase))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .zip(&CASES)
        .filter(|(routed, case)| routed.as_ref().is_some_and(|(_, steps)| match_template(steps, case.rust).is_some()))
        .count();
    println!("  node phrases that ran the Rust command before node learned anything: {misrouted}/{}", CASES.len());
    line("after rust learned its commands", &bench.score(&rust, Side::Rust)?, &bench.score(&node, Side::Node)?);

    let corrected = bench.correct(&node, Side::Node)?;
    println!("  node corrections: {corrected}");
    line("after node learned its commands", &bench.score(&rust, Side::Rust)?, &bench.score(&node, Side::Node)?);

    let undone = bench.correct(&rust, Side::Rust)?;
    println!("  rust phrases that went wrong after that, corrected again: {undone}");
    line("after rust was corrected back", &bench.score(&rust, Side::Rust)?, &bench.score(&node, Side::Node)?);
    Ok(())
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn main() -> Result<()> {
    let scratch = Scratch(std::env::temp_dir().join(format!("jli-bench-context-{}", std::process::id())));
    let cwd = scratch.0.join("app");
    std::fs::create_dir_all(&cwd)?;
    let context = Context { cwd: cwd.clone(), directory: "app".into(), inside_git_repository: false, platform: platform() };
    let bench = Bench { models: Models::from_env(), context, cwd, quiet: Ui::new(false) };
    for which in [Arm::OneScope, Arm::Projects, Arm::GlobalSkills] {
        arm(&bench, &scratch.0, which)?;
    }
    if let Some((requests, tokens, cost)) = bench.models.system1.usage() {
        println!("\nSystem 1: {requests} requests, {tokens} input tokens, about ${cost:.4}");
    }
    Ok(())
}
