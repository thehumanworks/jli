use std::collections::HashMap;
use std::fmt;

use anyhow::Result;
use regex::Regex;

use crate::args;
use crate::context::{list_branches, list_entries, Context};
use crate::system2::{extract, name_arguments};
use crate::types::{Slot, SlotKind};
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
/// a value the request states by name or alias, a value the agent already supplied, a quoted string or a
/// number in the request, a System 1 pick among real candidates, then the agent, and the slot's
/// default only as a last resort.
pub fn fill_slots(
    models: &Models,
    query: &str,
    skill: &Template,
    context: &Context,
    provided: &HashMap<String, String>,
) -> Result<Filled> {
    let reading = args::read(query, skill.slots);
    let mut filled = Filled { phrase: reading.phrase, ..Filled::default() };
    let mut pending: Vec<&Slot> = Vec::new();
    let slots = in_template_order(skill);

    for (name, value) in reading.named {
        filled.set(&name, value, Source::Request);
    }
    // A name the skill does not know may be another name for one of its slots. The agent says
    // which; the value stays the user's own. Without an answer the argument blocks the run below.
    let mut unmatched = reading.unmatched;
    let unnamed: Vec<&Slot> = slots.iter().copied().filter(|slot| !filled.has(&slot.name)).collect();
    if !unmatched.is_empty() && !unnamed.is_empty() {
        if let Ok((names, ms)) = name_arguments(&models.llm, skill.steps, skill.scenario, &unnamed, &unmatched) {
            filled.ms_system2 += ms;
            for (key, name) in names {
                let Some(slot) = unnamed.iter().find(|slot| slot.name == name && !filled.has(&slot.name)) else { continue };
                let Some((_, value)) = unmatched.iter().find(|(stated, _)| *stated == key).cloned() else { continue };
                let value = if slot.kind == SlotKind::Number { args::as_number(&value) } else { value };
                filled.set(&slot.name, value, Source::Request);
                filled.aliases.push((slot.name.clone(), key.clone()));
                unmatched.retain(|(stated, _)| *stated != key);
            }
        }
    }
    filled.unmatched = unmatched.into_iter().map(|(key, _)| key).collect();
    for slot in &slots {
        if filled.has(&slot.name) {
            continue;
        }
        if let Some(given) = provided.get(&slot.name).filter(|given| !given.is_empty()) {
            filled.set(&slot.name, given.clone(), Source::Agent);
        }
    }
    // Values with no name go to slots in template order. Quoted strings are only handed out when
    // there is one for every text slot still open: with fewer, which slot each belongs to is a
    // judgement, and the agent that must write the others makes it.
    let open = |filled: &Filled, kind: SlotKind| -> Vec<&Slot> {
        slots.iter().copied().filter(|slot| slot.kind == kind && !filled.has(&slot.name)).collect()
    };
    let text = open(&filled, SlotKind::Text);
    if reading.quoted.len() >= text.len() {
        for (slot, value) in text.into_iter().zip(reading.quoted) {
            filled.set(&slot.name, value, Source::Pattern);
        }
    }
    for (slot, value) in open(&filled, SlotKind::Number).into_iter().zip(reading.numbers) {
        filled.set(&slot.name, value, Source::Pattern);
    }

    // An argument the skill does not take stops the run, so no model is asked for the rest.
    if !filled.unmatched.is_empty() {
        expand_files(&mut filled, skill.slots, context);
        return Ok(filled);
    }

    for slot in slots {
        if filled.has(&slot.name) {
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
    let for_agent: Vec<&Slot> =
        pending.iter().copied().filter(|slot| slot.kind == SlotKind::Text || slot.default.is_none()).collect();
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
            Some(default) => filled.set(&slot.name, default.clone(), Source::Default),
            None => filled.missing.push(slot.name.clone()),
        }
    }
    // After expansion, so the values are those of the command that is logged, which `jli fix` reads back.
    expand_files(&mut filled, skill.slots, context);
    // Learn the shape of the request, not the instance: a value found in it by any means is hidden too.
    filled.phrase = shape(&filled.phrase, skill.steps, &render(skill.steps, &filled.values).join(" && "));
    Ok(filled)
}

/// The phrase that is learned for a request that ran `command` with these steps: `phrase`, which
/// already hides what the request stated, with every value the command was given hidden as well.
/// The values are read back from the command, so `jli fix` arrives at the same text from the event log.
fn shape(phrase: &str, steps: &[String], command: &str) -> String {
    match match_template(steps, command) {
        Some(values) => args::hide_values(phrase, values.values().map(String::as_str)),
        None => phrase.to_string(),
    }
}

/// What a logged request was learned as, worked out again from the request and the command it ran.
/// With no command, or steps that no longer produce it, only the stated values are hidden.
pub fn learned_phrase(query: &str, steps: &[String], slots: &[Slot], command: Option<&str>) -> String {
    let stated = args::phrase(query, slots);
    match command {
        Some(command) => shape(&stated, steps, command),
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

fn render_with(steps: &[String], values: &HashMap<String, String>, quote: fn(&str) -> String) -> Vec<String> {
    steps
        .iter()
        .map(|step| {
            parts(step)
                .into_iter()
                .map(|part| match part {
                    Part::Text(text) => text.to_string(),
                    Part::Slot(name) => match values.get(name) {
                        Some(value) => quote(value),
                        None => format!("{{{name}}}"),
                    },
                })
                .collect()
        })
        .collect()
}

/// Substitute shell-quoted slot values into the step templates.
pub fn render(steps: &[String], values: &HashMap<String, String>) -> Vec<String> {
    render_with(steps, values, shell_quote)
}

/// The same, with values quoted for fish, for steps handed to a fish shell.
pub fn render_fish(steps: &[String], values: &HashMap<String, String>) -> Vec<String> {
    render_with(steps, values, fish_quote)
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
    let mut expanded = Expanded { value: value.to_string(), not_set: vec![] };
    if !value.contains(['~', '$']) || cwd.join(value).symlink_metadata().is_ok() {
        return expanded;
    }
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
pub fn match_template(steps: &[String], command: &str) -> Option<HashMap<String, String>> {
    captures(steps, command).filter(|values| !values.values().any(|value| is_flag(value)))
}

/// True when `steps` produce `command` only by taking a flag as a slot value, as `date {format}` does
/// for `date -I`.
pub fn slots_a_flag(steps: &[String], command: &str) -> bool {
    captures(steps, command).is_some_and(|values| values.values().any(|value| is_flag(value)))
}

/// The value of each slot that makes `steps` produce `command`, flags included.
fn captures(steps: &[String], command: &str) -> Option<HashMap<String, String>> {
    let joined = steps.join(" && ");
    let mut names = Vec::new();
    let mut pattern = String::from("^");
    for part in parts(&joined) {
        match part {
            Part::Text(text) => pattern.push_str(&literal_pattern(text)),
            Part::Slot(name) => {
                names.push(name);
                pattern.push_str(r#"(\S+|'[^']*'|"[^"]*")"#);
            }
        }
    }
    pattern.push('$');
    let captures = Regex::new(&pattern).ok()?.captures(command.trim())?;
    let mut values = HashMap::new();
    for (index, name) in names.into_iter().enumerate() {
        let value = captures.get(index + 1).map_or("", |found| found.as_str());
        values.insert(name.to_string(), unquote(value).to_string());
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
    proposed: &[String],
    values: &HashMap<String, String>,
) -> Option<HashMap<String, String>> {
    let captured = match_template(existing, &proposed.join(" && "))?;
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
            &values(&[("name", "x"), ("HOME", "no")]),
        );
        assert_eq!(rendered, ["echo ${HOME} x {} {1x} {x}", "find . -exec ls {} \\;"]);
        assert_eq!(render(&steps(&["wc -l {file}"]), &values(&[])), ["wc -l {file}"]);
    }

    #[test]
    fn values_are_shell_quoted_unless_plain() {
        let template = steps(&["git commit -m {message}"]);
        assert_eq!(render(&template, &values(&[("message", "first draft")])), ["git commit -m 'first draft'"]);
        assert_eq!(render(&template, &values(&[("message", "it's")])), [r"git commit -m 'it'\''s'"]);
        assert_eq!(
            render(&template, &values(&[("message", "a/b.c:d=e@f%g+h,i-j_k")])),
            ["git commit -m a/b.c:d=e@f%g+h,i-j_k"]
        );
        assert_eq!(render(&template, &values(&[("message", "")])), ["git commit -m ''"]);
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
            match_proposal(&list, &steps(&["ls -la"]), &values(&[])),
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
        assert_eq!(match_proposal(&log, &renamed, &values(&[("n", "10")])), Some(values(&[("count", "10")])));
        assert_eq!(match_proposal(&log, &renamed, &values(&[])), Some(values(&[])), "a value still to find is left open");
        assert_eq!(match_proposal(&log, &steps(&["git log --oneline -n 5"]), &values(&[])), Some(values(&[("count", "5")])));
        assert_eq!(match_proposal(&log, &steps(&["git log -n {n}"]), &values(&[("n", "10")])), None);

        let commit = steps(&["git commit -m {message}"]);
        let said = values(&[("msg", "it's done")]);
        assert_eq!(
            match_proposal(&commit, &steps(&["git commit -m {msg}"]), &said),
            Some(values(&[("message", "it's done")])),
            "a value moves by name, never through shell quoting"
        );
        let literal = steps(&["git log --oneline -n 10"]);
        assert_eq!(match_proposal(&literal, &steps(&["git log --oneline -n 10"]), &values(&[])), Some(values(&[])));
        assert_eq!(
            match_proposal(&literal, &renamed, &values(&[("n", "10")])),
            None,
            "a literal skill does not stand in for a template, even with the same value"
        );
        let echo = steps(&["echo {text}"]);
        assert_eq!(match_proposal(&echo, &steps(&["echo {a}-{b}"]), &values(&[("a", "x"), ("b", "y")])), None);
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
        let rendered = render(&steps(&["ls {dir}"]), &values(&[("dir", &value)]));
        assert_eq!(rendered, ["ls 'my notes; rm -rf /'"], "a variable's value cannot inject either");
        assert_eq!(render(&steps(&["ls {dir}"]), &values(&[("dir", "/home/ada/notes")])), ["ls /home/ada/notes"]);
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
        assert_eq!(render(&template, &one("=ls")), ["echo '=ls'"], "zsh expands a leading =");
        assert_eq!(render(&template, &one("a=b")), ["echo a=b"]);
        assert_eq!(render_fish(&template, &one(r"a\b")), [r"echo 'a\\b'"]);
        assert_eq!(render_fish(&template, &one("it's")), [r"echo 'it\'s'"]);
        assert_eq!(render_fish(&template, &one(r"end\")), [r"echo 'end\\'"]);
        assert_eq!(render_fish(&template, &one("%self")), ["echo '%self'"]);
        assert_eq!(render_fish(&template, &one("/tmp/a.b")), ["echo /tmp/a.b"]);
    }

    #[test]
    fn paths_in_the_request_are_found() {
        assert_eq!(
            path_words("count the lines of ~/notes/n.txt, and src/main.rs; then ~ and $HOME/x"),
            ["~/notes/n.txt", "src/main.rs", "~", "$HOME/x"]
        );
        assert!(path_words("open https://x.io/a and dir=src/a, it costs $5 or ~5 notes.txt").is_empty());
    }
}
