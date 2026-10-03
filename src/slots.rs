use std::collections::HashMap;
use std::fmt;

use anyhow::Result;
use regex::Regex;

use crate::args;
use crate::context::{list_branches, list_entries, Context};
use crate::system2::{extract, name_arguments};
use crate::types::{add_alias, with_checked_aliases, Choice, Slot, SlotKind};
use crate::Models;

/// How a slot value was found, for the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Stated by name in the request: `name=value` or `name: value`.
    Request,
    Agent,
    System1,
    Pattern,
    Default,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Request => "request",
            Source::Agent => "agent",
            Source::System1 => "system1",
            Source::Pattern => "pattern",
            Source::Default => "default",
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filled {
    pub values: HashMap<String, String>,
    /// Slots that could not be given a value.
    pub missing: Vec<String>,
    /// How each value was found, in the order they were found.
    pub sources: Vec<(String, Source)>,
    /// `name=value` arguments in the request that name no slot of the skill.
    pub unmatched: Vec<String>,
    /// Names the request used for a slot that the skill did not know yet, as (slot, alias).
    pub aliases: Vec<(String, String)>,
    /// The request with its stated values hidden: what is learned as a phrasing.
    pub phrase: String,
    /// File values that were expanded, as (slot, the value as written).
    pub expanded: Vec<(String, String)>,
    /// Variables a file value names that are not set, as (slot, variable). Left as written.
    pub not_set: Vec<(String, String)>,
    /// Optional slots nothing gave a value: left out of the command with their flags (ADR 24).
    pub omitted: Vec<String>,
    /// Values stated for a slot with choices that name none of them, as (slot, value). They block the run.
    pub not_a_choice: Vec<(String, String)>,
    pub ms_system1: u64,
    pub ms_system2: u64,
}

impl Filled {
    fn set(&mut self, name: &str, value: String, source: Source) {
        self.values.insert(name.to_string(), value);
        match self.sources.iter_mut().find(|(known, _)| known == name) {
            Some(entry) => entry.1 = source,
            None => self.sources.push((name.to_string(), source)),
        }
    }

    fn has(&self, name: &str) -> bool {
        self.values.get(name).is_some_and(|value| !value.is_empty())
    }

    /// Given a value, or stated with one that it does not take: either way nothing else fills it.
    fn settled(&self, name: &str) -> bool {
        self.has(name) || self.not_a_choice.iter().any(|(slot, _)| slot == name)
    }

    /// Give `slot` a value. A slot with choices takes only one of them, under the choice's own spelling.
    fn offer(&mut self, slot: &Slot, value: String, source: Source) -> bool {
        if slot.choices.is_empty() {
            self.set(&slot.name, value, source);
            return true;
        }
        match slot.choice(&value) {
            Some(choice) => {
                self.set(&slot.name, choice.value.clone(), source);
                true
            }
            None => false,
        }
    }
}

/// Whether `word` is in `text` as a word of its own, compared without case: `debug` in "a debug
/// build", but not in "debugging" or "pre-debug".
fn names_word(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let text = text.to_lowercase();
    let word = word.to_lowercase();
    let joins = |ch: Option<char>| ch.is_some_and(|ch| ch.is_alphanumeric() || ch == '-' || ch == '_');
    text.match_indices(&word)
        .any(|(at, _)| !joins(text[..at].chars().next_back()) && !joins(text[at + word.len()..].chars().next()))
}

const PICK_CONFIDENCE: f64 = 0.6;

/// What fill_slots needs to know about a skill.
pub struct Template<'a> {
    pub steps: &'a [String],
    pub scenario: &'a str,
    pub slots: &'a [Slot],
}

/// The skill's slots in the order the steps first use them, then any the steps never use.
fn in_template_order<'a>(skill: &Template<'a>) -> Vec<&'a Slot> {
    let mut ordered: Vec<&Slot> = Vec::new();
    for step in skill.steps {
        for part in parts(step) {
            if let Part::Slot(name) = part {
                if let Some(slot) = skill.slots.iter().find(|slot| slot.name == name) {
                    if !ordered.iter().any(|seen| seen.name == slot.name) {
                        ordered.push(slot);
                    }
                }
            }
        }
    }
    for slot in skill.slots {
        if !ordered.iter().any(|seen| seen.name == slot.name) {
            ordered.push(slot);
        }
    }
    ordered
}

/// Give every `{slot}` a value, most certain and cheapest method first:
/// a value the request states by name or alias, a value the agent already supplied, a quoted string,
/// a number or one of a slot's choices named in the request, a System 1 pick among real candidates,
/// then the agent, and the slot's default only as a last resort. An optional slot is never asked of
/// the agent: with nothing for it, it is left out of the command (ADR 24).
pub fn fill_slots(
    models: &Models,
    query: &str,
    skill: &Template,
    context: &Context,
    provided: &HashMap<String, String>,
) -> Result<Filled> {
    let reading = args::read(query, skill.slots);
    // The request with its stated values hidden, so a word inside a commit message names no choice.
    let unstated = reading.phrase.clone();
    let mut filled = Filled { phrase: reading.phrase, ..Filled::default() };
    let mut pending: Vec<&Slot> = Vec::new();
    let slots = in_template_order(skill);
    let slot_called = |name: &str| slots.iter().copied().find(|slot| slot.name == name);

    for (name, value) in reading.named {
        let Some(slot) = slot_called(&name) else { continue };
        if !filled.offer(slot, value.clone(), Source::Request) {
            filled.not_a_choice.push((name, value));
        }
    }
    // A name the skill does not know may be another name for one of its slots. The agent says
    // which; the value stays the user's own. Without an answer the argument blocks the run below.
    let mut unmatched = reading.unmatched;
    let unnamed: Vec<&Slot> = slots.iter().copied().filter(|slot| !filled.settled(&slot.name)).collect();
    if !unmatched.is_empty() && !unnamed.is_empty() {
        if let Ok((names, ms)) = name_arguments(&models.llm, skill.steps, skill.scenario, &unnamed, &unmatched) {
            filled.ms_system2 += ms;
            for (key, name) in names {
                let Some(slot) = unnamed.iter().find(|slot| slot.name == name && !filled.settled(&slot.name)) else {
                    continue;
                };
                let Some((_, value)) = unmatched.iter().find(|(stated, _)| *stated == key).cloned() else { continue };
                let value = if slot.kind == SlotKind::Number { args::as_number(&value) } else { value };
                if !filled.offer(slot, value.clone(), Source::Request) {
                    filled.not_a_choice.push((slot.name.clone(), value));
                }
                filled.aliases.push((slot.name.clone(), key.clone()));
                unmatched.retain(|(stated, _)| *stated != key);
            }
        }
    }
    filled.unmatched = unmatched.into_iter().map(|(key, _)| key).collect();
    for slot in &slots {
        if filled.settled(&slot.name) {
            continue;
        }
        if let Some(given) = provided.get(&slot.name).filter(|given| !given.is_empty()) {
            filled.offer(slot, given.clone(), Source::Agent);
        }
    }
    // Values with no name go to slots in template order. Quoted strings are only handed out when
    // there is one for every text slot still open: with fewer, which slot each belongs to is a
    // judgement, and the agent that must write the others makes it. An optional slot is not written
    // by the agent, so with fewer the slots that must have a value take them first.
    let open = |filled: &Filled, kind: SlotKind| -> Vec<&Slot> {
        let free = |slot: &&Slot| slot.kind == kind && slot.choices.is_empty() && !filled.settled(&slot.name);
        slots.iter().copied().filter(free).collect()
    };
    let text = open(&filled, SlotKind::Text);
    let needed: Vec<&Slot> = text.iter().copied().filter(|slot| !slot.optional).collect();
    let takers = if reading.quoted.len() >= text.len() {
        text
    } else if reading.quoted.len() >= needed.len() {
        needed
    } else {
        vec![]
    };
    for (slot, value) in takers.into_iter().zip(reading.quoted) {
        filled.set(&slot.name, value, Source::Pattern);
    }
    for (slot, value) in open(&filled, SlotKind::Number).into_iter().zip(reading.numbers) {
        filled.set(&slot.name, value, Source::Pattern);
    }
    // A choice the request names as a word of its own: "a debug build". Two named is no answer.
    for slot in slots.iter().filter(|slot| !slot.choices.is_empty()) {
        let named: Vec<&Choice> = slot.choices.iter().filter(|choice| names_word(&unstated, &choice.value)).collect();
        if let ([choice], false) = (named.as_slice(), filled.settled(&slot.name)) {
            filled.set(&slot.name, choice.value.clone(), Source::Pattern);
        }
    }

    // An argument the skill does not take, or a value it does not offer, stops the run, so no model
    // is asked for the rest.
    if !filled.unmatched.is_empty() || !filled.not_a_choice.is_empty() {
        expand_files(&mut filled, skill.slots, context);
        return Ok(filled);
    }

    for slot in slots {
        if filled.settled(&slot.name) {
            continue;
        }
        if !slot.choices.is_empty() {
            // Its default, or leaving it out, is what the person said to do when the request names
            // none; only a slot with neither has one picked by System 1.
            if slot.default.is_none() && !slot.optional {
                let options: Vec<String> = slot.choices.iter().map(|choice| choice.value.clone()).collect();
                let about = if slot.description.trim().is_empty() { &slot.name } else { &slot.description };
                let picked = models.system1.pick(query, about, &options)?;
                filled.ms_system1 += picked.ms;
                if let Some(value) = picked.value.filter(|_| picked.confidence >= PICK_CONFIDENCE) {
                    filled.set(&slot.name, value, Source::System1);
                    continue;
                }
            }
            pending.push(slot);
            continue;
        }
        if matches!(slot.kind, SlotKind::File | SlotKind::Branch) {
            let options = if slot.kind == SlotKind::File {
                // The top level of the working directory, and any path the request names elsewhere.
                let mut options = list_entries(&context.cwd);
                for word in path_words(query) {
                    if !options.contains(&word) {
                        options.push(word);
                    }
                }
                options
            } else {
                list_branches(&context.cwd)
            };
            if !options.is_empty() {
                let picked = models.system1.pick(query, &slot.description, &options)?;
                filled.ms_system1 += picked.ms;
                if let Some(value) = picked.value.filter(|_| picked.confidence >= PICK_CONFIDENCE) {
                    filled.set(&slot.name, value, Source::System1);
                    continue;
                }
            }
        }
        pending.push(slot);
    }

    // Text always goes to the agent when the request did not quote it; a default is only a last resort.
    // An optional slot and a slot with choices are not: what the request leaves unsaid there is the
    // person's to decide, and they did, with the default or by leaving it out.
    let for_agent: Vec<&Slot> = pending
        .iter()
        .copied()
        .filter(|slot| !slot.optional && slot.choices.is_empty() && (slot.kind == SlotKind::Text || slot.default.is_none()))
        .collect();
    if !for_agent.is_empty() {
        let (values, ms) = extract(&models.llm, query, skill.steps, skill.scenario, &for_agent)?;
        filled.ms_system2 += ms;
        for slot in &for_agent {
            if let Some(value) = values.get(&slot.name).filter(|value| !value.is_empty()) {
                filled.set(&slot.name, value.clone(), Source::Agent);
            }
        }
    }
    for slot in pending {
        if filled.has(&slot.name) {
            continue;
        }
        match &slot.default {
            Some(default) => {
                let value = slot.choice(default).map_or_else(|| default.clone(), |choice| choice.value.clone());
                filled.set(&slot.name, value, Source::Default);
            }
            None if slot.optional => filled.omitted.push(slot.name.clone()),
            None => filled.missing.push(slot.name.clone()),
        }
    }
    // After expansion, so the values are those of the command that is logged, which `jli fix` reads back.
    expand_files(&mut filled, skill.slots, context);
    // Learn the shape of the request, not the instance: a value found in it by any means is hidden too.
    let command = render(skill.steps, skill.slots, &filled.values).join(" && ");
    filled.phrase = shape(&filled.phrase, skill.steps, skill.slots, &command);
    Ok(filled)
}

/// The phrase that is learned for a request that ran `command` with these steps: `phrase`, which
/// already hides what the request stated, with every value the command was given hidden as well.
/// The values are read back from the command, so `jli fix` arrives at the same text from the event log.
fn shape(phrase: &str, steps: &[String], slots: &[Slot], command: &str) -> String {
    let Some(values) = match_skill(steps, slots, command) else { return phrase.to_string() };
    let mut hidden: Vec<String> = values.into_values().collect();
    hidden.extend(written_forms(phrase, &hidden));
    args::hide_values(phrase, hidden.iter().map(String::as_str))
}

/// The words of `phrase` that expand to one of `values` the way a file value is expanded (ADR 14):
/// `~/notes` in the request for `/home/x/notes` in the command. Expanded with the environment as it
/// is now, so `jli fix` finds the same words as long as the variables have not changed.
fn written_forms(phrase: &str, values: &[String]) -> Vec<String> {
    let env = |name: &str| std::env::var(name).ok();
    let mut words: Vec<String> = Vec::new();
    for word in path_words(phrase) {
        // A full stop that ends the sentence is not part of the path.
        let bare = word.strip_suffix('.').map(str::to_string);
        for word in std::iter::once(word).chain(bare) {
            let expanded = expand_words(&word, env).value;
            if expanded != word && values.contains(&expanded) && !words.contains(&word) {
                words.push(word);
            }
        }
    }
    words
}

/// What a logged request was learned as, worked out again from the request and the command it ran.
/// With no command, or steps that no longer produce it, only the stated values are hidden.
pub fn learned_phrase(query: &str, steps: &[String], slots: &[Slot], command: Option<&str>) -> String {
    let stated = args::phrase(query, slots);
    match command {
        Some(command) => shape(&stated, steps, slots, command),
        None => stated,
    }
}

/// Expand `~` and `$NAME` in every file value, whoever found it. Text is left as written: a
/// commit message that says `$5` means five dollars.
fn expand_files(filled: &mut Filled, slots: &[Slot], context: &Context) {
    for slot in slots.iter().filter(|slot| slot.kind == SlotKind::File) {
        let Some(written) = filled.values.get(&slot.name).cloned() else { continue };
        let expanded = expand_path(&written, &context.cwd, |name| std::env::var(name).ok());
        for variable in expanded.not_set {
            filled.not_set.push((slot.name.clone(), variable));
        }
        if expanded.value != written {
            filled.values.insert(slot.name.clone(), expanded.value);
            filled.expanded.push((slot.name.clone(), written));
        }
    }
}

/// A piece of a step template: literal text, or a `{name}` placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part<'a> {
    Text(&'a str),
    Slot(&'a str),
}

/// Splits a template at its placeholders: `{name}`, but not the shell's own `${name}`,
/// and not the `{}` of `find -exec`.
fn parts(template: &str) -> Vec<Part<'_>> {
    let bytes = template.as_bytes();
    let mut out = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && (i == 0 || bytes[i - 1] != b'$') {
            let name_start = i + 1;
            let mut j = name_start;
            if j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
                j += 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'}' {
                    out.push(Part::Text(&template[text_start..i]));
                    out.push(Part::Slot(&template[name_start..j]));
                    i = j + 1;
                    text_start = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push(Part::Text(&template[text_start..]));
    out
}

/// A value as one word for bash and zsh. A leading `=` is quoted too: zsh would read `=name` as
/// the path of the command `name` (ADR 14).
fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && !value.starts_with('=')
        && value.chars().all(|ch| ch.is_ascii_alphanumeric() || "_-.,/:=@%+".contains(ch));
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// A value as one word for fish, whose single quotes give `\'` and `\\` a meaning that bash's do not.
fn fish_quote(value: &str) -> String {
    let plain = !value.is_empty() && value.chars().all(|ch| ch.is_ascii_alphanumeric() || "_-.,/:=@+".contains(ch));
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\\', r"\\").replace('\'', r"\'"))
    }
}

/// What one placeholder becomes in a command.
enum Written<'a> {
    /// A value, quoted for the shell.
    Value(&'a str),
    /// A choice's text, exactly as the person wrote it.
    Text(&'a str),
    /// Nothing: the placeholder goes, and so does its flag.
    Nothing,
    /// No value yet: the placeholder stays as it is.
    Open,
}

fn written<'a>(slot: Option<&'a Slot>, value: Option<&'a String>) -> Written<'a> {
    match (slot, value) {
        (Some(slot), Some(value)) => match slot.choice(value) {
            Some(choice) if choice.text.trim().is_empty() => Written::Nothing,
            Some(choice) => Written::Text(choice.text.trim()),
            None => Written::Value(value),
        },
        (None, Some(value)) => Written::Value(value),
        (Some(slot), None) if slot.optional => Written::Nothing,
        _ => Written::Open,
    }
}

/// How much of `text` is kept when the placeholder after it writes nothing: all but the space before
/// the placeholder and, when it is written there as a word of its own, the slot's flag and the space before it.
fn kept_before(text: &str, flag: Option<&str>) -> usize {
    let end = text.trim_end().len();
    let Some(flag) = flag.map(str::trim).filter(|flag| !flag.is_empty()) else { return end };
    match text[..end].strip_suffix(flag) {
        Some(rest) if rest.is_empty() || rest.ends_with(char::is_whitespace) => rest.trim_end().len(),
        _ => end,
    }
}

fn render_with(
    steps: &[String],
    slots: &[Slot],
    values: &HashMap<String, String>,
    quote: fn(&str) -> String,
) -> Vec<String> {
    steps
        .iter()
        .map(|step| {
            let mut out = String::new();
            for part in parts(step) {
                match part {
                    Part::Text(text) => out.push_str(text),
                    Part::Slot(name) => {
                        let slot = slots.iter().find(|slot| slot.name == name);
                        match written(slot, values.get(name)) {
                            Written::Value(value) => out.push_str(&quote(value)),
                            Written::Text(text) => out.push_str(text),
                            Written::Nothing => out.truncate(kept_before(&out, slot.and_then(|slot| slot.flag.as_deref()))),
                            Written::Open => out.push_str(&format!("{{{name}}}")),
                        }
                    }
                }
            }
            out
        })
        .collect()
}

/// Substitute slot values into the step templates: each value shell-quoted, a choice as its text, and
/// an optional slot with no value left out with its flag (ADR 24). With no slots, every placeholder
/// that has a value takes it quoted.
pub fn render(steps: &[String], slots: &[Slot], values: &HashMap<String, String>) -> Vec<String> {
    render_with(steps, slots, values, shell_quote)
}

/// The same, with values quoted for fish, for steps handed to a fish shell.
pub fn render_fish(steps: &[String], slots: &[Slot], values: &HashMap<String, String>) -> Vec<String> {
    render_with(steps, slots, values, fish_quote)
}

/// Whether `flag` is written just before every use of `{name}` in the steps, as a word of its own
/// (`-n {count}`) or with the value joined to it (`--profile={profile}`), so that dropping the one
/// can drop the other. A flag begins with `-` and has no space in it.
pub fn flag_fits(steps: &[String], name: &str, flag: &str) -> bool {
    let flag = flag.trim();
    if !flag.starts_with('-') || flag.contains(char::is_whitespace) {
        return false;
    }
    let mut used = false;
    for step in steps {
        let parts = parts(step);
        for (at, part) in parts.iter().enumerate() {
            if *part != Part::Slot(name) {
                continue;
            }
            used = true;
            let Some(Part::Text(before)) = at.checked_sub(1).map(|at| parts[at]) else { return false };
            let fits = match before.trim_end().strip_suffix(flag) {
                Some("") => at == 1,
                Some(rest) => rest.ends_with(char::is_whitespace),
                None => false,
            };
            if !fits {
                return false;
            }
        }
    }
    used
}

/// The text a use of `{name}` is joined to, `--profile=` in `--profile={profile}`, when there is
/// any: leaving the slot out without a flag would leave that text behind.
pub fn joined_before(steps: &[String], name: &str) -> Option<String> {
    steps.iter().find_map(|step| {
        let parts = parts(step);
        parts.iter().enumerate().find_map(|(at, part)| match (part, at.checked_sub(1).map(|at| parts[at])) {
            (Part::Slot(used), Some(Part::Text(before))) if *used == name && !before.is_empty() => {
                let word = before.rsplit(char::is_whitespace).next().unwrap_or_default();
                (!word.is_empty()).then(|| word.to_string())
            }
            _ => None,
        })
    })
}

/// Slots as a model wrote them, held to what a model may say (ADR 24): a flag not written just before
/// its placeholder is dropped; a slot is optional only together with its flag, so no model leaves out
/// a value the command takes without one; an optional slot has no default, or leaving it out would
/// never happen; and there are no choices, whose text reaches the shell unquoted.
pub fn checked_slots(mut slots: Vec<Slot>, steps: &[String]) -> Vec<Slot> {
    for slot in &mut slots {
        slot.choices.clear();
        slot.flag = slot.flag.take().map(|flag| flag.trim().to_string()).filter(|flag| flag_fits(steps, &slot.name, flag));
        slot.optional &= slot.flag.is_some();
        if slot.optional {
            slot.default = None;
        }
    }
    with_checked_aliases(slots)
}

/// What a person set on a slot survives a write that replaces a skill's slots, as its aliases do: a
/// slot that keeps its name keeps its choices and its default where the new one has none, and stays
/// optional, with its flag as long as that flag is still written before the placeholder.
pub fn keep_settings(slots: &mut [Slot], known: &[Slot], steps: &[String]) {
    for before in known {
        for alias in &before.aliases {
            add_alias(slots, &before.name, alias);
        }
        let Some(slot) = slots.iter_mut().find(|slot| slot.name == before.name) else { continue };
        if slot.choices.is_empty() {
            slot.choices = before.choices.clone();
        }
        if slot.default.is_none() {
            slot.default = before.default.clone();
        }
        if before.optional && !slot.optional {
            let flag = before.flag.clone().filter(|flag| flag_fits(steps, &slot.name, flag));
            if flag.is_some() || before.flag.is_none() {
                slot.optional = true;
                slot.flag = flag;
            }
        }
    }
}

/// A path value after `~` and `$NAME` have been read from jli's own environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expanded {
    pub value: String,
    /// Variables the value names that are unset or empty. They are left as written.
    pub not_set: Vec<String>,
}

fn is_name_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Expand a path the way a shell would, but only these: a leading `~` or `~/` to `$HOME`, and
/// `$NAME` or `${NAME}` to that variable. Nothing is ever run: `$(…)`, backticks, `${NAME:-x}`,
/// `$1` and `$$` stay as written, as does a variable that is unset or empty, and a value that
/// names an existing path just as it is written. The result is quoted like any value (ADR 7).
pub fn expand_path(value: &str, cwd: &std::path::Path, env: impl Fn(&str) -> Option<String>) -> Expanded {
    if !value.contains(['~', '$']) || cwd.join(value).symlink_metadata().is_ok() {
        return Expanded { value: value.to_string(), not_set: vec![] };
    }
    expand_words(value, env)
}

/// `expand_path` without the check for a path that exists as written.
fn expand_words(value: &str, env: impl Fn(&str) -> Option<String>) -> Expanded {
    let mut expanded = Expanded { value: value.to_string(), not_set: vec![] };
    let lookup = |name: &str| env(name).filter(|found| !found.is_empty());
    let mut out = String::new();
    let mut rest = value;
    if value == "~" || value.starts_with("~/") {
        match lookup("HOME") {
            Some(home) => {
                out.push_str(&home);
                rest = &value[1..];
            }
            None => expanded.not_set.push("HOME".into()),
        }
    }
    let mut chars = rest.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        let after = &rest[at + 1..];
        let (name, written) = if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                Some(close) => (&braced[..close], &rest[at..at + close + 3]),
                None => ("", ""),
            }
        } else {
            let len = after.find(|ch: char| !is_name_char(ch)).unwrap_or(after.len());
            (&after[..len], &rest[at..at + 1 + len])
        };
        let valid = name.starts_with(is_name_start) && name.chars().all(is_name_char);
        if !valid {
            out.push('$');
            continue;
        }
        match lookup(name) {
            Some(found) => out.push_str(&found),
            None => {
                out.push_str(written);
                if !expanded.not_set.iter().any(|known| known == name) {
                    expanded.not_set.push(name.to_string());
                }
            }
        }
        for _ in 1..written.chars().count() {
            chars.next();
        }
    }
    expanded.value = out;
    expanded
}

/// Words in the request that are paths outside the working directory's top level, such as
/// `~/notes/todo.md` or `src/main.rs`, so a file slot can be given one of them.
fn path_words(query: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in query.split_whitespace() {
        let word = word.trim_matches(['"', '\'', '`']).trim_end_matches([',', ';', ':', '!', '?', ')']);
        let variable =
            word.strip_prefix('$').is_some_and(|name| name.starts_with(|ch: char| is_name_start(ch) || ch == '{'));
        let pathlike = word.contains('/') || word == "~" || word.starts_with("~/") || variable;
        if pathlike && !word.contains("://") && !word.contains('=') && !found.iter().any(|seen| seen == word) {
            found.push(word.to_string());
        }
    }
    found
}

/// A literal piece of a template as a pattern: escaped, with any run of whitespace matching any run.
fn literal_pattern(text: &str) -> String {
    let mut pattern = String::new();
    let mut run = String::new();
    let mut in_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !in_space {
                pattern.push_str(&regex::escape(&run));
                run.clear();
                pattern.push_str(r"\s+");
                in_space = true;
            }
        } else {
            in_space = false;
            run.push(ch);
        }
    }
    pattern.push_str(&regex::escape(&run));
    pattern
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && (bytes[0] == b'\'' || bytes[0] == b'"') && bytes[bytes.len() - 1] == bytes[0] {
        let inner = &value[1..value.len() - 1];
        if !inner.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
            return inner;
        }
    }
    value
}

/// A flag is part of what a command does, never a value (ADR 7): `-`, `-n`, `--all`, `'-x'`.
pub fn is_flag(value: &str) -> bool {
    value.trim_start().starts_with('-')
}

/// True when the steps hold no placeholder: a skill that runs one command and no other.
pub fn is_literal(steps: &[String]) -> bool {
    !steps.iter().any(|step| parts(step).iter().any(|part| matches!(part, Part::Slot(_))))
}

/// If `command` is what `steps` would produce for some slot values, return those values.
/// Lets a taught command be recognised as a known skill with different values, with no model call.
/// A value that would be a flag is not a match: `cd -` is not `cd {directory}` with `-` (ADR 19).
/// Every slot is read as one that must have a value; `match_skill` knows a skill's optional slots and choices.
pub fn match_template(steps: &[String], command: &str) -> Option<HashMap<String, String>> {
    match_skill(steps, &[], command)
}

/// `match_template` for a skill with its slots: an optional slot and its flag may be absent from the
/// command, and a slot with choices is read back from the text of one, as that choice's value. An
/// optional slot that is absent has no value in the result.
pub fn match_skill(steps: &[String], slots: &[Slot], command: &str) -> Option<HashMap<String, String>> {
    captures(steps, slots, command).filter(|values| !values.values().any(|value| is_flag(value)))
}

/// True when `steps` produce `command` only by taking a flag as a slot value, as `date {format}` does
/// for `date -I`.
pub fn slots_a_flag(steps: &[String], command: &str) -> bool {
    captures(steps, &[], command).is_some_and(|values| values.values().any(|value| is_flag(value)))
}

/// Whether a slot can write nothing: an optional one, or one with a choice whose text is empty.
fn can_be_absent(slot: Option<&Slot>) -> bool {
    slot.is_some_and(|slot| slot.optional || slot.choices.iter().any(|choice| choice.text.trim().is_empty()))
}

/// The pattern for what a placeholder writes, as one capture group: a word or a quoted string, or for a
/// slot with choices the text of one of them, longest first.
fn value_pattern(slot: Option<&Slot>) -> String {
    let Some(slot) = slot.filter(|slot| !slot.choices.is_empty()) else { return r#"(\S+|'[^']*'|"[^"]*")"#.to_string() };
    let mut texts: Vec<&str> =
        slot.choices.iter().map(|choice| choice.text.trim()).filter(|text| !text.is_empty()).collect();
    texts.sort_by_key(|text| std::cmp::Reverse(text.len()));
    let texts: Vec<String> = texts.into_iter().map(literal_pattern).collect();
    format!("({})", texts.join("|"))
}

/// The value a slot was given, read back from what its placeholder wrote, or None when it wrote nothing.
fn read_back(slot: Option<&Slot>, found: Option<&str>) -> Option<String> {
    let Some(slot) = slot.filter(|slot| !slot.choices.is_empty()) else {
        return found.map(|found| unquote(found).to_string());
    };
    fn words(text: &str) -> Vec<&str> {
        text.split_whitespace().collect()
    }
    let wrote = found.map(words).unwrap_or_default();
    slot.choices.iter().find(|choice| words(&choice.text) == wrote).map(|choice| choice.value.clone())
}

/// The value of each slot that makes `steps` produce `command`, flags included.
fn captures(steps: &[String], slots: &[Slot], command: &str) -> Option<HashMap<String, String>> {
    let joined = steps.join(" && ");
    let parts = parts(&joined);
    let slot_called = |name: &str| slots.iter().find(|slot| slot.name == name);
    let mut names = Vec::new();
    let mut pattern = String::from("^");
    for (at, part) in parts.iter().enumerate() {
        match *part {
            Part::Text(text) => {
                // What leads up to a placeholder that can write nothing goes with it, as `render` drops it.
                let next = match parts.get(at + 1) {
                    Some(Part::Slot(name)) => slot_called(name).filter(|slot| can_be_absent(Some(slot))),
                    _ => None,
                };
                let kept = next.map_or(text.len(), |slot| kept_before(text, slot.flag.as_deref()));
                pattern.push_str(&literal_pattern(&text[..kept]));
            }
            Part::Slot(name) => {
                names.push(name);
                let slot = slot_called(name);
                let value = value_pattern(slot);
                if can_be_absent(slot) {
                    let Some(Part::Text(before)) = at.checked_sub(1).map(|at| parts[at]) else { return None };
                    let lead = &before[kept_before(before, slot.and_then(|slot| slot.flag.as_deref()))..];
                    pattern.push_str(&format!("(?:{}{value})?", literal_pattern(lead)));
                } else {
                    pattern.push_str(&value);
                }
            }
        }
    }
    pattern.push('$');
    let captures = Regex::new(&pattern).ok()?.captures(command.trim())?;
    let mut values = HashMap::new();
    for (index, name) in names.into_iter().enumerate() {
        let slot = slot_called(name);
        let found = captures.get(index + 1).map(|found| found.as_str());
        let found = if can_be_absent(slot) { found } else { Some(found.unwrap_or("")) };
        if let Some(value) = read_back(slot, found) {
            values.insert(name.to_string(), value);
        }
    }
    Some(values)
}

/// If `existing` produces every command a proposed template can, return the values for `existing`'s
/// own slots. That is the same template with its placeholders renamed, `-n {n}` for `-n {count}`,
/// or a literal command it produces, `-n 5`. A value proposed for a placeholder moves by name to the
/// slot it lines up with, never through shell quoting; one with no value is left for the caller.
///
/// A proposal more general than `existing`, `-n {n}` against a literal `-n 3`, is not a match:
/// taking the literal skill would lose the template, and every later count would run `-n 3`.
pub fn match_proposal(
    existing: &[String],
    slots: &[Slot],
    proposed: &[String],
    values: &HashMap<String, String>,
) -> Option<HashMap<String, String>> {
    let captured = match_skill(existing, slots, &proposed.join(" && "))?;
    let mut out = HashMap::new();
    for (slot, value) in captured {
        match parts(&value).as_slice() {
            [Part::Text(text)] => {
                out.insert(slot, (*text).to_string());
            }
            // One placeholder and nothing around it: the proposal's slot is this one under another name.
            [Part::Text(""), Part::Slot(name), Part::Text("")] => {
                if let Some(given) = values.get(*name).filter(|given| !given.is_empty()) {
                    out.insert(slot, given.clone());
                }
            }
            // Text around a placeholder, `{a}-{b}` for `{text}`: not the same template.
            _ => return None,
        }
    }
    Some(out)
}

/// Placeholders used in steps that no slot declares, in order of first use.
pub fn undeclared(steps: &[String], slots: &[Slot]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for step in steps {
        for part in parts(step) {
            if let Part::Slot(name) = part {
                if !slots.iter().any(|slot| slot.name == name) && !out.iter().any(|seen| seen == name) {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn steps(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn placeholders_do_not_match_shell_syntax() {
        let rendered = render(
            &steps(&["echo ${HOME} {name} {} {1x} {{name}}", "find . -exec ls {} \\;"]),
            &[],
            &values(&[("name", "x"), ("HOME", "no")]),
        );
        assert_eq!(rendered, ["echo ${HOME} x {} {1x} {x}", "find . -exec ls {} \\;"]);
        assert_eq!(render(&steps(&["wc -l {file}"]), &[], &values(&[])), ["wc -l {file}"]);
    }

    #[test]
    fn values_are_shell_quoted_unless_plain() {
        let template = steps(&["git commit -m {message}"]);
        assert_eq!(render(&template, &[], &values(&[("message", "first draft")])), ["git commit -m 'first draft'"]);
        assert_eq!(render(&template, &[], &values(&[("message", "it's")])), [r"git commit -m 'it'\''s'"]);
        assert_eq!(
            render(&template, &[], &values(&[("message", "a/b.c:d=e@f%g+h,i-j_k")])),
            ["git commit -m a/b.c:d=e@f%g+h,i-j_k"]
        );
        assert_eq!(render(&template, &[], &values(&[("message", "")])), ["git commit -m ''"]);
    }

    #[test]
    fn templates_recognise_their_own_commands() {
        let log = steps(&["git log --oneline -n {count}"]);
        assert_eq!(match_template(&log, "git log --oneline -n 10"), Some(values(&[("count", "10")])));
        assert_eq!(match_template(&log, "  git   log --oneline -n 10 "), Some(values(&[("count", "10")])));
        assert_eq!(match_template(&log, "git log --oneline -n"), None);
        assert_eq!(match_template(&log, "git log --oneline -n 10 --all"), None);
        let commit = steps(&["git commit -m {message}"]);
        assert_eq!(match_template(&commit, "git commit -m 'two words'"), Some(values(&[("message", "two words")])));
        assert_eq!(match_template(&commit, "git commit -m \"x\""), Some(values(&[("message", "x")])));
        let fixed = steps(&["ls -ltr"]);
        assert_eq!(match_template(&fixed, "ls -ltr"), Some(values(&[])));
        assert_eq!(match_template(&fixed, "ls -lt"), None);
        let chained = steps(&["git add -A", "git commit -m {message}"]);
        assert!(match_template(&chained, "git add -A && git commit -m wip").is_some());
        let regexy = steps(&["grep -E 'a.b' {file}"]);
        assert!(match_template(&regexy, "grep -E 'axb' notes.txt").is_none(), "literals are escaped");
    }

    #[test]
    fn a_flag_is_never_a_slot_value() {
        let cd = steps(&["cd {directory}"]);
        assert_eq!(match_template(&cd, "cd -"), None, "cd - goes back; it is not a directory called -");
        assert_eq!(match_template(&cd, "cd src"), Some(values(&[("directory", "src")])));
        assert!(slots_a_flag(&cd, "cd -"));
        assert!(!slots_a_flag(&cd, "cd src"));
        let list = steps(&["ls {directory}"]);
        assert_eq!(match_template(&list, "ls -la"), None);
        assert_eq!(match_template(&list, "ls '-la'"), None, "quoting a flag does not make it a value");
        let head = steps(&["head -n {count} {file}"]);
        assert_eq!(match_template(&head, "head -n -5 notes.txt"), None);
        assert_eq!(match_template(&head, "head -n 5 notes.txt"), Some(values(&[("count", "5"), ("file", "notes.txt")])));
        let commit = steps(&["git commit -m {message}"]);
        assert_eq!(
            match_template(&commit, "git commit -m 'fix -x handling'"),
            Some(values(&[("message", "fix -x handling")]))
        );
        assert_eq!(
            match_proposal(&list, &[], &steps(&["ls -la"]), &values(&[])),
            None,
            "a proposal with a flag where a skill has a slot is another command"
        );
        assert!(is_literal(&steps(&["ls -ltr", "find . -exec ls {} \\;"])));
        assert!(!is_literal(&steps(&["git add -A", "git commit -m {message}"])));
    }

    #[test]
    fn a_proposal_is_recognised_as_a_known_template() {
        let log = steps(&["git log --oneline -n {count}"]);
        let renamed = steps(&["git log --oneline -n {n}"]);
        assert_eq!(match_proposal(&log, &[], &renamed, &values(&[("n", "10")])), Some(values(&[("count", "10")])));
        assert_eq!(
            match_proposal(&log, &[], &renamed, &values(&[])),
            Some(values(&[])),
            "a value still to find is left open"
        );
        assert_eq!(
            match_proposal(&log, &[], &steps(&["git log --oneline -n 5"]), &values(&[])),
            Some(values(&[("count", "5")]))
        );
        assert_eq!(match_proposal(&log, &[], &steps(&["git log -n {n}"]), &values(&[("n", "10")])), None);

        let commit = steps(&["git commit -m {message}"]);
        let said = values(&[("msg", "it's done")]);
        assert_eq!(
            match_proposal(&commit, &[], &steps(&["git commit -m {msg}"]), &said),
            Some(values(&[("message", "it's done")])),
            "a value moves by name, never through shell quoting"
        );
        let literal = steps(&["git log --oneline -n 10"]);
        assert_eq!(match_proposal(&literal, &[], &steps(&["git log --oneline -n 10"]), &values(&[])), Some(values(&[])));
        assert_eq!(
            match_proposal(&literal, &[], &renamed, &values(&[("n", "10")])),
            None,
            "a literal skill does not stand in for a template, even with the same value"
        );
        let echo = steps(&["echo {text}"]);
        assert_eq!(match_proposal(&echo, &[], &steps(&["echo {a}-{b}"]), &values(&[("a", "x"), ("b", "y")])), None);
    }

    #[test]
    fn the_learned_phrase_can_be_worked_out_again_from_the_command() {
        let count = steps(&["wc -l {file}"]);
        assert_eq!(
            learned_phrase("how many lines in notes.txt?", &count, &[], Some("wc -l notes.txt")),
            "how many lines in …?"
        );
        assert_eq!(learned_phrase("how many lines in notes.txt", &count, &[], None), "how many lines in notes.txt");
        assert_eq!(
            learned_phrase("how many lines in notes.txt", &steps(&["cat {file}"]), &[], Some("wc -l notes.txt")),
            "how many lines in notes.txt",
            "steps that no longer produce the command hide nothing more"
        );
    }

    #[test]
    fn undeclared_placeholders_are_found_once() {
        let slots = vec![Slot {
            name: "file".into(),
            kind: SlotKind::File,
            description: String::new(),
            default: None,
            aliases: vec![],
            ..Slot::default()
        }];
        assert_eq!(undeclared(&steps(&["cp {file} {dest}", "ls {dest} ${HOME}"]), &slots), ["dest"]);
    }

    fn env(name: &str) -> Option<String> {
        match name {
            "HOME" => Some("/home/ada".into()),
            "PROJECT" => Some("jli".into()),
            "SPACED" => Some("my notes; rm -rf /".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    fn expand(value: &str) -> (String, Vec<String>) {
        let expanded = expand_path(value, std::path::Path::new("/nonexistent-cwd"), env);
        (expanded.value, expanded.not_set)
    }

    #[test]
    fn a_path_expands_home_and_variables_and_nothing_else() {
        assert_eq!(expand("~").0, "/home/ada");
        assert_eq!(expand("~/notes").0, "/home/ada/notes");
        assert_eq!(expand("$HOME/src").0, "/home/ada/src");
        assert_eq!(expand("${HOME}/src/$PROJECT").0, "/home/ada/src/jli");
        assert_eq!(expand("a/${PROJECT}b").0, "a/jlib");
        assert_eq!(expand("~ada/notes").0, "~ada/notes", "another user's home is not looked up");
        assert_eq!(expand("notes/~").0, "notes/~", "only a leading ~");
        for literal in ["$(rm -rf ~)", "`id`", "${HOME:-x}", "$1", "$$", "$", "a$", "${", "${1}", "x$?y"] {
            assert_eq!(expand(literal).0, literal, "{literal}");
        }
        assert_eq!(expand("$NOPE/x"), ("$NOPE/x".into(), vec!["NOPE".into()]), "an unset variable is left as written");
        assert_eq!(expand("$EMPTY/build"), ("$EMPTY/build".into(), vec!["EMPTY".into()]), "never /build");
        let no_home = expand_path("~/x", std::path::Path::new("/nonexistent-cwd"), |_| None);
        assert_eq!((no_home.value.as_str(), no_home.not_set), ("~/x", vec!["HOME".to_string()]));
    }

    #[test]
    fn an_expanded_value_is_still_quoted() {
        let (value, _) = expand("$SPACED");
        let rendered = render(&steps(&["ls {dir}"]), &[], &values(&[("dir", &value)]));
        assert_eq!(rendered, ["ls 'my notes; rm -rf /'"], "a variable's value cannot inject either");
        assert_eq!(render(&steps(&["ls {dir}"]), &[], &values(&[("dir", "/home/ada/notes")])), ["ls /home/ada/notes"]);
    }

    #[test]
    fn a_value_that_names_an_existing_path_is_kept() {
        let dir = std::env::temp_dir().join(format!("jli-expand-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("$PROJECT")).unwrap();
        assert_eq!(expand_path("$PROJECT", &dir, env).value, "$PROJECT");
        assert_eq!(expand_path("$PROJECT/x", &dir, env).value, "jli/x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quoting_holds_in_zsh_and_fish_too() {
        let one = |value: &str| values(&[("v", value)]);
        let template = steps(&["echo {v}"]);
        assert_eq!(render(&template, &[], &one("=ls")), ["echo '=ls'"], "zsh expands a leading =");
        assert_eq!(render(&template, &[], &one("a=b")), ["echo a=b"]);
        assert_eq!(render_fish(&template, &[], &one(r"a\b")), [r"echo 'a\\b'"]);
        assert_eq!(render_fish(&template, &[], &one("it's")), [r"echo 'it\'s'"]);
        assert_eq!(render_fish(&template, &[], &one(r"end\")), [r"echo 'end\\'"]);
        assert_eq!(render_fish(&template, &[], &one("%self")), ["echo '%self'"]);
        assert_eq!(render_fish(&template, &[], &one("/tmp/a.b")), ["echo /tmp/a.b"]);
    }

    #[test]
    fn paths_in_the_request_are_found() {
        assert_eq!(
            path_words("count the lines of ~/notes/n.txt, and src/main.rs; then ~ and $HOME/x"),
            ["~/notes/n.txt", "src/main.rs", "~", "$HOME/x"]
        );
        assert!(path_words("open https://x.io/a and dir=src/a, it costs $5 or ~5 notes.txt").is_empty());
    }

    fn optional(name: &str, flag: Option<&str>) -> Slot {
        Slot { name: name.into(), optional: true, flag: flag.map(String::from), ..Slot::default() }
    }

    fn profile() -> Slot {
        Slot {
            name: "profile".into(),
            choices: vec![
                Choice { value: "release".into(), text: "--release".into() },
                Choice { value: "debug".into(), text: String::new() },
                Choice { value: "fast".into(), text: "--profile fast".into() },
            ],
            ..Slot::default()
        }
    }

    #[test]
    fn an_optional_slot_with_no_value_is_left_out_with_its_flag() {
        let log = steps(&["git log --oneline -n {count}"]);
        let count = [optional("count", Some("-n"))];
        assert_eq!(render(&log, &count, &values(&[])), ["git log --oneline"]);
        assert_eq!(render(&log, &count, &values(&[("count", "5")])), ["git log --oneline -n 5"]);
        assert_eq!(
            render(&log, &[], &values(&[])),
            ["git log --oneline -n {count}"],
            "a slot that must have a value stays open"
        );
        let middle = steps(&["git log -n {count} --oneline", "echo done"]);
        assert_eq!(render(&middle, &count, &values(&[])), ["git log --oneline", "echo done"]);
        let joined = steps(&["cargo build --profile={profile}"]);
        assert_eq!(render(&joined, &[optional("profile", Some("--profile="))], &values(&[])), ["cargo build"]);
        assert_eq!(
            render(&steps(&["ls -la {dir}"]), &[optional("dir", None)], &values(&[])),
            ["ls -la"],
            "no flag: only the value"
        );
        assert_eq!(
            render_fish(&log, &count, &values(&[("count", "it's")])),
            [r"git log --oneline -n 'it\'s'"],
            "a value is quoted as ever"
        );
    }

    #[test]
    fn a_choice_writes_its_own_text_as_written() {
        let build = steps(&["cargo build {profile} --locked"]);
        let slots = [profile()];
        assert_eq!(render(&build, &slots, &values(&[("profile", "release")])), ["cargo build --release --locked"]);
        assert_eq!(
            render(&build, &slots, &values(&[("profile", "Debug")])),
            ["cargo build --locked"],
            "an empty text writes nothing"
        );
        assert_eq!(render(&build, &slots, &values(&[("profile", "fast")])), ["cargo build --profile fast --locked"]);
        assert_eq!(render(&build, &slots, &values(&[])), ["cargo build {profile} --locked"]);
    }

    #[test]
    fn a_command_is_read_back_through_optional_slots_and_choices() {
        let log = steps(&["git log --oneline -n {count}"]);
        let count = [optional("count", Some("-n"))];
        assert_eq!(match_skill(&log, &count, "git log --oneline"), Some(values(&[])));
        assert_eq!(match_skill(&log, &count, "git  log --oneline -n 7"), Some(values(&[("count", "7")])));
        assert_eq!(match_skill(&log, &count, "git log --oneline -n"), None);
        assert_eq!(match_template(&log, "git log --oneline"), None, "without the slots every value is needed");
        let middle = steps(&["git log -n {count} --oneline"]);
        assert_eq!(match_skill(&middle, &count, "git log --oneline"), Some(values(&[])));
        assert_eq!(match_skill(&middle, &count, "git log -n 2 --oneline"), Some(values(&[("count", "2")])));

        let list = steps(&["ls {dir}"]);
        let dir = [optional("dir", None)];
        assert_eq!(match_skill(&list, &dir, "ls"), Some(values(&[])));
        assert_eq!(match_skill(&list, &dir, "ls src"), Some(values(&[("dir", "src")])));
        assert_eq!(match_skill(&list, &dir, "ls -la"), None, "a flag is still never a value");

        let build = steps(&["cargo build {profile} --locked"]);
        let slots = [profile()];
        assert_eq!(match_skill(&build, &slots, "cargo build --release --locked"), Some(values(&[("profile", "release")])));
        assert_eq!(match_skill(&build, &slots, "cargo build --locked"), Some(values(&[("profile", "debug")])));
        assert_eq!(
            match_skill(&build, &slots, "cargo build --profile  fast --locked"),
            Some(values(&[("profile", "fast")]))
        );
        assert_eq!(match_skill(&build, &slots, "cargo build --verbose --locked"), None, "a choice is one of its texts");

        // What is rendered is read back as the values it was rendered from.
        for (slots, steps, given) in [
            (count.to_vec(), log.clone(), values(&[("count", "5")])),
            (count.to_vec(), middle.clone(), values(&[])),
            (vec![profile()], build.clone(), values(&[("profile", "fast")])),
        ] {
            let command = render(&steps, &slots, &given).join(" && ");
            assert_eq!(match_skill(&steps, &slots, &command), Some(given), "{command}");
        }
    }

    #[test]
    fn a_flag_fits_only_where_it_is_written_before_its_placeholder() {
        let log = steps(&["git log -n {count}", "echo -n {count}"]);
        assert!(flag_fits(&log, "count", "-n"));
        assert!(flag_fits(&steps(&["cargo build --profile={profile}"]), "profile", "--profile="));
        assert!(!flag_fits(&steps(&["git log -n {count}", "echo {count}"]), "count", "-n"), "every use");
        assert!(!flag_fits(&log, "count", "n"), "a flag begins with -");
        assert!(!flag_fits(&steps(&["git log --no-n {count}"]), "count", "-n"), "a whole word");
        assert!(!flag_fits(&steps(&["git log {count}"]), "count", "-n"));
        assert!(!flag_fits(&steps(&["git log -n 3"]), "count", "-n"), "a slot the steps never use");
        assert_eq!(joined_before(&steps(&["cargo build --profile={profile}"]), "profile").as_deref(), Some("--profile="));
        assert_eq!(joined_before(&steps(&["ls -la {dir}"]), "dir"), None);
    }

    #[test]
    fn a_model_slot_is_optional_only_with_its_flag_and_never_has_choices() {
        let checked = checked_slots(
            vec![
                Slot { default: Some("3".into()), ..optional("count", Some("-n")) },
                optional("dir", None),
                Slot { choices: profile().choices, ..optional("word", Some("-x")) },
            ],
            &steps(&["ls {dir} | head -n {count} && echo {word}"]),
        );
        assert_eq!((checked[0].optional, checked[0].flag.as_deref(), &checked[0].default), (true, Some("-n"), &None));
        assert_eq!((checked[1].optional, &checked[1].flag), (false, &None), "a model never leaves out a positional value");
        assert_eq!((checked[2].optional, &checked[2].flag, checked[2].choices.len()), (false, &None, 0));
    }

    #[test]
    fn what_a_person_set_survives_new_slots() {
        let before = vec![
            Slot { aliases: vec!["n".into()], ..optional("count", Some("-n")) },
            Slot { default: Some("release".into()), ..profile() },
        ];
        let mut after =
            vec![Slot { name: "count".into(), ..Slot::default() }, Slot { name: "profile".into(), ..Slot::default() }];
        keep_settings(&mut after, &before, &steps(&["git log -n {count} {profile}"]));
        assert_eq!(
            (after[0].optional, after[0].flag.as_deref(), after[0].aliases.as_slice()),
            (true, Some("-n"), ["n".to_string()].as_slice())
        );
        assert_eq!((after[1].default.as_deref(), after[1].choices.len()), (Some("release"), 3));
        let mut moved = vec![Slot { name: "count".into(), ..Slot::default() }];
        keep_settings(&mut moved, &before, &steps(&["git log {count}"]));
        assert!(!moved[0].optional, "a flag no longer before the placeholder does not make it optional");
    }

    #[test]
    fn a_choice_is_named_as_a_word_of_its_own() {
        assert!(names_word("a Debug build", "debug"));
        assert!(names_word("debug", "debug"));
        assert!(!names_word("debugging it", "debug"));
        assert!(!names_word("a pre-release", "release"));
        assert!(!names_word("anything", ""));
    }
}
