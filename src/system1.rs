//! System 1, the decision model. The only module that uses `typesafe-jev`.

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use typesafe_jev::{Choice, Client, Config, Content, Error, Noul, Questions, Reply, Score};

use crate::context::Context;
use crate::types::{Risk, Skill};

/// Label for "nothing in the matrix fits". Gives low-fit requests somewhere to land.
pub const NONE: &str = "none-of-these";
/// A choice question accepts at most 255 labels; stay well under and prefilter beyond it.
pub const MAX_MATRIX: usize = 200;

/// Question ids, shared by the request and the lookup of its answers.
const IS_COMMAND_REQUEST: &str = "isCommandRequest";
const INTENT_RISK: &str = "intentRisk";
const SKILL: &str = "skill";
const VALUE: &str = "value";

/// The System 1 client, built on first use so that commands that make no call need no key.
pub struct System1 {
    client: OnceLock<std::result::Result<Client, Error>>,
}

impl Default for System1 {
    fn default() -> Self {
        System1::from_env()
    }
}

impl System1 {
    /// Reads `TYPESAFE_API_KEY` when the first question is asked.
    pub fn from_env() -> System1 {
        System1 { client: OnceLock::new() }
    }

    /// A System 1 that never touches the network: `answer` gets each request body and returns
    /// the response body. For tests and local fakes.
    pub fn fake(answer: impl Fn(&Value) -> Value + Send + Sync + 'static) -> System1 {
        let transport = move |body: &[u8]| -> std::result::Result<Reply, String> {
            let request: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
            Ok(Reply { status: 200, retry_after: None, body: answer(&request).to_string() })
        };
        let client = Client::with_transport(transport, Config { backoff_scale: 0.0, max_retries: 0, ..interactive() });
        let cell = OnceLock::new();
        let _ = cell.set(Ok(client));
        System1 { client: cell }
    }

    fn client(&self) -> Result<&Client> {
        self.client.get_or_init(|| Client::from_env(interactive())).as_ref().map_err(|error| explain(error.clone()))
    }

    /// Requests, tokens and an estimated cost so far, when a client has been made.
    pub fn usage(&self) -> Option<(u64, u64, f64)> {
        let client = self.client.get()?.as_ref().ok()?;
        let usage = client.usage();
        Some((usage.requests(), usage.input_tokens(), usage.cost_usd()))
    }
}

/// The crate's defaults suit batch work. An interactive prompt must not hang on a stalled call:
/// a 10 second timeout per attempt and two retries. See docs/adr/0003-system-1.md.
fn interactive() -> Config {
    Config { timeout: Duration::from_secs(10), max_retries: 2, ..Config::default() }
}

fn explain(error: Error) -> anyhow::Error {
    match error {
        Error::Auth(message) if message.contains(typesafe_jev::API_KEY_VAR) => anyhow!("System 1: {message}"),
        Error::Auth(message) => anyhow!("System 1: {message}. Check {}.", typesafe_jev::API_KEY_VAR),
        Error::TokenLimit(message) => anyhow!("System 1: the request is too large for the model: {message}"),
        Error::InvalidRequest(message) => anyhow!("System 1 refused the request: {message}"),
        other => anyhow!("System 1: {other}"),
    }
}

fn missing(id: &str) -> anyhow::Error {
    anyhow!("System 1 returned no answer to `{id}`")
}

fn content(value: Value) -> Content {
    Content::try_from(value).expect("criteria are JSON objects, arrays or strings")
}

/// The static half of the matrix: whether a command is wanted at all.
fn is_command_request() -> Noul {
    Noul::new(content(json!([
        "The user has asked explicitly to run a command",
        "The user's request can only be fulfilled if a command is ran",
        "The user has explicitly typed a command",
    ])))
    .no(content(json!({
        "intent": "No command execution was requested, explicitly or implicitly",
        "examples": ["How much is 2 + 2", "Help me understand the text I will paste below:"],
    })))
    .yes(content(json!({
        "intent": "The user's intent can only be fulfilled by running one or more commands, whether by explicit request or implicitly inferred",
        "examples": [
            "Edit the file file.txt",
            "What changes are unstaged in this repository?",
            "Run 'ls -la'",
            "Commit my work with a meaningful, yet short, message",
            "Where am I?",
            "Which files are in here?",
        ],
    })))
}

/// The static half of the matrix: how much the request could change or destroy.
fn intent_risk() -> Score {
    Score::new(
        "How much could fulfilling this request change or destroy?",
        [
            "Read-only: it only inspects or displays information",
            "It creates or modifies local files or state, and can be undone",
            "It deletes data, rewrites history, or changes remote systems, and is hard to undo",
        ],
    )
}

/// The criterion a skill row renders to.
fn criterion(skill: &Skill) -> Map<String, Value> {
    let mut entry = Map::new();
    entry.insert("runs".into(), skill.steps.join(" && ").into());
    entry.insert("scenario".into(), skill.scenario.clone().into());
    if !skill.examples.is_empty() {
        entry.insert("examples".into(), json!(skill.examples));
    }
    if !skill.counter_examples.is_empty() {
        entry.insert("doesNotApplyTo".into(), json!(skill.counter_examples));
    }
    entry
}

/// The learned half: database rows rendered into choice criteria.
/// Learning is nothing more than these descriptions getting richer.
pub fn build_matrix<'a>(skills: impl IntoIterator<Item = &'a Skill>) -> Choice {
    let mut matrix = Choice::labels("Which known command fulfils the user's request?", std::iter::empty::<String>());
    for skill in skills {
        matrix = matrix.option(skill.label.clone(), Content::Object(criterion(skill)));
    }
    let mut none = Map::new();
    none.insert("scenario".into(), "None of the listed commands does what the user asked".into());
    matrix.option(NONE, Content::Object(none))
}

fn words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|ch: char| !(ch.is_ascii_lowercase() || ch.is_ascii_digit()))
        .filter(|word| !word.is_empty())
        .map(String::from)
        .collect()
}

/// Keep the matrix under the label cap: most lexically relevant rows first, then most used.
pub fn shortlist<'a>(skills: &'a [Skill], query: &str, limit: usize) -> Vec<&'a Skill> {
    if skills.len() <= limit {
        return skills.iter().collect();
    }
    let wanted = words(query);
    let relevance = |skill: &Skill| -> f64 {
        let mut text = vec![skill.label.as_str(), skill.scenario.as_str()];
        text.extend(skill.steps.iter().map(String::as_str));
        text.extend(skill.examples.iter().map(String::as_str));
        let have = words(&text.join(" "));
        let hits = wanted.iter().filter(|word| have.contains(*word)).count();
        hits as f64 * 10.0 + (skill.uses as f64).ln_1p()
    };
    let mut ranked: Vec<(f64, &Skill)> = skills.iter().map(|skill| (relevance(skill), skill)).collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked.into_iter().take(limit).map(|(_, skill)| skill).collect()
}

/// Read the risk rubric pessimistically. The expected score would average a
/// "safe or destructive" split into "modifies"; the distribution does not.
pub fn read_risk(probabilities: &BTreeMap<u32, f64>) -> Risk {
    let p = |level: u32| probabilities.get(&level).copied().unwrap_or(0.0);
    if p(2) >= 0.3 {
        Risk::Destructive
    } else if p(1) + p(2) >= 0.5 {
        Risk::Modifies
    } else {
        Risk::Safe
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub label: String,
    pub probability: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// The matched skill, or None when the model chose "none of these".
    pub skill: Option<Skill>,
    /// Reported confidence in the chosen label, 0 to 1.
    pub confidence: f64,
    /// Probability that the request needs a command at all.
    pub is_command: f64,
    /// Risk the model reads from the request itself.
    pub intent_risk: Risk,
    /// Labels by probability, highest first.
    pub candidates: Vec<Candidate>,
    pub matrix_size: usize,
    pub ms: u64,
    pub model: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Picked {
    pub value: Option<String>,
    pub confidence: f64,
    pub ms: u64,
}

impl System1 {
    /// One System 1 call: gate, risk and routing answered together against the same state.
    pub fn decide(&self, query: &str, context: &Context, skills: &[Skill]) -> Result<Decision> {
        let rows = shortlist(skills, query, MAX_MATRIX);
        let state = json!({
            "request": query,
            "context": {
                "directory": context.directory,
                "insideGitRepository": context.inside_git_repository,
                "platform": context.platform,
            },
        });
        let questions = Questions::new()
            .with(IS_COMMAND_REQUEST, is_command_request())
            .with(INTENT_RISK, intent_risk())
            .with(SKILL, build_matrix(rows.iter().copied()));
        let client = self.client()?;
        let started = Instant::now();
        let response = client.ask(&state, &questions).map_err(explain)?;
        let ms = started.elapsed().as_millis() as u64;

        let gate = response.noul(IS_COMMAND_REQUEST).ok_or_else(|| missing(IS_COMMAND_REQUEST))?;
        let risk = response.score(INTENT_RISK).ok_or_else(|| missing(INTENT_RISK))?;
        let skill = response.choice(SKILL).ok_or_else(|| missing(SKILL))?;
        let mut candidates: Vec<Candidate> = skill
            .probabilities
            .iter()
            .map(|(label, probability)| Candidate { label: label.clone(), probability: *probability })
            .collect();
        candidates.sort_by(|a, b| b.probability.total_cmp(&a.probability));

        Ok(Decision {
            skill: rows.iter().find(|row| row.label == skill.choice).map(|row| (*row).clone()),
            confidence: skill.confidence,
            is_command: gate.noul,
            intent_risk: read_risk(&risk.probabilities),
            candidates,
            matrix_size: rows.len(),
            ms,
            model: response.model.clone(),
            tokens: response.usage.input_tokens.unwrap_or(0) + response.usage.output_tokens.unwrap_or(0),
        })
    }

    /// Fill a slot by letting System 1 pick among concrete candidates (files, branches).
    pub fn pick(&self, query: &str, slot_description: &str, options: &[String]) -> Result<Picked> {
        let question = Choice::labels(
            format!("Which option is {slot_description}, as referred to in the request?"),
            options.iter().take(MAX_MATRIX),
        )
        .option(NONE, "The request does not refer to any of the listed options");
        let client = self.client()?;
        let started = Instant::now();
        let response = client.ask(&json!({ "request": query }), &Questions::new().with(VALUE, question)).map_err(explain)?;
        let ms = started.elapsed().as_millis() as u64;
        let answer = response.choice(VALUE).ok_or_else(|| missing(VALUE))?;
        let value = (answer.choice != NONE).then(|| answer.choice.clone());
        Ok(Picked { value, confidence: answer.confidence, ms })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Origin;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    pub(crate) fn skill(label: &str, step: &str) -> Skill {
        Skill {
            label: label.into(),
            steps: vec![step.into()],
            scenario: format!("the user wants {label}"),
            examples: vec![],
            counter_examples: vec![],
            slots: vec![],
            risk: Risk::Safe,
            origin: Origin::Seed,
            uses: 0,
            created_at: String::new(),
            last_used_at: None,
            created_host: String::new(),
        }
    }

    fn context() -> Context {
        Context { cwd: PathBuf::from("/tmp/jli"), directory: "jli".into(), inside_git_repository: true, platform: "darwin" }
    }

    #[test]
    fn the_request_matches_the_wire_format() {
        let seen = Arc::new(Mutex::new(Value::Null));
        let recorder = seen.clone();
        let system1 = System1::fake(move |request| {
            *recorder.lock().unwrap() = request.clone();
            json!({
                "model": "jev-1.13.0",
                "answers": {
                    "isCommandRequest": {"type": "noul", "noul": 0.66},
                    "intentRisk": {"type": "score", "score": 0.1, "confidence": 0.9,
                                   "legend": {"0": "a", "1": "b", "2": "c"},
                                   "probabilities": {"0": 0.9, "1": 0.1, "2": 0.0}},
                    "skill": {"type": "choice", "choice": "list-files-by-time", "confidence": 0.97,
                              "probabilities": {"list-files": 0.01, "list-files-by-time": 0.98, "none-of-these": 0.02}}
                },
                "usage": {"input_tokens": 100, "output_tokens": 3}
            })
        });
        let mut by_time = skill("list-files-by-time", "ls -ltr");
        by_time.examples = vec!["newest files".into()];
        by_time.counter_examples = vec!["biggest files".into()];
        let skills = vec![skill("list-files", "ls"), by_time];
        let decision = system1.decide("show me the newest files", &context(), &skills).unwrap();

        assert_eq!(decision.skill.as_ref().map(|s| s.label.as_str()), Some("list-files-by-time"));
        assert_eq!((decision.confidence, decision.is_command, decision.intent_risk), (0.97, 0.66, Risk::Safe));
        assert_eq!(decision.candidates[0].label, "list-files-by-time");
        assert_eq!(decision.candidates[1].label, NONE);
        assert_eq!((decision.matrix_size, decision.tokens, decision.model.as_str()), (2, 103, "jev-1.13.0"));

        let request = seen.lock().unwrap().clone();
        assert_eq!(request["model"], "jev-latest");
        assert_eq!(
            request["state"],
            json!({"request": "show me the newest files",
                   "context": {"directory": "jli", "insideGitRepository": true, "platform": "darwin"}})
        );
        let ids: Vec<_> = request["questions"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(ids, ["isCommandRequest", "intentRisk", "skill"]);
        assert_eq!(request["questions"]["isCommandRequest"]["type"], "noul");
        assert_eq!(request["questions"]["isCommandRequest"]["instructions"].as_array().unwrap().len(), 3);
        assert_eq!(request["questions"]["intentRisk"]["criteria"].as_array().unwrap().len(), 3);
        let criteria = &request["questions"]["skill"]["criteria"];
        let labels: Vec<_> = criteria.as_object().unwrap().keys().cloned().collect();
        assert_eq!(labels, ["list-files", "list-files-by-time", NONE]);
        assert_eq!(
            criteria["list-files-by-time"],
            json!({"runs": "ls -ltr", "scenario": "the user wants list-files-by-time",
                   "examples": ["newest files"], "doesNotApplyTo": ["biggest files"]})
        );
        let keys: Vec<_> = criteria["list-files-by-time"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["runs", "scenario", "examples", "doesNotApplyTo"], "key order is kept");
        assert_eq!(criteria["list-files"], json!({"runs": "ls", "scenario": "the user wants list-files"}));
        assert_eq!(criteria[NONE], json!({"scenario": "None of the listed commands does what the user asked"}));
    }

    #[test]
    fn a_missing_answer_is_an_error() {
        let system1 = System1::fake(|_| json!({"model": "m", "answers": {}}));
        let error = system1.decide("x", &context(), &[]).unwrap_err();
        assert!(error.to_string().contains("isCommandRequest"), "{error}");
    }

    #[test]
    fn picks_among_options_and_maps_none() {
        let system1 = System1::fake(|request| {
            let criteria = &request["questions"]["value"]["criteria"];
            assert_eq!(criteria["notes.txt"], Value::Null);
            assert_eq!(criteria[NONE], "The request does not refer to any of the listed options");
            assert_eq!(request["state"], json!({"request": "count lines of notes"}));
            json!({"model": "m", "answers": {"value": {"type": "choice", "choice": NONE, "confidence": 0.8,
                                                      "probabilities": {NONE: 0.8}}}})
        });
        let picked = system1.pick("count lines of notes", "the file", &["notes.txt".into(), "todo.md".into()]).unwrap();
        assert_eq!((picked.value, picked.confidence), (None, 0.8));
    }

    #[test]
    fn risk_is_read_from_the_distribution() {
        let risk = |p0: f64, p1: f64, p2: f64| read_risk(&BTreeMap::from([(0, p0), (1, p1), (2, p2)]));
        assert_eq!(risk(0.6, 0.0, 0.4), Risk::Destructive, "a split is not averaged into modifies");
        assert_eq!(risk(0.5, 0.3, 0.2), Risk::Modifies);
        assert_eq!(risk(0.9, 0.1, 0.0), Risk::Safe);
        assert_eq!(read_risk(&BTreeMap::new()), Risk::Safe);
    }

    #[test]
    fn the_shortlist_prefers_overlap_then_use() {
        let mut skills: Vec<Skill> = (0..5).map(|n| skill(&format!("skill-{n}"), "true")).collect();
        skills[3].scenario = "show the newest files".into();
        skills[1].uses = 50;
        let picked: Vec<_> = shortlist(&skills, "newest files please", 2).iter().map(|s| s.label.clone()).collect();
        assert_eq!(picked, ["skill-3", "skill-1"]);
        assert_eq!(shortlist(&skills, "x", 10).len(), 5);
    }
}
