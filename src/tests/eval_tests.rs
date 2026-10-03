//! Measuring the matrix (`jli eval`), optimising it, and the check a row gets before it is written.
//! Offline, against the fakes in the parent module.

use super::*;

/// A System 1 answer with runner-ups: `(label, probability)` after the choice.
fn ranked(choice: &str, confidence: f64, others: &[(&str, f64)]) -> Value {
    let mut answer = decision(choice, confidence, 0.9, SAFE);
    let probabilities = &mut answer["answers"]["skill"]["probabilities"];
    for (label, probability) in others {
        probabilities[*label] = json!(probability);
    }
    answer
}

fn request_of(request: &Value) -> String {
    request["state"]["request"].as_str().unwrap_or_default().to_string()
}

fn has_label(request: &Value, label: &str) -> bool {
    request["questions"]["skill"]["criteria"].get(label).is_some()
}

fn scenario_of(request: &Value, label: &str) -> String {
    request["questions"]["skill"]["criteria"][label]["scenario"].as_str().unwrap_or_default().to_string()
}

fn add(scratch: &Scratch, label: &str, step: &str, scenario: &str, examples: &[&str], uses: usize) {
    scratch
        .brain
        .add_skill(&NewSkill {
            label: label.into(),
            steps: vec![step.into()],
            scenario: scenario.into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: examples.iter().map(|example| example.to_string()).collect(),
            slots: vec![],
        })
        .unwrap();
    for _ in 0..uses {
        scratch.brain.record_use(label).unwrap();
    }
}

fn row(label: &str, steps: &str, scenario: &str) -> Value {
    json!({
        "existingLabel": null, "label": label, "steps": [steps], "scenario": scenario,
        "slots": [], "slotValues": [], "risk": "safe",
    })
}

fn teach(scratch: &Scratch, models: &Models, phrase: &str, command: &str) -> crate::learn::Learned {
    let input = LearnInput {
        phrase: phrase.into(),
        steps: vec![command.into()],
        risk: None,
        mode: Mode::Semi,
        threshold: 0.5,
        cwd: scratch.work.clone(),
    };
    learn_phrase(&scratch.brain, models, &input, &quiet()).unwrap()
}

// ── Declined routes ──────────────────────────────────────────────────────────

#[test]
fn a_declined_route_is_logged_as_its_own_event() {
    let scratch = Scratch::new("declined-event");
    let (models, _) = models(|_| decision("list-files", 0.9, 0.9, SAFE), no_language_model());
    let mut script = Script::new(&[Some(false)], &["ls -ltr"]);
    go(&scratch, &models, "newest files", &mut script, &scratch.options(Mode::Semi));
    let events = scratch.brain.events(10).unwrap();
    assert_eq!(events.len(), 2, "the declined attempt and the correction are two rows");
    let (taught, declined) = (&events[0], &events[1]);
    assert_eq!((declined.path, declined.label.as_deref()), (Path::System1, Some("list-files")));
    assert_eq!((declined.accepted, declined.learned.as_deref()), (false, Some("corrected")));
    assert_eq!((declined.confidence, declined.command.as_deref()), (Some(0.9), Some("ls")));
    assert_eq!((taught.path, taught.label.as_deref()), (Path::Taught, Some("list-files-by-time")));
    assert!(taught.accepted);
    assert_eq!(taught.confidence, None, "System 1's confidence was in the label turned down");

    // Turned down with nothing typed: one row, marked the same way.
    let mut skipped = Script::new(&[Some(false)], &[""]);
    let outcome = go(&scratch, &models, "files please", &mut skipped, &scratch.options(Mode::Semi));
    assert_eq!(outcome.blocked.as_deref(), Some("declined"));
    let last = &scratch.brain.events(1).unwrap()[0];
    assert_eq!(
        (last.label.as_deref(), last.accepted, last.learned.as_deref()),
        (Some("list-files"), false, Some("corrected"))
    );
}

#[test]
fn asking_the_agent_after_a_no_logs_the_declined_attempt_first() {
    let scratch = Scratch::new("declined-clarify");
    let proposal = json!({
        "kind": "existing", "existingLabel": "list-files-by-time", "label": null, "steps": [],
        "scenario": "", "slots": [], "slotValues": [], "risk": "safe", "explanation": "Newest last.", "answer": null,
    });
    let (models, _) = models(|_| decision("list-files", 0.9, 0.9, SAFE), vec![("proposal", proposal)]);
    let mut script = Script::new(&[Some(false), Some(true)], &["?"]);
    go(&scratch, &models, "files by date", &mut script, &scratch.options(Mode::Semi));
    let events = scratch.brain.events(10).unwrap();
    let rows: Vec<_> = events
        .iter()
        .map(|event| (event.path, event.label.as_deref(), event.learned.as_deref() == Some("corrected")))
        .collect();
    assert_eq!(rows, [(Path::System2, Some("list-files-by-time"), false), (Path::System1, Some("list-files"), true)]);
}

// ── Checking a row before it is written ──────────────────────────────────────

/// Four busy skills whose newest phrasings fill the eight checks the old rule made, most used first,
/// and one skill nobody has used yet.
fn crowded(scratch: &Scratch) {
    for name in ["a", "b", "c", "d"] {
        let examples = [format!("busy {name} one"), format!("busy {name} two")];
        let examples: Vec<&str> = examples.iter().map(String::as_str).collect();
        add(scratch, &format!("busy-{name}"), "true", "the user wants busy work", &examples, 10);
    }
    add(
        scratch,
        "line-count",
        "wc -l notes.txt",
        "the user wants the lines of notes.txt counted",
        &["how long is notes.txt"],
        0,
    );
}

/// Each busy phrasing goes home; the taught phrase goes to the new row with `line-count` as runner-up.
fn crowded_system1(new: &'static str, stolen: impl Fn(bool) -> Value + Send + Sync + 'static) -> impl Fn(&Value) -> Value {
    move |request| {
        let phrase = request_of(request);
        let with_new = has_label(request, new);
        if phrase == "tally things up" {
            return if with_new {
                ranked(new, 0.9, &[("line-count", 0.06)])
            } else {
                decision("line-count", 0.4, 0.9, SAFE)
            };
        }
        if phrase == "how long is notes.txt" {
            return stolen(with_new);
        }
        if let Some(name) = phrase.strip_prefix("busy ").and_then(|rest| rest.split(' ').next()) {
            return decision(&format!("busy-{name}"), 0.9, 0.9, SAFE);
        }
        decision(NONE, 0.5, 0.9, SAFE)
    }
}

#[test]
fn a_broad_row_that_steals_a_little_used_skills_phrasing_is_rejected() {
    let scratch = Scratch::new("verify-low-use");
    crowded(&scratch);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    let system1 = crowded_system1("tally-everything", |with_new| {
        if with_new {
            decision("tally-everything", 0.8, 0.9, SAFE)
        } else {
            decision("line-count", 0.9, 0.9, SAFE)
        }
    });
    let (models, replies) = models(
        move |request| {
            seen.lock().unwrap().push(request_of(request));
            system1(request)
        },
        vec![
            ("matrix_row", row("tally-everything", "wc -l *", "the user wants anything at all counted")),
            ("matrix_row", row("tally-everything", "wc -l *", "the user wants anything at all counted")),
        ],
    );
    let learned = teach(&scratch, &models, "tally things up", "wc -l *");
    assert!(asked.lock().unwrap().iter().any(|phrase| phrase == "how long is notes.txt"), "the closest skill is checked");
    assert_eq!((learned.created, learned.translated), (true, false), "the broad row is not kept");
    let skill = scratch.brain.skill(&learned.label).unwrap().unwrap();
    assert_eq!(skill.scenario, "the user wants: tally things up");
    let seen = replies.seen.lock().unwrap();
    let retry: Value = serde_json::from_str(seen[1]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        retry["previousAttemptProblem"],
        "the scenario is too broad: it captured \"how long is notes.txt\", which belongs to line-count"
    );
}

#[test]
fn a_row_that_knocks_a_neighbour_below_the_threshold_is_rejected() {
    let scratch = Scratch::new("verify-knocked");
    crowded(&scratch);
    let system1 =
        crowded_system1("tally-everything", |with_new| decision("line-count", if with_new { 0.3 } else { 0.9 }, 0.9, SAFE));
    let (models, replies) = models(
        system1,
        vec![
            ("matrix_row", row("tally-everything", "wc -l *", "the user wants anything at all counted")),
            ("matrix_row", row("tally-everything", "wc -l *", "the user wants anything at all counted")),
        ],
    );
    let learned = teach(&scratch, &models, "tally things up", "wc -l *");
    assert!(!learned.translated);
    let seen = replies.seen.lock().unwrap();
    let retry: Value = serde_json::from_str(seen[1]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        retry["previousAttemptProblem"],
        "with it in the matrix, \"how long is notes.txt\", which belongs to line-count, falls to 0.30 confidence"
    );
}

#[test]
fn a_neighbour_that_was_already_weak_does_not_block_a_row() {
    let scratch = Scratch::new("verify-weak");
    crowded(&scratch);
    let system1 = crowded_system1("tally-everything", |_| decision("line-count", 0.3, 0.9, SAFE));
    let (models, _) =
        models(system1, vec![("matrix_row", row("tally-everything", "wc -l *", "the user wants anything at all counted"))]);
    let learned = teach(&scratch, &models, "tally things up", "wc -l *");
    assert_eq!((learned.label.as_str(), learned.translated), ("tally-everything", true));
}

#[test]
fn a_check_that_cannot_reach_system1_stores_no_draft() {
    let scratch = Scratch::new("verify-offline");
    let (models, _) = models(
        |_| json!({ "model": "jev-test", "answers": {} }),
        vec![("matrix_row", row("disk-usage", "du -sh .", "the user wants the size of everything"))],
    );
    let learned = teach(&scratch, &models, "how big is this folder", "du -sh .");
    assert_eq!((learned.created, learned.translated), (true, false));
    let skill = scratch.brain.skill(&learned.label).unwrap().unwrap();
    assert_eq!(skill.scenario, "the user wants: how big is this folder", "the unchecked scenario never reached the brain");
}

// ── A skill the agent proposed ───────────────────────────────────────────────

fn count_lines_proposal() -> Value {
    json!({
        "kind": "new", "existingLabel": null, "label": "count-lines", "steps": ["wc -l {file}"],
        "scenario": "the user wants something about files",
        "slots": [{ "name": "file", "kind": "file", "description": "the file to count", "default": null }],
        "slotValues": [{ "name": "file", "value": "notes.txt" }],
        "risk": "safe", "explanation": "Counts lines.", "answer": null,
    })
}

fn rewrite(scenario: &str) -> Value {
    json!({ "scenario": scenario, "dropExamples": [], "counterExamples": [], "explanation": "Sharper." })
}

#[test]
fn a_proposed_skill_that_fails_the_check_gets_one_rewrite() {
    let scratch = Scratch::new("agent-rewrite");
    std::fs::write(scratch.work.join("notes.txt"), "one\ntwo\n").unwrap();
    let (models, replies) = models(
        |request| {
            if scenario_of(request, "count-lines").contains("number of lines") {
                decision("count-lines", 0.9, 0.95, SAFE)
            } else {
                decision(NONE, 0.7, 0.95, SAFE)
            }
        },
        vec![
            ("proposal", count_lines_proposal()),
            ("matrix_rewrite", rewrite("the user wants the number of lines in one file")),
        ],
    );
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "how many lines are in notes.txt", &mut Script::default(), &options);
    assert_eq!(outcome.output.as_deref().map(str::trim), Some("2 notes.txt"));
    assert_eq!(
        outcome.learned,
        ["new skill count-lines", "the scenario of count-lines was rewritten after the System 1 check"]
    );
    let skill = scratch.brain.skill("count-lines").unwrap().unwrap();
    assert_eq!(skill.scenario, "the user wants the number of lines in one file");
    let seen = replies.seen.lock().unwrap();
    let input: Value = serde_json::from_str(seen[1]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(input["previousAttemptProblem"].as_str().unwrap().starts_with("the phrase itself routed to no skill"));
    assert_eq!(input["skill"]["label"], "count-lines");
}

#[test]
fn a_proposed_skill_that_fails_twice_keeps_the_request_as_its_scenario() {
    let scratch = Scratch::new("agent-fallback");
    std::fs::write(scratch.work.join("notes.txt"), "one\n").unwrap();
    let (models, _) = models(
        |_| decision(NONE, 0.7, 0.95, SAFE),
        vec![("proposal", count_lines_proposal()), ("matrix_rewrite", rewrite("the user wants lines"))],
    );
    let options = RouteOptions { yes: true, ..scratch.options(Mode::Semi) };
    let outcome = go(&scratch, &models, "how many lines are in notes.txt", &mut Script::default(), &options);
    assert_eq!(outcome.learned, ["new skill count-lines", "count-lines keeps the request as its scenario"]);
    let skill = scratch.brain.skill("count-lines").unwrap().unwrap();
    assert_eq!(skill.scenario, "the user wants: how many lines are in notes.txt");
    assert_eq!(skill.examples, ["how many lines are in notes.txt"]);
}
