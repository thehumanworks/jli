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
        return Ok(filled);
    }

    for slot in slots {
        if filled.has(&slot.name) {
            continue;
        }
        if matches!(slot.kind, SlotKind::File | SlotKind::Branch) {
            let options = if slot.kind == SlotKind::File { list_entries(&context.cwd) } else { list_branches(&context.cwd) };
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
    Ok(filled)
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

fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty() && value.chars().all(|ch| ch.is_ascii_alphanumeric() || "_-.,/:=@%+".contains(ch));
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Substitute shell-quoted slot values into the step templates.
pub fn render(steps: &[String], values: &HashMap<String, String>) -> Vec<String> {
    steps
        .iter()
        .map(|step| {
            parts(step)
                .into_iter()
                .map(|part| match part {
                    Part::Text(text) => text.to_string(),
                    Part::Slot(name) => match values.get(name) {
                        Some(value) => shell_quote(value),
                        None => format!("{{{name}}}"),
                    },
                })
                .collect()
        })
        .collect()
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

/// If `command` is what `steps` would produce for some slot values, return those values.
/// Lets a taught command be recognised as a known skill with different values, with no model call.
pub fn match_template(steps: &[String], command: &str) -> Option<HashMap<String, String>> {
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
}
