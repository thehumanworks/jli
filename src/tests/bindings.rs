use super::*;

#[test]
fn explicit_correction_survives_one_ten_fifty_lessons_and_reopen() {
    let scratch = Scratch::new("binding-retention");
    let (wrong, _) = models(|_| decision("list-files", 1.0, 1.0, SAFE), no_language_model());
    let ran = go(&scratch, &wrong, "newest files", &mut Script::default(), &yes(&scratch));
    let fix = Fix { event: ran.event_id, how: FixHow::Command(vec!["ls -ltr".into()]), run: false };
    app(&scratch, "default", no_model_at_all()).fix(&quiet_flags(), fix).unwrap();
    for lesson in 1..=50 {
        scratch.brain.link_phrase("list-files-by-time", &format!("later lesson {lesson}")).unwrap();
        scratch.brain.add_counter_example("list-files", &format!("later rejection {lesson}")).unwrap();
        if [1, 10, 50].contains(&lesson) {
            let reopened = Brain::open(BrainConfig::file(&scratch.root.join("brain.db"))).unwrap();
            let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
            let outcome = route(&reopened, &wrong, "newest files", &quiet(), &mut Script::default(), &dry).unwrap();
            assert_eq!(outcome.path, Path::Binding);
            assert_eq!(outcome.commands, ["ls -ltr"], "after {lesson} lessons");
            assert_eq!(outcome.ms_system1, 0);
        }
    }
    assert!(!lists(&scratch, "list-files-by-time").0.contains(&"newest files".into()));
    assert!(!lists(&scratch, "list-files").1.contains(&"newest files".into()));
    let binding = scratch.brain.bindings().unwrap().remove(0);
    assert_eq!((binding.rule.source.as_str(), binding.rule.terminal), ("fix", false));
    assert!(scratch.brain.revoke_binding(binding.id).unwrap());
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    assert_eq!(go(&scratch, &wrong, "newest files", &mut Script::default(), &dry).label.as_deref(), Some("list-files"));
}

#[test]
fn a_typed_correction_and_teach_bind_but_ordinary_lessons_do_not() {
    let scratch = Scratch::new("binding-explicit");
    let (wrong, _) = models(|_| decision("list-files", 1.0, 1.0, SAFE), no_language_model());
    go(&scratch, &wrong, "date order", &mut Script::new(&[Some(false)], &["ls -ltr"]), &scratch.options(Mode::Manual));
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    assert_eq!(go(&scratch, &no_model_at_all(), "date order", &mut Script::default(), &dry).commands, ["ls -ltr"]);
    let mut app = app(&scratch, "default", no_model_at_all());
    let flags = parse(&["--mode=manual".into(), "--quiet".into()]).unwrap().flags;
    app.dispatch(&flags, &["teach".into(), "files by age".into()], &["ls -ltr".into()]).unwrap();
    assert_eq!(go(&scratch, &no_model_at_all(), "files by age", &mut Script::default(), &dry).commands, ["ls -ltr"]);
    scratch.brain.link_phrase("list-files-by-time", "ordinary phrasing").unwrap();
    assert_eq!(scratch.brain.bindings().unwrap().len(), 2);
}

#[test]
fn value_only_fix_keeps_the_skill_and_new_stated_values_are_exact() {
    let scratch = Scratch::new("binding-values");
    let (wrong_value, _) = models(|_| decision("git-commit", 1.0, 1.0, SAFE), no_language_model());
    let ran = go(&scratch, &wrong_value, "commit message=\"wip\"", &mut Script::default(), &yes(&scratch));
    let fix = Fix { event: ran.event_id, how: FixHow::Command(vec!["git commit -m other".into()]), run: false };
    app(&scratch, "default", no_model_at_all()).fix(&quiet_flags(), fix).unwrap();
    let binding = scratch.brain.bindings().unwrap().remove(0);
    assert!(binding.rule.rejected.is_empty());
    assert!(lists(&scratch, "git-commit").1.is_empty());
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let result =
        go(&scratch, &no_model_at_all(), "commit message=\"delete all files; $(oops)\"", &mut Script::default(), &dry);
    assert_eq!(result.commands, ["git commit -m 'delete all files; $(oops)'"]);
    assert_eq!(result.path, Path::Binding);
}

#[test]
fn rules_are_scoped_and_global_targets_never_turn_into_project_shadows() {
    let scratch = Scratch::new("binding-scope");
    scratch.brain.bind_phrase("files", "list-files", "teach", false).unwrap();
    let project = in_project(&scratch, "one");
    project.bind_phrase("files", "list-files-by-time", "teach", false).unwrap();
    let other = in_project(&scratch, "two");
    let outsider = Brain::open_as(BrainConfig::file(&scratch.root.join("brain.db")), "other").unwrap();
    assert!(outsider.bindings().unwrap().is_empty());
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let check = |brain: &Brain| route(brain, &no_model_at_all(), "files", &quiet(), &mut Script::default(), &dry).unwrap();
    assert_eq!(check(&project).commands, ["ls -ltr"]);
    assert_eq!(check(&other).commands, ["ls"]);
    let inherited = other.bindings().unwrap().remove(0).id;
    assert!(!other.revoke_binding(inherited).unwrap());
    other
        .add_skill_if_missing(&NewSkill {
            label: "list-files".into(),
            steps: vec!["echo shadow".into()],
            scenario: "shadow".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![],
        })
        .unwrap();
    assert_eq!(code(&check(&other)), Some("binding_unavailable"));
    assert_eq!(check(&scratch.brain).commands, ["ls"]);
}

#[test]
fn missing_replaced_and_literal_value_changed_targets_fail_closed() {
    let scratch = Scratch::new("binding-stale");
    scratch.brain.bind_phrase("files", "list-files", "teach", false).unwrap();
    scratch.brain.forget("list-files").unwrap();
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    assert_eq!(code(&go(&scratch, &no_model_at_all(), "files", &mut Script::default(), &dry)), Some("binding_unavailable"));
    scratch
        .brain
        .add_skill(&NewSkill {
            label: "list-files".into(),
            steps: vec!["echo changed".into()],
            scenario: "changed".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![],
        })
        .unwrap();
    assert_eq!(code(&go(&scratch, &no_model_at_all(), "files", &mut Script::default(), &dry)), Some("binding_unavailable"));
    scratch.brain.bind_phrase("say 'one'", "list-files", "teach", false).unwrap();
    assert_eq!(
        code(&go(&scratch, &no_model_at_all(), "say 'two'", &mut Script::default(), &dry)),
        Some("binding_unavailable")
    );
}

fn existing(label: &str) -> Value {
    json!({"kind": "existing", "existingLabel": label, "label": null, "steps": [], "scenario": "", "slots": [],
        "slotValues": [], "risk": "safe", "explanation": "fixture", "answer": null})
}

#[test]
fn unresolved_rejections_survive_a_confident_model_and_repeated_agent_proposal() {
    let scratch = Scratch::new("binding-reject");
    scratch.brain.reject_phrase("files", "list-files", "fix", false).unwrap();
    for lesson in 0..50 {
        scratch.brain.add_counter_example("list-files", &format!("later {lesson}")).unwrap();
    }
    let (adversary, _) = models(
        |request| {
            assert!(request["questions"]["skill"]["criteria"].get("list-files").is_none());
            decision("list-files", 1.0, 1.0, SAFE)
        },
        vec![("proposal", existing("list-files"))],
    );
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    assert_eq!(code(&go(&scratch, &adversary, "files", &mut Script::default(), &dry)), Some("already_rejected"));
}

#[test]
fn correction_plans_bind_on_acceptance_but_do_not_overwrite_newer_decisions() {
    for newer in [false, true] {
        let scratch = Scratch::new(if newer { "binding-plan-newer" } else { "binding-plan" });
        let (wrong, _) = models(|_| decision("list-files", 1.0, 1.0, SAFE), no_language_model());
        let ran = go(&scratch, &wrong, "date order", &mut Script::default(), &yes(&scratch));
        let (agent, _) = models(|_| panic!("fix skips System 1"), vec![("proposal", existing("list-files-by-time"))]);
        let fix = Fix { event: ran.event_id, how: FixHow::Expected("by time".into()), run: false };
        app(&scratch, "default", agent).fix(&quiet_flags(), fix).unwrap();
        assert!(scratch.brain.bindings().unwrap()[0].rule.target.is_none());
        if newer {
            scratch.brain.bind_phrase("date order", "list-files", "teach", false).unwrap();
        }
        let result = plans(&scratch, &no_model_at_all(), &[1], &[], &mut Script::default());
        assert_eq!(result[0].commands, ["ls -ltr"]);
        let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
        let outcome = go(&scratch, &no_model_at_all(), "date order", &mut Script::default(), &dry);
        assert_eq!(outcome.commands, if newer { vec!["ls"] } else { vec!["ls -ltr"] });
    }
}

#[test]
fn concurrent_rejections_and_examples_lose_no_durable_decision() {
    let scratch = Scratch::new("binding-concurrent");
    let config = BrainConfig::file(&scratch.root.join("brain.db"));
    // Open before the barrier, as ordinary CLI processes do before routing.
    let start = Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = ["list-files", "list-files-by-time"]
        .into_iter()
        .map(|label| {
            let config = config.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                let brain = Brain::open(config).unwrap();
                start.wait();
                brain.reject_phrase("files", label, "fix", false).unwrap();
                for n in 0..50 {
                    brain.link_phrase(label, &format!("later {label} {n}")).unwrap();
                }
            })
        })
        .collect();
    start.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    let rule = &scratch.brain.bindings().unwrap()[0].rule;
    assert_eq!(rule.rejected.len(), 2);
    assert!(rule.target.is_none());
}

#[test]
fn supported_generalisation_keeps_positive_and_negative_corrections() {
    let scratch = Scratch::new("binding-upgrade");
    let literal = NewSkill {
        label: "count".into(),
        steps: vec!["echo 3".into()],
        scenario: "count".into(),
        risk: Risk::Safe,
        origin: Origin::Taught,
        examples: vec![],
        slots: vec![],
    };
    scratch.brain.add_skill(&literal).unwrap();
    scratch.brain.bind_phrase("show 3", "count", "teach", false).unwrap();
    scratch.brain.reject_phrase("wrong count", "count", "fix", false).unwrap();
    let general = NewSkill {
        steps: vec!["echo {count}".into()],
        slots: vec![Slot {
            name: "count".into(),
            kind: SlotKind::Number,
            description: "count".into(),
            default: Some("3".into()),
            aliases: vec![],
            optional: false,
            flag: None,
            choices: vec![],
        }],
        ..literal.clone()
    };
    assert!(scratch.brain.upgrade_skill("count", &literal.steps, &general).unwrap());
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    assert_eq!(go(&scratch, &no_model_at_all(), "show 3", &mut Script::default(), &dry).commands, ["echo 3"]);
    let (adversary, _) = models(
        |request| {
            assert!(request["questions"]["skill"]["criteria"].get("count").is_none());
            decision("count", 1.0, 1.0, SAFE)
        },
        vec![("proposal", existing("count"))],
    );
    assert_eq!(code(&go(&scratch, &adversary, "wrong count", &mut Script::default(), &dry)), Some("already_rejected"));
}

#[test]
fn forget_phrase_rejects_a_durable_choice_even_after_its_example_expires() {
    for expire in [false, true] {
        let scratch = Scratch::new(if expire { "binding-forget-evicted" } else { "binding-forget" });
        let mut app = app(&scratch, "default", no_model_at_all());
        let flags = parse(&["--mode=manual".into(), "--quiet".into()]).unwrap().flags;
        app.dispatch(&flags, &["teach".into(), "files".into()], &["ls".into()]).unwrap();
        if expire {
            for n in 0..50 {
                scratch.brain.link_phrase("list-files", &format!("lesson {n}")).unwrap();
            }
        }
        assert_eq!(app.dispatch(&flags, &["forget-phrase".into(), "list-files".into(), "files".into()], &[]).unwrap(), 0);
        let rule = &scratch.brain.bindings().unwrap()[0].rule;
        assert!(rule.target.is_none());
        assert_eq!(rule.rejected[0].label, "list-files");
    }
}

#[test]
fn forgetting_an_inherited_target_keeps_a_different_project_choice() {
    let scratch = Scratch::new("binding-forget-inherited");
    scratch.brain.bind_phrase("files", "list-files", "teach", false).unwrap();
    let mut project = in_project(&scratch, "one");
    project.bind_phrase("files", "list-files-by-time", "teach", false).unwrap();
    let edit = crate::hand::PhraseEdit { label: "list-files".into(), phrase: "files".into(), global: false, dry_run: false };
    crate::hand::forget_phrase(&mut project, &edit).unwrap();
    let dry = RouteOptions { dry_run: true, ..scratch.options(Mode::Semi) };
    let result = route(&project, &no_model_at_all(), "files", &quiet(), &mut Script::default(), &dry).unwrap();
    assert_eq!(result.commands, ["ls -ltr"]);
    // Rejecting a different skill locally also preserves the inherited positive choice.
    let other = in_project(&scratch, "two");
    other.reject_phrase("files", "list-files-by-time", "fix", false).unwrap();
    let result = route(&other, &no_model_at_all(), "files", &quiet(), &mut Script::default(), &dry).unwrap();
    assert_eq!(result.commands, ["ls"]);
}
