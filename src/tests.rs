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
use crate::types::{error_code, Coded, Mode, Origin, Path, Risk, Slot, SlotKind};
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
    assert_eq!(outcome.label.as_deref(), Some("list-by-date"), "a rejected skill is never chosen again");
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
    let expected = |path: &str| crate::slots::render(&["ls {d}".to_string()], &[("d".into(), path.into())].into());
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
    let expected = crate::slots::render(&["wc -l {f}".to_string()], &[("f".into(), format!("{home}/notes/n.txt"))].into());
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
    // missing_value, dry_run, destructive and the plan codes have their own tests above.
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
    let slot =
        Slot { name: "script".into(), kind: SlotKind::Text, description: String::new(), default: None, aliases: vec![] };
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
