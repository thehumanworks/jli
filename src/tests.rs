//! The router and the learning loop, offline: System 1 is a fake transport, the language models
//! are a fake transport, and the person at the keyboard is a script.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::learn::{learn_phrase, LearnInput};
use crate::llm::Llm;
use crate::router::{route, Outcome, RouteOptions, DRY_RUN, NOT_A_COMMAND};
use crate::store::{now_iso, Brain, BrainConfig, NewSkill};
use crate::system1::{System1, NONE};
use crate::types::{Mode, Origin, Path, Risk, Slot, SlotKind};
use crate::ui::{Prompt, Ui};
use crate::Models;

/// A temp directory with a brain in it and an empty working directory.
struct Scratch {
    root: PathBuf,
    work: PathBuf,
    brain: Brain,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!(
            "jli-test-{name}-{}-{}",
            std::process::id(),
            now_iso().replace([':', '.'], "")
        ));
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let brain = Brain::open(BrainConfig::file(&root.join("brain.db"))).unwrap();
        Scratch { root, work, brain }
    }

    fn options(&self, mode: Mode) -> RouteOptions {
        RouteOptions { capture: true, ..RouteOptions::new(self.work.clone(), 0.5, mode) }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A System 1 answer to the decide call.
fn decision(choice: &str, confidence: f64, is_command: f64, risk: [f64; 3]) -> Value {
    json!({
        "model": "jev-test",
        "answers": {
            "isCommandRequest": { "type": "noul", "noul": is_command },
            "intentRisk": {
                "type": "score", "score": risk[1] + 2.0 * risk[2], "confidence": 0.9,
                "legend": { "0": "read", "1": "modify", "2": "destroy" },
                "probabilities": { "0": risk[0], "1": risk[1], "2": risk[2] },
            },
            "skill": {
                "type": "choice", "choice": choice, "confidence": confidence,
                "probabilities": { choice: confidence, NONE: 1.0 - confidence },
            },
        },
        "usage": { "input_tokens": 10, "output_tokens": 1 },
    })
}

const SAFE: [f64; 3] = [0.9, 0.1, 0.0];

/// Language model replies by schema name, in order. Any other call fails the test.
struct Replies {
    queue: Mutex<HashMap<String, VecDeque<Value>>>,
    seen: Mutex<Vec<Value>>,
}

fn models(system1: impl Fn(&Value) -> Value + Send + Sync + 'static, replies: Vec<(&str, Value)>) -> (Models, Arc<Replies>) {
    let mut queue: HashMap<String, VecDeque<Value>> = HashMap::new();
    for (name, reply) in replies {
        queue.entry(name.to_string()).or_default().push_back(reply);
    }
    let replies = Arc::new(Replies { queue: Mutex::new(queue), seen: Mutex::new(vec![]) });
    let shared = replies.clone();
    let llm = Llm::with_transport(move |call| {
        let body: Value = serde_json::from_slice(call.body).unwrap();
        let name = body["response_format"]["json_schema"]["name"].as_str().unwrap().to_string();
        shared.seen.lock().unwrap().push(body);
        let reply = shared.queue.lock().unwrap().get_mut(&name).and_then(VecDeque::pop_front);
        let reply = reply.unwrap_or_else(|| panic!("unexpected language model call: {name}"));
        Ok((200, json!({ "choices": [{ "message": { "content": reply.to_string() } }] }).to_string()))
    });
    (Models { system1: System1::fake(system1), llm }, replies)
}

fn no_language_model() -> Vec<(&'static str, Value)> {
    vec![]
}

/// The person at the keyboard, answering from a script.
#[derive(Default)]
struct Script {
    confirms: VecDeque<Option<bool>>,
    asks: VecDeque<Option<String>>,
    questions: Vec<String>,
}

impl Script {
    fn new(confirms: &[Option<bool>], asks: &[&str]) -> Script {
        Script {
            confirms: confirms.iter().copied().collect(),
            asks: asks.iter().map(|answer| Some(answer.to_string())).collect(),
            questions: vec![],
        }
    }
}

impl Prompt for Script {
    fn read_line(&mut self) -> Option<String> {
        None
    }

    fn confirm(&mut self, question: &str) -> Option<bool> {
        self.questions.push(question.to_string());
        self.confirms.pop_front().expect("an unexpected confirmation")
    }

    fn ask(&mut self, question: &str) -> Option<String> {
        self.questions.push(question.to_string());
        self.asks.pop_front().expect("an unexpected question")
    }
}

fn quiet() -> Ui {
    Ui::new(false)
}

fn go(scratch: &Scratch, models: &Models, query: &str, script: &mut Script, options: &RouteOptions) -> Outcome {
    route(&scratch.brain, models, query, &quiet(), script, options).unwrap()
}

#[test]
fn a_dry_run_shows_the_system1_route_and_logs_nothing() {
    let scratch = Scratch::new("dry");
    let (models, _) = models(|_| decision("list-files-by-time", 0.98, 0.9, SAFE), no_language_model());
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "show me files sorted by most recent change", &mut Script::default(), &options);
    assert_eq!(outcome.path, Path::System1);
    assert_eq!(outcome.commands, ["ls -ltr"]);
    assert_eq!(outcome.blocked.as_deref(), Some(DRY_RUN));
    assert!(!outcome.ran);
    assert_eq!(outcome.exit_code(), 0);
    assert!(scratch.brain.events(10).unwrap().is_empty(), "a dry run logs nothing");
}

#[test]
fn quoted_text_and_numbers_fill_slots_without_a_model() {
    let scratch = Scratch::new("slots");
    let (models, _) = models(|_| decision("git-commit", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "commit with message \"first draft\"", &mut Script::default(), &options);
    assert_eq!(outcome.commands, ["git commit -m 'first draft'"]);
}

#[test]
fn a_request_that_is_not_a_command_runs_nothing() {
    let scratch = Scratch::new("not-a-command");
    let (models, _) = models(|_| decision(NONE, 0.9, 0.05, SAFE), no_language_model());
    let outcome = go(&scratch, &models, "how much is 2 + 2", &mut Script::default(), &scratch.options(Mode::Semi));
    assert_eq!(outcome.blocked.as_deref(), Some(NOT_A_COMMAND));
    assert_eq!(outcome.exit_code(), 0);
    let events = scratch.brain.events(10).unwrap();
    assert_eq!((events.len(), events[0].path, events[0].accepted), (1, Path::None, false));
}

#[test]
fn without_anyone_to_confirm_nothing_runs() {
    let scratch = Scratch::new("no-terminal");
    let (models, _) = models(|_| decision("git-stage-all", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
    let mut script = Script::new(&[None], &[]);
    let outcome = go(&scratch, &models, "stage everything", &mut script, &scratch.options(Mode::Semi));
    assert_eq!(outcome.blocked.as_deref(), Some("needs confirmation"));
    assert_eq!(outcome.exit_code(), 3);
    assert!(!outcome.ran);
}

#[test]
fn a_destructive_command_is_never_approved_by_yes() {
    let scratch = Scratch::new("destructive");
    let proposal = json!({
        "kind": "new", "existingLabel": null, "label": "delete-log-files", "steps": ["rm -f *.log"],
        "scenario": "the user wants every .log file in the directory deleted", "slots": [], "slotValues": [],
        "risk": "modifies", "explanation": "Deletes the log files.", "answer": null,
    });
    let (models, _) = models(|_| decision(NONE, 0.8, 0.95, [0.1, 0.3, 0.6]), vec![("proposal", proposal)]);
    let mut script = Script::new(&[None], &[]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "delete all the log files", &mut script, &options);
    assert_eq!(outcome.risk, Some(Risk::Destructive), "the intent risk outvotes the agent's");
    assert_eq!(outcome.blocked.as_deref(), Some("destructive commands need an interactive yes or --yolo"));
    assert_eq!(script.questions, ["Run it?"], "--yes still asks");
    assert_eq!(outcome.exit_code(), 3);
}

#[test]
fn yolo_runs_a_destructive_command_without_asking() {
    let scratch = Scratch::new("yolo");
    let log = scratch.work.join("old.log");
    std::fs::write(&log, "noise\n").unwrap();
    let proposal = json!({
        "kind": "new", "existingLabel": null, "label": "delete-log-files", "steps": ["rm -f *.log"],
        "scenario": "the user wants every .log file in the directory deleted", "slots": [], "slotValues": [],
        "risk": "destructive", "explanation": "Deletes the log files.", "answer": null,
    });
    let (models, _) = models(|_| decision(NONE, 0.8, 0.95, [0.1, 0.3, 0.6]), vec![("proposal", proposal)]);
    let mut script = Script::default();
    let options = RouteOptions { yolo: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "delete all the log files", &mut script, &options);
    assert_eq!(outcome.risk, Some(Risk::Destructive));
    assert!(script.questions.is_empty(), "--yolo never asks");
    assert!(outcome.ran && outcome.exit_code == Some(0) && outcome.blocked.is_none());
    assert!(!log.exists(), "the command really ran");
}

#[test]
fn an_agent_proposal_that_runs_becomes_a_skill() {
    let scratch = Scratch::new("system2");
    std::fs::write(scratch.work.join("notes.txt"), "one\ntwo\n").unwrap();
    let proposal = json!({
        "kind": "new", "existingLabel": null, "label": "count-lines", "steps": ["wc -l {file}"],
        "scenario": "the user wants the number of lines in a file",
        "slots": [{ "name": "file", "kind": "file", "description": "the file to count", "default": null }],
        "slotValues": [{ "name": "file", "value": "notes.txt" }],
        "risk": "safe", "explanation": "Counts lines.", "answer": null,
    });
    let (models, replies) = models(|_| decision(NONE, 0.7, 0.95, SAFE), vec![("proposal", proposal)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "how many lines are in notes.txt", &mut Script::default(), &options);
    assert_eq!(outcome.path, Path::System2);
    assert_eq!(outcome.commands, ["wc -l notes.txt"]);
    assert!(outcome.ran && outcome.exit_code == Some(0));
    assert!(outcome.output.as_deref().unwrap().contains("notes.txt"));
    assert_eq!(outcome.learned, ["new skill count-lines"]);
    let skill = scratch.brain.skill("count-lines").unwrap().unwrap();
    assert_eq!((skill.origin, skill.uses), (Origin::Agent, 1));
    assert_eq!(skill.examples, ["how many lines are in notes.txt"]);
    assert_eq!(skill.slots[0].kind, SlotKind::File);

    let body = &replies.seen.lock().unwrap()[0];
    assert_eq!(body["reasoning"], json!({ "effort": "low" }));
    let input: Value = serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["request"], "how many lines are in notes.txt");
    assert_eq!(input["directoryEntries"], json!(["notes.txt"]));
    assert_eq!(input["knownSkills"].as_array().unwrap().len(), 9);
}

#[test]
fn declining_and_typing_a_known_command_relinks_the_phrase() {
    let scratch = Scratch::new("decline");
    let (models, _) = models(|_| decision("list-files", 0.9, 0.9, SAFE), no_language_model());
    let mut script = Script::new(&[Some(false)], &["ls -ltr"]);
    let outcome = go(&scratch, &models, "newest files", &mut script, &scratch.options(Mode::Semi));
    assert_eq!(outcome.path, Path::Taught);
    assert_eq!(outcome.label.as_deref(), Some("list-files-by-time"));
    assert!(outcome.ran);
    let wrong = scratch.brain.skill("list-files").unwrap().unwrap();
    assert_eq!(wrong.counter_examples, ["newest files"]);
    let right = scratch.brain.skill("list-files-by-time").unwrap().unwrap();
    assert_eq!(right.examples, ["newest files"]);
    assert_eq!(scratch.brain.events(1).unwrap()[0].path, Path::Taught);
}

#[test]
fn the_same_command_with_another_value_is_not_a_counter_example() {
    let scratch = Scratch::new("same-skill");
    let (models, _) = models(|_| decision("git-commit", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
    let mut script = Script::new(&[Some(false)], &["git commit -m other"]);
    let outcome = go(&scratch, &models, "commit as \"wip\"", &mut script, &scratch.options(Mode::Semi));
    assert_eq!(outcome.label.as_deref(), Some("git-commit"));
    let skill = scratch.brain.skill("git-commit").unwrap().unwrap();
    assert!(skill.counter_examples.is_empty());
    assert_eq!(skill.examples, ["commit as \"wip\""]);
}

#[test]
fn asking_the_agent_after_a_no_rejects_the_skill() {
    let scratch = Scratch::new("clarify");
    let proposal = json!({
        "kind": "existing", "existingLabel": "list-files-by-time", "label": null, "steps": [],
        "scenario": "", "slots": [], "slotValues": [], "risk": "safe", "explanation": "Newest last.", "answer": null,
    });
    let (models, replies) = models(|_| decision("list-files", 0.9, 0.9, SAFE), vec![("proposal", proposal)]);
    let mut script = Script::new(&[Some(false), Some(true)], &["?"]);
    let outcome = go(&scratch, &models, "files by date", &mut script, &scratch.options(Mode::Semi));
    assert_eq!((outcome.path, outcome.label.as_deref()), (Path::System2, Some("list-files-by-time")));
    assert_eq!(scratch.brain.skill("list-files").unwrap().unwrap().counter_examples, ["files by date"]);
    assert_eq!(scratch.brain.skill("list-files-by-time").unwrap().unwrap().examples, ["files by date"]);
    let body = &replies.seen.lock().unwrap()[0];
    let input: Value = serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["rejectedSkills"], json!([{ "label": "list-files", "steps": ["ls"] }]));
    assert_eq!(input["clarification"], Value::Null);
    assert!(input["knownSkills"].as_array().unwrap().iter().all(|skill| skill["label"] != "list-files"));
}

#[test]
fn auto_mode_offers_numbered_options_after_a_no() {
    let scratch = Scratch::new("auto-decline");
    let suggestion = json!({
        "verdict": "not_applicable", "explanation": "You said no.",
        "options": [
            { "command": "ls -la", "description": "everything, with details" },
            { "command": "ls -ltr", "description": "newest last" },
            { "command": "ls", "description": "already tried" },
        ],
    });
    let (models, _) = models(|_| decision("list-files", 0.9, 0.9, SAFE), vec![("suggestion", suggestion)]);
    let mut script = Script::new(&[Some(false)], &["2"]);
    let mut options = scratch.options(Mode::Auto);
    options.capture = true;
    let outcome = go(&scratch, &models, "recently changed files", &mut script, &options);
    assert_eq!(outcome.commands, ["ls -ltr"]);
    assert_eq!(outcome.label.as_deref(), Some("list-files-by-time"));
    assert!(script.questions[1].starts_with("Pick a number"), "{:?}", script.questions);
}

#[test]
fn auto_mode_reads_an_environment_failure_as_a_right_route() {
    let scratch = Scratch::new("auto-env");
    let suggestion = json!({
        "verdict": "environment", "explanation": "This directory is not a git repository.",
        "options": [{ "command": "git init", "description": "start a repository" }],
    });
    let (models, replies) = models(|_| decision("git-status", 0.9, 0.9, SAFE), vec![("suggestion", suggestion)]);
    let mut script = Script::new(&[Some(true)], &[]);
    let outcome = go(&scratch, &models, "what changed here", &mut script, &scratch.options(Mode::Auto));
    assert_eq!(outcome.exit_code, Some(128));
    let skill = scratch.brain.skill("git-status").unwrap().unwrap();
    assert!(skill.counter_examples.is_empty());
    assert!(skill.examples.contains(&"what changed here".to_string()), "a human yes vouches for the route");
    let body = &replies.seen.lock().unwrap()[0];
    let input: Value = serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["failure"]["exitCode"], 128);
    assert!(input["failure"]["stderr"].as_str().unwrap().contains("not a git repository"));
}

#[test]
fn teaching_a_template_variant_needs_no_model() {
    let scratch = Scratch::new("teach-variant");
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "git-recent-commits".into(),
            steps: vec!["git log --oneline -n {count}".into()],
            scenario: "the user wants the most recent commits".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec!["show the last 3 commits".into()],
            slots: vec![Slot { name: "count".into(), kind: SlotKind::Number, description: "how many".into(), default: None }],
        })
        .unwrap();
    let (models, _) = models(|_| panic!("System 1 must not be asked"), no_language_model());
    let input = LearnInput {
        phrase: "last 10 commits please".into(),
        steps: vec!["git log --oneline -n 10".into()],
        risk: None,
        mode: Mode::Semi,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    let learned = learn_phrase(&scratch.brain, &models, &input, &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created), ("git-recent-commits", false));
    let skill = scratch.brain.skill("git-recent-commits").unwrap().unwrap();
    assert_eq!(skill.examples, ["show the last 3 commits", "last 10 commits please"]);
}

fn draft(steps: &str, slot_value: &str) -> Value {
    json!({
        "existingLabel": null, "label": "git-recent-commits", "steps": [steps],
        "scenario": "the user wants a one-line summary of the most recent commits, a given number of them",
        "slots": [{ "name": "count", "kind": "number", "description": "how many commits", "default": null }],
        "slotValues": [{ "name": "count", "value": slot_value }], "risk": "safe",
    })
}

#[test]
fn a_translated_row_is_kept_when_system1_confirms_it() {
    let scratch = Scratch::new("teach-verified");
    let (models, _) = models(
        |request| {
            if request["state"]["request"] == "show the last 3 commits" {
                decision("git-recent-commits", 0.92, 0.9, SAFE)
            } else {
                decision("git-status", 0.9, 0.9, SAFE)
            }
        },
        vec![("matrix_row", draft("git log --oneline -n {count}", "3"))],
    );
    let input = LearnInput {
        phrase: "show the last 3 commits".into(),
        steps: vec!["git log --oneline -n 3".into()],
        risk: None,
        mode: Mode::Semi,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    let learned = learn_phrase(&scratch.brain, &models, &input, &quiet()).unwrap();
    assert_eq!(learned, crate::learn::Learned { label: "git-recent-commits".into(), created: true, translated: true });
    let skill = scratch.brain.skill("git-recent-commits").unwrap().unwrap();
    assert_eq!(skill.steps, ["git log --oneline -n {count}"]);
    assert_eq!(skill.slots[0].name, "count");
    assert!(skill.scenario.starts_with("the user wants a one-line summary"));
}

#[test]
fn a_row_that_fails_verification_twice_keeps_the_phrase_as_its_scenario() {
    let scratch = Scratch::new("teach-rejected");
    let (models, replies) = models(
        |_| decision("git-status", 0.9, 0.9, SAFE),
        vec![
            ("matrix_row", draft("git log --oneline -n {count}", "3")),
            ("matrix_row", draft("git log --oneline -n {count}", "3")),
        ],
    );
    let input = LearnInput {
        phrase: "show the last 3 commits".into(),
        steps: vec!["git log --oneline -n 3".into()],
        risk: None,
        mode: Mode::Semi,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    let learned = learn_phrase(&scratch.brain, &models, &input, &quiet()).unwrap();
    assert_eq!((learned.created, learned.translated), (true, false));
    let skill = scratch.brain.skill(&learned.label).unwrap().unwrap();
    assert_eq!(skill.scenario, "the user wants: show the last 3 commits");
    assert_eq!(skill.steps, ["git log --oneline -n {count}"], "the faithful slots survive");
    let seen = replies.seen.lock().unwrap();
    let retry: Value = serde_json::from_str(seen[1]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(retry["previousAttemptProblem"].as_str().unwrap().starts_with("the phrase itself routed to git-status"));
}

#[test]
fn a_flag_is_never_a_slot() {
    let scratch = Scratch::new("teach-flag");
    let flagged = json!({
        "existingLabel": null, "label": "date-iso", "steps": ["date {format}"],
        "scenario": "the user wants today's date in ISO format",
        "slots": [{ "name": "format", "kind": "text", "description": "format", "default": null }],
        "slotValues": [{ "name": "format", "value": "-I" }], "risk": "safe",
    });
    let (models, _) = models(
        |request| {
            if request["state"]["request"] == "date in iso" {
                decision("date-iso", 0.9, 0.9, SAFE)
            } else {
                decision("git-status", 0.9, 0.9, SAFE)
            }
        },
        vec![("matrix_row", flagged)],
    );
    let input = LearnInput {
        phrase: "date in iso".into(),
        steps: vec!["date -I".into()],
        risk: None,
        mode: Mode::Semi,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    learn_phrase(&scratch.brain, &models, &input, &quiet()).unwrap();
    let skill = scratch.brain.skill("date-iso").unwrap().unwrap();
    assert_eq!(skill.steps, ["date -I"]);
    assert!(skill.slots.is_empty());
}

#[test]
fn an_unreachable_translator_falls_back_to_manual() {
    let scratch = Scratch::new("teach-offline");
    let (models, _) = models(|_| panic!("System 1 must not be asked"), no_language_model());
    let models = Models { llm: Llm::with_transport(|_| Err("connection refused".into())), ..models };
    let input = LearnInput {
        phrase: "how big is this folder".into(),
        steps: vec!["du -sh .".into()],
        risk: Some(Risk::Safe),
        mode: Mode::Semi,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    let learned = learn_phrase(&scratch.brain, &models, &input, &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created, learned.translated), ("du-sh", true, false));
    let skill = scratch.brain.skill("du-sh").unwrap().unwrap();
    assert_eq!((skill.scenario.as_str(), skill.risk), ("the user wants: how big is this folder", Risk::Safe));
}

#[test]
fn the_json_outcome_keeps_its_field_names_and_order() {
    let scratch = Scratch::new("json");
    let (models, _) = models(|_| decision("print-working-directory", 0.99, 0.4, SAFE), no_language_model());
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "where am I", &mut Script::default(), &options);
    let value = serde_json::to_value(&outcome).unwrap();
    let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        ["query", "path", "label", "commands", "confidence", "risk", "ran", "exitCode", "blocked", "learned", "answer", "output", "msSystem1", "msSystem2"]
    );
    assert_eq!(value["path"], "system1");
    assert_eq!(value["risk"], "safe");
    assert!(value["output"].as_str().unwrap().trim_end().ends_with("work"));
    assert_eq!(value["learned"], json!([]), "a 0.99 route teaches nothing new");
}
