use std::path::{Path as FsPath, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _, Result};

use crate::router::{route, Outcome, RouteOptions, DEFAULT_THRESHOLD};
use crate::store::{Brain, BrainConfig};
use crate::types::{Origin, Path, DEFAULT_MODE};
use crate::ui::{c, duration, Terminal, Ui};
use crate::Models;

/// Each lesson is asked twice, in different words. The first time the brain has
/// never seen the task. The second time it has, and the phrasing is new, so a
/// fast answer is generalisation and not a cache hit.
struct Lesson {
    task: &'static str,
    first: &'static str,
    again: &'static str,
}

const LESSONS: [Lesson; 5] = [
    Lesson { task: "already known", first: "what files are here", again: "which files do I have in this folder" },
    Lesson { task: "new skill with a number", first: "show the last 3 commits", again: "give me the 2 most recent commits" },
    Lesson { task: "new skill with a file", first: "how many lines are in notes.txt", again: "count the lines of todo.md" },
    Lesson { task: "new skill", first: "which branch am I on", again: "what's the current branch?" },
    Lesson { task: "seeded skill", first: "what's changed in the repo?", again: "anything changed in this repo?" },
];

fn git(cwd: &FsPath, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(["-c", "user.name=jli", "-c", "user.email=jli@example.invalid"])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("could not run git")?;
    if !status.success() {
        bail!("git {} failed in the demo sandbox", args.join(" "));
    }
    Ok(())
}

/// A fresh directory under the system temp dir, like `mkdtemp`.
fn temp_dir(prefix: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    for attempt in 0u32..100 {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos();
        let dir = base.join(format!("{prefix}{:x}{:x}{attempt}", std::process::id(), nanos));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("could not create {}", dir.display())),
        }
    }
    bail!("could not create a temporary directory")
}

fn make_sandbox(dir: &FsPath) -> Result<PathBuf> {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo)?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    let files =
        [("notes.txt", "one\ntwo\nthree\nfour\n"), ("todo.md", "- learn\n- adapt\n"), ("app.ts", "console.log('hello');\n")];
    for (name, body) in files {
        std::fs::write(repo.join(name), body)?;
        git(&repo, &["add", name])?;
        git(&repo, &["commit", "-q", "-m", &format!("add {name}")])?;
    }
    std::fs::write(repo.join("todo.md"), "- learn\n- adapt\n- ship\n")?;
    Ok(repo)
}

fn describe(outcome: &Outcome) -> String {
    let ms = format!("{:>7}", duration(outcome.ms_system1 + outcome.ms_system2));
    match outcome.path {
        Path::System1 => format!("{} {ms}", c::cyan("⚡ system 1")),
        Path::System2 => format!("{} {ms}", c::magenta("◆ system 2")),
        _ => format!("{} {ms}", c::dim("· nothing ")),
    }
}

struct Sandbox(PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Teach a fresh brain in a throwaway git repository, then ask again in other words.
pub fn run_demo(user: &str) -> Result<i32> {
    let sandbox = Sandbox(temp_dir("jli-demo-")?);
    let cwd = make_sandbox(&sandbox.0)?;
    let brain = Brain::open_as(BrainConfig::file(&sandbox.0.join("brain.db")), user)?;
    let models = Models::from_env();
    let mut terminal = Terminal::new();
    let ui = Ui::new(true);
    let options = RouteOptions { yes: true, ..RouteOptions::new(cwd.clone(), DEFAULT_THRESHOLD, DEFAULT_MODE) };
    let mut before = Vec::new();
    let mut after = Vec::new();

    ui.line(c::bold("\njli demo: a fresh brain, a throwaway git repository"));
    ui.line(c::dim(format!("sandbox {}\n", cwd.display())));

    ui.line(c::bold("━━ Pass 1: first time it hears each request ━━"));
    for lesson in &LESSONS {
        ui.line(format!("\n{} {}", c::cyan("›"), c::bold(lesson.first)));
        before.push(route(&brain, &models, lesson.first, &ui, &mut terminal, &options)?);
    }

    ui.line(c::bold("\n━━ Pass 2: the same tasks, in words it has never seen ━━"));
    for lesson in &LESSONS {
        ui.line(format!("\n{} {}", c::cyan("›"), c::bold(lesson.again)));
        after.push(route(&brain, &models, lesson.again, &ui, &mut terminal, &options)?);
    }

    ui.line(c::bold("\n━━ What changed ━━"));
    for (lesson, (a, b)) in LESSONS.iter().zip(before.iter().zip(&after)) {
        ui.line(format!(
            "  {:<28} {}  {}  {}  {}",
            lesson.task,
            describe(a),
            c::dim("then"),
            describe(b),
            c::dim(b.commands.join(" && "))
        ));
    }
    let total = |outcomes: &[Outcome]| outcomes.iter().map(|o| o.ms_system1 + o.ms_system2).sum::<u64>();
    let instant = |outcomes: &[Outcome]| outcomes.iter().filter(|o| o.path == Path::System1).count();
    ui.line(format!(
        "\n  answered instantly: {} of {} {} {}   time spent deciding: {} {} {}",
        instant(&before),
        LESSONS.len(),
        c::dim("then"),
        c::green(format!("{} of {}", instant(&after), LESSONS.len())),
        duration(total(&before)),
        c::dim("then"),
        c::green(duration(total(&after)))
    ));
    let learned: Vec<String> = brain
        .skills()?
        .into_iter()
        .filter(|skill| skill.origin == Origin::Agent)
        .map(|skill| c::bold(skill.label))
        .collect();
    let learned = if learned.is_empty() { "none".to_string() } else { learned.join(", ") };
    ui.line(format!("  skills the agent taught the decision model: {learned}\n"));
    Ok(if instant(&after) > instant(&before) { 0 } else { 1 })
}
