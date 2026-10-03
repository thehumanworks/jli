//! The router and the learning loop, offline: System 1 is a fake transport, the language models
//! are a fake transport, and the person at the keyboard is a script.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::cli::{App, Fix, FixHow, Flags};
use crate::learn::{learn_phrase, LearnInput};
use crate::llm::Llm;
use crate::plan::{run_plans, RunFlags, SlotValue};
use crate::router::{route, Outcome, RouteOptions, DRY_RUN, NEEDS_CALLER_SHELL, NOT_A_COMMAND};
use crate::shell::{HandOff, Shell};
use crate::store::{now_iso, Brain, BrainConfig, NewSkill};
use crate::system1::{System1, NONE};
use crate::types::{error_code, Choice, Coded, Mode, Origin, Path, Risk, Slot, SlotKind};
use crate::ui::{Prompt, Ui};
use crate::Models;

/// Measuring the matrix, optimising it, and checking a row before it is written.
mod eval_tests;

/// A temp directory with a brain in it and an empty working directory.
struct Scratch {
    root: PathBuf,
    work: PathBuf,
    brain: Brain,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        // A git hook exports where the repository is, and in a worktree that path is absolute, so
        // `git` run in the scratch directory would find this repository. The tests expect no repository.
        for variable in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            std::env::remove_var(variable);
        }
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

/// System 1 that answers `before` until the matrix holds `label`, and routes there once it does:
/// a proposed skill passes the check it gets before it is stored.
fn once_known(label: &'static str, before: Value) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
    move |request| {
        let criteria = &request["questions"]["skill"]["criteria"];
        // A stored example of another skill still goes home, as the check expects of a neighbour.
        let owner = criteria.as_object().and_then(|rows| {
            rows.iter().find(|(_, row)| row["examples"].as_array().is_some_and(|e| e.contains(&request["state"]["request"])))
        });
        match owner {
            Some((owner, _)) if owner != label => decision(owner, 0.9, 0.95, SAFE),
            _ if criteria.get(label).is_some() => decision(label, 0.9, 0.95, SAFE),
            _ => before.clone(),
        }
    }
}

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
    /// What each question was about, as (question, the lines shown before it).
    shown: Vec<(String, Vec<String>)>,
}

impl Script {
    fn new(confirms: &[Option<bool>], asks: &[&str]) -> Script {
        Script {
            confirms: confirms.iter().copied().collect(),
            asks: asks.iter().map(|answer| Some(answer.to_string())).collect(),
            ..Script::default()
        }
    }

    /// The lines shown before the question that starts with `question`, joined.
    fn about(&self, question: &str) -> String {
        let shown = self.shown.iter().find(|(asked, _)| asked.starts_with(question));
        shown.unwrap_or_else(|| panic!("never asked {question}")).1.join("\n")
    }
}

impl Prompt for Script {
    fn read_line(&mut self) -> Option<String> {
        None
    }

    fn confirm(&mut self, about: &[String], question: &str) -> Option<bool> {
        self.questions.push(question.to_string());
        self.shown.push((question.to_string(), about.to_vec()));
        self.confirms.pop_front().expect("an unexpected confirmation")
    }

    fn ask(&mut self, about: &[String], question: &str) -> Option<String> {
        self.questions.push(question.to_string());
        self.shown.push((question.to_string(), about.to_vec()));
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

fn greeting_skill(scratch: &Scratch) {
    let slot = |name: &str, kind: SlotKind, default: Option<&str>| Slot {
        name: name.into(),
        kind,
        default: default.map(String::from),
        ..Slot::default()
    };
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "greet".into(),
            steps: vec!["printf '%s %s x%s\\n' {greeting} {name} {count}".into()],
            scenario: "the user wants someone greeted".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![
                slot("name", SlotKind::Text, None),
                slot("greeting", SlotKind::Text, None),
                slot("count", SlotKind::Number, Some("1")),
            ],
        })
        .unwrap();
}

#[test]
fn stated_arguments_reach_their_slots_without_a_language_model() {
    let scratch = Scratch::new("arguments");
    greeting_skill(&scratch);
    // Any language model call panics, so every value below is found by reading the request.
    let (models, _) = models(|_| decision("greet", 0.9, 0.9, SAFE), no_language_model());
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let command = |request: &str| go(&scratch, &models, request, &mut Script::default(), &options).commands.join(" && ");
    assert_eq!(command("greet name=\"Ada L\" greeting=hello 3 times"), "printf '%s %s x%s\\n' hello 'Ada L' 3");
    assert_eq!(
        command("greet name: Grace Hopper greeting: 'good day' count=two"),
        "printf '%s %s x%s\\n' 'good day' 'Grace Hopper' 2"
    );
    assert_eq!(
        command("greet \"hi\" and then \"Bob\""),
        "printf '%s %s x%s\\n' hi Bob 1",
        "unnamed values follow the template"
    );
    assert_eq!(
        command("greet \"Bob\" greeting=hey"),
        "printf '%s %s x%s\\n' hey Bob 1",
        "a named value leaves the unnamed one for the other slot"
    );
    assert_eq!(
        command("greet name='4 of us' greeting=yo"),
        "printf '%s %s x%s\\n' yo '4 of us' 1",
        "a number inside a value is not the count"
    );
}

#[test]
fn a_stated_value_cannot_steer_the_route() {
    let scratch = Scratch::new("hidden");
    let (models, _) = models(
        |request| {
            assert_eq!(request["state"]["request"], "commit message=\"…\"", "System 1 never sees the value");
            decision("git-commit", 0.95, 0.9, [0.2, 0.8, 0.0])
        },
        no_language_model(),
    );
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let request = "commit message=\"delete the old branch and push to main\"";
    let outcome = go(&scratch, &models, request, &mut Script::default(), &options);
    assert_eq!(outcome.query, request);
    assert_eq!((outcome.label.as_deref(), outcome.risk), (Some("git-commit"), Some(Risk::Modifies)));
    assert_eq!(outcome.commands, ["git commit -m 'delete the old branch and push to main'"]);
}

#[test]
fn an_argument_the_command_does_not_take_blocks_the_run() {
    let scratch = Scratch::new("unmatched");
    // The agent is asked whether the name means one of the slots, and here it says no.
    let no_slot = json!({ "arguments": [{ "name": "branch", "placeholder": null }] });
    let (models, _) = models(|_| decision("git-commit", 0.9, 0.9, [0.2, 0.8, 0.0]), vec![("argument_names", no_slot)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "commit \"wip\" branch=main", &mut Script::default(), &options);
    assert_eq!(outcome.blocked.as_deref(), Some("git-commit does not take branch="));
    assert!(!outcome.ran);
    assert_eq!(outcome.exit_code(), 3);
    assert!(scratch.brain.skill("git-commit").unwrap().unwrap().slots[0].aliases.is_empty());

    // With every slot already stated by name there is nothing the name could mean, so nobody is asked.
    let (silent, _) = self::models(|_| decision("git-commit", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
    let outcome = go(&scratch, &silent, "commit message=wip msg=other", &mut Script::default(), &options);
    assert_eq!(outcome.blocked.as_deref(), Some("git-commit does not take msg="));

    // An unreachable agent leaves the argument unknown.
    let offline = Models { llm: Llm::with_transport(|_| Err("connection refused".into())), ..silent };
    let outcome = go(&scratch, &offline, "commit msg=wip", &mut Script::default(), &options);
    assert_eq!(outcome.blocked.as_deref(), Some("git-commit does not take msg="));
}

#[test]
fn another_name_for_a_slot_is_worked_out_once_and_kept() {
    let scratch = Scratch::new("alias");
    greeting_skill(&scratch);
    let named = json!({ "arguments": [{ "name": "who", "placeholder": "name" }] });
    let (models, replies) = models(|_| decision("greet", 0.97, 0.9, SAFE), vec![("argument_names", named.clone())]);

    // A dry run uses the name and keeps nothing.
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "greet who=\"Ada L\" greeting=hello", &mut Script::default(), &dry);
    assert_eq!(outcome.commands, ["printf '%s %s x%s\\n' hello 'Ada L' 1"]);
    assert!(scratch.brain.skill("greet").unwrap().unwrap().slots[0].aliases.is_empty());
    {
        let seen = replies.seen.lock().unwrap();
        let input: Value = serde_json::from_str(seen[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(input["arguments"], json!([{ "name": "who", "value": "Ada L" }]));
        let offered: Vec<&str> =
            input["placeholders"].as_array().unwrap().iter().map(|slot| slot["name"].as_str().unwrap()).collect();
        assert_eq!(offered, ["name", "count"], "a slot the request already names is not on offer");
    }

    let (asked, _) = self::models(|_| decision("greet", 0.97, 0.9, SAFE), vec![("argument_names", named)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &asked, "greet who=\"Ada L\" greeting=hello", &mut Script::default(), &options);
    assert_eq!(outcome.output.as_deref(), Some("hello Ada L x1\n"));
    assert_eq!(outcome.learned, ["who= means {name} in greet"]);
    assert_eq!(scratch.brain.skill("greet").unwrap().unwrap().slots[0].aliases, ["who"]);

    // From now on the name is read by the parser: any language model call would panic.
    let (silent, _) = self::models(|_| decision("greet", 0.97, 0.9, SAFE), no_language_model());
    let outcome = go(&scratch, &silent, "greet WHO=Bob greeting=hey 2 times", &mut Script::default(), &options);
    assert_eq!(outcome.output.as_deref(), Some("hey Bob x2\n"));
    assert!(outcome.learned.is_empty());
    let outcome = go(&scratch, &silent, "say greeting: 'good day' to who: Grace Hopper", &mut Script::default(), &options);
    assert_eq!(outcome.output.as_deref(), Some("good day Grace Hopper x1\n"), "an alias works as a label too");
}

#[test]
fn the_agent_cannot_put_a_value_where_the_request_already_put_one() {
    let scratch = Scratch::new("alias-refused");
    greeting_skill(&scratch);
    // Two names for one slot, a slot that is not on offer, and a name the user never typed.
    let greedy = json!({ "arguments": [
        { "name": "who", "placeholder": "name" },
        { "name": "person", "placeholder": "name" },
        { "name": "who", "placeholder": "greeting" },
        { "name": "times", "placeholder": "count" },
    ] });
    let (models, _) = models(|_| decision("greet", 0.97, 0.9, SAFE), vec![("argument_names", greedy)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "greet who=Ada person=Bob greeting=hi", &mut Script::default(), &options);
    assert_eq!(outcome.blocked.as_deref(), Some("greet does not take person="));
    assert!(!outcome.ran);
    let slots = scratch.brain.skill("greet").unwrap().unwrap().slots;
    assert!(slots.iter().all(|slot| slot.aliases.is_empty()), "a blocked run keeps nothing");
}

#[test]
fn a_proposed_skill_keeps_only_the_aliases_that_hold_up() {
    let scratch = Scratch::new("alias-proposed");
    std::fs::create_dir(scratch.work.join("src")).unwrap();
    let proposal = json!({
        "kind": "new", "existingLabel": null, "label": "list-directory", "steps": ["ls {directory} | head -n {count}"],
        "scenario": "the user wants the first entries of a directory",
        "slots": [
            { "name": "directory", "kind": "file", "description": "the directory", "default": null, "aliases": ["dir", "Path", "count", "-d", ""] },
            { "name": "count", "kind": "number", "description": "how many", "default": "5", "aliases": ["n", "dir", "directory"] },
        ],
        "slotValues": [{ "name": "count", "value": "5" }],
        "risk": "safe", "explanation": "Lists.", "answer": null,
    });
    let (models, _) = models(once_known("list-directory", decision(NONE, 0.7, 0.95, SAFE)), vec![("proposal", proposal)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "first entries of dir=src", &mut Script::default(), &options);
    assert_eq!(outcome.commands, ["ls src | head -n 5"], "the user's own name reaches the slot the agent named properly");
    let slots = scratch.brain.skill("list-directory").unwrap().unwrap().slots;
    assert_eq!(slots[0].aliases, ["dir", "path"]);
    assert_eq!(slots[1].aliases, ["n"]);
}

#[test]
fn a_phrasing_is_learned_without_its_values() {
    let scratch = Scratch::new("phrase");
    greeting_skill(&scratch);
    let (models, _) = models(|_| decision("greet", 0.6, 0.9, SAFE), no_language_model());
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "say greeting: good evening name=\"Ada L\"", &mut Script::default(), &options);
    assert_eq!(outcome.output.as_deref(), Some("good evening Ada L x1\n"));
    assert_eq!(outcome.learned, ["\"say greeting: … name=\"…\"\" means greet"]);
    assert_eq!(scratch.brain.skill("greet").unwrap().unwrap().examples, ["say greeting: … name=\"…\""]);
    assert_eq!(
        scratch.brain.events(1).unwrap()[0].query,
        "say greeting: good evening name=\"Ada L\"",
        "history keeps the request whole"
    );
}

#[test]
fn a_stated_argument_outranks_the_agent() {
    let scratch = Scratch::new("outranks");
    let proposal = json!({
        "kind": "existing", "existingLabel": "git-commit", "label": null, "steps": [], "scenario": "",
        "slots": [], "slotValues": [{ "name": "message", "value": "the agent's own idea" }],
        "risk": "modifies", "explanation": "Commits.", "answer": null,
    });
    let (models, replies) = models(|_| decision(NONE, 0.8, 0.95, [0.2, 0.8, 0.0]), vec![("proposal", proposal)]);
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "save my work message=mine", &mut Script::default(), &options);
    assert_eq!(outcome.commands, ["git commit -m mine"]);
    let seen = replies.seen.lock().unwrap();
    let input: Value = serde_json::from_str(seen[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["arguments"], json!([{ "name": "message", "value": "mine" }]));
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
    let (models, _) =
        models(once_known("delete-log-files", decision(NONE, 0.8, 0.95, [0.1, 0.3, 0.6])), vec![("proposal", proposal)]);
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
    let (models, replies) = models(once_known("count-lines", decision(NONE, 0.7, 0.95, SAFE)), vec![("proposal", proposal)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "how many lines are in notes.txt", &mut Script::default(), &options);
    assert_eq!(outcome.path, Path::System2);
    assert_eq!(outcome.commands, ["wc -l notes.txt"]);
    assert!(outcome.ran && outcome.exit_code == Some(0));
    assert!(outcome.output.as_deref().unwrap().contains("notes.txt"));
    assert_eq!(outcome.learned, ["new skill count-lines"]);
    let skill = scratch.brain.skill("count-lines").unwrap().unwrap();
    assert_eq!((skill.origin, skill.uses), (Origin::Agent, 1));
    assert_eq!(skill.examples, ["how many lines are in …"], "the shape of the request, not the file it named");
    assert_eq!(skill.slots[0].kind, SlotKind::File);

    let body = &replies.seen.lock().unwrap()[0];
    assert_eq!(body["reasoning"], json!({ "enabled": false }), "Mercury stalls with reasoning on");
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
    assert_eq!(skill.examples, ["commit as \"…\""], "the phrasing is learned without its value");
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
    // A pre-push hook in a linked worktree inherits an absolute GIT_DIR, and with it `git status`
    // succeeds anywhere. This test needs it to fail outside a repository.
    std::env::remove_var("GIT_DIR");
    std::env::remove_var("GIT_WORK_TREE");
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
            slots: vec![Slot {
                name: "count".into(),
                kind: SlotKind::Number,
                description: "how many".into(),
                default: None,
                aliases: vec![],
                ..Slot::default()
            }],
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
    assert_eq!(
        learned,
        crate::learn::Learned { label: "git-recent-commits".into(), created: true, translated: true, upgraded: false }
    );
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

fn first_lines_skill(scratch: &Scratch) {
    std::fs::write(scratch.work.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "first-lines".into(),
            steps: vec!["head -n {count} notes.txt".into()],
            scenario: "the user wants the first lines of the notes".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec!["top of the notes".into()],
            slots: vec![Slot {
                name: "count".into(),
                kind: SlotKind::Number,
                description: "how many lines".into(),
                default: None,
                aliases: vec![],
                ..Slot::default()
            }],
        })
        .unwrap();
}

fn new_proposal(label: &str, step: &str, slots: Value, values: Value) -> Value {
    json!({
        "kind": "new", "existingLabel": null, "label": label, "steps": [step],
        "scenario": "the user wants it", "slots": slots, "slotValues": values,
        "risk": "safe", "explanation": "Does it.", "answer": null,
    })
}

#[test]
fn a_new_proposal_that_a_known_skill_already_runs_is_that_skill() {
    let scratch = Scratch::new("same-as-known");
    first_lines_skill(&scratch);
    let before = scratch.brain.skills().unwrap().len();
    // The agent calls the slot {lines} and gives its value under that name.
    let proposal = new_proposal(
        "head-of-notes",
        "head -n {lines} notes.txt",
        json!([{ "name": "lines", "kind": "number", "description": "how many", "default": null }]),
        json!([{ "name": "lines", "value": "2" }]),
    );
    let (models, _) = models(|_| decision("first-lines", 0.3, 0.95, SAFE), vec![("proposal", proposal)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "print the opening couple of lines", &mut Script::default(), &options);
    assert_eq!((outcome.path, outcome.label.as_deref()), (Path::System2, Some("first-lines")));
    assert_eq!(outcome.commands, ["head -n 2 notes.txt"], "the agent's value reaches the known slot");
    assert_eq!(outcome.output.as_deref(), Some("one\ntwo\n"));
    assert_eq!(outcome.learned, ["\"print the opening couple of lines\" means first-lines"]);
    assert_eq!(scratch.brain.skills().unwrap().len(), before, "no second row");
    let skill = scratch.brain.skill("first-lines").unwrap().unwrap();
    assert_eq!(skill.examples, ["top of the notes", "print the opening couple of lines"]);

    // A literal command that a seeded skill runs, and the same command once that skill is rejected.
    let proposal = new_proposal("list-by-date", "ls -ltr", json!([]), json!([]));
    let (models, _) = self::models(|_| decision(NONE, 0.4, 0.95, SAFE), vec![("proposal", proposal.clone())]);
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "files by date", &mut Script::default(), &dry);
    assert_eq!(outcome.label.as_deref(), Some("list-files-by-time"));
    let (models, _) = self::models(|_| decision(NONE, 0.4, 0.95, SAFE), vec![("proposal", proposal)]);
    let rejected = RouteOptions { rejected: vec!["list-files-by-time".into()], ..dry };
    let outcome = go(&scratch, &models, "files by date", &mut Script::default(), &rejected);
    // Its command is refused rather than stored as a second row (ADR 20).
    assert_eq!((code(&outcome), outcome.label.as_deref()), (Some("already_rejected"), Some("list-files-by-time")));
    assert!(scratch.brain.skill("list-by-date").unwrap().is_none());
}

#[test]
fn a_learned_phrasing_hides_every_value_and_jli_fix_finds_it_again() {
    let scratch = Scratch::new("shape");
    first_lines_skill(&scratch);
    greeting_skill(&scratch);
    let (models, _) = models(
        |request| match request["state"]["request"].as_str().unwrap() {
            text if text.starts_with("greet") => decision("greet", 0.6, 0.9, SAFE),
            _ => decision("first-lines", 0.6, 0.9, SAFE),
        },
        no_language_model(),
    );
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    for (request, learned) in [
        ("show the first three lines of the notes", "show the first … lines of the notes"),
        ("greet name=\"Ada L\" greeting=hello 3 times", "greet name=\"…\" greeting=… … times"),
    ] {
        let outcome = go(&scratch, &models, request, &mut Script::default(), &options);
        assert!(outcome.ran, "{request}");
        assert_eq!(outcome.learned, [format!("\"{learned}\" means {}", outcome.label.clone().unwrap())]);

        // What `jli fix` works out from the event log is exactly what was stored.
        let event = scratch.brain.events(1).unwrap().remove(0);
        let label = event.label.clone().unwrap();
        let skill = scratch.brain.skill(&label).unwrap().unwrap();
        assert!(skill.examples.contains(&learned.to_string()));
        let again = crate::slots::learned_phrase(&event.query, &skill.steps, &skill.slots, event.command.as_deref());
        assert_eq!(again, learned);
        assert!(scratch.brain.remove_example(&label, &again).unwrap());
    }
}

#[test]
fn exit_1_with_nothing_on_stderr_after_a_yes_still_teaches_a_new_skill() {
    let grep = || new_proposal("find-todos", "grep -rn TODO .", json!([]), json!([]));
    // Captured (--json) and streamed: in both, a new proposal's stderr is seen.
    for capture in [true, false] {
        let scratch = Scratch::new("exit-one");
        let (models, _) = models(once_known("find-todos", decision(NONE, 0.4, 0.95, SAFE)), vec![("proposal", grep())]);
        let options = RouteOptions { capture, ..scratch.options(Mode::Semi) };
        let outcome = go(&scratch, &models, "where are my todos", &mut Script::new(&[Some(true)], &[]), &options);
        assert_eq!(outcome.exit_code, Some(1), "grep found nothing");
        assert_eq!(outcome.learned, ["new skill find-todos"], "capture: {capture}");
    }

    // Under --yes nobody vouched for it, so only a clean exit counts.
    let scratch = Scratch::new("exit-one-yes");
    let (models, _) = models(once_known("find-todos", decision(NONE, 0.4, 0.95, SAFE)), vec![("proposal", grep())]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "where are my todos", &mut Script::default(), &options);
    assert_eq!(outcome.exit_code, Some(1));
    assert!(outcome.learned.is_empty());
    assert!(scratch.brain.skill("find-todos").unwrap().is_none());

    // Exit 1 with an error on stderr is a failure, yes or not.
    let cat = new_proposal("show-notes", "cat notes.txt", json!([]), json!([]));
    let (models, _) = self::models(once_known("show-notes", decision(NONE, 0.4, 0.95, SAFE)), vec![("proposal", cat)]);
    let options = scratch.options(Mode::Semi);
    let outcome = go(&scratch, &models, "show my notes", &mut Script::new(&[Some(true)], &[]), &options);
    assert_eq!(outcome.exit_code, Some(1));
    assert!(outcome.learned.is_empty());
    assert!(scratch.brain.skill("show-notes").unwrap().is_none());
}

#[test]
fn auto_mode_learns_a_new_skill_that_failed_where_it_ran() {
    let scratch = Scratch::new("auto-env-new");
    let proposal = new_proposal("git-branches", "git branch -a", json!([]), json!([]));
    let suggestion = json!({
        "verdict": "environment", "explanation": "This directory is not a git repository.", "options": [],
    });
    let (models, _) = models(
        once_known("git-branches", decision(NONE, 0.4, 0.95, SAFE)),
        vec![("proposal", proposal), ("suggestion", suggestion)],
    );
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Auto) };
    let outcome = go(&scratch, &models, "list every branch", &mut Script::default(), &options);
    assert_eq!(outcome.exit_code, Some(128));
    assert_eq!(outcome.learned, ["new skill git-branches"]);
    let skill = scratch.brain.skill("git-branches").unwrap().unwrap();
    assert_eq!(skill.examples, ["list every branch"]);
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
        [
            "query",
            "path",
            "label",
            "commands",
            "confidence",
            "risk",
            "ran",
            "exitCode",
            "blocked",
            "learned",
            "answer",
            "output",
            "msSystem1",
            "msSystem2",
            "callerShell",
            "handedOff",
            "eventId",
            "planId",
            "phrase",
            "values",
            "blockedCode",
            "missing",
            "unknownArguments",
            "accepts",
            "candidates",
            "stdout",
            "stderr",
            "diagnosis",
            "error",
            "errorMessage"
        ]
    );
    assert_eq!(value["eventId"], json!(scratch.brain.events(1).unwrap()[0].id));
    assert_eq!(value["candidates"][0], json!({ "label": "print-working-directory", "probability": 0.99 }));
    assert_eq!(value["stdout"], value["output"], "nothing on stderr, so stdout is all of the output");
    assert_eq!(value["stderr"], "");
    assert_eq!((value["blockedCode"].clone(), value["error"].clone()), (Value::Null, Value::Null));
    assert_eq!(value["path"], "system1");
    assert_eq!(value["risk"], "safe");
    assert!(value["output"].as_str().unwrap().trim_end().ends_with("work"));
    assert_eq!(value["learned"], json!([]), "a 0.99 route teaches nothing new");
}

/// A skill with one step template per entry and one file slot called `directory`.
fn directory_skill(scratch: &Scratch, label: &str, steps: &[&str]) {
    scratch
        .brain
        .add_skill(&NewSkill {
            label: label.into(),
            steps: steps.iter().map(|step| step.to_string()).collect(),
            scenario: format!("the user wants {label}"),
            risk: Risk::Safe,
            origin: Origin::Agent,
            examples: vec![],
            slots: vec![Slot {
                name: "directory".into(),
                kind: SlotKind::File,
                description: "the directory".into(),
                default: None,
                aliases: vec!["dir".into()],
                ..Slot::default()
            }],
        })
        .unwrap();
}

/// The file the shell function would have made, empty.
fn hand_off(scratch: &Scratch, shell: Shell) -> HandOff {
    let path = scratch.root.join("handoff");
    std::fs::write(&path, "").unwrap();
    HandOff { path, shell }
}

#[test]
fn a_cd_without_the_shell_function_is_refused_and_teaches_nothing() {
    let scratch = Scratch::new("cd-refused");
    std::fs::create_dir(scratch.work.join("sub")).unwrap();
    directory_skill(&scratch, "go-to", &["cd {directory}"]);
    let (first, _) = models(|_| decision("go-to", 0.9, 0.9, SAFE), no_language_model());
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &first, "go to dir=sub", &mut Script::default(), &options);
    assert!(!outcome.ran && !outcome.handed_off);
    assert_eq!(outcome.blocked.as_deref(), Some(NEEDS_CALLER_SHELL));
    assert_eq!(outcome.exit_code(), 3, "not reported as a success");
    assert_eq!(outcome.caller_shell, ["cd sub"], "an agent can apply it itself");
    assert!(outcome.learned.is_empty());
    let skill = scratch.brain.skill("go-to").unwrap().unwrap();
    assert!(skill.examples.is_empty() && skill.uses == 0, "no phrasing, no use");

    // A new skill the agent proposes is not learned either.
    let proposal = json!({
        "kind": "new", "existingLabel": null, "label": "enter-directory", "steps": ["cd {directory}"],
        "scenario": "the user wants to move into a directory",
        "slots": [{ "name": "directory", "kind": "file", "description": "where to go", "default": null }],
        "slotValues": [{ "name": "directory", "value": "sub" }],
        "risk": "safe", "explanation": "Changes directory.", "answer": null,
    });
    let (models, _) = models(|_| decision(NONE, 0.7, 0.95, SAFE), vec![("proposal", proposal)]);
    let outcome = go(&scratch, &models, "move into sub", &mut Script::default(), &options);
    assert_eq!(outcome.blocked.as_deref(), Some(NEEDS_CALLER_SHELL));
    assert!(scratch.brain.skill("enter-directory").unwrap().is_none());
}

#[test]
fn a_cd_is_handed_to_the_shell_function_and_learned_like_any_command() {
    let scratch = Scratch::new("cd-handed");
    std::fs::create_dir(scratch.work.join("my sub")).unwrap();
    directory_skill(&scratch, "go-to", &["cd {directory}"]);
    let (models, _) = models(|_| decision("go-to", 0.9, 0.9, SAFE), no_language_model());
    let target = hand_off(&scratch, Shell::Zsh);
    let options = RouteOptions { yes: true, hand_off: Some(target.clone()), ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "go to dir=\"my sub\"", &mut Script::default(), &options);
    assert!(outcome.ran && outcome.handed_off);
    assert_eq!((outcome.exit_code(), outcome.blocked.as_deref()), (0, None));
    assert_eq!(outcome.caller_shell, ["cd 'my sub'"]);
    assert_eq!(std::fs::read_to_string(&target.path).unwrap(), "cd 'my sub'\n", "exactly the rendered step");
    assert_eq!(outcome.learned, ["\"go to dir=\"…\"\" means go-to"]);
    let json = serde_json::to_value(&outcome).unwrap();
    assert_eq!((json["callerShell"].clone(), json["handedOff"].clone()), (json!(["cd 'my sub'"]), json!(true)));
}

#[test]
fn a_cd_that_would_fail_fails_here_and_is_not_handed_over() {
    let scratch = Scratch::new("cd-missing");
    directory_skill(&scratch, "go-to", &["cd {directory}"]);
    let (models, _) = models(|_| decision("go-to", 0.9, 0.9, SAFE), no_language_model());
    let target = hand_off(&scratch, Shell::Bash);
    let options = RouteOptions { yes: true, hand_off: Some(target.clone()), ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "go to dir=nowhere", &mut Script::default(), &options);
    assert!(outcome.ran && !outcome.handed_off);
    assert_eq!(outcome.exit_code(), 1);
    assert!(outcome.output.as_deref().unwrap().contains("nowhere"), "{:?}", outcome.output);
    assert_eq!(std::fs::read_to_string(&target.path).unwrap(), "");
    assert!(outcome.learned.is_empty(), "--yes needs a clean exit");
}

#[test]
fn steps_after_a_cd_run_in_the_calling_shell_too() {
    let scratch = Scratch::new("cd-chain");
    directory_skill(&scratch, "make-and-enter", &["mkdir -p {directory}", "cd {directory}", "touch it's-here"]);
    let (models, _) = models(|_| decision("make-and-enter", 0.9, 0.9, SAFE), no_language_model());
    let target = hand_off(&scratch, Shell::Zsh);
    let options = RouteOptions { yes: true, hand_off: Some(target.clone()), ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "make and enter dir=new", &mut Script::default(), &options);
    assert!(scratch.work.join("new").is_dir(), "the step before the cd ran here");
    assert!(!scratch.work.join("new/it's-here").exists(), "the step after it did not");
    assert_eq!(outcome.caller_shell, ["cd new", "touch it's-here"]);
    assert_eq!(std::fs::read_to_string(&target.path).unwrap(), "{ cd new\n} && { touch it's-here\n}\n");
    assert!(outcome.handed_off);
}

#[test]
fn a_fish_shell_gets_values_quoted_for_fish() {
    let scratch = Scratch::new("cd-fish");
    std::fs::create_dir(scratch.work.join("it's")).unwrap();
    directory_skill(&scratch, "go-to", &["cd {directory}"]);
    let (models, _) = models(|_| decision("go-to", 0.9, 0.9, SAFE), no_language_model());
    let target = hand_off(&scratch, Shell::Fish);
    let options = RouteOptions { yes: true, hand_off: Some(target.clone()), ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "go to dir=\"it's\"", &mut Script::default(), &options);
    assert_eq!(outcome.commands, [r"cd 'it'\''s'"], "jli's own shell is still bash");
    assert_eq!(std::fs::read_to_string(&target.path).unwrap(), "cd 'it\\'s'\n");
}

#[test]
fn a_dry_run_shows_what_the_calling_shell_would_run() {
    let scratch = Scratch::new("cd-dry");
    directory_skill(&scratch, "go-to", &["cd {directory}"]);
    let (models, _) = models(|_| decision("go-to", 0.9, 0.9, SAFE), no_language_model());
    let target = hand_off(&scratch, Shell::Zsh);
    for hand_off in [None, Some(target.clone())] {
        let options = RouteOptions { dry_run: true, hand_off, ..scratch.options(Mode::Semi) };
        let outcome = go(&scratch, &models, "go to dir=src", &mut Script::default(), &options);
        assert_eq!((outcome.blocked.as_deref(), outcome.exit_code()), (Some(DRY_RUN), 0));
        assert_eq!((outcome.caller_shell.as_slice(), outcome.handed_off), (["cd src".to_string()].as_slice(), false));
    }
    assert_eq!(std::fs::read_to_string(&target.path).unwrap(), "");
}

#[test]
fn a_typed_cd_after_a_no_needs_the_shell_function_too() {
    let scratch = Scratch::new("cd-typed");
    std::fs::create_dir(scratch.work.join("sub")).unwrap();
    let (models, _) = models(|_| decision("list-files", 0.9, 0.9, SAFE), no_language_model());
    let mut script = Script::new(&[Some(false)], &["cd sub"]);
    let outcome = go(&scratch, &models, "into sub", &mut script, &scratch.options(Mode::Manual));
    assert_eq!(outcome.blocked.as_deref(), Some(NEEDS_CALLER_SHELL));
    assert!(!outcome.ran);
    assert!(!scratch.brain.skills().unwrap().iter().any(|skill| skill.steps == ["cd sub"]), "not learned");

    let target = hand_off(&scratch, Shell::Bash);
    let mut script = Script::new(&[Some(false)], &["cd sub"]);
    let options = RouteOptions { hand_off: Some(target.clone()), ..scratch.options(Mode::Manual) };
    let outcome = go(&scratch, &models, "into sub", &mut script, &options);
    assert!(outcome.ran && outcome.handed_off);
    assert_eq!(outcome.path, Path::Taught);
    assert_eq!(std::fs::read_to_string(&target.path).unwrap(), "cd sub\n");
}

#[test]
fn a_home_path_reaches_the_command_expanded_and_text_stays_literal() {
    let scratch = Scratch::new("expand");
    directory_skill(&scratch, "list-dir", &["ls {directory}"]);
    let home = std::env::var("HOME").unwrap();
    let (listing, _) = models(|_| decision("list-dir", 0.9, 0.9, SAFE), no_language_model());
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let expected = |path: &str| crate::slots::render(&["ls {d}".to_string()], &[], &[("d".into(), path.into())].into());
    for request in ["list dir=~/notes", "list dir=$HOME/notes", "list dir=${HOME}/notes", "list dir: ~/notes"] {
        let outcome = go(&scratch, &listing, request, &mut Script::default(), &options);
        assert_eq!(outcome.commands, expected(&format!("{home}/notes")), "{request}");
    }
    let outcome = go(&scratch, &listing, "list dir='$(touch pwned)/x'", &mut Script::default(), &options);
    assert_eq!(outcome.commands, ["ls '$(touch pwned)/x'"], "nothing is run to expand a value");

    let (models, _) = models(|_| decision("git-commit", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
    let outcome = go(&scratch, &models, "commit message='costs $5, see ~/x and $HOME'", &mut Script::default(), &options);
    assert_eq!(outcome.commands, ["git commit -m 'costs $5, see ~/x and $HOME'"], "a text value is as written");
}

#[test]
fn a_path_in_the_request_is_offered_to_the_file_pick() {
    let scratch = Scratch::new("pick-path");
    std::fs::write(scratch.work.join("n.txt"), "x\n").unwrap();
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "count-lines".into(),
            steps: vec!["wc -l {file}".into()],
            scenario: "the user wants the lines of a file counted".into(),
            risk: Risk::Safe,
            origin: Origin::Agent,
            examples: vec![],
            slots: vec![Slot {
                name: "file".into(),
                kind: SlotKind::File,
                description: "the file to count".into(),
                default: None,
                aliases: vec![],
                ..Slot::default()
            }],
        })
        .unwrap();
    let (models, _) = models(
        |request| {
            let Some(pick) = request["questions"].get("value") else { return decision("count-lines", 0.9, 0.9, SAFE) };
            let options = pick.to_string();
            assert!(options.contains("n.txt") && options.contains("~/notes/n.txt"), "{options}");
            json!({"model": "m", "answers": {"value": {"type": "choice", "choice": "~/notes/n.txt", "confidence": 0.9,
                                                      "probabilities": {"~/notes/n.txt": 0.9}}}})
        },
        no_language_model(),
    );
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "count the lines of ~/notes/n.txt", &mut Script::default(), &options);
    let home = std::env::var("HOME").unwrap();
    let expected =
        crate::slots::render(&["wc -l {f}".to_string()], &[], &[("f".into(), format!("{home}/notes/n.txt"))].into());
    assert_eq!(outcome.commands, expected);
}

#[test]
fn a_skill_one_user_taught_is_invisible_to_every_other_user() {
    let scratch = Scratch::new("users");
    let path = scratch.root.join("brain.db");
    let agent = Brain::open_as(BrainConfig::file(&path), "agent").unwrap();
    let taught = LearnInput {
        phrase: "how big is this folder".into(),
        steps: vec!["du -sh .".into()],
        risk: Some(Risk::Safe),
        mode: Mode::Manual,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    let (silent, _) = models(|_| panic!("manual teaching asks nobody"), no_language_model());
    assert_eq!(learn_phrase(&agent, &silent, &taught, &quiet()).unwrap().label, "du-sh");

    // System 1 is shown each user's own matrix: the default user's and another user's do not hold it.
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    for brain in [&scratch.brain, &Brain::open_as(BrainConfig::file(&path), "other").unwrap()] {
        let (models, _) = models(
            |request| {
                let sent = request.to_string();
                assert!(
                    sent.contains("git status") && !sent.contains("du -sh ."),
                    "another user's skill reached the matrix"
                );
                decision(NONE, 0.9, 0.05, SAFE)
            },
            no_language_model(),
        );
        let outcome = route(brain, &models, "how big is this folder", &quiet(), &mut Script::default(), &options).unwrap();
        assert!(outcome.label.is_none() && !outcome.ran);
    }

    let (models, _) = models(
        |request| {
            assert!(request.to_string().contains("du -sh ."));
            decision("du-sh", 0.99, 0.9, SAFE)
        },
        no_language_model(),
    );
    let outcome = route(&agent, &models, "how big is this folder", &quiet(), &mut Script::default(), &options).unwrap();
    assert_eq!((outcome.label.as_deref(), outcome.ran), (Some("du-sh"), true));
    assert_eq!(agent.events(10).unwrap().len(), 1);
    assert_eq!(agent.skill("du-sh").unwrap().unwrap().uses, 1);
    assert_eq!(scratch.brain.events(10).unwrap().len(), 1, "the default user's own requests only");
    assert!(
        scratch.brain.last_executed(&scratch.work.to_string_lossy()).unwrap().is_none(),
        "so its fix cannot reach the agent's run"
    );
}

#[test]
fn a_label_taken_while_the_agent_was_thinking_is_not_overwritten() {
    let scratch = Scratch::new("label-race");
    std::fs::write(scratch.work.join("notes.txt"), "one\ntwo\n").unwrap();
    let path = scratch.root.join("brain.db");
    let proposal = json!({
        "kind": "new", "existingLabel": null, "label": "count-lines", "steps": ["wc -l {file}"],
        "scenario": "the user wants the number of lines in a file",
        "slots": [{ "name": "file", "kind": "file", "description": "the file to count", "default": null }],
        "slotValues": [{ "name": "file", "value": "notes.txt" }],
        "risk": "safe", "explanation": "Counts lines.", "answer": null,
    });
    // After this request read the matrix, another jli on the same brain stores its own count-lines.
    let (models, _) = models(
        move |request| {
            let other = Brain::open(BrainConfig::file(&path)).unwrap();
            other
                .add_skill_if_missing(&NewSkill {
                    label: "count-lines".into(),
                    steps: vec!["grep -c . {file}".into()],
                    scenario: "theirs".into(),
                    risk: Risk::Safe,
                    origin: Origin::Taught,
                    examples: vec![],
                    slots: vec![],
                })
                .unwrap();
            once_known("count-lines", decision(NONE, 0.7, 0.95, SAFE))(request)
        },
        vec![("proposal", proposal)],
    );
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "how many lines are in notes.txt", &mut Script::default(), &options);
    assert!(outcome.ran);
    assert_eq!(outcome.label.as_deref(), Some("count-lines-x"));
    assert_eq!(outcome.learned, ["new skill count-lines-x"]);
    assert_eq!(scratch.brain.skill("count-lines").unwrap().unwrap().steps, ["grep -c . {file}"]);
    let mine = scratch.brain.skill("count-lines-x").unwrap().unwrap();
    assert_eq!((mine.steps.as_slice(), mine.uses), (["wc -l {file}".to_string()].as_slice(), 1));
    assert_eq!(scratch.brain.events(1).unwrap()[0].label.as_deref(), Some("count-lines-x"));
}

#[test]
fn a_label_taken_while_the_translator_was_writing_is_not_overwritten() {
    let scratch = Scratch::new("teach-race");
    let path = scratch.root.join("brain.db");
    let row = draft("git log --oneline -n {count}", "3");
    let llm = Llm::with_transport(move |_| {
        // Another jli takes the label between this teach reading the matrix and writing its row.
        Brain::open(BrainConfig::file(&path))
            .unwrap()
            .add_skill_if_missing(&NewSkill {
                label: "git-recent-commits".into(),
                steps: vec!["git log -n {count}".into()],
                scenario: "theirs".into(),
                risk: Risk::Safe,
                origin: Origin::Taught,
                examples: vec![],
                slots: vec![],
            })
            .unwrap();
        Ok((200, json!({ "choices": [{ "message": { "content": row.to_string() } }] }).to_string()))
    });
    let system1 = System1::fake(|request| {
        // The row is checked in memory under the label it was given, before the other writer's row is seen.
        if request["state"]["request"] == "show the last 3 commits" {
            decision("git-recent-commits", 0.92, 0.9, SAFE)
        } else {
            decision("git-status", 0.9, 0.9, SAFE)
        }
    });
    let input = LearnInput {
        phrase: "show the last 3 commits".into(),
        steps: vec!["git log --oneline -n 3".into()],
        risk: None,
        mode: Mode::Semi,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    let learned = learn_phrase(&scratch.brain, &Models { system1, llm }, &input, &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created, learned.translated), ("git-recent-commits-x", true, true));
    assert_eq!(scratch.brain.skill("git-recent-commits").unwrap().unwrap().scenario, "theirs");
    assert_eq!(scratch.brain.skill("git-recent-commits-x").unwrap().unwrap().steps, ["git log --oneline -n {count}"]);
}

// ── Plans, blocked codes and corrections without a terminal (ADR 18) ─────────

fn plans(scratch: &Scratch, models: &Models, ids: &[i64], values: &[(&str, &str)], script: &mut Script) -> Vec<Outcome> {
    plans_with(scratch, models, ids, values, script, RunFlags { yolo: false, capture: true })
}

fn plans_with(
    scratch: &Scratch,
    models: &Models,
    ids: &[i64],
    values: &[(&str, &str)],
    script: &mut Script,
    flags: RunFlags,
) -> Vec<Outcome> {
    let values: Vec<(String, String)> = values.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    run_plans(&scratch.brain, models, ids, &values, &quiet(), script, flags).unwrap()
}

fn no_model_at_all() -> Models {
    models(|_| panic!("a plan runs with no System 1 call"), no_language_model()).0
}

fn code(outcome: &Outcome) -> Option<&str> {
    outcome.blocked_code.as_deref()
}

#[test]
fn a_planned_proposal_runs_as_planned_once_with_no_model_call() {
    let scratch = Scratch::new("plan-run");
    std::fs::write(scratch.work.join("notes.txt"), "one\ntwo\n").unwrap();
    let proposal = new_proposal(
        "count-lines",
        "wc -l {file}",
        json!([{ "name": "file", "kind": "file", "description": "the file to count", "default": null }]),
        json!([{ "name": "file", "value": "notes.txt" }]),
    );
    let (models, _) = models(|_| decision(NONE, 0.7, 0.95, SAFE), vec![("proposal", proposal)]);
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let planned = go(&scratch, &models, "how many lines are in notes.txt", &mut Script::default(), &dry);
    assert_eq!((code(&planned), planned.exit_code()), (Some("dry_run"), 0));
    assert_eq!(planned.phrase.as_deref(), Some("how many lines are in …"));
    assert_eq!(planned.values, [SlotValue { name: "file".into(), value: "notes.txt".into(), source: "agent".into() }]);
    let id = planned.plan_id.expect("a dry run stores a plan");
    assert!(scratch.brain.events(10).unwrap().is_empty(), "a dry run still logs no event");
    assert!(scratch.brain.skill("count-lines").unwrap().is_none(), "and learns nothing");

    // Every model call panics from here on.
    let silent = no_model_at_all();
    let ran = plans(&scratch, &silent, &[id], &[], &mut Script::default());
    let [outcome] = ran.as_slice() else { panic!("one outcome per plan") };
    assert!(outcome.ran && outcome.exit_code == Some(0));
    assert_eq!(outcome.commands, planned.commands, "the commands run are the commands planned");
    assert_eq!(outcome.stdout.as_deref().map(str::trim), Some("2 notes.txt"));
    assert_eq!((outcome.path, outcome.plan_id), (Path::System2, Some(id)));
    assert_eq!(
        outcome.learned,
        ["new skill count-lines", "count-lines was not checked with System 1"],
        "a plan asks no model"
    );
    let skill = scratch.brain.skill("count-lines").unwrap().unwrap();
    assert_eq!((skill.examples.as_slice(), skill.uses), (["how many lines are in …".to_string()].as_slice(), 1));
    let event = scratch.brain.events(1).unwrap().remove(0);
    assert_eq!((Some(event.id), event.cwd.as_str()), (outcome.event_id, scratch.work.to_str().unwrap()));
    assert_eq!(event.command.as_deref(), Some("wc -l notes.txt"));

    let again = plans(&scratch, &silent, &[id], &[], &mut Script::default());
    assert_eq!((code(&again[0]), again[0].ran, again[0].exit_code()), (Some("plan_already_run"), false, 3));
    assert_eq!(scratch.brain.events(10).unwrap().len(), 1);
}

#[test]
fn a_plan_is_refused_after_it_expires_and_to_another_user() {
    let scratch = Scratch::new("plan-refused");
    let (models, _) = models(|_| decision("list-files-by-time", 0.98, 0.9, SAFE), no_language_model());
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let first = go(&scratch, &models, "newest files", &mut Script::default(), &dry).plan_id.unwrap();
    let second = go(&scratch, &models, "newest files", &mut Script::default(), &dry).plan_id.unwrap();

    let other = Brain::open_as(BrainConfig::file(&scratch.root.join("brain.db")), "other").unwrap();
    let outcomes =
        run_plans(&other, &no_model_at_all(), &[first], &[], &quiet(), &mut Script::default(), RunFlags::default()).unwrap();
    assert_eq!(code(&outcomes[0]), Some("plan_not_found"), "another user's plan is not theirs to see");
    assert!(other.events(10).unwrap().is_empty());

    scratch.brain.expire_plan(second).unwrap();
    let outcomes = plans(&scratch, &no_model_at_all(), &[second, 999], &[], &mut Script::default());
    assert_eq!(code(&outcomes[0]), Some("plan_expired"));
    assert_eq!(code(&outcomes[1]), Some("not_run"), "nothing after a refusal runs");
    let outcomes = plans(&scratch, &no_model_at_all(), &[999], &[], &mut Script::default());
    assert_eq!(code(&outcomes[0]), Some("plan_not_found"));

    // The owner can still run the one that has not expired.
    let outcomes = plans(&scratch, &no_model_at_all(), &[first], &[], &mut Script::default());
    assert!(outcomes[0].ran);
}

#[test]
fn a_destructive_plan_still_needs_yolo() {
    let scratch = Scratch::new("plan-destructive");
    let log = scratch.work.join("old.log");
    std::fs::write(&log, "noise\n").unwrap();
    let proposal = new_proposal("delete-log-files", "rm -f *.log", json!([]), json!([]));
    let (models, _) = models(|_| decision(NONE, 0.8, 0.95, [0.1, 0.3, 0.6]), vec![("proposal", proposal)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let blocked = go(&scratch, &models, "delete all the log files", &mut Script::new(&[None], &[]), &options);
    assert_eq!(code(&blocked), Some("destructive"));
    let id = blocked.plan_id.expect("a run nobody could confirm stores a plan");
    assert_eq!(scratch.brain.events(10).unwrap().len(), 1, "a blocked run is logged as before");

    let refused = plans(&scratch, &no_model_at_all(), &[id], &[], &mut Script::new(&[None], &[]));
    assert_eq!((code(&refused[0]), refused[0].ran), (Some("destructive"), false));
    assert!(log.exists());
    let declined = plans(&scratch, &no_model_at_all(), &[id], &[], &mut Script::new(&[Some(false)], &[]));
    assert_eq!(code(&declined[0]), Some("declined"));

    let yolo = RunFlags { yolo: true, capture: true };
    let ran = plans_with(&scratch, &no_model_at_all(), &[id], &[], &mut Script::default(), yolo);
    assert!(ran[0].ran && ran[0].exit_code == Some(0), "refusing it did not use it up");
    assert!(!log.exists());
}

#[test]
fn plans_run_in_order_and_stop_at_the_first_failure() {
    let scratch = Scratch::new("plan-chain");
    for (label, step) in
        [("say-one", "echo one >> trail"), ("show-missing", "cat missing.txt"), ("say-three", "echo three >> trail")]
    {
        scratch
            .brain
            .add_skill(&NewSkill {
                label: label.into(),
                steps: vec![step.into()],
                scenario: format!("runs {step}"),
                risk: Risk::Safe,
                origin: Origin::Taught,
                examples: vec![],
                slots: vec![],
            })
            .unwrap();
    }
    let (models, _) =
        models(|request| decision(request["state"]["request"].as_str().unwrap(), 0.99, 0.9, SAFE), no_language_model());
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let id = |label: &str| go(&scratch, &models, label, &mut Script::default(), &dry).plan_id.unwrap();
    let (one, missing, three) = (id("say-one"), id("show-missing"), id("say-three"));

    let outcomes = plans(&scratch, &no_model_at_all(), &[one, three], &[], &mut Script::default());
    assert!(outcomes.iter().all(|outcome| outcome.ran && outcome.exit_code == Some(0)));
    assert_eq!(std::fs::read_to_string(scratch.work.join("trail")).unwrap(), "one\nthree\n", "in the order given");

    let (one, three) = (id("say-one"), id("say-three"));
    let outcomes = plans(&scratch, &no_model_at_all(), &[one, missing, three], &[], &mut Script::default());
    assert_eq!(outcomes.len(), 3, "every plan gets an outcome");
    assert_eq!((outcomes[1].ran, outcomes[1].exit_code), (true, Some(1)));
    assert!(outcomes[1].stderr.as_deref().unwrap().contains("missing.txt"));
    assert_eq!((code(&outcomes[2]), outcomes[2].ran, outcomes[2].plan_id), (Some("not_run"), false, Some(three)));
    assert_eq!(outcomes[2].commands, ["echo three >> trail"]);
    assert_eq!(std::fs::read_to_string(scratch.work.join("trail")).unwrap(), "one\nthree\none\n");
    assert!(
        plans(&scratch, &no_model_at_all(), &[three], &[], &mut Script::default())[0].ran,
        "a plan not run is still there"
    );
}

#[test]
fn missing_values_are_given_to_jli_run_without_routing() {
    let scratch = Scratch::new("plan-missing");
    greeting_skill(&scratch);
    let nothing = json!({ "values": [{ "name": "name", "value": null }] });
    let (models, _) = models(|_| decision("greet", 0.9, 0.9, SAFE), vec![("slot_values", nothing)]);
    let blocked = go(&scratch, &models, "greet greeting=hello", &mut Script::default(), &scratch.options(Mode::Semi));
    assert_eq!((code(&blocked), blocked.exit_code()), (Some("missing_value"), 3));
    let names: Vec<&str> = blocked.missing.iter().map(|slot| slot.name.as_str()).collect();
    assert_eq!(names, ["name"]);
    assert_eq!(blocked.accepts.len(), 3, "every slot the skill takes");
    let id = blocked.plan_id.expect("an incomplete plan is stored too");

    let silent = no_model_at_all();
    let wrong = vec![("nobody".to_string(), "x".to_string())];
    let refused = run_plans(&scratch.brain, &silent, &[id], &wrong, &quiet(), &mut Script::default(), RunFlags::default());
    assert!(refused.is_err(), "a value no plan is missing is refused before anything runs");
    let still = plans(&scratch, &silent, &[id], &[], &mut Script::default());
    assert_eq!((code(&still[0]), still[0].missing.len()), (Some("missing_value"), 1));
    let ran = plans(&scratch, &silent, &[id], &[("name", "Ada 'L'")], &mut Script::default());
    assert_eq!(ran[0].output.as_deref(), Some("hello Ada 'L' x1\n"), "the caller's value, as written");
    let given = ran[0].values.iter().find(|value| value.name == "name").unwrap();
    assert_eq!(given.source, "caller");
    assert_eq!(scratch.brain.skill("greet").unwrap().unwrap().examples, ["greet greeting=…"]);
}

#[test]
fn an_unknown_argument_is_a_plan_waiting_for_its_values() {
    let scratch = Scratch::new("plan-unknown");
    greeting_skill(&scratch);
    let no_slot = json!({ "arguments": [{ "name": "who", "placeholder": null }] });
    let (models, _) = models(|_| decision("greet", 0.9, 0.9, SAFE), vec![("argument_names", no_slot)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let blocked = go(&scratch, &models, "greet who=Ada greeting=hi", &mut Script::default(), &options);
    assert_eq!(code(&blocked), Some("unknown_argument"));
    assert_eq!(blocked.unknown_arguments, ["who"]);
    let names: Vec<&str> = blocked.missing.iter().map(|slot| slot.name.as_str()).collect();
    assert_eq!(names, ["name", "count"], "fill_slots stops at the unknown argument, so these never got a value");
    let id = blocked.plan_id.unwrap();
    let ran = plans(&scratch, &no_model_at_all(), &[id], &[("name", "Ada"), ("count", "two")], &mut Script::default());
    assert_eq!(ran[0].output.as_deref(), Some("hi Ada x2\n"), "a number slot reads a number word, as in a request");
}

#[test]
fn every_way_of_not_running_has_its_code() {
    let scratch = Scratch::new("codes");
    let (models, _) = models(|_| decision(NONE, 0.9, 0.05, SAFE), no_language_model());
    let outcome = go(&scratch, &models, "how much is 2 + 2", &mut Script::default(), &scratch.options(Mode::Semi));
    assert_eq!((code(&outcome), outcome.plan_id), (Some("not_a_command"), None));

    let (models, _) = self::models(|_| decision("git-stage-all", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
    let outcome = go(&scratch, &models, "stage everything", &mut Script::new(&[None], &[]), &scratch.options(Mode::Semi));
    assert_eq!(code(&outcome), Some("needs_confirmation"));
    assert!(outcome.plan_id.is_some() && outcome.event_id.is_some());
    let mut declining = Script::new(&[Some(false)], &[""]);
    let outcome = go(&scratch, &models, "stage everything", &mut declining, &scratch.options(Mode::Semi));
    assert_eq!((code(&outcome), outcome.plan_id), (Some("declined"), None), "a no is an answer, not a plan");

    let (models, _) = self::models(|_| decision("git-commit", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "commit message=wip msg=other", &mut Script::default(), &options);
    assert_eq!(code(&outcome), Some("unknown_argument"));
    // missing_value, dry_run, destructive and the plan codes have their own tests above, and
    // already_rejected has the_agent_cannot_bring_back_a_rejected_command_as_a_second_skill.
}

#[test]
fn auto_mode_reports_its_diagnosis_when_nobody_can_answer() {
    let scratch = Scratch::new("diagnosis");
    let suggestion = json!({
        "verdict": "wrong_command", "explanation": "That shows the status; the request was about branches.",
        "options": [{ "command": "git branch", "description": "the local branches" }],
    });
    let (models, _) = models(|_| decision("git-status", 0.9, 0.9, SAFE), vec![("suggestion", suggestion)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Auto) };
    let mut script = Script::default();
    script.asks.push_back(None);
    let outcome = go(&scratch, &models, "which branches are there", &mut script, &options);
    assert!(outcome.ran);
    let diagnosis = outcome.diagnosis.expect("the diagnosis that was paid for is returned");
    assert_eq!(diagnosis.options[0].command, "git branch");
    let value = serde_json::to_value(&diagnosis).unwrap();
    assert_eq!(value["verdict"], "wrong_command");
}

#[test]
fn a_storage_failure_after_the_command_ran_does_not_hide_that_it_ran() {
    let scratch = Scratch::new("after-run");
    let root = scratch.root.display().to_string();
    // The command takes away write permission on the brain's directory, so the writes after it fail.
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "lock-up".into(),
            steps: vec![format!("chmod 555 '{root}' && echo locked")],
            scenario: "the user wants it locked".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![],
        })
        .unwrap();
    let (models, _) = models(|_| decision("lock-up", 0.6, 0.9, SAFE), no_language_model());
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let result = route(&scratch.brain, &models, "lock it up", &quiet(), &mut Script::default(), &options);
    let _ = std::fs::set_permissions(&scratch.root, std::os::unix::fs::PermissionsExt::from_mode(0o755));
    let outcome = result.expect("the outcome comes back, not an error");
    assert!(outcome.ran && outcome.exit_code == Some(0));
    assert_eq!(outcome.stdout.as_deref(), Some("locked\n"));
    assert_eq!(outcome.error.as_deref(), Some("storage"), "{:?}", outcome.error_message);
    assert!(outcome.event_id.is_none(), "the event could not be logged either");
}

#[test]
fn errors_have_a_kind() {
    let tagged = anyhow::Error::new(Coded { code: "model", message: "Mercury did not answer".into() }).context("asking");
    assert_eq!(error_code(&tagged), "model");
    assert_eq!(error_code(&anyhow::anyhow!("something else")), "internal");
    let offline = Models { llm: Llm::with_transport(|_| Err("connection refused".into())), ..no_model_at_all() };
    let scratch = Scratch::new("model-error");
    let options = RouteOptions { agent_only: true, ..scratch.options(Mode::Semi) };
    let error = route(&scratch.brain, &offline, "anything", &quiet(), &mut Script::default(), &options).unwrap_err();
    assert_eq!(error_code(&error), "model");
    let missing = crate::llm::endpoint_from(|_| None).unwrap_err();
    assert_eq!(error_code(&missing), "configuration");
}

/// An `App` on the scratch brain, as `jli fix` runs with no terminal.
fn app(scratch: &Scratch, user: &str, models: Models) -> App {
    App {
        brain: Brain::open_as(BrainConfig::file(&scratch.root.join("brain.db")), user).unwrap(),
        models,
        terminal: crate::ui::Terminal::not_a_terminal(),
        project: None,
    }
}

fn quiet_flags() -> Flags {
    Flags::quiet()
}

fn lists(scratch: &Scratch, label: &str) -> (Vec<String>, Vec<String>) {
    let skill = scratch.brain.skill(label).unwrap().unwrap();
    (skill.examples, skill.counter_examples)
}

fn yes(scratch: &Scratch) -> RouteOptions {
    RouteOptions { yes: true, ..scratch.options(Mode::Semi) }
}

#[test]
fn fixing_an_event_with_a_command_teaches_what_a_no_at_the_prompt_teaches() {
    // At the prompt: declining_and_typing_a_known_command_relinks_the_phrase.
    let typed = Scratch::new("fix-parity-typed");
    let (models, _) = models(|_| decision("list-files", 0.9, 0.9, SAFE), no_language_model());
    go(&typed, &models, "newest files", &mut Script::new(&[Some(false)], &["ls -ltr"]), &typed.options(Mode::Semi));

    // With no terminal: the route ran, then `jli fix <eventId> -- ls -ltr`.
    let fixed = Scratch::new("fix-parity-fixed");
    let (models, _) = self::models(|_| decision("list-files", 0.9, 0.9, SAFE), no_language_model());
    let ran = go(&fixed, &models, "newest files", &mut Script::default(), &yes(&fixed));
    assert_eq!(lists(&fixed, "list-files").0, ["newest files"], "at 0.9 the phrasing was learned");
    let fix = Fix { event: ran.event_id, how: FixHow::Command(vec!["ls -ltr".into()]), run: false };
    assert_eq!(app(&fixed, "default", no_model_at_all()).fix(&quiet_flags(), fix).unwrap(), 0);

    for label in ["list-files", "list-files-by-time"] {
        assert_eq!(lists(&fixed, label), lists(&typed, label), "{label}");
    }
    assert_eq!(lists(&fixed, "list-files").1, ["newest files"]);
    let correction = fixed.brain.corrections().unwrap().remove(0);
    assert_eq!(
        (correction.event_id, correction.terminal, correction.how.as_str()),
        (ran.event_id.unwrap(), false, "command")
    );
    assert_eq!(correction.result_event_id, None, "learned, not run");
    assert!(!fixed.brain.events(1).unwrap()[0].accepted, "the corrected event is not this directory's last route any more");

    // It returned a plan of the right command; running it runs that, with no model, and counts a use.
    let outcomes = plans(&fixed, &no_model_at_all(), &[1], &[], &mut Script::default());
    assert!(outcomes[0].ran && outcomes[0].commands == ["ls -ltr"]);
    let uses = |scratch: &Scratch| scratch.brain.skill("list-files-by-time").unwrap().unwrap().uses;
    assert_eq!(uses(&fixed), uses(&typed));
    assert_eq!(lists(&fixed, "list-files-by-time"), lists(&typed, "list-files-by-time"));
}

#[test]
fn fixing_an_event_with_the_same_command_and_another_value_is_not_a_counter_example() {
    let scratch = Scratch::new("fix-same-skill");
    let (models, _) = models(|_| decision("git-commit", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
    let ran = go(&scratch, &models, "commit as \"wip\"", &mut Script::default(), &yes(&scratch));
    assert!(ran.ran, "it ran, and failed outside a repository");
    let fix = Fix { event: ran.event_id, how: FixHow::Command(vec!["git commit -m other".into()]), run: false };
    app(&scratch, "default", no_model_at_all()).fix(&quiet_flags(), fix).unwrap();
    let (examples, counter) = lists(&scratch, "git-commit");
    assert!(counter.is_empty());
    assert_eq!(examples, ["commit as \"…\""], "as the_same_command_with_another_value_is_not_a_counter_example stores it");
}

#[test]
fn fixing_an_event_with_what_was_expected_returns_a_plan_from_the_agent() {
    let scratch = Scratch::new("fix-expected");
    let (models, _) = models(|_| decision("list-files", 0.9, 0.9, SAFE), no_language_model());
    let ran = go(&scratch, &models, "files by date", &mut Script::default(), &yes(&scratch));
    let proposal = json!({
        "kind": "existing", "existingLabel": "list-files-by-time", "label": null, "steps": [],
        "scenario": "", "slots": [], "slotValues": [], "risk": "safe", "explanation": "Newest last.", "answer": null,
    });
    let (models, replies) = self::models(|_| panic!("only the agent is asked"), vec![("proposal", proposal)]);
    let fix = Fix { event: ran.event_id, how: FixHow::Expected("the newest last".into()), run: false };
    assert_eq!(app(&scratch, "default", models).fix(&quiet_flags(), fix).unwrap(), 0, "a plan, not a run");
    let input: Value =
        serde_json::from_str(replies.seen.lock().unwrap()[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["clarification"], "the newest last");
    assert_eq!(input["rejectedSkills"], json!([{ "label": "list-files", "steps": ["ls"] }]));
    assert_eq!(lists(&scratch, "list-files").1, ["files by date"]);
    assert_eq!(scratch.brain.events(10).unwrap().len(), 1, "the agent's answer was not run");

    let outcomes = plans(&scratch, &no_model_at_all(), &[1], &[], &mut Script::default());
    assert_eq!((outcomes[0].label.as_deref(), outcomes[0].ran), (Some("list-files-by-time"), true));
    assert_eq!(
        lists(&scratch, "list-files-by-time").0,
        ["files by date"],
        "an agent route teaches its phrasing when it runs"
    );
    let correction = scratch.brain.corrections().unwrap().remove(0);
    assert_eq!((correction.how.as_str(), correction.detail.as_deref()), ("expected", Some("the newest last")));
}

#[test]
fn another_users_event_cannot_be_fixed() {
    let scratch = Scratch::new("fix-other-user");
    let (models, _) = models(|_| decision("list-files", 0.9, 0.9, SAFE), no_language_model());
    let ran = go(&scratch, &models, "what is here", &mut Script::default(), &yes(&scratch));
    let fix = Fix { event: ran.event_id, how: FixHow::Command(vec!["ls -ltr".into()]), run: false };
    assert_eq!(app(&scratch, "other", no_model_at_all()).fix(&quiet_flags(), fix).unwrap(), 1);
    assert!(lists(&scratch, "list-files").1.is_empty());
}

#[test]
fn a_plan_that_must_run_in_the_calling_shell_is_not_run_by_jli_run() {
    let scratch = Scratch::new("plan-cd");
    std::fs::create_dir(scratch.work.join("src")).unwrap();
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "into-src".into(),
            steps: vec!["cd src".into()],
            scenario: "the user wants to be in src".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![],
        })
        .unwrap();
    let (models, _) = models(|_| decision("into-src", 0.99, 0.9, SAFE), no_language_model());
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let id = go(&scratch, &models, "go into src", &mut Script::default(), &dry).plan_id.unwrap();
    let outcomes = plans(&scratch, &no_model_at_all(), &[id], &[], &mut Script::default());
    assert_eq!((code(&outcomes[0]), outcomes[0].ran), (Some("needs_caller_shell"), false));
    assert_eq!(outcomes[0].caller_shell, ["cd src"]);
}

// ── Projects (ADR 17) ───────────────────────────────────────────────────────────

fn in_project(scratch: &Scratch, project: &str) -> Brain {
    Brain::open_in(BrainConfig::file(&scratch.root.join("brain.db")), "default", Some(project)).unwrap()
}

/// The criteria System 1 is shown for a request in `brain`, from a dry run that asks nobody else.
fn criteria(scratch: &Scratch, brain: &Brain, request: &str) -> Value {
    let seen = Arc::new(Mutex::new(Value::Null));
    let recorder = seen.clone();
    let (models, _) = self::models(
        move |sent| {
            *recorder.lock().unwrap() = sent["questions"]["skill"]["criteria"].clone();
            decision(NONE, 0.9, 0.05, SAFE)
        },
        no_language_model(),
    );
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    route(brain, &models, request, &quiet(), &mut Script::default(), &options).unwrap();
    let criteria = seen.lock().unwrap().clone();
    criteria
}

#[test]
fn run_the_tests_means_one_command_in_one_project_and_another_in_the_next() {
    let scratch = Scratch::new("ping-pong");
    // Taught before either project existed, so it is global.
    let (silent, _) = models_without_calls();
    let taught = LearnInput {
        phrase: "run the tests".into(),
        steps: vec!["echo cargo test".into()],
        risk: Some(Risk::Safe),
        mode: Mode::Manual,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    assert_eq!(learn_phrase(&scratch.brain, &silent, &taught, &quiet()).unwrap().label, "echo-cargo-test");
    let (rust, node) = (in_project(&scratch, "rust"), in_project(&scratch, "node"));

    // In the node project System 1 picks it; the person says no and types what they meant.
    let (models, _) = self::models(|_| decision("echo-cargo-test", 0.9, 0.9, SAFE), no_language_model());
    let mut script = Script::new(&[Some(false)], &["echo npm test"]);
    let outcome = route(&node, &models, "run the tests", &quiet(), &mut script, &scratch.options(Mode::Manual)).unwrap();
    assert_eq!((outcome.path, outcome.label.as_deref()), (Path::Taught, Some("echo-npm-test")));
    assert_eq!(outcome.output.as_deref(), Some("npm test\n"));
    assert_eq!(node.project_skills().unwrap().len(), 1, "the new skill is the node project's");

    // The global row is untouched, so the rust project is shown exactly what it was before.
    let row = scratch.brain.skill("echo-cargo-test").unwrap().unwrap();
    assert_eq!((row.examples.as_slice(), row.counter_examples.len()), (["run the tests".to_string()].as_slice(), 0));
    let in_rust = criteria(&scratch, &rust, "run the tests");
    assert_eq!(in_rust["echo-cargo-test"]["examples"], json!(["run the tests"]));
    assert!(in_rust["echo-cargo-test"].get("doesNotApplyTo").is_none() && in_rust.get("echo-npm-test").is_none());
    let in_node = criteria(&scratch, &node, "run the tests");
    assert_eq!(in_node["echo-cargo-test"]["doesNotApplyTo"], json!(["run the tests"]));
    assert!(in_node["echo-cargo-test"].get("examples").is_none(), "the phrasing is not both an example and not one");
    assert_eq!(in_node["echo-npm-test"]["examples"], json!(["run the tests"]));

    // Back in the rust project the global skill still runs, and nothing it learns reaches the node project.
    let (models, _) = self::models(|_| decision("echo-cargo-test", 0.8, 0.9, SAFE), no_language_model());
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Manual) };
    let outcome = route(&rust, &models, "please run the tests", &quiet(), &mut Script::default(), &options).unwrap();
    assert_eq!(outcome.output.as_deref(), Some("cargo test\n"));
    assert_eq!(outcome.learned, ["\"please run the tests\" means echo-cargo-test"]);
    assert_eq!(rust.skill("echo-cargo-test").unwrap().unwrap().examples, ["run the tests", "please run the tests"]);
    assert_eq!(scratch.brain.skill("echo-cargo-test").unwrap().unwrap().examples, ["run the tests"]);
    assert_eq!(criteria(&scratch, &node, "run the tests"), in_node, "the node project is shown what it was");
    assert_eq!(scratch.brain.skill("echo-cargo-test").unwrap().unwrap().uses, 1, "a use is counted on the global row");
}

#[test]
fn a_skill_learned_in_a_project_stays_there_unless_taught_globally() {
    let scratch = Scratch::new("project-learn");
    std::fs::write(scratch.work.join("notes.txt"), "one\ntwo\n").unwrap();
    let mut shop = in_project(&scratch, "shop");
    let proposal = new_proposal(
        "count-lines",
        "wc -l {file}",
        json!([{ "name": "file", "kind": "file", "description": "the file", "default": null }]),
        json!([{ "name": "file", "value": "notes.txt" }]),
    );
    let (models, _) = self::models(once_known("count-lines", decision(NONE, 0.7, 0.95, SAFE)), vec![("proposal", proposal)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome =
        route(&shop, &models, "how many lines are in notes.txt", &quiet(), &mut Script::default(), &options).unwrap();
    assert_eq!(outcome.learned, ["new skill count-lines"]);
    assert_eq!(shop.project_skills().unwrap()[0].label, "count-lines");
    assert!(scratch.brain.skill("count-lines").unwrap().is_none(), "not global");
    assert_eq!(shop.events(10).unwrap().len(), 1);
    assert!(scratch.brain.events(10).unwrap().is_empty(), "the request is in the project's history");

    // `teach --global` from inside the project.
    let (silent, _) = models_without_calls();
    let taught = LearnInput {
        phrase: "how big is this folder".into(),
        steps: vec!["du -sh .".into()],
        risk: Some(Risk::Safe),
        mode: Mode::Manual,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    let learned = shop.globally(|brain| learn_phrase(brain, &silent, &taught, &quiet())).unwrap();
    assert_eq!(scratch.brain.skill(&learned.label).unwrap().unwrap().steps, ["du -sh ."]);
    assert!(shop.project_skills().unwrap().iter().all(|skill| skill.label != learned.label));
    assert!(shop.skill(&learned.label).unwrap().is_some(), "and the project sees it");
}

fn models_without_calls() -> (Models, Arc<Replies>) {
    self::models(|_| panic!("System 1 must not be asked"), no_language_model())
}

// ── jli generalize ──────────────────────────────────────────────────────────────

use crate::generalize::{generalize, GeneralizeOptions, Generalized};

/// A rust project with one skill, `cargo test -p api`, and a node project that means `npm test` by "run the tests".
fn two_projects(scratch: &Scratch) -> (Brain, Brain) {
    let (rust, node) = (in_project(scratch, "rust"), in_project(scratch, "node"));
    let skill = |label: &str, step: &str, examples: &[&str]| NewSkill {
        label: label.into(),
        steps: vec![step.into()],
        scenario: format!("the user wants {step} run"),
        risk: Risk::Safe,
        origin: Origin::Taught,
        examples: examples.iter().map(|example| example.to_string()).collect(),
        slots: vec![],
    };
    rust.add_skill(&skill("api-tests", "cargo test -p api", &["run the api tests"])).unwrap();
    rust.add_counter_example("api-tests", "test the docs").unwrap();
    node.add_skill(&skill("npm-test", "npm test", &["run the tests"])).unwrap();
    (rust, node)
}

fn parameterized() -> Value {
    json!({
        "decision": "parameterize", "reason": "Any Rust workspace can test one package.", "mergeInto": null,
        "label": "cargo-test-package", "steps": ["cargo test -p {package}"],
        "scenario": "the user wants the tests of one package of a Rust workspace run",
        "slots": [{ "name": "package", "kind": "text", "description": "the package", "default": null, "aliases": ["pkg"] }],
        "risk": "safe",
    })
}

/// System 1 that sends the project skill's own phrasing to `label` and every other phrasing elsewhere.
fn routes_own_phrasing_to(label: &'static str) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
    move |sent| {
        if sent["state"]["request"] == "run the api tests" {
            decision(label, 0.9, 0.9, SAFE)
        } else {
            decision("npm-test", 0.9, 0.9, SAFE)
        }
    }
}

fn options_for(scratch: &Scratch) -> GeneralizeOptions {
    GeneralizeOptions {
        label: None,
        all_projects: false,
        dry_run: false,
        approve_all: false,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    }
}

fn nothing_written(scratch: &Scratch, rust: &Brain, done: &Generalized) {
    assert!(done.written.is_empty());
    assert_eq!(rust.project_skills().unwrap()[0].label, "api-tests", "the project row is intact");
    assert!(scratch.brain.skill("cargo-test-package").unwrap().is_none(), "no global row appeared");
    assert!(rust.events(10).unwrap().is_empty());
}

#[test]
fn generalize_writes_nothing_without_approval() {
    let scratch = Scratch::new("generalize-refused");
    let (rust, _node) = two_projects(&scratch);
    let options = options_for(&scratch);

    // The person says no.
    let (models, replies) =
        self::models(routes_own_phrasing_to("cargo-test-package"), vec![("generalization", parameterized())]);
    let mut script = Script::new(&[Some(false)], &[]);
    let done = generalize(&rust, &models, &options, &quiet(), &mut script).unwrap();
    assert_eq!((done.proposed, script.questions.as_slice()), (1, ["Make it global?".to_string()].as_slice()));
    nothing_written(&scratch, &rust, &done);
    let sent: Value =
        serde_json::from_str(replies.seen.lock().unwrap()[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(sent["projectSkill"]["steps"], json!(["cargo test -p api"]));
    assert_eq!(sent["sameIntentElsewhere"][0]["project"], "node", "the node project's tests are shown to the agent");
    assert_eq!(sent["globalSkills"].as_array().unwrap().len(), 9);

    // Nobody at the keyboard, and no --approve-all.
    let (models, _) = self::models(routes_own_phrasing_to("cargo-test-package"), vec![("generalization", parameterized())]);
    let done = generalize(&rust, &models, &options, &quiet(), &mut Script::new(&[None], &[])).unwrap();
    nothing_written(&scratch, &rust, &done);

    // A dry run shows the proposal and asks nothing (an unexpected question would panic).
    let (models, _) = self::models(routes_own_phrasing_to("cargo-test-package"), vec![("generalization", parameterized())]);
    let dry = GeneralizeOptions { dry_run: true, ..options_for(&scratch) };
    let done = generalize(&rust, &models, &dry, &quiet(), &mut Script::default()).unwrap();
    assert_eq!(done.proposed, 1);
    nothing_written(&scratch, &rust, &done);

    // Outside a project there is nothing to generalize unless every project is asked for.
    let (models, _) = models_without_calls();
    assert!(generalize(&scratch.brain, &models, &options, &quiet(), &mut Script::default()).unwrap().nothing);
    let (models, _) = self::models(routes_own_phrasing_to("cargo-test-package"), vec![("generalization", parameterized())]);
    let every =
        GeneralizeOptions { all_projects: true, label: Some("api-tests".into()), dry_run: true, ..options_for(&scratch) };
    assert_eq!(generalize(&scratch.brain, &models, &every, &quiet(), &mut Script::default()).unwrap().proposed, 1);
    let only = GeneralizeOptions { label: Some("git-status".into()), ..options_for(&scratch) };
    let (models, _) = models_without_calls();
    assert!(generalize(&rust, &models, &only, &quiet(), &mut Script::default()).unwrap().nothing, "already global");
}

#[test]
fn an_approved_generalization_moves_the_skill_to_the_global_ones() {
    let scratch = Scratch::new("generalize-approved");
    let (rust, node) = two_projects(&scratch);
    let (models, _) = self::models(routes_own_phrasing_to("cargo-test-package"), vec![("generalization", parameterized())]);
    let mut script = Script::new(&[Some(true)], &[]);
    let done = generalize(&rust, &models, &options_for(&scratch), &quiet(), &mut script).unwrap();
    assert_eq!(done.written, [("rust".to_string(), "api-tests".to_string(), "cargo-test-package".to_string())]);

    let row = scratch.brain.skill("cargo-test-package").unwrap().unwrap();
    assert_eq!(row.steps, ["cargo test -p {package}"]);
    assert_eq!((row.slots[0].name.as_str(), row.slots[0].aliases.as_slice()), ("package", ["pkg".to_string()].as_slice()));
    assert_eq!(row.examples, ["run the api tests"], "the project's phrasings go with it");
    assert!(row.counter_examples.is_empty());
    assert!(rust.project_skills().unwrap().is_empty(), "the project row is removed");
    assert_eq!(rust.skill("cargo-test-package").unwrap().unwrap().counter_examples, ["test the docs"]);
    assert!(node.skill("cargo-test-package").unwrap().unwrap().counter_examples.is_empty());
    let event = rust.events(1).unwrap().remove(0);
    assert_eq!((event.query.as_str(), event.label, event.accepted), ("jli generalize api-tests", None, false));
    assert!(event.learned.unwrap().contains("the project row was removed"));

    // --approve-all writes what passes without asking; a command a global skill runs joins that skill.
    let slot = Slot { name: "script".into(), kind: SlotKind::Text, ..Slot::default() };
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "npm-script".into(),
            steps: vec!["npm run {script}".into()],
            scenario: "the user wants an npm script run".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![slot],
        })
        .unwrap();
    node.add_skill(&NewSkill {
        label: "lint".into(),
        steps: vec!["npm run lint".into()],
        scenario: "the user wants the code linted".into(),
        risk: Risk::Safe,
        origin: Origin::Taught,
        examples: vec!["run the api tests".into()],
        slots: vec![],
    })
    .unwrap();
    let promote = json!({
        "decision": "promote", "reason": "It is general.", "mergeInto": null, "label": "npm-lint",
        "steps": ["npm run lint"], "scenario": "the user wants the code linted", "slots": [], "risk": "safe",
    });
    let (models, _) = self::models(routes_own_phrasing_to("npm-script"), vec![("generalization", promote)]);
    let all = GeneralizeOptions { approve_all: true, label: Some("lint".into()), ..options_for(&scratch) };
    let done = generalize(&node, &models, &all, &quiet(), &mut Script::default()).unwrap();
    assert_eq!(done.written[0].2, "npm-script", "the proposal became a merge into the skill that already runs it");
    assert!(scratch.brain.skill("npm-lint").unwrap().is_none());
    assert_eq!(scratch.brain.skill("npm-script").unwrap().unwrap().examples, ["run the api tests"]);
}

#[test]
fn a_generalization_that_fails_a_check_is_never_offered() {
    let scratch = Scratch::new("generalize-checks");
    let (rust, _node) = two_projects(&scratch);
    let options = options_for(&scratch);
    let with = |changes: Value| {
        let mut reply = parameterized();
        for (key, value) in changes.as_object().unwrap() {
            reply[key] = value.clone();
        }
        reply
    };
    let flag_slot = json!([{ "name": "flag", "kind": "text", "description": "", "default": null, "aliases": [] }]);
    // Each is refused before System 1 or the person is asked: either would panic.
    for (reply, why) in [
        (with(json!({ "steps": ["cargo test --workspace"] })), "it does not give back the project's command"),
        (with(json!({ "steps": ["cargo test {flag} api"], "slots": flag_slot })), "a flag is never a slot"),
        (with(json!({ "slots": [] })), "an undeclared slot"),
        (with(json!({ "decision": "merge", "mergeInto": "git-status" })), "git-status does not run it"),
        (with(json!({ "decision": "keep" })), "kept in the project"),
    ] {
        let (models, _) =
            self::models(|_| panic!("System 1 is asked only about a proposal that passed"), vec![("generalization", reply)]);
        let done = generalize(&rust, &models, &options, &quiet(), &mut Script::default()).unwrap();
        assert_eq!(done.proposed, 0, "{why}");
        nothing_written(&scratch, &rust, &done);
    }
    // System 1 does not send the project's own phrasing to the new row.
    let (models, _) = self::models(routes_own_phrasing_to("npm-test"), vec![("generalization", parameterized())]);
    let done = generalize(&rust, &models, &options, &quiet(), &mut Script::default()).unwrap();
    nothing_written(&scratch, &rust, &done);
    // Or the new row would take a phrasing that belongs to the node project: there "run the tests" is npm test.
    let takes_node = |takes: bool| {
        move |sent: &Value| {
            let request = sent["state"]["request"].as_str().unwrap();
            if request == "run the tests" {
                return decision(if takes { "cargo-test-package" } else { "npm-test" }, 0.9, 0.9, SAFE);
            }
            once_known("cargo-test-package", decision(NONE, 0.9, 0.9, SAFE))(sent)
        }
    };
    let (models, _) = self::models(takes_node(true), vec![("generalization", parameterized())]);
    let done = generalize(&rust, &models, &options, &quiet(), &mut Script::default()).unwrap();
    nothing_written(&scratch, &rust, &done);
    // The same proposal, with the node project's phrasing staying home, is written.
    let (models, _) = self::models(takes_node(false), vec![("generalization", parameterized())]);
    let approve = GeneralizeOptions { approve_all: true, ..options_for(&scratch) };
    assert_eq!(generalize(&rust, &models, &approve, &quiet(), &mut Script::default()).unwrap().written.len(), 1);
}

// ── One row per command: flags, literal skills and labels taken by the same command (ADR 19) ──

fn teach(phrase: &str, command: &str, mode: Mode, scratch: &Scratch) -> LearnInput {
    LearnInput {
        phrase: phrase.into(),
        steps: vec![command.into()],
        risk: None,
        mode,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    }
}

/// A skill that runs one fixed command, as manual mode or an agent's literal proposal leaves one.
fn literal_skill(brain: &Brain, label: &str, step: &str, examples: &[&str]) {
    brain
        .add_skill(&NewSkill {
            label: label.into(),
            steps: vec![step.into()],
            scenario: format!("the user wants: {}", examples.first().copied().unwrap_or(step)),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: examples.iter().map(|example| example.to_string()).collect(),
            slots: vec![],
        })
        .unwrap();
}

/// System 1 that sends `phrase` to `label` and every other request to git-status, as the check expects.
fn routes_phrase_to(phrase: &'static str, label: &'static str) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
    move |request| {
        if request["state"]["request"] == phrase {
            decision(label, 0.92, 0.9, SAFE)
        } else {
            decision("git-status", 0.9, 0.9, SAFE)
        }
    }
}

#[test]
fn teaching_cd_minus_beside_cd_directory_makes_its_own_skill() {
    let scratch = Scratch::new("teach-cd-minus");
    directory_skill(&scratch, "go-to-directory", &["cd {directory}"]);
    let (models, _) = models(|_| panic!("System 1 must not be asked"), no_language_model());
    let learned =
        learn_phrase(&scratch.brain, &models, &teach("go back", "cd -", Mode::Manual, &scratch), &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created), ("cd", true), "- is a flag, not a directory");
    assert_eq!(scratch.brain.skill("cd").unwrap().unwrap().steps, ["cd -"]);
    assert!(scratch.brain.skill("go-to-directory").unwrap().unwrap().examples.is_empty());
}

#[test]
fn a_translator_that_names_a_template_with_a_flag_for_its_value_is_not_believed() {
    let scratch = Scratch::new("teach-cd-minus-translated");
    directory_skill(&scratch, "go-to-directory", &["cd {directory}"]);
    // The translator calls `cd -` the known skill with `-` for the directory, the same mistake #17 was.
    let row = json!({
        "existingLabel": "go-to-directory", "label": "go-back", "steps": ["cd {directory}"],
        "scenario": "the user wants to go back to the previous directory",
        "slots": [{ "name": "directory", "kind": "file", "description": "the directory", "default": null }],
        "slotValues": [{ "name": "directory", "value": "-" }], "risk": "safe",
    });
    let (models, _) = models(routes_phrase_to("go back", "go-back"), vec![("matrix_row", row)]);
    let learned = learn_phrase(&scratch.brain, &models, &teach("go back", "cd -", Mode::Semi, &scratch), &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created), ("go-back", true), "- is a flag, not a directory");
    assert_eq!(scratch.brain.skill("go-back").unwrap().unwrap().steps, ["cd -"]);
    assert!(scratch.brain.skill("go-to-directory").unwrap().unwrap().examples.is_empty());
}

#[test]
fn teaching_a_literal_skills_command_makes_it_general_in_place() {
    let scratch = Scratch::new("teach-upgrade");
    literal_skill(&scratch.brain, "recent-three", "git log --oneline -n 3", &["show the last 3 commits"]);
    scratch.brain.add_counter_example("recent-three", "show the last 3 tags").unwrap();
    scratch.brain.record_use("recent-three").unwrap();
    let before = scratch.brain.skill("recent-three").unwrap().unwrap();
    // The translator writes the template, and names the literal skill as the one that runs the command.
    let mut row = draft("git log --oneline -n {count}", "3");
    row["existingLabel"] = json!("recent-three");
    let (models, _) = models(routes_phrase_to("list the 3 newest commits", "recent-three"), vec![("matrix_row", row)]);
    let input = teach("list the 3 newest commits", "git log --oneline -n 3", Mode::Semi, &scratch);
    let learned = learn_phrase(&scratch.brain, &models, &input, &quiet()).unwrap();
    assert_eq!(
        learned,
        crate::learn::Learned { label: "recent-three".into(), created: false, translated: true, upgraded: true }
    );
    let after = scratch.brain.skill("recent-three").unwrap().unwrap();
    assert_eq!(after.steps, ["git log --oneline -n {count}"]);
    assert_eq!((after.slots[0].name.as_str(), after.slots[0].default.as_deref()), ("count", Some("3")));
    assert!(after.scenario.starts_with("the user wants a one-line summary"));
    assert_eq!(after.examples, ["show the last 3 commits", "list the 3 newest commits"]);
    assert_eq!(after.counter_examples, ["show the last 3 tags"]);
    assert_eq!((after.uses, after.created_at.as_str(), after.origin), (1, before.created_at.as_str(), Origin::Taught));
    assert!(scratch.brain.skill("git-recent-commits").unwrap().is_none(), "no second row");

    // Every count now runs, and a phrasing that names none still runs what the literal skill ran.
    let (models, _) = self::models(|_| decision("recent-three", 0.95, 0.9, SAFE), no_language_model());
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let seven = go(&scratch, &models, "show the last 7 commits", &mut Script::default(), &dry);
    assert_eq!(seven.commands, ["git log --oneline -n 7"]);
    let some = go(&scratch, &models, "show me some recent commits", &mut Script::default(), &dry);
    assert_eq!(some.commands, ["git log --oneline -n 3"]);
}

#[test]
fn a_literal_skill_takes_the_phrase_when_there_is_nothing_to_generalise() {
    let scratch = Scratch::new("teach-literal-kept");
    literal_skill(&scratch.brain, "list-everything", "ls -la", &["everything here"]);
    let unchanged = json!({
        "existingLabel": "list-everything", "label": "list-all", "steps": ["ls -la"],
        "scenario": "the user wants every file listed with details", "slots": [], "slotValues": [], "risk": "safe",
    });
    let (models, replies) = models(|_| panic!("System 1 must not be asked"), vec![("matrix_row", unchanged)]);
    let learned =
        learn_phrase(&scratch.brain, &models, &teach("all of it", "ls -la", Mode::Semi, &scratch), &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created, learned.upgraded), ("list-everything", false, false));
    assert_eq!(scratch.brain.skill("list-everything").unwrap().unwrap().examples, ["everything here", "all of it"]);
    assert_eq!(replies.seen.lock().unwrap().len(), 1, "the translator is asked once");

    // A seed is a fixed command written by hand: it takes the phrase with no model at all.
    let (models, _) = self::models(|_| panic!("System 1 must not be asked"), no_language_model());
    let learned =
        learn_phrase(&scratch.brain, &models, &teach("oldest last", "ls -ltr", Mode::Semi, &scratch), &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created), ("list-files-by-time", false));
}

#[test]
fn a_template_wins_over_a_literal_skill_that_runs_the_same_command() {
    let scratch = Scratch::new("teach-template-first");
    literal_skill(&scratch.brain, "recent-three", "git log --oneline -n 3", &["show the last 3 commits"]);
    // The literal skill is the more used, so it comes first in the matrix.
    scratch.brain.record_use("recent-three").unwrap();
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "git-recent-commits".into(),
            steps: vec!["git log --oneline -n {count}".into()],
            scenario: "the user wants the most recent commits".into(),
            risk: Risk::Safe,
            origin: Origin::Agent,
            examples: vec!["latest commits".into()],
            slots: vec![Slot {
                name: "count".into(),
                kind: SlotKind::Number,
                description: "how many".into(),
                default: None,
                aliases: vec![],
                ..Slot::default()
            }],
        })
        .unwrap();
    let (models, _) = models(|_| panic!("System 1 must not be asked"), no_language_model());
    let input = teach("three newest commits", "git log --oneline -n 3", Mode::Semi, &scratch);
    let learned = learn_phrase(&scratch.brain, &models, &input, &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created), ("git-recent-commits", false));
    assert_eq!(scratch.brain.skill("recent-three").unwrap().unwrap().examples, ["show the last 3 commits"]);
}

#[test]
fn a_global_literal_skill_is_not_made_general_from_inside_a_project() {
    let scratch = Scratch::new("teach-upgrade-project");
    literal_skill(&scratch.brain, "recent-three", "git log --oneline -n 3", &["show the last 3 commits"]);
    let shop = in_project(&scratch, "shop");
    let (models, _) = models(
        routes_phrase_to("three newest commits", "git-recent-commits"),
        vec![("matrix_row", draft("git log --oneline -n {count}", "3"))],
    );
    let input = teach("three newest commits", "git log --oneline -n 3", Mode::Semi, &scratch);
    let learned = learn_phrase(&shop, &models, &input, &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created, learned.upgraded), ("git-recent-commits", true, false));
    assert_eq!(shop.project_skills().unwrap()[0].steps, ["git log --oneline -n {count}"]);
    let global = scratch.brain.skill("recent-three").unwrap().unwrap();
    assert_eq!((global.steps.as_slice(), global.examples.len()), (["git log --oneline -n 3".to_string()].as_slice(), 1));
}

#[test]
fn a_literal_skill_another_writer_made_general_first_is_joined() {
    let scratch = Scratch::new("teach-upgrade-race");
    literal_skill(&scratch.brain, "recent-three", "git log --oneline -n 3", &["show the last 3 commits"]);
    let path = scratch.root.join("brain.db");
    let row = draft("git log --oneline -n {count}", "3");
    let llm = Llm::with_transport(move |_| {
        // Another jli makes the same skill general while this one's translator is writing.
        let other = Brain::open(BrainConfig::file(&path)).unwrap();
        let general = NewSkill {
            label: "recent-three".into(),
            steps: vec!["git log --oneline -n {n}".into()],
            scenario: "theirs".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![Slot {
                name: "n".into(),
                kind: SlotKind::Number,
                description: String::new(),
                default: None,
                aliases: vec![],
                ..Slot::default()
            }],
        };
        other.upgrade_skill("recent-three", &["git log --oneline -n 3".into()], &general).unwrap();
        Ok((200, json!({ "choices": [{ "message": { "content": row.to_string() } }] }).to_string()))
    });
    let system1 = System1::fake(routes_phrase_to("three newest commits", "recent-three"));
    let input = teach("three newest commits", "git log --oneline -n 3", Mode::Semi, &scratch);
    let learned = learn_phrase(&scratch.brain, &Models { system1, llm }, &input, &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created, learned.upgraded), ("recent-three", false, false));
    let skill = scratch.brain.skill("recent-three").unwrap().unwrap();
    assert_eq!(
        (skill.steps.as_slice(), skill.scenario.as_str()),
        (["git log --oneline -n {n}".to_string()].as_slice(), "theirs")
    );
    assert_eq!(skill.examples, ["show the last 3 commits", "three newest commits"]);
    assert!(scratch.brain.skill("recent-three-x").unwrap().is_none());
}

#[test]
fn a_label_taken_by_the_same_command_while_the_agent_was_thinking_is_joined() {
    let scratch = Scratch::new("label-race-same-agent");
    std::fs::write(scratch.work.join("notes.txt"), "one\ntwo\n").unwrap();
    let path = scratch.root.join("brain.db");
    let proposal = json!({
        "kind": "new", "existingLabel": null, "label": "count-lines", "steps": ["wc -l {file}"],
        "scenario": "the user wants the number of lines in a file",
        "slots": [{ "name": "file", "kind": "file", "description": "the file to count", "default": null }],
        "slotValues": [{ "name": "file", "value": "notes.txt" }],
        "risk": "safe", "explanation": "Counts lines.", "answer": null,
    });
    // After this request read the matrix, another jli on the same brain stores the same command.
    let (models, _) = models(
        move |request| {
            Brain::open(BrainConfig::file(&path))
                .unwrap()
                .add_skill_if_missing(&NewSkill {
                    label: "count-lines".into(),
                    steps: vec!["wc -l {path}".into()],
                    scenario: "theirs".into(),
                    risk: Risk::Safe,
                    origin: Origin::Agent,
                    examples: vec!["lines in a file".into()],
                    slots: vec![],
                })
                .unwrap();
            once_known("count-lines", decision(NONE, 0.7, 0.95, SAFE))(request)
        },
        vec![("proposal", proposal)],
    );
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "how many lines are in notes.txt", &mut Script::default(), &options);
    assert!(outcome.ran);
    assert_eq!(outcome.label.as_deref(), Some("count-lines"));
    assert_eq!(outcome.learned, ["\"how many lines are in …\" means count-lines"]);
    let theirs = scratch.brain.skill("count-lines").unwrap().unwrap();
    assert_eq!((theirs.scenario.as_str(), theirs.uses), ("theirs", 1));
    assert_eq!(theirs.examples, ["lines in a file", "how many lines are in …"]);
    assert!(scratch.brain.skill("count-lines-x").unwrap().is_none());
}

#[test]
fn a_command_another_writer_taught_while_the_translator_was_writing_is_joined() {
    let scratch = Scratch::new("teach-race-same");
    let path = scratch.root.join("brain.db");
    let row = draft("git log --oneline -n {count}", "3");
    let llm = Llm::with_transport(move |_| {
        // Another jli teaches the same command meanwhile; its translator named it differently (seen live).
        Brain::open(BrainConfig::file(&path))
            .unwrap()
            .add_skill_if_missing(&NewSkill {
                label: "show-recent-commits".into(),
                steps: vec!["git log --oneline -n {n}".into()],
                scenario: "theirs".into(),
                risk: Risk::Safe,
                origin: Origin::Taught,
                examples: vec!["latest commits".into()],
                slots: vec![],
            })
            .unwrap();
        Ok((200, json!({ "choices": [{ "message": { "content": row.to_string() } }] }).to_string()))
    });
    let system1 = System1::fake(routes_phrase_to("show the last 3 commits", "git-recent-commits"));
    let input = teach("show the last 3 commits", "git log --oneline -n 3", Mode::Semi, &scratch);
    let learned = learn_phrase(&scratch.brain, &Models { system1, llm }, &input, &quiet()).unwrap();
    assert_eq!((learned.label.as_str(), learned.created, learned.translated), ("show-recent-commits", false, false));
    let theirs = scratch.brain.skill("show-recent-commits").unwrap().unwrap();
    assert_eq!(theirs.examples, ["latest commits", "show the last 3 commits"]);
    assert!(scratch.brain.skill("git-recent-commits").unwrap().is_none(), "no second row");
}

// ── What an accepted route writes back (ADR 20) ──────────────────────────────

fn existing_proposal(label: &str) -> Value {
    json!({
        "kind": "existing", "existingLabel": label, "label": null, "steps": [],
        "scenario": "", "slots": [], "slotValues": [], "risk": "safe", "explanation": "Known.", "answer": null,
    })
}

fn file_slot(name: &str) -> Value {
    json!([{ "name": name, "kind": "file", "description": "the file", "default": null }])
}

#[test]
fn an_agent_route_to_a_known_skill_links_its_phrasing_and_drops_the_counter_example() {
    let scratch = Scratch::new("link-agent");
    scratch.brain.add_counter_example("list-files-by-time", "files by date").unwrap();
    let agent =
        || models(|_| decision(NONE, 0.4, 0.95, SAFE), vec![("proposal", existing_proposal("list-files-by-time"))]).0;
    let outcome = go(&scratch, &agent(), "files by date", &mut Script::default(), &yes(&scratch));
    assert_eq!((outcome.path, outcome.label.as_deref(), outcome.ran), (Path::System2, Some("list-files-by-time"), true));
    assert_eq!(outcome.learned, ["\"files by date\" means list-files-by-time"]);
    let (examples, counter) = lists(&scratch, "list-files-by-time");
    assert!(examples.contains(&"files by date".to_string()));
    assert!(counter.is_empty(), "never an example and a counter-example of one skill at once");

    // A plan of an agent route, run after its phrasing became a counter-example, links it the same way.
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let planned = go(&scratch, &agent(), "newest stuff", &mut Script::default(), &dry);
    scratch.brain.add_counter_example("list-files-by-time", "newest stuff").unwrap();
    let ran = plans(&scratch, &no_model_at_all(), &[planned.plan_id.unwrap()], &[], &mut Script::default());
    assert!(ran[0].ran);
    let (examples, counter) = lists(&scratch, "list-files-by-time");
    assert!(examples.contains(&"newest stuff".to_string()));
    assert!(counter.is_empty(), "jli run writes back as the route does");
}

#[test]
fn the_agent_cannot_bring_back_a_rejected_command_as_a_second_skill() {
    let scratch = Scratch::new("rejected-again");
    std::fs::write(scratch.work.join("notes.txt"), "one\ntwo\n").unwrap();
    let values = json!([{ "name": "file", "value": "notes.txt" }]);
    let count = new_proposal("count-lines-in-file", "wc -l {file}", file_slot("file"), values);
    let (models, _) = models(once_known("count-lines-in-file", decision(NONE, 0.4, 0.95, SAFE)), vec![("proposal", count)]);
    let ran = go(&scratch, &models, "how many lines are in notes.txt", &mut Script::default(), &yes(&scratch));
    assert_eq!(ran.learned, ["new skill count-lines-in-file"]);
    let before = scratch.brain.skills().unwrap().len();

    // `jli fix`: told the skill was wrong, the agent proposes its command again under another slot name.
    let again = new_proposal(
        "count-lines-in-file",
        "wc -l {path}",
        file_slot("path"),
        json!([{ "name": "path", "value": "notes.txt" }]),
    );
    // Were it run, the second row would pass System 1's check as `count-lines-in-file-x`.
    let check = once_known("count-lines-in-file-x", decision(NONE, 0.4, 0.95, SAFE));
    let (models, _) = self::models(check, vec![("proposal", again)]);
    let fix = Fix { event: ran.event_id, how: FixHow::Unlearn, run: false };
    assert_eq!(app(&scratch, "default", models).fix(&Flags::quiet_yes(), fix).unwrap(), 3, "refused, not run");
    assert_eq!(scratch.brain.skills().unwrap().len(), before, "no count-lines-in-file-x");
    let refused = scratch.brain.events(1).unwrap().remove(0);
    assert!(!refused.accepted && refused.exit_code.is_none(), "nothing ran");
    // Not a second verdict: `jli eval` counts the rejection once, the repeat as a retry of it.
    assert!(!crate::eval::is_corrected(&refused));
    let mut oldest_first = scratch.brain.events(10).unwrap();
    oldest_first.reverse();
    let correctness = crate::eval::correctness(&oldest_first);
    assert_eq!((correctness.requests, correctness.turned_down, correctness.blocked), (1, 1, 0));
    assert_eq!(scratch.brain.skill("count-lines-in-file").unwrap().unwrap().counter_examples, ["how many lines are in …"]);

    // At the router: the same command, a more general one than a rejected literal, or the label itself.
    let rejecting = |label: &str| RouteOptions { rejected: vec![label.into()], ..yes(&scratch) };
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "count-notes".into(),
            steps: vec![format!("grep -c TODO {}/notes.txt", std::env::var("HOME").unwrap())],
            scenario: "the user wants the TODOs in the notes counted".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![],
        })
        .unwrap();
    let before = scratch.brain.skills().unwrap().len();
    for (rejected, proposal, commands) in [
        ("list-files-by-time", new_proposal("list-by-date", "ls -ltr", json!([]), json!([])), "ls -ltr"),
        (
            "count-notes",
            new_proposal(
                "count-todos",
                "grep -c TODO {file}",
                file_slot("file"),
                json!([{ "name": "file", "value": "~/notes.txt" }]),
            ),
            &format!("grep -c TODO {}/notes.txt", std::env::var("HOME").unwrap()),
        ),
        ("list-files-by-time", existing_proposal("list-files-by-time"), "ls -ltr"),
    ] {
        let (models, _) = self::models(|_| decision(NONE, 0.4, 0.95, SAFE), vec![("proposal", proposal)]);
        let outcome = go(&scratch, &models, "show them", &mut Script::default(), &rejecting(rejected));
        assert_eq!((code(&outcome), outcome.ran, outcome.exit_code()), (Some("already_rejected"), false, 3), "{rejected}");
        assert_eq!(outcome.label.as_deref(), Some(rejected), "the outcome names the skill it ran into");
        assert_eq!(outcome.commands, [commands], "what would have run, a file value expanded");
        assert!(outcome.learned.is_empty() && outcome.plan_id.is_none());
    }
    assert_eq!(scratch.brain.skills().unwrap().len(), before);
}

#[test]
fn a_home_path_is_hidden_as_the_request_wrote_it() {
    let scratch = Scratch::new("hide-home");
    let home = std::env::var("HOME").unwrap();
    let steps = ["wc -l {file}".to_string()];
    let slots: Vec<Slot> = serde_json::from_value(file_slot("file")).unwrap();
    // The agent proposes the skill and gives the value already expanded, or as the request wrote it.
    for (request, value) in [
        ("how many lines are in ~/notes/n.txt", format!("{home}/notes/n.txt")),
        ("how many lines are in ~/notes/n.txt.", "~/notes/n.txt".to_string()),
        ("how many lines are in $HOME/notes/n.txt", format!("{home}/notes/n.txt")),
        ("how many lines are in ${HOME}/notes/n.txt, please", "${HOME}/notes/n.txt".to_string()),
    ] {
        let proposal =
            new_proposal("count-lines", &steps[0], file_slot("file"), json!([{ "name": "file", "value": value }]));
        let (models, _) = models(|_| decision(NONE, 0.4, 0.95, SAFE), vec![("proposal", proposal)]);
        let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
        let outcome = go(&scratch, &models, request, &mut Script::default(), &dry);
        let phrase = outcome.phrase.clone().unwrap();
        assert!(!phrase.contains("notes") && phrase.starts_with("how many lines are in …"), "{request}: {phrase}");
        // `jli fix` works it out again from the request and the command, and arrives at the same text.
        let again = crate::slots::learned_phrase(request, &steps, &slots, Some(&outcome.commands.join(" && ")));
        assert_eq!(again, phrase, "{request}");
    }

    // A System 1 pick of the path the request named, run, learned, and found again from the event log.
    directory_skill(&scratch, "show-dir", &["ls -d {directory}"]);
    let (models, _) = models(
        |request| {
            let Some(pick) = request["questions"].get("value") else { return decision("show-dir", 0.9, 0.9, SAFE) };
            assert!(pick.to_string().contains("$HOME"));
            json!({"model": "m", "answers": {"value": {"type": "choice", "choice": "$HOME", "confidence": 0.9,
                                                      "probabilities": {"$HOME": 0.9}}}})
        },
        no_language_model(),
    );
    let outcome = go(&scratch, &models, "is $HOME there", &mut Script::default(), &yes(&scratch));
    assert!(outcome.ran && outcome.exit_code == Some(0));
    assert_eq!(outcome.learned, ["\"is … there\" means show-dir"]);
    let event = scratch.brain.events(1).unwrap().remove(0);
    let skill = scratch.brain.skill("show-dir").unwrap().unwrap();
    let again = crate::slots::learned_phrase(&event.query, &skill.steps, &skill.slots, event.command.as_deref());
    assert_eq!(again, "is … there");
    assert!(scratch.brain.remove_example("show-dir", &again).unwrap());
}

#[test]
fn an_agent_proposal_that_gives_a_known_template_a_flag_is_a_command_of_its_own() {
    let scratch = Scratch::new("agent-flag-value");
    directory_skill(&scratch, "list-directory", &["ls {directory}"]);
    directory_skill(&scratch, "go-to-directory", &["cd {directory}"]);
    let flag = |value: &str| json!([{ "name": "directory", "value": value }]);

    // The agent names the known skill with `-la` for its directory, as the translator named `cd {directory}`
    // with `-` for `cd -` (#17). The command it renders is a skill of its own, not a phrasing of that one.
    let mut named = existing_proposal("list-directory");
    named["slotValues"] = flag("-la");
    let (models, _) = models(once_known("ls-la", decision(NONE, 0.4, 0.95, SAFE)), vec![("proposal", named)]);
    let outcome = go(&scratch, &models, "show everything here in detail", &mut Script::default(), &yes(&scratch));
    assert_eq!((outcome.label.as_deref(), outcome.ran, outcome.exit_code()), (Some("ls-la"), true, 0));
    assert_eq!(outcome.commands, ["ls -la"]);
    assert_eq!(outcome.learned, ["new skill ls-la"]);
    let made = scratch.brain.skill("ls-la").unwrap().unwrap();
    assert!(made.steps == ["ls -la"] && made.slots.is_empty(), "the flag is written into the command");
    assert!(lists(&scratch, "list-directory").0.is_empty(), "-la is a flag, not a directory");

    // A new proposal of the same template under another slot name, with the flag for its value, is that
    // command too: here the row just made, which takes the phrasing.
    let renamed = new_proposal("list-all", "ls {dir}", file_slot("dir"), json!([{ "name": "dir", "value": "-la" }]));
    let (models, _) = self::models(|_| decision(NONE, 0.4, 0.95, SAFE), vec![("proposal", renamed)]);
    let outcome = go(&scratch, &models, "long listing of this folder", &mut Script::default(), &yes(&scratch));
    assert_eq!((outcome.label.as_deref(), outcome.commands.clone()), (Some("ls-la"), vec!["ls -la".to_string()]));
    assert_eq!(outcome.learned, ["\"long listing of this folder\" means ls-la"]);
    assert!(scratch.brain.skill("list-all").unwrap().is_none());
    assert!(lists(&scratch, "list-directory").0.is_empty());

    // `cd {directory}` with `-`, the case of the issue, shown in a dry run.
    let mut back = existing_proposal("go-to-directory");
    back["slotValues"] = flag("-");
    let (models, _) = self::models(|_| decision(NONE, 0.4, 0.95, SAFE), vec![("proposal", back)]);
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "go back", &mut Script::default(), &dry);
    assert_eq!((outcome.label.as_deref(), outcome.commands.clone()), (Some("cd"), vec!["cd -".to_string()]));
}

#[test]
fn a_rejected_template_does_not_refuse_its_command_with_a_flag() {
    let scratch = Scratch::new("rejected-flag");
    std::fs::create_dir(scratch.work.join("sub")).unwrap();
    directory_skill(&scratch, "list-directory", &["ls {directory}"]);
    let mut listed = existing_proposal("list-directory");
    listed["slotValues"] = json!([{ "name": "directory", "value": "sub" }]);
    let (models, _) = models(|_| decision(NONE, 0.4, 0.95, SAFE), vec![("proposal", listed)]);
    let ran = go(&scratch, &models, "what is in sub", &mut Script::default(), &yes(&scratch));
    assert_eq!((ran.label.as_deref(), ran.commands.clone()), (Some("list-directory"), vec!["ls sub".to_string()]));

    // `jli fix` rejects `ls {directory}`, and the agent proposes `ls -la`: a flag is never a slot value
    // (ADR 19), so `ls -la` is not the rejected command, and it runs and is learned (ADR 20's gap closed).
    let proposal = new_proposal("list-all", "ls -la", json!([]), json!([]));
    let (models, _) = self::models(once_known("list-all", decision(NONE, 0.4, 0.95, SAFE)), vec![("proposal", proposal)]);
    let fix = Fix { event: ran.event_id, how: FixHow::Unlearn, run: false };
    assert_eq!(app(&scratch, "default", models).fix(&Flags::quiet_yes(), fix).unwrap(), 0, "run, not refused");
    let event = scratch.brain.events(1).unwrap().remove(0);
    assert!(event.accepted && event.label.as_deref() == Some("list-all"), "{event:?}");
    assert_eq!(scratch.brain.skill("list-all").unwrap().unwrap().steps, ["ls -la"]);

    // Named as the rejected skill with `-la` for its directory, the proposal is still `ls -la`, which
    // the rejected template does not run: it is the row just learned, not a repeat.
    let mut named = existing_proposal("list-directory");
    named["slotValues"] = json!([{ "name": "directory", "value": "-la" }]);
    let (models, _) = self::models(|_| decision(NONE, 0.4, 0.95, SAFE), vec![("proposal", named)]);
    let rejected = RouteOptions { rejected: vec!["list-directory".into()], dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "what is in sub", &mut Script::default(), &rejected);
    assert_eq!((code(&outcome), outcome.label.as_deref()), (Some("dry_run"), Some("list-all")));
}

// ── Hand edits: jli alias and jli forget-phrase (ADR 21) ────────────────────────

use crate::hand::{self, AliasEdit, PhraseEdit};

/// The scratch brain opened a second time, mutable, as `jli` opens it for a subcommand.
fn mutable(scratch: &Scratch) -> Brain {
    Brain::open(BrainConfig::file(&scratch.root.join("brain.db"))).unwrap()
}

fn alias_edit(label: &str, slot: &str, name: &str, remove: bool) -> AliasEdit {
    AliasEdit { label: label.into(), slot: slot.into(), name: name.into(), remove, global: false, dry_run: false }
}

fn phrase_edit(label: &str, phrase: &str) -> PhraseEdit {
    PhraseEdit { label: label.into(), phrase: phrase.into(), global: false, dry_run: false }
}

fn aliases(brain: &Brain, label: &str, slot: &str) -> Vec<String> {
    let skill = brain.skill(label).unwrap().unwrap();
    skill.slots.into_iter().find(|known| known.name == slot).unwrap().aliases
}

/// `du -sh {directory}` with the phrasing that was taught to it by mistake in issue #23. Inserted in
/// the brain's own scope, so inside a project it can hide a global skill of the same label.
fn directory_size(brain: &Brain, label: &str) {
    brain
        .add_skill_if_missing(&NewSkill {
            label: label.into(),
            steps: vec!["du -sh {directory}".into()],
            scenario: "the user wants the total size of a directory".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec!["how big is this folder".into(), "size of each file here".into()],
            slots: vec![Slot {
                name: "directory".into(),
                kind: SlotKind::File,
                description: "the directory".into(),
                default: Some(".".into()),
                aliases: vec![],
                ..Slot::default()
            }],
        })
        .unwrap();
}

#[test]
fn a_slot_alias_is_added_and_removed_by_hand() {
    let scratch = Scratch::new("hand-alias");
    first_lines_skill(&scratch);
    let mut brain = mutable(&scratch);

    // Stored the way a learned name is (ADR 12): lower case, `-` as `_`.
    let done = hand::alias(&mut brain, &alias_edit("first-lines", "count", "Line-Count", false)).unwrap();
    assert_eq!(done.refused, None);
    assert_eq!(done.message, "line_count is now another name for {count} in first-lines.");
    assert_eq!(aliases(&scratch.brain, "first-lines", "count"), ["line_count"]);
    assert_eq!(
        (done.json["alias"].as_str(), done.json["changed"].as_bool(), done.json["writtenTo"].as_str()),
        (Some("line_count"), Some(true), Some("global"))
    );

    // The parser reads it at once, with no model: an unexpected call would panic.
    let (models, _) = models(|_| decision("first-lines", 0.9, 0.9, SAFE), no_language_model());
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "top of the notes line_count=2", &mut Script::default(), &options);
    assert_eq!(outcome.commands, ["head -n 2 notes.txt"]);

    // Again, in another spelling: nothing changes, and it is not a refusal.
    let again = hand::alias(&mut brain, &alias_edit("first-lines", "count", "LINE_COUNT", false)).unwrap();
    assert_eq!((again.refused, again.json["changed"].as_bool()), (None, Some(false)));
    assert_eq!(hand::alias(&mut brain, &alias_edit("first-lines", "count", "count", false)).unwrap().refused, None);

    let refusals = [
        (alias_edit("no-such-skill", "count", "n", false), hand::SKILL_NOT_FOUND),
        (alias_edit("first-lines", "lines", "n", false), hand::SLOT_NOT_FOUND),
        (alias_edit("first-lines", "count", "how many", false), hand::ALIAS_INVALID),
        (alias_edit("first-lines", "count", "n", true), hand::ALIAS_NOT_FOUND),
        (alias_edit("first-lines", "count", "count", true), hand::ALIAS_NOT_FOUND),
        (alias_edit("list-files", "directory", "dir", false), hand::SLOT_NOT_FOUND),
    ];
    for (edit, code) in refusals {
        let done = hand::alias(&mut brain, &edit).unwrap();
        assert_eq!(done.refused, Some(code), "{edit:?}: {}", done.message);
    }
    assert_eq!(aliases(&scratch.brain, "first-lines", "count"), ["line_count"], "a refusal writes nothing");

    // A slot keeps at most eight other names.
    for name in ["n", "lines", "num", "how_many", "rows", "top", "k"] {
        assert_eq!(hand::alias(&mut brain, &alias_edit("first-lines", "count", name, false)).unwrap().refused, None);
    }
    let full = hand::alias(&mut brain, &alias_edit("first-lines", "count", "amount", false)).unwrap();
    assert_eq!(full.refused, Some(hand::ALIASES_FULL));

    // A dry run says what it would do and writes nothing.
    let dry = AliasEdit { dry_run: true, ..alias_edit("first-lines", "n", "line_count", true) };
    let done = hand::alias(&mut brain, &dry).unwrap();
    assert!(done.message.starts_with("Dry run: nothing was written."), "{}", done.message);
    assert_eq!(aliases(&scratch.brain, "first-lines", "count").len(), 8);
    assert!(!done.json["aliases"].as_array().unwrap().contains(&json!("line_count")));

    // Removed, with the slot named by any of its names and the alias in any spelling.
    let done = hand::alias(&mut brain, &alias_edit("first-lines", "n", "Line-Count", true)).unwrap();
    assert_eq!(done.message, "line_count is no longer another name for {count} in first-lines.");
    assert!(!aliases(&scratch.brain, "first-lines", "count").contains(&"line_count".to_string()));
    // So line_count= is a name the skill does not know again, and the agent is asked about it (ADR 12).
    let no_slot = json!({ "arguments": [{ "name": "line_count", "placeholder": null }] });
    let (asked, _) = self::models(|_| decision("first-lines", 0.9, 0.9, SAFE), vec![("argument_names", no_slot)]);
    let outcome = go(&scratch, &asked, "top of the notes line_count=2", &mut Script::default(), &options);
    assert_eq!(outcome.unknown_arguments, ["line_count"]);
}

#[test]
fn a_phrasing_taught_to_the_wrong_skill_is_forgotten_and_kept_as_a_counter_example() {
    let scratch = Scratch::new("hand-forget-phrase");
    directory_size(&scratch.brain, "directory-size");
    let mut brain = mutable(&scratch);

    // A dry run first: nothing is written.
    let dry = PhraseEdit { dry_run: true, ..phrase_edit("directory-size", "size of each file here") };
    let done = hand::forget_phrase(&mut brain, &dry).unwrap();
    assert_eq!(done.json["counterExamples"], json!(["size of each file here"]));
    assert_eq!(lists(&scratch, "directory-size").1.len(), 0);

    // Matched as phrasings are, without case or surrounding space; the stored text is what is kept.
    let done = hand::forget_phrase(&mut brain, &phrase_edit("directory-size", "  Size of each file HERE ")).unwrap();
    assert_eq!(done.refused, None);
    assert_eq!(
        done.message,
        "\"size of each file here\" no longer means directory-size, and is kept as a counter-example so that it is not learned straight back."
    );
    assert_eq!(
        lists(&scratch, "directory-size"),
        (vec!["how big is this folder".to_string()], vec!["size of each file here".to_string()])
    );
    let shown = criteria(&scratch, &scratch.brain, "size of each file here");
    assert_eq!(shown["directory-size"]["examples"], json!(["how big is this folder"]));
    assert_eq!(shown["directory-size"]["doesNotApplyTo"], json!(["size of each file here"]));

    // Asked again, it is already done; a phrasing the skill never had, or a skill that is not there, is refused.
    let again = hand::forget_phrase(&mut brain, &phrase_edit("directory-size", "size of each file here")).unwrap();
    assert_eq!((again.refused, again.json["changed"].as_bool()), (None, Some(false)));
    let missing = hand::forget_phrase(&mut brain, &phrase_edit("directory-size", "disk usage")).unwrap();
    assert_eq!(missing.refused, Some(hand::PHRASE_NOT_FOUND));
    assert_eq!(
        missing.message,
        "directory-size has no phrasing \"disk usage\". Its phrasings are: \"how big is this folder\"."
    );
    let nothing = hand::forget_phrase(&mut brain, &phrase_edit("no-such-skill", "anything")).unwrap();
    assert_eq!(nothing.refused, Some(hand::SKILL_NOT_FOUND));

    // Teaching it back by hand lifts the counter-example, as for any phrasing.
    scratch.brain.link_phrase("directory-size", "size of each file here").unwrap();
    assert_eq!(lists(&scratch, "directory-size").1.len(), 0);
}

#[test]
fn a_forgotten_phrasing_routed_back_to_its_skill_is_one_list_or_the_other() {
    let scratch = Scratch::new("hand-forget-then-route");
    directory_size(&scratch.brain, "directory-size");
    let mut brain = mutable(&scratch);
    let phrase = "size of each file here";
    let forget = |brain: &mut Brain| hand::forget_phrase(brain, &phrase_edit("directory-size", phrase)).unwrap();
    let route_at = |confidence: f64| {
        let (models, _) = models(move |_| decision("directory-size", confidence, 0.95, SAFE), no_language_model());
        go(&scratch, &models, phrase, &mut Script::default(), &yes(&scratch))
    };
    let only_examples = (vec!["how big is this folder".to_string(), phrase.to_string()], vec![]);
    let only_counter = (vec!["how big is this folder".to_string()], vec![phrase.to_string()]);

    // A route System 1 is unsure of (below 0.95), accepted, teaches the phrasing back: it is linked as
    // teaching links it (ADR 20), so it leaves the counter-examples and the edit is undone, not doubled.
    forget(&mut brain);
    let outcome = route_at(0.9);
    assert!(outcome.ran);
    assert_eq!(outcome.learned, [format!("\"{phrase}\" means directory-size")]);
    assert_eq!(lists(&scratch, "directory-size"), only_examples);

    // A confident route (0.95 or above) writes nothing (ADR 5): the phrasing stays a counter-example only.
    forget(&mut brain);
    let outcome = route_at(0.96);
    assert!(outcome.ran && outcome.learned.is_empty());
    assert_eq!(lists(&scratch, "directory-size"), only_counter);
}

#[test]
fn forgetting_a_global_phrasing_inside_a_project_stays_in_that_project() {
    let scratch = Scratch::new("hand-forget-in-project");
    directory_size(&scratch.brain, "directory-size");
    let mut shop = in_project(&scratch, "shop");
    let blog = in_project(&scratch, "blog");
    let blog_before = criteria(&scratch, &blog, "size of each file here");

    let done = hand::forget_phrase(&mut shop, &phrase_edit("directory-size", "size of each file here")).unwrap();
    assert_eq!(done.json["writtenTo"], json!("project_lesson"));
    assert!(
        done.message.contains("in project shop") && done.message.contains("jli forget-phrase --global"),
        "{}",
        done.message
    );
    let in_shop = criteria(&scratch, &shop, "size of each file here");
    assert_eq!(in_shop["directory-size"]["examples"], json!(["how big is this folder"]));
    assert_eq!(in_shop["directory-size"]["doesNotApplyTo"], json!(["size of each file here"]));
    // The global row is untouched, so every other project is shown what it was.
    assert_eq!(lists(&scratch, "directory-size").0, ["how big is this folder", "size of each file here"]);
    assert_eq!(criteria(&scratch, &blog, "size of each file here"), blog_before);

    // A phrasing the project learned itself is the project's alone, and the message does not send anyone elsewhere.
    shop.add_example("directory-size", "how much room does this take").unwrap();
    let done = hand::forget_phrase(&mut shop, &phrase_edit("directory-size", "how much room does this take")).unwrap();
    assert_eq!(done.json["writtenTo"], json!("project_lesson"));
    assert!(!done.message.contains("other projects"), "{}", done.message);
    assert!(!criteria(&scratch, &shop, "x")["directory-size"]["examples"].to_string().contains("room"));

    // --global edits the row itself, for every project.
    let everywhere = PhraseEdit { global: true, ..phrase_edit("directory-size", "how big is this folder") };
    let done = hand::forget_phrase(&mut shop, &everywhere).unwrap();
    assert_eq!((done.json["writtenTo"].as_str(), done.json["project"].as_str()), (Some("global"), Some("shop")));
    assert_eq!(lists(&scratch, "directory-size").1, ["how big is this folder"]);
    assert_eq!(criteria(&scratch, &blog, "x")["directory-size"]["doesNotApplyTo"], json!(["how big is this folder"]));

    // A project's own skill is edited in its own row.
    directory_size(&shop, "shop-size");
    let done = hand::forget_phrase(&mut shop, &phrase_edit("shop-size", "size of each file here")).unwrap();
    assert_eq!(done.json["writtenTo"], json!("project"));
    assert_eq!(shop.project_skills().unwrap()[0].counter_examples, ["size of each file here"]);
}

#[test]
fn an_alias_from_inside_a_project_is_kept_where_the_skill_is() {
    let scratch = Scratch::new("hand-alias-in-project");
    directory_size(&scratch.brain, "directory-size");
    let mut shop = in_project(&scratch, "shop");

    // A name for a value does not steer a route, so it goes to the global row, as a learned one does (ADR 17).
    let done = hand::alias(&mut shop, &alias_edit("directory-size", "directory", "dir", false)).unwrap();
    assert_eq!(done.json["writtenTo"], json!("global"));
    assert_eq!(
        done.message,
        "dir is now another name for {directory} in directory-size. directory-size is a global skill, so this holds in every project."
    );
    assert_eq!(aliases(&scratch.brain, "directory-size", "directory"), ["dir"]);

    // A project skill of the same label hides the global one: a plain edit is the project's, --global the global row's.
    directory_size(&shop, "directory-size");
    let done = hand::alias(&mut shop, &alias_edit("directory-size", "directory", "folder", false)).unwrap();
    assert_eq!(done.json["writtenTo"], json!("project"));
    assert_eq!(aliases(&shop, "directory-size", "directory"), ["folder"]);
    assert_eq!(aliases(&scratch.brain, "directory-size", "directory"), ["dir"]);
    let global = AliasEdit { global: true, ..alias_edit("directory-size", "directory", "dir", true) };
    let done = hand::alias(&mut shop, &global).unwrap();
    assert_eq!(done.message, "dir is no longer another name for {directory} in directory-size, in every project.");
    assert!(aliases(&scratch.brain, "directory-size", "directory").is_empty());
    assert_eq!(aliases(&shop, "directory-size", "directory"), ["folder"]);
}

#[test]
fn a_hand_edit_never_reaches_another_users_skill() {
    let scratch = Scratch::new("hand-users");
    directory_size(&scratch.brain, "directory-size");
    let mut agent = Brain::open_as(BrainConfig::file(&scratch.root.join("brain.db")), "agent").unwrap();
    let refused = hand::alias(&mut agent, &alias_edit("directory-size", "directory", "dir", false)).unwrap();
    assert_eq!(refused.refused, Some(hand::SKILL_NOT_FOUND), "another user's skill is not there to edit");

    // A shared skill is the other user's own copy: an edit there stays there.
    scratch.brain.share("directory-size", "agent").unwrap();
    hand::alias(&mut agent, &alias_edit("directory-size", "directory", "dir", false)).unwrap();
    hand::forget_phrase(&mut agent, &phrase_edit("directory-size", "size of each file here")).unwrap();
    assert_eq!(aliases(&agent, "directory-size", "directory"), ["dir"]);
    assert_eq!(agent.skill("directory-size").unwrap().unwrap().counter_examples, ["size of each file here"]);
    assert!(aliases(&scratch.brain, "directory-size", "directory").is_empty());
    assert_eq!(
        lists(&scratch, "directory-size"),
        (vec!["how big is this folder".to_string(), "size of each file here".to_string()], vec![])
    );
    assert_eq!(
        agent.shared_from().unwrap().get("directory-size").map(String::as_str),
        Some("default"),
        "still shown as shared"
    );
}

// ── Optional slots, defaults and choices (ADR 24) ───────────────────────────────

use crate::hand::SlotEdit;

fn release_or_debug() -> Vec<Choice> {
    vec![Choice { value: "release".into(), text: "--release".into() }, Choice { value: "debug".into(), text: String::new() }]
}

/// `echo ran --count {count}`, whose count may be left out with its flag, and `echo build {profile}`,
/// whose profile is release or debug, and release when the request names neither.
fn optional_skills(scratch: &Scratch) {
    let add = |label: &str, step: &str, slot: Slot| {
        let skill = NewSkill {
            label: label.into(),
            steps: vec![step.into()],
            scenario: format!("the user wants {label}"),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![slot],
        };
        scratch.brain.add_skill(&skill).unwrap();
    };
    let count = Slot {
        name: "count".into(),
        kind: SlotKind::Number,
        optional: true,
        flag: Some("--count".into()),
        ..Slot::default()
    };
    add("count-run", "echo ran --count {count}", count);
    let profile = Slot {
        name: "profile".into(),
        description: "the cargo profile".into(),
        default: Some("release".into()),
        choices: release_or_debug(),
        ..Slot::default()
    };
    add("build-app", "echo build {profile}", profile);
}

#[test]
fn an_optional_slot_nothing_names_is_left_out_with_its_flag_and_no_model() {
    let scratch = Scratch::new("optional");
    optional_skills(&scratch);
    // Any language model call panics: the agent is never asked to fill an optional slot.
    let (models, _) = models(|_| decision("count-run", 0.9, 0.9, SAFE), no_language_model());
    let outcome = go(&scratch, &models, "run it", &mut Script::default(), &yes(&scratch));
    assert_eq!(
        (outcome.commands.as_slice(), outcome.output.as_deref()),
        (["echo ran".to_string()].as_slice(), Some("ran\n"))
    );
    assert!(outcome.values.is_empty());
    assert_eq!((outcome.accepts[0].optional, outcome.accepts[0].flag.as_deref()), (true, Some("--count")));

    let command =
        |request: &str| go(&scratch, &models, request, &mut Script::default(), &yes(&scratch)).commands.join(" && ");
    assert_eq!(command("run it 5 times"), "echo ran --count 5", "a number in the request");
    assert_eq!(command("run it count=two"), "echo ran --count 2", "stated by name");

    // A dry run's plan is ready: an optional slot is not a value it waits for, nor one it takes later.
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "run it", &mut Script::default(), &dry);
    assert!(outcome.missing.is_empty());
    let late = [("count".to_string(), "3".to_string())];
    let flags = RunFlags { yolo: false, capture: true };
    let refused = run_plans(
        &scratch.brain,
        &no_model_at_all(),
        &[outcome.plan_id.unwrap()],
        &late,
        &quiet(),
        &mut Script::default(),
        flags,
    );
    assert_eq!(refused.unwrap_err().to_string(), "none of these plans is missing a value for count=");
    let ran = plans(&scratch, &no_model_at_all(), &[outcome.plan_id.unwrap()], &[], &mut Script::default());
    assert_eq!((ran[0].commands.as_slice(), ran[0].output.as_deref()), (["echo ran".to_string()].as_slice(), Some("ran\n")));
}

#[test]
fn a_slot_with_choices_writes_what_the_choice_says_and_its_default_when_none_is_named() {
    let scratch = Scratch::new("choices");
    optional_skills(&scratch);
    let (models, _) = models(|_| decision("build-app", 0.9, 0.9, SAFE), no_language_model());
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let route = |request: &str| go(&scratch, &models, request, &mut Script::default(), &options);
    let outcome = route("build the app");
    assert_eq!(outcome.commands, ["echo build --release"]);
    assert_eq!(outcome.values, [SlotValue { name: "profile".into(), value: "release".into(), source: "default".into() }]);
    let outcome = route("build the app in debug mode");
    assert_eq!(outcome.commands, ["echo build"], "a choice with no text writes nothing");
    assert_eq!((outcome.values[0].value.as_str(), outcome.values[0].source.as_str()), ("debug", "pattern"));
    assert_eq!(outcome.phrase.as_deref(), Some("build the app in … mode"), "the value is hidden from what is learned");
    assert_eq!(route("build profile=DEBUG").values[0].value, "debug", "named without case, kept as the choice spells it");
    assert_eq!(route("build profile: debug please").commands, ["echo build"], "a label takes one word");
    assert_eq!(route("a release build, not a debug one").commands, ["echo build --release"], "two named is the default");
    assert_eq!(route("build the debugging tools").commands, ["echo build --release"], "debug inside a word is not named");
}

#[test]
fn a_value_a_slot_does_not_offer_blocks_the_run_until_one_it_does() {
    let scratch = Scratch::new("not-a-choice");
    optional_skills(&scratch);
    let (models, _) = models(|_| decision("build-app", 0.9, 0.9, SAFE), no_language_model());
    let outcome = go(&scratch, &models, "build profile=fast", &mut Script::default(), &yes(&scratch));
    assert!(!outcome.ran);
    assert_eq!(code(&outcome), Some("missing_value"));
    assert_eq!(outcome.blocked.as_deref(), Some("{profile} takes release or debug, not fast"));
    assert_eq!((outcome.missing[0].name.as_str(), outcome.missing[0].choices.len()), ("profile", 2));

    let id = outcome.plan_id.unwrap();
    let refused = plans(&scratch, &no_model_at_all(), &[id], &[("profile", "fast")], &mut Script::default());
    assert_eq!(code(&refused[0]), Some("missing_value"));
    assert!(refused[0].blocked.as_deref().unwrap().ends_with("({profile} takes release or debug, not fast)"));
    let ran = plans(&scratch, &no_model_at_all(), &[id], &[("profile", "Release")], &mut Script::default());
    assert_eq!(ran[0].commands, ["echo build --release"]);
    assert_eq!(ran[0].output.as_deref(), Some("build --release\n"));
}

#[test]
fn a_slot_with_choices_and_no_default_is_picked_by_system1_among_them() {
    let scratch = Scratch::new("choice-pick");
    let target = Slot {
        name: "target".into(),
        description: "where to deploy".into(),
        choices: vec![
            Choice { value: "staging".into(), text: "--staging".into() },
            Choice { value: "production".into(), text: "--prod".into() },
        ],
        ..Slot::default()
    };
    let skill = NewSkill {
        label: "deploy".into(),
        steps: vec!["echo deploy {target}".into()],
        scenario: "the user wants the app deployed".into(),
        risk: Risk::Modifies,
        origin: Origin::Taught,
        examples: vec![],
        slots: vec![target],
    };
    scratch.brain.add_skill(&skill).unwrap();
    let (models, _) = models(
        |request| {
            let Some(pick) = request["questions"].get("value") else { return decision("deploy", 0.9, 0.9, SAFE) };
            let options = pick.to_string();
            assert!(options.contains("staging") && options.contains("production"), "{options}");
            json!({"model": "m", "answers": {"value": {"type": "choice", "choice": "staging", "confidence": 0.8,
                                                      "probabilities": {"staging": 0.8}}}})
        },
        no_language_model(),
    );
    let options = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "ship it to the test servers", &mut Script::default(), &options);
    assert_eq!(outcome.commands, ["echo deploy --staging"]);
    assert_eq!(outcome.values[0].source, "system1");
}

#[test]
fn the_agent_makes_a_flag_and_its_value_optional_but_never_a_bare_value() {
    let scratch = Scratch::new("optional-proposed");
    let slots = json!([
        { "name": "count", "kind": "number", "description": "how many", "default": "3", "aliases": [], "optional": true, "flag": "--count" },
        { "name": "word", "kind": "text", "description": "a word", "default": null, "aliases": [], "optional": true, "flag": null },
    ]);
    let values = json!([{ "name": "word", "value": "hi" }]);
    let proposal = new_proposal("count-run", "echo ran --count {count} {word}", slots, values);
    let (models, _) = models(once_known("count-run", decision(NONE, 0.4, 0.95, SAFE)), vec![("proposal", proposal)]);
    let outcome = go(&scratch, &models, "run it saying hi", &mut Script::default(), &yes(&scratch));
    assert_eq!(outcome.commands, ["echo ran hi"]);
    assert_eq!(outcome.output.as_deref(), Some("ran hi\n"));
    let slots = scratch.brain.skill("count-run").unwrap().unwrap().slots;
    assert_eq!((slots[0].optional, slots[0].flag.as_deref(), &slots[0].default), (true, Some("--count"), &None));
    assert!(!slots[1].optional, "an optional value with no flag is not the agent's to drop");

    // Next time the count can be said, and is written with its flag; nobody is asked.
    let (silent, _) = self::models(|_| decision("count-run", 0.9, 0.9, SAFE), no_language_model());
    let outcome = go(&scratch, &silent, "run it 4 times saying \"yo\"", &mut Script::default(), &yes(&scratch));
    assert_eq!(outcome.output.as_deref(), Some("ran --count 4 yo\n"));
}

#[test]
fn a_taught_flag_value_is_optional_and_the_command_without_it_joins_the_skill() {
    let scratch = Scratch::new("teach-optional");
    let mut row = draft("git log --oneline -n {count}", "3");
    row["slots"][0]["optional"] = json!(true);
    row["slots"][0]["flag"] = json!("-n");
    row["slots"][0]["default"] = json!("3");
    let (models, _) = models(
        |request| {
            if request["state"]["request"] == "show the last 3 commits" {
                decision("git-recent-commits", 0.92, 0.9, SAFE)
            } else {
                decision("git-status", 0.9, 0.9, SAFE)
            }
        },
        vec![("matrix_row", row)],
    );
    let teach = |phrase: &str, command: &str, models: &Models| {
        let input = LearnInput {
            phrase: phrase.into(),
            steps: vec![command.into()],
            risk: None,
            mode: Mode::Semi,
            threshold: 0.5,
            cwd: scratch.work.clone(),
        };
        learn_phrase(&scratch.brain, models, &input, &quiet()).unwrap()
    };
    assert!(teach("show the last 3 commits", "git log --oneline -n 3", &models).created);
    let skill = scratch.brain.skill("git-recent-commits").unwrap().unwrap();
    assert_eq!((skill.slots[0].optional, skill.slots[0].flag.as_deref()), (true, Some("-n")));
    assert_eq!(skill.slots[0].default, None, "a model's default would never let the count be left out");

    // The command without the count is the same skill: no model is asked.
    let (silent, _) = self::models(|_| panic!("System 1 must not be asked"), no_language_model());
    let learned = teach("show the log", "git log --oneline", &silent);
    assert_eq!((learned.label.as_str(), learned.created), ("git-recent-commits", false));
    let skill = scratch.brain.skill("git-recent-commits").unwrap().unwrap();
    assert_eq!(skill.examples, ["show the last 3 commits", "show the log"]);
}

#[test]
fn the_agent_is_shown_a_slot_as_its_prompt_was_measured() {
    let scratch = Scratch::new("placeholders");
    let message = Slot {
        name: "message".into(),
        description: "the commit message".into(),
        flag: Some("-m".into()),
        ..Slot::default()
    };
    assert!(scratch.brain.set_slot("git-commit", &message).unwrap());
    let written = json!({ "values": [{ "name": "message", "value": "wip" }] });
    let (models, replies) = models(|_| decision("git-commit", 0.9, 0.9, [0.2, 0.8, 0.0]), vec![("slot_values", written)]);
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "commit my work", &mut Script::default(), &dry);
    assert_eq!(outcome.commands, ["git commit -m wip"]);
    let seen = replies.seen.lock().unwrap();
    let input: Value = serde_json::from_str(seen[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        input["placeholders"],
        json!([{ "name": "message", "kind": "text", "description": "the commit message" }]),
        "a flag, like any setting of ADR 24, is not part of what the agent is shown"
    );
}

fn slot_edit(label: &str, slot: &str) -> SlotEdit {
    SlotEdit { label: label.into(), slot: slot.into(), ..SlotEdit::default() }
}

#[test]
fn a_placeholder_is_given_choices_and_a_default_by_hand() {
    let scratch = Scratch::new("hand-slot");
    // Taught as written, with a placeholder no slot declares: it blocks until `jli slot` declares it.
    let (label, _) = scratch.brain.learn_command("build the app", &["echo build {profile}".into()], Risk::Safe).unwrap();
    assert_eq!(label, "echo-build-profile");
    let (models, _) = models(|_| decision("echo-build-profile", 0.9, 0.9, SAFE), no_language_model());
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    assert_eq!(code(&go(&scratch, &models, "build the app", &mut Script::default(), &dry)), Some("missing_value"));

    let mut brain = mutable(&scratch);
    let edit = SlotEdit {
        choices: Some(release_or_debug()),
        default: Some(Some("Release".into())),
        ..slot_edit(&label, "profile")
    };
    let dry_edit = hand::slot(&mut brain, &SlotEdit { dry_run: true, ..edit.clone() }).unwrap();
    assert!(dry_edit.message.starts_with("Dry run: nothing was written."), "{}", dry_edit.message);
    assert!(scratch.brain.skill(&label).unwrap().unwrap().slots.is_empty());
    let done = hand::slot(&mut brain, &edit).unwrap();
    assert_eq!(done.refused, None);
    assert_eq!(
        done.message,
        "{profile} in echo-build-profile is now a slot: release → --release, debug → nothing; default release."
    );
    assert_eq!((done.json["declared"].as_bool(), done.json["default"].as_str()), (Some(true), Some("release")));
    assert_eq!(go(&scratch, &models, "build the app", &mut Script::default(), &dry).commands, ["echo build --release"]);
    assert_eq!(go(&scratch, &models, "a debug build", &mut Script::default(), &dry).commands, ["echo build"]);
    let again = hand::slot(&mut brain, &edit).unwrap();
    assert!(again.message.ends_with("Nothing was changed."), "{}", again.message);
}

#[test]
fn a_slot_is_made_optional_with_its_flag_by_hand_and_keeps_its_aliases() {
    let scratch = Scratch::new("hand-optional");
    first_lines_skill(&scratch);
    let mut brain = mutable(&scratch);
    hand::alias(&mut brain, &alias_edit("first-lines", "count", "n", false)).unwrap();
    let edit = SlotEdit { optional: Some(true), flag: Some(Some("-n".into())), ..slot_edit("first-lines", "N") };
    let done = hand::slot(&mut brain, &edit).unwrap();
    assert_eq!(done.message, "{count} in first-lines is now: optional, left out with -n.");
    assert_eq!(aliases(&scratch.brain, "first-lines", "count"), ["n"]);
    let (models, _) = models(|_| decision("first-lines", 0.9, 0.9, SAFE), no_language_model());
    let outcome = go(&scratch, &models, "top of the notes", &mut Script::default(), &yes(&scratch));
    assert_eq!(outcome.commands, ["head notes.txt"]);
    assert_eq!(outcome.output.as_deref(), Some("one\ntwo\nthree\n"));

    // `--required` puts it back as it was.
    hand::slot(&mut brain, &SlotEdit { optional: Some(false), ..slot_edit("first-lines", "count") }).unwrap();
    assert!(!scratch.brain.skill("first-lines").unwrap().unwrap().slots[0].optional);
}

#[test]
fn a_slot_setting_that_cannot_hold_is_refused_and_changes_nothing() {
    let scratch = Scratch::new("hand-slot-refused");
    first_lines_skill(&scratch);
    optional_skills(&scratch);
    let joined = NewSkill {
        label: "cargo-profile".into(),
        steps: vec!["cargo build --profile={profile}".into()],
        scenario: "the user wants a build".into(),
        risk: Risk::Safe,
        origin: Origin::Taught,
        examples: vec![],
        slots: vec![Slot { name: "profile".into(), ..Slot::default() }],
    };
    scratch.brain.add_skill(&joined).unwrap();
    let before = scratch.brain.skills().unwrap();
    let mut brain = mutable(&scratch);
    let choice = |value: &str| Choice { value: value.into(), text: value.into() };
    for (edit, code) in [
        (SlotEdit { default: Some(Some("1".into())), ..slot_edit("no-such-skill", "count") }, hand::SKILL_NOT_FOUND),
        (SlotEdit { default: Some(Some("1".into())), ..slot_edit("first-lines", "lines") }, hand::SLOT_NOT_FOUND),
        (SlotEdit { flag: Some(Some("-x".into())), ..slot_edit("first-lines", "count") }, hand::FLAG_INVALID),
        (SlotEdit { flag: Some(Some("n".into())), ..slot_edit("first-lines", "count") }, hand::FLAG_INVALID),
        (SlotEdit { optional: Some(true), ..slot_edit("cargo-profile", "profile") }, hand::FLAG_INVALID),
        (SlotEdit { choices: Some(vec![choice("-x")]), ..slot_edit("build-app", "profile") }, hand::CHOICE_INVALID),
        (SlotEdit { choices: Some(vec![choice("two words")]), ..slot_edit("build-app", "profile") }, hand::CHOICE_INVALID),
        (
            SlotEdit { choices: Some(vec![choice("a"), choice("A")]), ..slot_edit("build-app", "profile") },
            hand::CHOICE_INVALID,
        ),
        (SlotEdit { default: Some(Some("fast".into())), ..slot_edit("build-app", "profile") }, hand::DEFAULT_INVALID),
        (SlotEdit { choices: Some(vec![choice("dev")]), ..slot_edit("build-app", "profile") }, hand::DEFAULT_INVALID),
    ] {
        let done = hand::slot(&mut brain, &edit).unwrap();
        assert_eq!(done.refused, Some(code), "{edit:?}: {}", done.message);
    }
    let joined = SlotEdit { optional: Some(true), ..slot_edit("cargo-profile", "profile") };
    assert_eq!(
        hand::slot(&mut brain, &joined).unwrap().message,
        "{profile} is joined to --profile= in cargo build --profile={profile}, so leaving it out would leave --profile= behind. Name it as its flag too: --flag=--profile="
    );
    assert_eq!(scratch.brain.skills().unwrap(), before);
}

// ── Quiet runs: no trace, but a question shows what it asks about (ADR 27) ─────

/// The flags `JLI_QUIET=1` gives, with no flag on the command line.
fn jli_quiet() -> Flags {
    crate::cli::switches_from(Flags::default(), |name| (name == "JLI_QUIET").then(|| "1".to_string()))
}

#[test]
fn a_quiet_confirmation_shows_the_command_and_its_risk() {
    for (flags, why) in [(Flags::quiet(), "--quiet"), (jli_quiet(), "JLI_QUIET=1")] {
        let scratch = Scratch::new("quiet-confirm");
        let (models, _) = models(|_| decision("git-stage-all", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
        let mut script = Script::new(&[None], &[]);
        let options = scratch.options(Mode::Semi);
        let outcome = route(&scratch.brain, &models, "stage everything", &flags.ui(), &mut script, &options).unwrap();
        assert_eq!(code(&outcome), Some("needs_confirmation"));
        let about = script.about("Run it?");
        assert!(about.contains("→ ") && about.contains("git add -A"), "{why}: {about}");
        assert!(about.contains("risk:") && about.contains("modifies"), "{why}: {about}");
        assert_eq!(script.questions, ["Run it?"], "{why}: the question itself is as it was");
    }

    // With the trace on, the command was shown above the question already, so it is not shown twice.
    let scratch = Scratch::new("trace-confirm");
    let (models, _) = models(|_| decision("git-stage-all", 0.9, 0.9, [0.2, 0.8, 0.0]), no_language_model());
    let mut script = Script::new(&[None], &[]);
    route(&scratch.brain, &models, "stage everything", &Ui::new(true), &mut script, &scratch.options(Mode::Semi)).unwrap();
    assert_eq!(script.about("Run it?"), "");
}

#[test]
fn a_quiet_correction_shows_the_options_it_offers() {
    let suggestion = json!({
        "verdict": "not_applicable", "explanation": "You said no.",
        "options": [
            { "command": "ls -la", "description": "everything, with details" },
            { "command": "ls -ltr", "description": "newest last" },
        ],
    });
    for (ui, shown) in [(Flags::quiet().ui(), true), (Ui::new(true), false)] {
        let scratch = Scratch::new("quiet-correct");
        let (models, _) = models(|_| decision("list-files", 0.9, 0.9, SAFE), vec![("suggestion", suggestion.clone())]);
        let mut script = Script::new(&[Some(false)], &["2"]);
        let options = scratch.options(Mode::Auto);
        let outcome = route(&scratch.brain, &models, "recently changed files", &ui, &mut script, &options).unwrap();
        assert_eq!(outcome.commands, ["ls -ltr"]);
        let about = script.about("Pick a number");
        if shown {
            assert!(script.about("Run it?").contains("→ ") && script.about("Run it?").contains("ls"));
            for part in ["1", "ls -la", "everything, with details", "2", "ls -ltr", "newest last"] {
                assert!(about.contains(part), "{part} is not in {about}");
            }
        } else {
            assert_eq!((script.about("Run it?"), about), (String::new(), String::new()), "the trace showed them");
        }
    }

    // After a failed run under --yes nobody was asked "Run it?", so the diagnosis comes with the options.
    let scratch = Scratch::new("quiet-diagnosis");
    let wrong = json!({
        "verdict": "wrong_command", "explanation": "This directory is not a git repository.",
        "options": [{ "command": "ls", "description": "list the files instead" }],
    });
    let (models, _) = models(|_| decision("git-status", 0.9, 0.9, SAFE), vec![("suggestion", wrong)]);
    let mut script = Script::new(&[], &[""]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Auto) };
    let outcome = route(&scratch.brain, &models, "what changed here", &Flags::quiet().ui(), &mut script, &options).unwrap();
    assert_eq!(outcome.exit_code, Some(128));
    let about = script.about("Pick a number");
    assert!(about.contains("diagnosis") && about.contains("wrong command"), "{about}");
    assert!(about.contains("not a git repository") && about.contains("list the files instead"), "{about}");
}

#[test]
fn a_quiet_destructive_plan_shows_what_it_would_delete() {
    let scratch = Scratch::new("quiet-plan");
    let log = scratch.work.join("old.log");
    std::fs::write(&log, "noise\n").unwrap();
    let proposal = new_proposal("delete-log-files", "rm -f *.log", json!([]), json!([]));
    let (models, _) = models(|_| decision(NONE, 0.8, 0.95, [0.1, 0.3, 0.6]), vec![("proposal", proposal)]);
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let blocked = go(&scratch, &models, "delete all the log files", &mut Script::new(&[None], &[]), &options);
    let id = blocked.plan_id.unwrap();

    let mut script = Script::new(&[Some(false)], &[]);
    let flags = RunFlags { yolo: false, capture: true };
    run_plans(&scratch.brain, &no_model_at_all(), &[id], &[], &Flags::quiet().ui(), &mut script, flags).unwrap();
    let about = script.about("Run it?");
    assert!(about.contains("rm -f *.log") && about.contains("destructive"), "{about}");
    assert!(log.exists());
}

#[test]
fn a_quiet_generalization_shows_what_would_be_written() {
    for (ui, shown) in [(Ui::of(true, false), true), (Ui::new(true), false)] {
        let scratch = Scratch::new("quiet-generalize");
        let (rust, _node) = two_projects(&scratch);
        let (models, _) =
            self::models(routes_own_phrasing_to("cargo-test-package"), vec![("generalization", parameterized())]);
        let mut script = Script::new(&[Some(false)], &[]);
        generalize(&rust, &models, &options_for(&scratch), &ui, &mut script).unwrap();
        let about = script.about("Make it global?");
        if shown {
            for part in ["cargo-test-package", "cargo test -p {package}", "If approved:", "removed from rust"] {
                assert!(about.contains(part), "{part} is not in {about}");
            }
        } else {
            assert_eq!(about, "", "the trace showed it");
        }
    }
}
