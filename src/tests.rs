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
        if request["questions"]["skill"]["criteria"].get(label).is_some() {
            decision(label, 0.9, 0.95, SAFE)
        } else {
            before.clone()
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

fn greeting_skill(scratch: &Scratch) {
    let slot = |name: &str, kind: SlotKind, default: Option<&str>| Slot {
        name: name.into(),
        kind,
        description: String::new(),
        default: default.map(String::from),
        aliases: vec![],
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
    assert_eq!(skill.examples, ["how many lines are in notes.txt"]);
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
            "msSystem2"
        ]
    );
    assert_eq!(value["path"], "system1");
    assert_eq!(value["risk"], "safe");
    assert!(value["output"].as_str().unwrap().trim_end().ends_with("work"));
    assert_eq!(value["learned"], json!([]), "a 0.99 route teaches nothing new");
}
