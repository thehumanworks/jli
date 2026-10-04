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
        teaching: Mode::Semi.into(),
        threshold: 0.5,
        cwd: scratch.work.clone(),
        name: None,
        description: None,
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
    assert_eq!(skill.examples.len(), 1);
    assert_eq!(skill.scenario, format!("the user wants: {}", skill.examples[0]), "the request as it was learned");
}

// ── jli eval ─────────────────────────────────────────────────────────────────

use crate::eval::{self, Bounds, EvalOptions};
use crate::store::NewEvent;

fn options(scratch: &Scratch, dry_run: bool, optimise: Option<Bounds>) -> EvalOptions {
    EvalOptions { cwd: scratch.work.clone(), threshold: 0.5, only: None, limit: None, dry_run, optimise }
}

fn event(scratch: &Scratch, query: &str, label: Option<&str>, path: Path, accepted: bool, exit: Option<i64>) -> NewEvent {
    NewEvent {
        cwd: scratch.work.to_string_lossy().into_owned(),
        query: query.into(),
        path,
        label: label.map(String::from),
        confidence: Some(0.9),
        command: Some("du -sh .".into()),
        exit_code: exit,
        accepted,
        learned: None,
        ms_system1: Some(250),
        ms_system2: None,
    }
}

/// Two skills with stored examples, routed by a table. A held-out example never reaches the matrix.
fn measured_brain(scratch: &Scratch) {
    add(
        scratch,
        "disk-usage",
        "du -sh .",
        "the user wants disk use",
        &["how big is this folder", "space used here", "size of everything"],
        3,
    );
    add(scratch, "count-lines", "wc -l notes.txt", "the user wants lines counted", &["lines in a file"], 0);
}

fn table(request: &Value) -> Value {
    let phrase = request_of(request);
    for (label, criterion) in request["questions"]["skill"]["criteria"].as_object().unwrap() {
        let examples = criterion["examples"].as_array().cloned().unwrap_or_default();
        let counters = criterion["doesNotApplyTo"].as_array().cloned().unwrap_or_default();
        assert!(!examples.iter().chain(&counters).any(|example| example == &json!(phrase)), "{phrase} reached {label}");
    }
    match phrase.as_str() {
        "how big is this folder" => decision("disk-usage", 0.9, 0.9, SAFE),
        "space used here" => decision("disk-usage", 0.4, 0.9, SAFE),
        "size of everything" => decision("list-files-with-sizes", 0.8, 0.9, SAFE),
        "lines in a file" => decision("count-lines", 0.95, 0.9, SAFE),
        "What changes are unstaged in this repository?" => decision("git-status", 0.99, 0.9, SAFE),
        "how much room is used" => decision("disk-usage", 0.7, 0.9, SAFE),
        other => panic!("System 1 was not expecting {other:?}"),
    }
}

#[test]
fn leave_one_out_counts_home_steals_and_escalations() {
    let scratch = Scratch::new("loo");
    measured_brain(&scratch);
    let (models, _) = models(table, no_language_model());
    let report = eval::run(&scratch.brain, &models, &options(&scratch, false, None), &quiet()).unwrap();

    let loo = &report.leave_one_out;
    assert_eq!((loo.probes, loo.home, loo.stolen, loo.escalated), (5, 3, 1, 1));
    assert_eq!((loo.accuracy, loo.steal_rate, loo.escalation_rate), (0.6, 0.2, 0.2));
    // Home confidence: 0.9, 0.4 (home, but below the threshold), 0 (stolen), 0.95, 0.99.
    assert_eq!((loo.mean_confidence, loo.worst_confidence), (0.648, 0.0));
    assert_eq!(report.estimated_calls, 5);
    assert!((report.estimated_cost - 0.0003).abs() < 1e-9, "{}", report.estimated_cost);

    let disk = report.skills.iter().find(|skill| skill.label == "disk-usage").unwrap();
    assert_eq!((disk.summary.home, disk.summary.stolen, disk.summary.escalated), (1, 1, 1));
    assert_eq!(disk.steals.len(), 1);
    assert_eq!(disk.steals[0].phrase, "size of everything");
    assert_eq!(disk.steals[0].routed_to.as_deref(), Some("list-files-with-sizes"));
    let lowest: Vec<&str> = disk.lowest.iter().map(|line| line.phrase.as_str()).collect();
    assert_eq!(lowest, ["size of everything", "space used here", "how big is this folder"], "weakest first, home ones too");
    assert_eq!(report.skills[0].label, "disk-usage", "the biggest expected gain comes first");

    let stored = eval::trend(&scratch.brain).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(Some(format!("eval:{}", stored[0].ts)), report.stored);
    assert_eq!(stored[0].overall, report.overall);
}

#[test]
fn accepted_requests_are_replayed_and_the_rest_are_not() {
    let scratch = Scratch::new("replay");
    measured_brain(&scratch);
    let accepted = event(&scratch, "how much room is used", Some("disk-usage"), Path::System2, true, Some(0));
    scratch.brain.log_event(&accepted).unwrap();
    scratch.brain.log_event(&accepted).unwrap();
    let gone_dir = NewEvent { cwd: "/nowhere/at/all/proj".into(), ..accepted.clone() };
    scratch.brain.log_event(&gone_dir).unwrap();
    let declined = NewEvent { learned: Some("corrected".into()), accepted: false, ..accepted.clone() };
    scratch.brain.log_event(&NewEvent { query: "something else".into(), ..declined }).unwrap();
    let failed = event(&scratch, "something failing", Some("disk-usage"), Path::System1, true, Some(1));
    scratch.brain.log_event(&failed).unwrap();
    let forgotten = event(&scratch, "an old one", Some("no-longer-here"), Path::System1, true, Some(0));
    scratch.brain.log_event(&forgotten).unwrap();

    let (models, _) = models(table, no_language_model());
    let report = eval::run(&scratch.brain, &models, &options(&scratch, true, None), &quiet()).unwrap();
    assert_eq!((report.replay.probes, report.replay.home), (1, 1), "one distinct wording, accepted three times");
    assert_eq!(report.forgotten, 1);
    assert_eq!(report.rebuilt_contexts, 1, "the newest of the three ran in a directory that is gone");

    let (context, rebuilt) = eval::context_at("/nowhere/at/all/proj", Some("git status"), Some(0));
    assert!(rebuilt && context.inside_git_repository && context.directory == "proj");
    let (context, _) = eval::context_at("/nowhere/at/all/proj", Some("du -sh ."), Some(0));
    assert!(!context.inside_git_repository);
}

#[test]
fn a_dry_run_measures_and_writes_nothing() {
    let scratch = Scratch::new("dry-eval");
    measured_brain(&scratch);
    let (models, _) = models(table, no_language_model());
    let before = scratch.brain.skills().unwrap();
    let report = eval::run(&scratch.brain, &models, &options(&scratch, true, None), &quiet()).unwrap();
    assert_eq!(report.overall.probes, 5);
    assert_eq!(report.stored, None);
    assert!(scratch.brain.meta_with_prefix("eval:").unwrap().is_empty());
    assert_eq!(scratch.brain.skills().unwrap(), before);
    assert!(scratch.brain.events(10).unwrap().is_empty(), "measuring logs no requests");
}

#[test]
fn a_limit_takes_phrasings_from_every_skill_in_turn() {
    let scratch = Scratch::new("limit");
    measured_brain(&scratch);
    let (models, _) = models(table, no_language_model());
    let limited = EvalOptions { limit: Some(3), ..options(&scratch, false, None) };
    let report = eval::run(&scratch.brain, &models, &limited, &quiet()).unwrap();
    let owners: Vec<&str> = report.skills.iter().map(|skill| skill.label.as_str()).collect();
    assert_eq!(report.overall.probes, 3);
    assert_eq!(owners.len(), 3, "one each from three skills, not three from the first: {owners:?}");
    assert!(eval::trend(&scratch.brain).unwrap()[0].limited);

    let one = EvalOptions { only: Some("count-lines".into()), ..options(&scratch, true, None) };
    let report = eval::run(&scratch.brain, &models, &one, &quiet()).unwrap();
    assert_eq!((report.overall.probes, report.skills.len()), (1, 1));
}

// ── jli eval --optimise ──────────────────────────────────────────────────────

/// "space used here" is weak until disk-usage's scenario talks about total size. With `steals`, that
/// rewrite also takes count-lines' phrasing, which System 1 ranks close behind disk-usage.
fn optimisable(steals: bool) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
    move |request| {
        let sharper = scenario_of(request, "disk-usage").contains("total size");
        match request_of(request).as_str() {
            "space used here" if sharper => ranked("disk-usage", 0.92, &[("count-lines", 0.06)]),
            "space used here" => ranked("disk-usage", 0.4, &[("count-lines", 0.3)]),
            "lines in a file" if sharper && steals => decision("disk-usage", 0.9, 0.9, SAFE),
            "lines in a file" => decision("count-lines", 0.95, 0.9, SAFE),
            "how big is this folder" => decision("disk-usage", 0.9, 0.9, SAFE),
            "What changes are unstaged in this repository?" => decision("git-status", 0.99, 0.9, SAFE),
            other => panic!("System 1 was not expecting {other:?}"),
        }
    }
}

fn optimisable_brain(scratch: &Scratch) {
    add(scratch, "disk-usage", "du -sh .", "the user wants disk use", &["how big is this folder", "space used here"], 5);
    add(scratch, "count-lines", "wc -l notes.txt", "the user wants lines counted", &["lines in a file"], 0);
}

fn sharper() -> Value {
    json!({
        "scenario": "the user wants the total size of the current directory",
        "dropExamples": [], "counterExamples": [{ "label": "count-lines", "phrase": "how much disk this takes" }],
        "explanation": "Names the outcome.",
    })
}

fn bounds() -> Option<Bounds> {
    Some(Bounds { max_skills: 1, max_calls: 100 })
}

#[test]
fn a_rewrite_is_kept_when_the_measurement_improves_and_can_be_undone() {
    let scratch = Scratch::new("optimise-kept");
    optimisable_brain(&scratch);
    let (models, replies) = models(optimisable(false), vec![("matrix_rewrite", sharper())]);
    let report = eval::run(&scratch.brain, &models, &options(&scratch, false, bounds()), &quiet()).unwrap();
    let optimisation = report.optimisation.unwrap();
    assert_eq!(optimisation.ranking[0].label, "disk-usage");
    assert_eq!(optimisation.changes.len(), 1);
    let change = &optimisation.changes[0];
    assert!(change.kept, "{:?}", change.problem);
    assert!(change.score_after > change.score_before);
    assert_eq!((change.home_before, change.home_after, change.probes), (2, 3, 3));
    assert_eq!(optimisation.system2_calls, 1);

    let skill = scratch.brain.skill("disk-usage").unwrap().unwrap();
    assert_eq!(skill.scenario, "the user wants the total size of the current directory");
    assert_eq!(skill.examples, ["how big is this folder", "space used here"], "examples are kept");
    assert_eq!(scratch.brain.skill("count-lines").unwrap().unwrap().counter_examples, ["how much disk this takes"]);
    assert_eq!(scratch.brain.meta_with_prefix("optimised:").unwrap().len(), 1, "the change is on record");
    {
        let seen = replies.seen.lock().unwrap();
        let input: Value = serde_json::from_str(seen[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(input["skill"]["label"], "disk-usage");
        assert_eq!(input["weakPhrasings"][0]["phrase"], "space used here");
        assert_eq!(input["competingSkills"][0]["label"], "count-lines");
    }

    let undone = eval::undo(&scratch.brain, None, false).unwrap();
    assert_eq!(undone.label, "disk-usage");
    assert_eq!(scratch.brain.skill("disk-usage").unwrap().unwrap().scenario, "the user wants disk use");
    assert!(scratch.brain.skill("count-lines").unwrap().unwrap().counter_examples.is_empty());
    assert!(eval::undo(&scratch.brain, None, false).is_err(), "nothing left to undo");
}

#[test]
fn a_rewrite_that_takes_another_skills_phrasing_is_discarded() {
    let scratch = Scratch::new("optimise-regress");
    optimisable_brain(&scratch);
    let (models, replies) = models(optimisable(true), vec![("matrix_rewrite", sharper()), ("matrix_rewrite", sharper())]);
    let report = eval::run(&scratch.brain, &models, &options(&scratch, false, bounds()), &quiet()).unwrap();
    let changes = report.optimisation.unwrap().changes;
    assert_eq!(changes.len(), 2, "one retry with the measured problem");
    assert!(changes.iter().all(|change| !change.kept));
    let problem = "it made \"lines in a file\", which belongs to count-lines, route to disk-usage instead";
    assert_eq!(changes[0].problem.as_deref(), Some(problem));
    let seen = replies.seen.lock().unwrap();
    let retry: Value = serde_json::from_str(seen[1]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(retry["previousAttemptProblem"], problem);
    assert_eq!(scratch.brain.skill("disk-usage").unwrap().unwrap().scenario, "the user wants disk use");
    assert!(scratch.brain.meta_with_prefix("optimised:").unwrap().is_empty());
}

#[test]
fn a_dry_run_optimisation_shows_the_change_and_applies_none() {
    let scratch = Scratch::new("optimise-dry");
    optimisable_brain(&scratch);
    let (models, _) = models(optimisable(false), vec![("matrix_rewrite", sharper())]);
    let before = scratch.brain.skills().unwrap();
    let report = eval::run(&scratch.brain, &models, &options(&scratch, true, bounds()), &quiet()).unwrap();
    let optimisation = report.optimisation.unwrap();
    assert!(optimisation.changes[0].kept && !optimisation.applied);
    assert_eq!(scratch.brain.skills().unwrap(), before);
    assert!(scratch.brain.meta_with_prefix("").unwrap().iter().all(|(key, _)| key == "seeded"), "no meta at all");
}

/// What a project is shown for disk-usage: its scenario in the System 1 criteria, and as `skill` reads it.
fn disk_usage_scenario(scratch: &Scratch, brain: &Brain) -> (Value, String) {
    let shown = criteria(scratch, brain, "how big is it")["disk-usage"]["scenario"].clone();
    (shown, brain.skill("disk-usage").unwrap().unwrap().scenario)
}

#[test]
fn a_rewrite_measured_in_a_project_stays_in_that_project() {
    let scratch = Scratch::new("optimise-project");
    optimisable_brain(&scratch);
    let (a, b) = (in_project(&scratch, "a"), in_project(&scratch, "b"));
    let (old, new) = ("the user wants disk use", "the user wants the total size of the current directory");
    let (models, _) = models(optimisable(false), vec![("matrix_rewrite", sharper())]);
    let report = eval::run(&a, &models, &options(&scratch, false, bounds()), &quiet()).unwrap();
    let change = &report.optimisation.unwrap().changes[0];
    assert!(change.kept, "{:?}", change.problem);

    // Project a routes with the rewrite.
    assert_eq!(disk_usage_scenario(&scratch, &a), (json!(new), new.to_string()));
    assert_eq!(a.skill("count-lines").unwrap().unwrap().counter_examples, ["how much disk this takes"]);

    assert_eq!((change.project.as_deref(), change.project_had_scenario), (Some("a"), false));
    assert_eq!(a.project_scenarios().unwrap(), ["disk-usage"]);

    // Outside it, and in project b, nothing changed: the global row is as it was.
    for elsewhere in [&scratch.brain, &b] {
        assert_eq!(disk_usage_scenario(&scratch, elsewhere), (json!(old), old.to_string()));
        assert!(elsewhere.skill("count-lines").unwrap().unwrap().counter_examples.is_empty());
        assert!(elsewhere.project_scenarios().unwrap().is_empty());
    }

    // The change is on record in project a, so it is undone there and from nowhere else.
    assert!(eval::undo(&scratch.brain, None, false).is_err());
    assert!(eval::undo(&b, None, false).is_err());
    let undone = eval::undo(&a, None, false).unwrap();
    assert_eq!((undone.project.as_deref(), undone.scenario.as_str()), (Some("a"), old));
    assert_eq!(disk_usage_scenario(&scratch, &a), (json!(old), old.to_string()));
    assert!(a.project_scenarios().unwrap().is_empty(), "project a sees the global scenario again");
    assert!(a.skill("count-lines").unwrap().unwrap().counter_examples.is_empty());
}

#[test]
fn a_project_cannot_drop_a_global_phrasing_and_undo_puts_back_its_own_scenario() {
    let scratch = Scratch::new("optimise-project-own");
    optimisable_brain(&scratch);
    let a = in_project(&scratch, "a");
    let own = "the user wants disk use in this checkout";
    assert!(a.set_scenario("disk-usage", own).unwrap());
    let drop_only = json!({
        "scenario": own, "dropExamples": ["space used here"], "counterExamples": [],
        "explanation": "The phrasing was taught by mistake.",
    });
    let (models, replies) = models(optimisable(false), vec![("matrix_rewrite", drop_only), ("matrix_rewrite", sharper())]);
    let report = eval::run(&a, &models, &options(&scratch, false, bounds()), &quiet()).unwrap();
    let changes = report.optimisation.unwrap().changes;

    // Dropping it would remove a phrasing from the global row, which every project reads.
    let problem = "the rewrite changed nothing that can be kept here: \"space used here\" is a phrasing of the global \
                   disk-usage, which a project cannot drop";
    assert_eq!((changes[0].kept, changes[0].problem.as_deref()), (false, Some(problem)));
    let retry: Value =
        serde_json::from_str(replies.seen.lock().unwrap()[1]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(retry["previousAttemptProblem"], problem);
    assert!(changes[1].kept && changes[1].project_had_scenario, "{:?}", changes[1].problem);
    assert_eq!(scratch.brain.skill("disk-usage").unwrap().unwrap().examples, ["how big is this folder", "space used here"]);

    let undone = eval::undo(&a, Some("disk-usage"), false).unwrap();
    assert_eq!(undone.scenario, own);
    assert_eq!(a.project_scenario("disk-usage").unwrap().as_deref(), Some(own), "the project's own, not the global one");
    assert_eq!(scratch.brain.skill("disk-usage").unwrap().unwrap().scenario, "the user wants disk use");
}

#[test]
fn the_call_budget_stops_the_loop_before_it_spends() {
    let scratch = Scratch::new("optimise-budget");
    optimisable_brain(&scratch);
    let (models, _) = models(optimisable(false), no_language_model());
    let tight = Some(Bounds { max_skills: 3, max_calls: 2 });
    let report = eval::run(&scratch.brain, &models, &options(&scratch, true, tight), &quiet()).unwrap();
    let optimisation = report.optimisation.unwrap();
    assert!(optimisation.changes.is_empty());
    assert!(optimisation.stopped.unwrap().contains("--max-calls=2"));
}

// ── jli stats ────────────────────────────────────────────────────────────────

#[test]
fn correctness_keeps_declined_and_corrected_attempts_in_the_count() {
    let scratch = Scratch::new("correctness");
    let log = |event: NewEvent| scratch.brain.log_event(&event).unwrap();
    log(NewEvent { ms_system2: Some(900), ..event(&scratch, "a", Some("x"), Path::System1, true, Some(0)) });
    log(NewEvent { learned: Some("corrected".into()), ..event(&scratch, "b", Some("x"), Path::System1, false, None) });
    log(event(&scratch, "b", Some("y"), Path::Taught, true, Some(0)));
    log(event(&scratch, "c", Some("y"), Path::Taught, true, Some(0)));
    log(event(&scratch, "d", Some("x"), Path::System1, true, Some(1)));
    log(event(&scratch, "e", Some("x"), Path::System1, false, None));
    log(event(&scratch, "f", None, Path::None, false, None));
    let mut events = scratch.brain.events(10).unwrap();
    events.reverse();
    let tally = eval::correctness(&events);
    assert_eq!((tally.requests, tally.first_try, tally.turned_down, tally.failed, tally.blocked), (5, 1, 2, 1, 1));
    assert_eq!((tally.system1_routes, tally.system1_with_model), (4, 1));
    let evidence = eval::evidence(&events);
    assert_eq!((evidence["x"].judged, evidence["x"].correct), (2, 1));
}
