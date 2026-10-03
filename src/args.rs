//! Arguments the request states outright, read without any model. See docs/adr/0011-stated-arguments.md.
//!
//! Three forms are recognised:
//!   `name=value` or `name="some words"`   an argument for the slot called `name`
//!   `name: "some words"` or `name: rest`  the same in plainer words, once the skill is known
//!   `"some words"`                        a value with no name, given to text slots in order
//!
//! A slot answers to its name and to its aliases, so `dir=src` reaches `{directory}` once `dir` is
//! known as another name for it. See docs/adr/0012-slot-aliases.md.
//!
//! Stated values are hidden from routing: System 1 sees `commit message=…`, so the words of a
//! commit message cannot change which command is picked or how risky it looks.

use std::ops::Range;

use crate::types::{Slot, SlotKind};

/// What replaces a stated value in the text that is routed and learned.
pub const HIDDEN: &str = "…";

const NUMBER_WORDS: [(&str, &str); 10] = [
    ("one", "1"),
    ("two", "2"),
    ("three", "3"),
    ("four", "4"),
    ("five", "5"),
    ("six", "6"),
    ("seven", "7"),
    ("eight", "8"),
    ("nine", "9"),
    ("ten", "10"),
];

/// One value found in the request: `key=value`, or a quoted string with no key.
#[derive(Debug, Clone, PartialEq)]
struct Stated {
    key: Option<String>,
    value: String,
    /// The whole item, and the part of it that holds the value.
    span: Range<usize>,
    hide: Range<usize>,
}

fn closing(open: char) -> Option<char> {
    match open {
        '"' | '\'' | '`' => Some(open),
        '“' => Some('”'),
        _ => None,
    }
}

/// A quoted run that opens at `at`: the range between the quotes and where the run ends.
/// A closing quote followed by a letter or digit is an apostrophe, as in `'it's done'`.
fn quoted_at(text: &str, at: usize) -> Option<(Range<usize>, usize)> {
    let open = text.get(at..)?.chars().next()?;
    let close = closing(open)?;
    let start = at + open.len_utf8();
    let mut search = start;
    while let Some(found) = text[search..].find(close) {
        let end = search + found;
        let after = end + close.len_utf8();
        if end == start {
            return None;
        }
        if !text[after..].chars().next().is_some_and(char::is_alphanumeric) {
            return Some((start..end, after));
        }
        search = after;
    }
    None
}

/// Where the key of a `key=value` ends, when one starts at `at`.
fn key_at(text: &str, at: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if !(bytes.get(at)?.is_ascii_alphabetic() || bytes[at] == b'_') {
        return None;
    }
    let mut end = at + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'-') {
        end += 1;
    }
    (bytes.get(end) == Some(&b'=') && bytes.get(end + 1) != Some(&b'=')).then_some(end)
}

fn scan(text: &str) -> Vec<Stated> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut word_start = true;
    while let Some(ch) = text[at..].chars().next() {
        if word_start {
            if let Some((inner, end)) = quoted_at(text, at) {
                out.push(Stated { key: None, value: text[inner.clone()].to_string(), span: at..end, hide: inner });
                at = end;
                word_start = false;
                continue;
            }
            if let Some(key_end) = key_at(text, at) {
                let value_at = key_end + 1;
                let (hide, end) = quoted_at(text, value_at).unwrap_or_else(|| {
                    let end = text[value_at..].find(char::is_whitespace).map_or(text.len(), |n| value_at + n);
                    (value_at..end, end)
                });
                if !hide.is_empty() {
                    let key = Some(text[at..key_end].to_string());
                    out.push(Stated { key, value: text[hide.clone()].to_string(), span: at..end, hide });
                }
                at = end;
                word_start = false;
                continue;
            }
        }
        word_start = ch.is_whitespace() || ch == ':' || ch == '(';
        at += ch.len_utf8();
    }
    out
}

/// An alias shorter than this is only read as `key=value`. As a label, `d:` or `n "…"` is too
/// easily ordinary prose.
const SHORTEST_LABEL: usize = 3;

pub(crate) fn as_number(value: &str) -> String {
    let lower = value.to_lowercase();
    NUMBER_WORDS.iter().find(|(word, _)| *word == lower).map_or_else(|| value.to_string(), |(_, digits)| digits.to_string())
}

/// Whether the characters, read away from a word, start with a `-` and then another word.
fn hyphen_then_word(mut side: impl Iterator<Item = char>) -> bool {
    side.next() == Some('-') && side.next().is_some_and(char::is_alphanumeric)
}

/// Every number in the text, in order: digits standing alone, or a word from one to ten that is
/// not joined by `-` to another word. The "one" of "one-line" is part of an adjective, not a count.
fn numbers_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut at = 0;
    for word in text.split(|ch: char| !ch.is_alphanumeric()) {
        let (start, end) = (at, at + word.len());
        at = end + text[end..].chars().next().map_or(0, char::len_utf8);
        if word.is_empty() {
            continue;
        }
        if word.bytes().all(|byte| byte.is_ascii_digit()) {
            found.push(word.to_string());
            continue;
        }
        if hyphen_then_word(text[..start].chars().rev()) || hyphen_then_word(text[end..].chars()) {
            continue;
        }
        let lower = word.to_lowercase();
        if let Some((_, value)) = NUMBER_WORDS.iter().find(|(name, _)| *name == lower) {
            found.push(value.to_string());
        }
    }
    found
}

/// A slot's name or alias used as a label in the request: `message: …` or `message "…"`.
struct Label {
    slot: usize,
    start: usize,
    /// Where the value starts, after the colon and any spaces.
    value_at: usize,
    colon: bool,
}

fn labels(text: &str, items: &[Stated], slots: &[Slot], skip: &[bool]) -> Vec<Label> {
    let lower = text.to_ascii_lowercase();
    let inside = |at: usize| items.iter().any(|item| item.span.contains(&at));
    let mut found = Vec::new();
    for (slot, wanted) in slots.iter().enumerate() {
        if skip[slot] {
            continue;
        }
        let aliases = wanted.aliases.iter().filter(|alias| alias.len() >= SHORTEST_LABEL);
        // The slot's label is the first place any of its names is used as one.
        let mut first: Option<Label> = None;
        for name in std::iter::once(&wanted.name).chain(aliases).filter(|name| !name.is_empty()) {
            let name = name.to_ascii_lowercase();
            let mut from = 0;
            while let Some(offset) = lower[from..].find(&name) {
                let start = from + offset;
                let end = start + name.len();
                from = end;
                let boundary = !lower[..start].chars().next_back().is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
                if !boundary || inside(start) {
                    continue;
                }
                let rest = &text[end..];
                let after_name = rest.trim_start();
                let colon = after_name.starts_with(':');
                let value = if colon { after_name[1..].trim_start() } else { after_name };
                let value_at = text.len() - value.len();
                // Without a colon, only a quoted string right after the name counts.
                let quoted_next = items.iter().any(|item| item.key.is_none() && item.span.start == value_at);
                if colon || (quoted_next && value_at > end) {
                    if first.as_ref().is_none_or(|label| start < label.start) {
                        first = Some(Label { slot, start, value_at, colon });
                    }
                    break;
                }
            }
        }
        found.extend(first);
    }
    found.sort_by_key(|label| label.start);
    found
}

/// What the request states for a skill's slots.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    /// Values stated by name, as (slot name, value).
    pub named: Vec<(String, String)>,
    /// Quoted strings that carry no name, in order.
    pub quoted: Vec<String>,
    /// Numbers outside any stated value, in order.
    pub numbers: Vec<String>,
    /// `key=value` arguments whose key is no slot's name or alias, as (key, value).
    pub unmatched: Vec<(String, String)>,
    /// The request with every stated value hidden. This is what gets routed and learned.
    pub phrase: String,
}

/// Read the request against a skill's slots. With no slots, only the phrase and the raw
/// arguments are of use: that is how a request is read before a skill has been chosen.
pub fn read(query: &str, slots: &[Slot]) -> Reading {
    let items = scan(query);
    let mut reading = Reading::default();
    let mut value_of: Vec<Option<String>> = vec![None; slots.len()];
    let mut used = vec![false; items.len()];
    let mut hidden: Vec<Range<usize>> = items.iter().map(|item| item.hide.clone()).collect();
    let mut blanked: Vec<Range<usize>> = items.iter().map(|item| item.span.clone()).collect();

    for (index, item) in items.iter().enumerate() {
        let Some(key) = &item.key else { continue };
        used[index] = true;
        match slots.iter().position(|slot| slot.answers_to(key)) {
            Some(slot) if value_of[slot].is_none() => value_of[slot] = Some(item.value.clone()),
            Some(_) => {}
            None => reading.unmatched.push((key.clone(), item.value.clone())),
        }
    }

    let skip: Vec<bool> = value_of.iter().map(Option::is_some).collect();
    let found = labels(query, &items, slots, &skip);
    for (position, label) in found.iter().enumerate() {
        if let Some(index) = items.iter().position(|item| item.key.is_none() && item.span.start == label.value_at) {
            value_of[label.slot] = Some(items[index].value.clone());
            used[index] = true;
            continue;
        }
        if !label.colon {
            continue;
        }
        // Unquoted: a text slot takes the rest of the request, up to the next stated value.
        // Any other kind takes one word.
        let next_label = found.get(position + 1).map(|next| next.start);
        let next_item = items.iter().map(|item| item.span.start).find(|start| *start >= label.value_at);
        let mut end = [next_label, next_item].into_iter().flatten().min().unwrap_or(query.len());
        if slots[label.slot].kind != SlotKind::Text {
            end = query[label.value_at..end].find(char::is_whitespace).map_or(end, |n| label.value_at + n);
        }
        let raw = &query[label.value_at..end];
        let value = raw.trim_end();
        if !value.is_empty() {
            let span = label.value_at..label.value_at + value.len();
            value_of[label.slot] = Some(value.to_string());
            hidden.push(span.clone());
            blanked.push(span);
        }
    }

    for (slot, value) in slots.iter().zip(value_of) {
        if let Some(value) = value {
            let value = if slot.kind == SlotKind::Number { as_number(&value) } else { value };
            reading.named.push((slot.name.clone(), value));
        }
    }
    reading.quoted = items.iter().zip(&used).filter(|(_, used)| !**used).map(|(item, _)| item.value.clone()).collect();

    let outside = replace(query, &blanked, " ");
    reading.numbers = numbers_in(&outside);
    // A request that is nothing but a stated value is routed as it is.
    reading.phrase =
        if outside.chars().any(char::is_alphanumeric) { replace(query, &hidden, HIDDEN) } else { query.to_string() };
    reading
}

/// The request as it is routed and learned: stated values hidden.
pub fn phrase(query: &str, slots: &[Slot]) -> String {
    read(query, slots).phrase
}

/// Part of a word, for finding a value as a whole word: `src` is not hidden inside `src/main.rs`.
fn in_word(ch: char) -> bool {
    ch.is_alphanumeric() || "_-/.".contains(ch)
}

/// Whether a value is worth hiding wherever it appears: a number, or something with two
/// characters or more that has a letter or digit in it. `.` and `a` are not.
fn hideable(value: &str) -> bool {
    let digits = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    digits || (value.chars().count() >= 2 && value.chars().any(char::is_alphanumeric))
}

/// The phrase with each value hidden wherever it appears verbatim as a whole word, and a number
/// also where it is written as a word: `notes.txt` and `three` become `…`. A value the request
/// does not contain, such as one the agent wrote, hides nothing. Whatever filled the values, this
/// gives the same text, so `jli fix` can work it out again from the command that ran.
pub fn hide_values<'a>(phrase: &str, values: impl IntoIterator<Item = &'a str>) -> String {
    let mut words: Vec<String> = Vec::new();
    for value in values.into_iter().map(str::trim).filter(|value| hideable(value)) {
        words.push(value.to_string());
        let named = NUMBER_WORDS.iter().filter(|(_, digits)| *digits == value);
        words.extend(named.flat_map(|(word, _)| [word.to_string(), capitalised(word)]));
    }
    // Longest first, so `notes.txt` is hidden whole before `notes` could be.
    words.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for word in &words {
        let mut from = 0;
        while let Some(offset) = phrase[from..].find(word.as_str()) {
            let start = from + offset;
            let end = start + word.len();
            from = end;
            let before = phrase[..start].chars().next_back();
            let mut after = phrase[end..].chars();
            let next = after.next();
            // A full stop ends a sentence unless a word goes on after it.
            let stops = next == Some('.') && !after.next().is_some_and(char::is_alphanumeric);
            let whole = !before.is_some_and(in_word) && (!next.is_some_and(in_word) || stops);
            if whole && !ranges.iter().any(|taken| taken.start < end && start < taken.end) {
                ranges.push(start..end);
            }
        }
    }
    let hidden = replace(phrase, &ranges, HIDDEN);
    // As with stated values, a request that would be nothing but hidden values stays as it is.
    if hidden.chars().any(char::is_alphanumeric) {
        hidden
    } else {
        phrase.to_string()
    }
}

fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// The `name=value` arguments of a request, whatever skill it turns out to be for.
pub fn stated(query: &str) -> Vec<(String, String)> {
    scan(query).into_iter().filter_map(|item| item.key.map(|key| (key, item.value))).collect()
}

fn replace(text: &str, ranges: &[Range<usize>], with: &str) -> String {
    let mut ranges: Vec<&Range<usize>> = ranges.iter().collect();
    ranges.sort_by_key(|range| range.start);
    let mut out = String::new();
    let mut at = 0;
    for range in ranges {
        if range.start < at {
            continue;
        }
        out.push_str(&text[at..range.start]);
        out.push_str(with);
        at = range.end;
    }
    out.push_str(&text[at..]);
    out
}

/// Join command-line words into a request. The shell has already removed the quotes the user
/// typed, so a word with spaces in it must have been quoted: the quotes are put back, which
/// makes `jli commit message="two words"` read the same here as in an interactive session.
pub fn request_from(words: &[&str]) -> String {
    if words.len() == 1 {
        return words[0].to_string();
    }
    let quote = |value: &str| {
        let fits = [('"', '"'), ('\'', '\''), ('`', '`'), ('“', '”')]
            .into_iter()
            .find(|(open, close)| !value.contains(*open) && !value.contains(*close));
        match fits {
            // A value ending in a quote character would confuse the reader; such a word is rare enough to pass through.
            Some((open, close)) => format!("{open}{value}{close}"),
            None => value.to_string(),
        }
    };
    let joined: Vec<String> = words
        .iter()
        .map(|word| {
            if !word.is_empty() && !word.contains(char::is_whitespace) {
                return word.to_string();
            }
            match key_at(word, 0) {
                Some(key_end) => format!("{}={}", &word[..key_end], quote(&word[key_end + 1..])),
                None => quote(word),
            }
        })
        .collect();
    joined.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(name: &str, kind: SlotKind) -> Slot {
        Slot { name: name.into(), kind, description: String::new(), default: None, aliases: vec![] }
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
    }

    #[test]
    fn name_equals_value_is_an_argument() {
        let message = [slot("message", SlotKind::Text)];
        let reading = read("commit message=wip", &message);
        assert_eq!(reading.named, pairs(&[("message", "wip")]));
        assert_eq!(reading.phrase, "commit message=…");

        let reading = read(r#"commit Message="delete the branch, then push" now"#, &message);
        assert_eq!(reading.named, pairs(&[("message", "delete the branch, then push")]));
        assert_eq!(reading.phrase, r#"commit Message="…" now"#);
        assert!(reading.quoted.is_empty(), "a named value is not also a positional one");

        assert_eq!(read("commit message='it's done'", &message).named, pairs(&[("message", "it's done")]));
        assert_eq!(read("commit message=a=b", &message).named, pairs(&[("message", "a=b")]), "split on the first =");
        assert!(read("commit message= now", &message).named.is_empty(), "an empty value is not given");
        assert!(read("commit --message=wip -m=x", &message).named.is_empty(), "a flag is never an argument");
        assert!(read("is a==b true", &message).unmatched.is_empty());
        assert_eq!(read("commit message=one message=two", &message).named, pairs(&[("message", "one")]));
    }

    #[test]
    fn an_argument_for_no_slot_is_reported() {
        let reading = read("commit msg=wip", &[slot("message", SlotKind::Text)]);
        assert!(reading.named.is_empty());
        assert_eq!(reading.unmatched, pairs(&[("msg", "wip")]));
        let dashed =
            read("tag it tag-name=v1 count=three", &[slot("tag_name", SlotKind::Text), slot("count", SlotKind::Number)]);
        assert_eq!(dashed.named, pairs(&[("tag_name", "v1"), ("count", "3")]));
    }

    #[test]
    fn a_slot_name_labels_its_value() {
        let message = [slot("message", SlotKind::Text)];
        let reading = read("Commit this with a message: 'the message'", &message);
        assert_eq!(reading.named, pairs(&[("message", "the message")]));
        assert_eq!(reading.phrase, "Commit this with a message: '…'");

        let reading = read("commit with message: fix the parser bug", &message);
        assert_eq!(reading.named, pairs(&[("message", "fix the parser bug")]));
        assert_eq!(reading.phrase, "commit with message: …");

        assert_eq!(read("commit with the message \"wip\"", &message).named, pairs(&[("message", "wip")]));
        assert!(read("commit with a nice message", &message).named.is_empty());
        assert!(read("commit the messages: all", &message).named.is_empty(), "the name must be the whole word");

        let two = [slot("pattern", SlotKind::Text), slot("replacement", SlotKind::Text), slot("count", SlotKind::Number)];
        let reading = read("swap pattern: old name replacement: new name count: 2 times", &two);
        assert_eq!(reading.named, pairs(&[("pattern", "old name"), ("replacement", "new name"), ("count", "2")]));
        assert_eq!(reading.phrase, "swap pattern: … replacement: … count: … times");
        let mixed = read("swap replacement: 'b' for pattern='a'", &two);
        assert_eq!(mixed.named, pairs(&[("pattern", "a"), ("replacement", "b")]));
    }

    #[test]
    fn a_slot_answers_to_its_aliases() {
        let mut directory = slot("directory", SlotKind::File);
        directory.aliases = vec!["dir".into(), "d".into(), "target_path".into()];
        let slots = [directory, slot("count", SlotKind::Number)];
        for request in ["go dir=src", "go D=src", "go target-path=src", "go directory=src"] {
            let reading = read(request, &slots);
            assert_eq!(reading.named, pairs(&[("directory", "src")]), "{request}");
            assert!(reading.unmatched.is_empty(), "{request}");
        }
        assert_eq!(read("go dir=src", &slots).phrase, "go dir=…");
        assert_eq!(read("go dir=a directory=b", &slots).named, pairs(&[("directory", "a")]), "the first one stated wins");
        assert_eq!(read("go folder=src", &slots).unmatched, pairs(&[("folder", "src")]));

        assert_eq!(read("go to dir: src please", &slots).named, pairs(&[("directory", "src")]));
        assert_eq!(read("go to dir \"my files\"", &slots).named, pairs(&[("directory", "my files")]));
        assert_eq!(
            read("open dir: a, not directory: b", &slots).named,
            pairs(&[("directory", "a,")]),
            "the first label wins"
        );
        assert!(read("plan d: go home", &slots).named.is_empty(), "a short alias is not read as a label");
    }

    #[test]
    fn quoted_strings_and_numbers_are_positional() {
        let reading = read(r#"replace "foo" with “bar baz” in the 3 newest files, don't ask"#, &[]);
        assert_eq!(reading.quoted, ["foo", "bar baz"]);
        assert_eq!(reading.numbers, ["3"]);
        assert_eq!(reading.phrase, r#"replace "…" with “…” in the 3 newest files, don't ask"#);

        let reading = read("commit \"fix 3 bugs\" and show the last two commits", &[]);
        assert_eq!(reading.numbers, ["2"], "a number inside a stated value is not a number argument");
        assert_eq!(read("say `hi` or 'yo'", &[]).quoted, ["hi", "yo"]);
        assert_eq!(read("abc123 and v2", &[]).numbers, Vec::<String>::new());
        assert_eq!(read("an unclosed \"quote", &[]).quoted, Vec::<String>::new());
    }

    #[test]
    fn a_number_word_joined_by_a_hyphen_is_not_a_number() {
        // Issue #14: "one" in "one-line" filled the count ahead of 4.
        assert_eq!(read("one-line log of the newest 4 commits", &[]).numbers, ["4"]);
        assert_eq!(read("show a two-column, three-way diff", &[]).numbers, Vec::<String>::new());
        assert_eq!(read("twenty-one files", &[]).numbers, Vec::<String>::new());
        assert_eq!(read("one line log of the newest four commits", &[]).numbers, ["1", "4"]);
        assert_eq!(read("- one commit, then two - three", &[]).numbers, ["1", "2", "3"], "a dash alone joins nothing");
        assert_eq!(read("the newest five: one-liners", &[]).numbers, ["5"]);
        // Digits are left as they were: a date or a range still gives its numbers.
        assert_eq!(read("the last 5-10 commits", &[]).numbers, ["5", "10"]);
    }

    #[test]
    fn a_request_that_is_only_a_value_is_routed_whole() {
        assert_eq!(phrase("\"show me the files\"", &[]), "\"show me the files\"");
        assert_eq!(phrase("commit \"wip\"", &[]), "commit \"…\"");
        assert_eq!(stated("deploy env=prod region=\"eu west\" 'x'"), pairs(&[("env", "prod"), ("region", "eu west")]));
    }

    #[test]
    fn a_value_is_hidden_wherever_it_is_a_whole_word() {
        let hide = |phrase: &str, values: &[&str]| hide_values(phrase, values.iter().copied());
        assert_eq!(hide("how many lines are in notes.txt", &["notes.txt"]), "how many lines are in …");
        assert_eq!(hide("count the lines of todo.md.", &["todo.md"]), "count the lines of ….", "a full stop ends it");
        assert_eq!(hide("show the last 3 commits", &["3"]), "show the last … commits");
        assert_eq!(hide("show the last three commits", &["3"]), "show the last … commits", "a number written as a word");
        assert_eq!(hide("Three commits please", &["3"]), "… commits please");
        assert_eq!(hide("push main to origin", &["main"]), "push … to origin");

        assert_eq!(hide("list src/main.rs and src2", &["src"]), "list src/main.rs and src2", "never inside a word");
        assert_eq!(hide("open .gitignore", &["gitignore"]), "open .gitignore");
        assert_eq!(hide("show 13 commits", &["3"]), "show 13 commits");
        assert_eq!(hide("size of this folder", &["."]), "size of this folder", "no letter or digit: not hidden");
        assert_eq!(hide("make a branch", &["a"]), "make a branch", "one letter is not hidden");
        assert_eq!(hide("commit my work", &["fix the parser"]), "commit my work", "a value the agent wrote hides nothing");
        assert_eq!(hide("notes.txt", &["notes.txt"]), "notes.txt", "a request that is only a value stays whole");
        assert_eq!(hide("cat notes and notes.txt", &["notes", "notes.txt"]), "cat … and …");
        assert_eq!(hide("commit message=\"…\" about wip", &["wip"]), "commit message=\"…\" about …");
    }

    #[test]
    fn command_line_words_get_their_quotes_back() {
        assert_eq!(
            request_from(&["commit", "this", "with", "a", "message:", "the message"]),
            "commit this with a message: \"the message\""
        );
        assert_eq!(request_from(&["commit", "message=fix the bug"]), "commit message=\"fix the bug\"");
        assert_eq!(request_from(&["commit", "message=say \"hi\" now"]), "commit message='say \"hi\" now'");
        assert_eq!(request_from(&["commit", "message=wip"]), "commit message=wip");
        assert_eq!(
            request_from(&["commit with message 'x y'"]),
            "commit with message 'x y'",
            "one word is the whole request"
        );
        assert_eq!(request_from(&["echo", ""]), "echo \"\"");
        let typed = "commit message=\"fix the bug\"";
        let message = [slot("message", SlotKind::Text)];
        assert_eq!(read(&request_from(&["commit", "message=fix the bug"]), &message), read(typed, &message));
    }
}
