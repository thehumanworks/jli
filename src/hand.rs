//! Hand edits to one skill (ADR 21): `jli alias`, another name for a slot added or taken away, and
//! `jli forget-phrase`, a phrasing taken back. Each edit is written where the router would write the
//! same lesson (ADR 15, ADR 17), and is returned as a sentence and a JSON object for the CLI to print.

use anyhow::Result;
use serde_json::{json, Value};

use crate::slots::{flag_fits, joined_before, undeclared};
use crate::store::Brain;
use crate::types::{is_choice_value, is_key, normal_name, Choice, Slot, SlotKind, MAX_ALIASES};

/// `jli alias [--remove] [--global] <label> <slot> <name>`.
#[derive(Debug, Clone, PartialEq)]
pub struct AliasEdit {
    pub label: String,
    pub slot: String,
    pub name: String,
    pub remove: bool,
    /// Edit the user's global row even from inside a project that has a skill of the same label.
    pub global: bool,
    pub dry_run: bool,
}

/// `jli forget-phrase [--global] <label> "<phrase>"`.
#[derive(Debug, Clone, PartialEq)]
pub struct PhraseEdit {
    pub label: String,
    pub phrase: String,
    /// Forget it from the user's global row, for every project, rather than in this project only.
    pub global: bool,
    pub dry_run: bool,
}

/// `jli slot [--global] <label> <slot> [--default=<value>] [--optional | --required] [--flag=<flag>]
/// [--choice=<value>[=<text>] … | --no-choices] [--kind=<kind>]` (ADR 24). A setting left out is left as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlotEdit {
    pub label: String,
    pub slot: String,
    /// `--default=<value>`; `--default=` with nothing after it takes the default away.
    pub default: Option<Option<String>>,
    /// `--optional` or `--required`.
    pub optional: Option<bool>,
    /// `--flag=<flag>`; `--flag=` with nothing after it takes the flag away.
    pub flag: Option<Option<String>>,
    /// The `--choice=` values in order, which replace the slot's choices; `--no-choices` is an empty list.
    pub choices: Option<Vec<Choice>>,
    pub kind: Option<SlotKind>,
    /// Edit the user's global row even from inside a project that has a skill of the same label.
    pub global: bool,
    pub dry_run: bool,
}

/// What a hand edit did. A refused edit changed nothing and carries its error code.
#[derive(Debug, Clone, PartialEq)]
pub struct Edited {
    /// The sentence for a person.
    pub message: String,
    /// The `--json` outcome; `Value::Null` when the edit was refused.
    pub json: Value,
    pub refused: Option<&'static str>,
}

pub const SKILL_NOT_FOUND: &str = "skill_not_found";
pub const SLOT_NOT_FOUND: &str = "slot_not_found";
pub const ALIAS_INVALID: &str = "alias_invalid";
pub const ALIAS_TAKEN: &str = "alias_taken";
pub const ALIASES_FULL: &str = "aliases_full";
pub const ALIAS_NOT_FOUND: &str = "alias_not_found";
pub const PHRASE_NOT_FOUND: &str = "phrase_not_found";
pub const FLAG_INVALID: &str = "flag_invalid";
pub const CHOICE_INVALID: &str = "choice_invalid";
pub const DEFAULT_INVALID: &str = "default_invalid";

fn refused(code: &'static str, message: String) -> Result<Edited> {
    Ok(Edited { message, json: Value::Null, refused: Some(code) })
}

/// Where an edit lands: `global` (the user's row, every project sees it), `project` (the project's
/// own row) or `project_lesson` (what the project keeps about a global row, ADR 17).
fn written_to(brain: &Brain, label: &str, lesson: bool) -> Result<&'static str> {
    if brain.project().is_none() {
        return Ok("global");
    }
    if brain.project_skills()?.iter().any(|skill| skill.label == label) {
        return Ok("project");
    }
    Ok(if lesson { "project_lesson" } else { "global" })
}

/// The words after `alias`, or None when they are not its shape, so that `alias gs to git status`
/// is still a request. `--remove` is the command's own flag, so with it a wrong shape is a usage error.
pub fn alias_shape(words: &[String], dry_run: bool) -> Option<AliasEdit> {
    let remove = words.iter().any(|word| word == "--remove");
    let global = words.iter().any(|word| word == "--global");
    let plain: Vec<&String> = words.iter().filter(|word| *word != "--remove" && *word != "--global").collect();
    match plain.as_slice() {
        [label, slot, name] if ![label, slot, name].iter().any(|word| word.starts_with("--")) => Some(AliasEdit {
            label: label.to_string(),
            slot: slot.to_string(),
            name: name.to_string(),
            remove,
            global,
            dry_run,
        }),
        _ => None,
    }
}

/// The words after `forget-phrase`: a label, then the phrase, quoted or not.
pub fn phrase_shape(words: &[String], dry_run: bool) -> Option<PhraseEdit> {
    let global = words.iter().any(|word| word == "--global");
    let plain: Vec<&str> = words.iter().filter(|word| *word != "--global").map(String::as_str).collect();
    let (label, phrase) = plain.split_first()?;
    let phrase = phrase.join(" ").trim().to_string();
    (!phrase.is_empty() && !label.starts_with("--")).then(|| PhraseEdit {
        label: label.to_string(),
        phrase,
        global,
        dry_run,
    })
}

/// The words after `slot`: a label, a slot, and at least one setting, each written with `=` so that a
/// flag such as `-n` stays one word. None when they are not that shape, so `slot machine` is a request.
pub fn slot_shape(words: &[String], dry_run: bool) -> Option<SlotEdit> {
    let mut edit = SlotEdit { dry_run, ..SlotEdit::default() };
    let mut plain: Vec<&String> = Vec::new();
    let mut cleared = false;
    for word in words {
        if let Some(value) = word.strip_prefix("--default=") {
            edit.default = Some(Some(value.trim().to_string()).filter(|value| !value.is_empty()));
        } else if let Some(flag) = word.strip_prefix("--flag=") {
            edit.flag = Some(Some(flag.trim().to_string()).filter(|flag| !flag.is_empty()));
        } else if let Some(choice) = word.strip_prefix("--choice=") {
            // `--choice=release` writes release; `--choice=release=--release` writes --release; `debug=` nothing.
            let (value, text) = choice.split_once('=').unwrap_or((choice, choice));
            let choice = Choice { value: value.trim().to_string(), text: text.trim().to_string() };
            edit.choices.get_or_insert_with(Vec::new).push(choice);
        } else if let Some(kind) = word.strip_prefix("--kind=") {
            edit.kind = Some(SlotKind::parse(kind.trim())?);
        } else if word == "--no-choices" {
            cleared = true;
        } else if word == "--optional" || word == "--required" {
            let optional = word == "--optional";
            if edit.optional.is_some_and(|already| already != optional) {
                return None;
            }
            edit.optional = Some(optional);
        } else if word == "--global" {
            edit.global = true;
        } else if word.starts_with("--") {
            return None;
        } else {
            plain.push(word);
        }
    }
    if cleared {
        if edit.choices.is_some() {
            return None;
        }
        edit.choices = Some(vec![]);
    }
    let set = edit.default.is_some()
        || edit.optional.is_some()
        || edit.flag.is_some()
        || edit.choices.is_some()
        || edit.kind.is_some();
    match plain.as_slice() {
        [label, slot] if set => Some(SlotEdit { label: label.to_string(), slot: slot.to_string(), ..edit }),
        _ => None,
    }
}

/// A list of quoted phrasings, or of names, for a message.
fn listed(items: &[String], quoted: bool) -> String {
    let items: Vec<String> = items.iter().map(|item| if quoted { format!("\"{item}\"") } else { item.clone() }).collect();
    items.join(", ")
}

fn arguments(slots: &[Slot]) -> String {
    let each: Vec<String> = slots
        .iter()
        .map(|slot| match slot.aliases.as_slice() {
            [] => slot.name.clone(),
            aliases => format!("{} (also {})", slot.name, aliases.join(", ")),
        })
        .collect();
    each.join(", ")
}

/// Run `edit` against the user's global row when `global` is asked for inside a project.
fn scoped<T>(brain: &mut Brain, global: bool, edit: impl FnOnce(&Brain, Option<&str>) -> T) -> T {
    let project = brain.project().map(String::from);
    if global && project.is_some() {
        return brain.globally(|brain| edit(brain, project.as_deref()));
    }
    edit(brain, None)
}

/// Add or remove one other name of a slot. Like a name learned at run time (ADR 12), it is kept in
/// the row that holds the skill: inside a project that is the global row unless the project has its own.
pub fn alias(brain: &mut Brain, edit: &AliasEdit) -> Result<Edited> {
    scoped(brain, edit.global, |brain, from| alias_in(brain, edit, from))
}

/// `from` is the project an explicit `--global` was given in.
fn alias_in(brain: &Brain, edit: &AliasEdit, from: Option<&str>) -> Result<Edited> {
    let label = edit.label.as_str();
    let Some(skill) = brain.skill(label)? else {
        return refused(SKILL_NOT_FOUND, format!("No skill called {label}."));
    };
    if skill.slots.is_empty() {
        return refused(
            SLOT_NOT_FOUND,
            format!("{label} takes no arguments, so it has no slot to give another name: {}", skill.steps.join(" && ")),
        );
    }
    let Some(slot) = skill.slots.iter().find(|slot| slot.answers_to(&edit.slot)) else {
        return refused(
            SLOT_NOT_FOUND,
            format!("{label} has no slot called {}. Its arguments are: {}.", edit.slot, arguments(&skill.slots)),
        );
    };
    let name = normal_name(edit.name.trim());
    let own = normal_name(&slot.name) == name;
    let has = slot.aliases.iter().any(|alias| normal_name(alias) == name);
    let target = format!("{{{}}} in {label}", slot.name);
    if edit.remove {
        if own {
            return refused(
                ALIAS_NOT_FOUND,
                format!("{name} is the name {target} has in the command itself, so it cannot be removed."),
            );
        }
        if !has {
            let others = match slot.aliases.as_slice() {
                [] => "It has no other names.".to_string(),
                aliases => format!("Its other names are: {}.", listed(aliases, false)),
            };
            return refused(ALIAS_NOT_FOUND, format!("{name} is not another name for {target}. {others}"));
        }
    } else {
        if !is_key(&name) {
            return refused(
                ALIAS_INVALID,
                format!(
                    "Another name for a slot is typed as name=value, so it is a letter or _, then letters, digits, - or _. Got \"{}\".",
                    edit.name
                ),
            );
        }
        if let Some(owner) = skill.slots.iter().find(|other| other.name != slot.name && other.answers_to(&name)) {
            return refused(
                ALIAS_TAKEN,
                format!("{name} already means {{{}}} in {label}, so it cannot also mean {{{}}}.", owner.name, slot.name),
            );
        }
        if !own && !has && slot.aliases.len() >= MAX_ALIASES {
            return refused(
                ALIASES_FULL,
                format!(
                    "{target} already has {MAX_ALIASES} other names, the most a slot keeps. Remove one first: jli alias --remove {label} {} <name>",
                    slot.name
                ),
            );
        }
    }

    let where_ = written_to(brain, label, false)?;
    let place = match (where_, from, brain.project()) {
        (_, Some(_), _) => ", in every project".to_string(),
        ("project", _, Some(project)) => format!(", in project {project}"),
        (_, _, Some(_)) => format!(". {label} is a global skill, so this holds in every project"),
        _ => String::new(),
    };
    let unchanged = if edit.remove { false } else { own || has };
    let changed = match (unchanged, edit.dry_run) {
        (true, _) => false,
        (false, true) => true,
        (false, false) if edit.remove => brain.remove_alias(label, &slot.name, &name)?,
        (false, false) => brain.add_alias(label, &slot.name, &name)?,
    };
    let mut aliases = slot.aliases.clone();
    if edit.dry_run && changed {
        if edit.remove {
            aliases.retain(|alias| normal_name(alias) != name);
        } else {
            aliases.push(name.clone());
        }
    } else if let Some(now) = brain.skill(label)?.and_then(|skill| skill.slots.into_iter().find(|s| s.name == slot.name)) {
        aliases = now.aliases;
    }
    let message = match (edit.remove, changed, edit.dry_run) {
        (_, false, _) if own => format!("{name} is already the name of {target}. Nothing was changed."),
        (false, false, _) => format!("{name} is already another name for {target}. Nothing was changed."),
        (true, false, _) => format!("{name} is no longer another name for {target}. Nothing was changed."),
        (false, true, true) => format!("Dry run: nothing was written. {name} would be another name for {target}{place}."),
        (true, true, true) => {
            format!("Dry run: nothing was written. {name} would no longer be another name for {target}{place}.")
        }
        (false, true, false) => format!("{name} is now another name for {target}{place}."),
        (true, true, false) => format!("{name} is no longer another name for {target}{place}."),
    };
    let json = json!({
        "label": label,
        "slot": slot.name,
        "alias": name,
        "action": if edit.remove { "remove" } else { "add" },
        "changed": changed,
        "dryRun": edit.dry_run,
        "aliases": aliases,
        "writtenTo": where_,
        "project": from.or(brain.project()),
    });
    Ok(Edited { message, json, refused: None })
}

/// Set how one slot is filled and written (ADR 24): its default, whether it may be left out, the flag
/// left out with it, the choices it takes and what each puts in the command, and its kind. Like an
/// alias, it is kept in the row that holds the skill. A placeholder the steps use that no slot declares
/// is declared by it.
pub fn slot(brain: &mut Brain, edit: &SlotEdit) -> Result<Edited> {
    scoped(brain, edit.global, |brain, from| slot_in(brain, edit, from))
}

fn slot_in(brain: &Brain, edit: &SlotEdit, from: Option<&str>) -> Result<Edited> {
    let label = edit.label.as_str();
    let Some(skill) = brain.skill(label)? else {
        return refused(SKILL_NOT_FOUND, format!("No skill called {label}."));
    };
    let open = undeclared(&skill.steps, &skill.slots);
    let (before, declared) = match skill.slots.iter().find(|slot| slot.answers_to(&edit.slot)) {
        Some(slot) => (slot.clone(), false),
        None if open.contains(&edit.slot) => (Slot { name: edit.slot.clone(), ..Slot::default() }, true),
        None => {
            let args = if skill.slots.is_empty() { "none".to_string() } else { arguments(&skill.slots) };
            return refused(
                SLOT_NOT_FOUND,
                format!(
                    "{label} has no slot called {}. Its arguments are: {args}. A slot is a {{name}} in its command: {}",
                    edit.slot,
                    skill.steps.join(" && ")
                ),
            );
        }
    };
    let name = before.name.clone();
    let target = format!("{{{name}}} in {label}");
    let mut next = before.clone();
    if let Some(kind) = edit.kind {
        next.kind = kind;
    }
    if let Some(choices) = &edit.choices {
        for (at, choice) in choices.iter().enumerate() {
            if !is_choice_value(&choice.value) {
                return refused(
                    CHOICE_INVALID,
                    format!(
                        "A choice is one word a person would say, and not a flag: \"{}\" cannot be one. Write --choice=<value>, which puts the value itself in the command, or --choice=<value>=<text> to put other text there.",
                        choice.value
                    ),
                );
            }
            if choices[..at].iter().any(|earlier| earlier.value.eq_ignore_ascii_case(&choice.value)) {
                return refused(CHOICE_INVALID, format!("{} is given as a choice twice.", choice.value));
            }
        }
        next.choices = choices.clone();
    }
    if let Some(default) = &edit.default {
        next.default = default.clone();
    }
    // A default of a slot with choices is one of them, under the choice's own spelling.
    if let Some(default) = next.default.clone().filter(|_| !next.choices.is_empty()) {
        match next.choice(&default) {
            Some(choice) => next.default = Some(choice.value.clone()),
            None => {
                return refused(
                    DEFAULT_INVALID,
                    format!(
                        "{default} is not one of the choices of {target}: {}. Give a default among them, or --default= for none.",
                        next.choice_list()
                    ),
                )
            }
        }
    }
    if let Some(flag) = &edit.flag {
        if let Some(flag) = flag.as_deref().filter(|flag| !flag_fits(&skill.steps, &name, flag)) {
            return refused(
                FLAG_INVALID,
                format!(
                    "{flag} is not written just before {{{name}}} in {}, so it cannot be left out with it. A flag begins with - and sits right before the placeholder: -n {{count}}, --profile={{profile}}.",
                    skill.steps.join(" && ")
                ),
            );
        }
        next.flag = flag.clone();
    }
    if let Some(optional) = edit.optional {
        next.optional = optional;
    }
    if next.optional && next.flag.is_none() {
        if let Some(joined) = joined_before(&skill.steps, &name) {
            return refused(
                FLAG_INVALID,
                format!(
                    "{{{name}}} is joined to {joined} in {}, so leaving it out would leave {joined} behind. Name it as its flag too: --flag={joined}",
                    skill.steps.join(" && ")
                ),
            );
        }
    }

    let where_ = written_to(brain, label, false)?;
    let place = match (where_, from, brain.project()) {
        (_, Some(_), _) => ", in every project".to_string(),
        ("project", _, Some(project)) => format!(", in project {project}"),
        (_, _, Some(_)) => format!(". {label} is a global skill, so this holds in every project"),
        _ => String::new(),
    };
    let changed = match (next == before && !declared, edit.dry_run) {
        (true, _) => false,
        (false, true) => true,
        (false, false) => brain.set_slot(label, &next)?,
    };
    let now = if edit.dry_run {
        next
    } else {
        brain.skill(label)?.and_then(|skill| skill.slots.into_iter().find(|slot| slot.name == name)).unwrap_or(next)
    };
    let settings = match now.settings() {
        said if said.is_empty() => "it must have a value, and it has no default".to_string(),
        said => said,
    };
    let message = match (changed, edit.dry_run) {
        (false, _) => format!("{target} is already so: {settings}. Nothing was changed."),
        (true, true) => format!("Dry run: nothing was written. {target} would be: {settings}{place}."),
        (true, false) if declared => format!("{target} is now a slot: {settings}{place}."),
        (true, false) => format!("{target} is now: {settings}{place}."),
    };
    let json = json!({
        "label": label,
        "slot": name,
        "declared": declared,
        "changed": changed,
        "dryRun": edit.dry_run,
        "kind": now.kind,
        "default": now.default,
        "optional": now.optional,
        "flag": now.flag,
        "choices": now.choices,
        "writtenTo": where_,
        "project": from.or(brain.project()),
    });
    Ok(Edited { message, json, refused: None })
}

/// Take one phrasing back from a skill, and keep it as a counter-example so that the next request in
/// those words is not routed there and learned straight back (ADR 21). Inside a project, for a global
/// skill, both go to the project's lesson about the row (ADR 17); `--global` edits the row itself.
pub fn forget_phrase(brain: &mut Brain, edit: &PhraseEdit) -> Result<Edited> {
    scoped(brain, edit.global, |brain, from| forget_phrase_in(brain, edit, from))
}

fn forget_phrase_in(brain: &Brain, edit: &PhraseEdit, from: Option<&str>) -> Result<Edited> {
    let label = edit.label.as_str();
    let same = |a: &str, b: &str| a.trim().to_lowercase() == b.trim().to_lowercase();
    // As the project sees it: a lesson can already have hidden a global phrasing here, or added one.
    let Some(skill) = brain.skill(label)? else {
        return refused(SKILL_NOT_FOUND, format!("No skill called {label}."));
    };
    let where_ = written_to(brain, label, true)?;
    let project = from.or(brain.project());
    let outcome = |message: String, changed: bool, examples: &[String], counter: &[String]| Edited {
        message,
        json: json!({
            "label": label,
            "phrase": edit.phrase,
            "changed": changed,
            "dryRun": edit.dry_run,
            "examples": examples,
            "counterExamples": counter,
            "writtenTo": where_,
            "project": project,
        }),
        refused: None,
    };
    let Some(stored) = skill.examples.iter().find(|example| same(example, &edit.phrase)).cloned() else {
        if skill.counter_examples.iter().any(|counter| same(counter, &edit.phrase)) {
            let message =
                format!("\"{}\" is already a counter-example of {label}, not a phrasing. Nothing was changed.", edit.phrase);
            return Ok(outcome(message, false, &skill.examples, &skill.counter_examples));
        }
        let known = match skill.examples.as_slice() {
            [] => "It has no phrasings.".to_string(),
            examples => format!("Its phrasings are: {}.", listed(examples, true)),
        };
        return refused(PHRASE_NOT_FOUND, format!("{label} has no phrasing \"{}\". {known}", edit.phrase));
    };

    let (examples, counter) = if edit.dry_run {
        let examples: Vec<String> = skill.examples.iter().filter(|example| !same(example, &stored)).cloned().collect();
        let mut counter = skill.counter_examples.clone();
        if !counter.iter().any(|entry| same(entry, &stored)) {
            counter.push(stored.clone());
        }
        (examples, counter)
    } else {
        brain.remove_example(label, &stored)?;
        brain.add_counter_example(label, &stored)?;
        let now = brain.skill(label)?.unwrap_or(skill);
        (now.examples, now.counter_examples)
    };
    // Only a phrasing the global row holds is still seen by other projects; one this project learned is its own.
    let elsewhere = where_ == "project_lesson"
        && brain.global_skills()?.iter().any(|row| row.label == label && row.examples.iter().any(|e| same(e, &stored)));
    let (means, kept) =
        if edit.dry_run { ("would no longer mean", "would be kept") } else { ("no longer means", "is kept") };
    let dry = if edit.dry_run { "Dry run: nothing was written. " } else { "" };
    let message = match (where_, from, brain.project()) {
        (_, Some(_), _) => format!(
            "{dry}\"{stored}\" {means} {label}, in every project, and {kept} as a counter-example so that it is not learned straight back."
        ),
        ("project_lesson", _, Some(here)) if elsewhere => format!(
            "{dry}\"{stored}\" {means} {label} in project {here}, and {kept} there as a counter-example so that it is not learned straight back. \
             {label} is a global skill: other projects still have the phrasing. To forget it everywhere: jli forget-phrase --global {label} \"{stored}\""
        ),
        ("project" | "project_lesson", _, Some(here)) => format!(
            "{dry}\"{stored}\" {means} {label} in project {here}, and {kept} as a counter-example so that it is not learned straight back."
        ),
        _ => format!("{dry}\"{stored}\" {means} {label}, and {kept} as a counter-example so that it is not learned straight back."),
    };
    Ok(outcome(message, true, &examples, &counter))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn alias_is_a_command_only_in_its_own_shape() {
        let edit = alias_shape(&words(&["count-files", "directory", "folder"]), false).unwrap();
        assert_eq!(
            (edit.label.as_str(), edit.slot.as_str(), edit.name.as_str(), edit.remove),
            ("count-files", "directory", "folder", false)
        );
        let edit = alias_shape(&words(&["--remove", "count-files", "directory", "d", "--global"]), true).unwrap();
        assert!(edit.remove && edit.global && edit.dry_run);
        assert_eq!(alias_shape(&words(&["gs", "to", "git", "status"]), false), None, "a request");
        assert_eq!(alias_shape(&words(&["ll"]), false), None);
        assert_eq!(alias_shape(&words(&["a", "b", "--c"]), false), None);
    }

    #[test]
    fn slot_takes_its_settings_written_with_an_equals_sign() {
        let edit = slot_shape(
            &words(&[
                "build-app",
                "profile",
                "--choice=release=--release",
                "--choice=debug=",
                "--choice=fast",
                "--default=release",
            ]),
            false,
        )
        .unwrap();
        assert_eq!((edit.label.as_str(), edit.slot.as_str()), ("build-app", "profile"));
        let choices: Vec<(&str, &str)> =
            edit.choices.iter().flatten().map(|choice| (choice.value.as_str(), choice.text.as_str())).collect();
        assert_eq!(choices, [("release", "--release"), ("debug", ""), ("fast", "fast")]);
        assert_eq!(edit.default, Some(Some("release".into())));
        let edit = slot_shape(&words(&["log", "count", "--flag=-n", "--optional", "--default=", "--global"]), true).unwrap();
        assert_eq!((edit.flag, edit.optional, edit.default), (Some(Some("-n".into())), Some(true), Some(None)));
        assert!(edit.global && edit.dry_run);
        assert_eq!(
            slot_shape(&words(&["log", "count", "--no-choices", "--kind=number"]), false).unwrap().choices,
            Some(vec![])
        );
        assert_eq!(slot_shape(&words(&["machine", "learning"]), false), None, "no setting: a request");
        assert_eq!(slot_shape(&words(&["a", "b", "--default", "x"]), false), None, "a value is written with =");
        assert_eq!(slot_shape(&words(&["a", "b", "--optional", "--required"]), false), None);
        assert_eq!(slot_shape(&words(&["a", "b", "--choice=x", "--no-choices"]), false), None);
        assert_eq!(slot_shape(&words(&["a", "b", "--kind=folder"]), false), None);
        assert_eq!(slot_shape(&words(&["a", "b", "--nope"]), false), None);
    }

    #[test]
    fn a_slot_is_described_by_what_was_set_on_it() {
        let mut slot = Slot { name: "count".into(), optional: true, flag: Some("-n".into()), ..Slot::default() };
        assert_eq!(slot.settings(), "optional, left out with -n");
        slot.default = Some("3".into());
        assert_eq!(slot.settings(), "optional, left out with -n; default 3");
        let profile = Slot {
            choices: vec![
                Choice { value: "release".into(), text: "--release".into() },
                Choice { value: "debug".into(), text: String::new() },
            ],
            ..Slot::default()
        };
        assert_eq!(profile.settings(), "release → --release, debug → nothing");
        let plain = Slot {
            choices: vec![
                Choice { value: "release".into(), text: "release".into() },
                Choice { value: "debug".into(), text: String::new() },
            ],
            default: Some("release".into()),
            ..Slot::default()
        };
        assert_eq!(plain.settings(), "release, debug → nothing; default release", "a choice that writes its own name");
        assert_eq!(Slot::default().settings(), "");
    }

    #[test]
    fn forget_phrase_takes_the_phrase_quoted_or_not() {
        let edit = phrase_shape(&words(&["du-sh", "size of each file here"]), false).unwrap();
        assert_eq!((edit.label.as_str(), edit.phrase.as_str()), ("du-sh", "size of each file here"));
        let edit = phrase_shape(&words(&["--global", "du-sh", "size", "of", "each", "file", "here"]), false).unwrap();
        assert_eq!((edit.phrase.as_str(), edit.global), ("size of each file here", true));
        assert_eq!(phrase_shape(&words(&["du-sh"]), false), None);
        assert_eq!(phrase_shape(&words(&[]), false), None);
    }
}
