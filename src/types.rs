use serde::{Deserialize, Serialize};
use std::fmt;

/// How much damage a command can do. Drives the confirmation policy.
/// Ordered from least to most severe, so `max` is the more severe of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Safe,
    Modifies,
    Destructive,
}

impl Risk {
    pub const ALL: [Risk; 3] = [Risk::Safe, Risk::Modifies, Risk::Destructive];

    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Safe => "safe",
            Risk::Modifies => "modifies",
            Risk::Destructive => "destructive",
        }
    }

    pub fn parse(text: &str) -> Option<Risk> {
        Risk::ALL.into_iter().find(|risk| risk.as_str() == text)
    }
}

impl fmt::Display for Risk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a `{slot}` in a command template gets its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotKind {
    File,
    Branch,
    Number,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub name: String,
    pub kind: SlotKind,
    pub description: String,
    /// Used when nothing can be extracted from the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Other names the request may use for this slot: `dir=` for `{directory}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// How many other names one slot may collect.
pub const MAX_ALIASES: usize = 8;

/// Slot names and argument keys compare without case, and `-` is the same as `_`.
pub fn normal_name(name: &str) -> String {
    name.to_ascii_lowercase().replace('-', "_")
}

impl Slot {
    /// Whether an argument key means this slot: its name or one of its aliases.
    pub fn answers_to(&self, key: &str) -> bool {
        let key = normal_name(key);
        normal_name(&self.name) == key || self.aliases.iter().any(|alias| normal_name(alias) == key)
    }
}

/// Give the slot called `slot` another name. Refused when the alias could not be typed as a key,
/// or when it already means a slot of this skill.
pub fn add_alias(slots: &mut [Slot], slot: &str, alias: &str) -> bool {
    let mut letters = alias.trim().bytes();
    let typable = letters.next().is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && letters.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    let alias = normal_name(alias.trim());
    if !typable || slots.iter().any(|known| known.answers_to(&alias)) {
        return false;
    }
    match slots.iter_mut().find(|known| known.name == slot) {
        Some(known) if known.aliases.len() < MAX_ALIASES => {
            known.aliases.push(alias);
            true
        }
        _ => false,
    }
}

/// Slots as a model wrote them, with only the aliases that hold up: the first slot to claim a name keeps it.
pub fn with_checked_aliases(mut slots: Vec<Slot>) -> Vec<Slot> {
    let claimed: Vec<(String, Vec<String>)> =
        slots.iter_mut().map(|slot| (slot.name.clone(), std::mem::take(&mut slot.aliases))).collect();
    for (slot, aliases) in claimed {
        for alias in aliases {
            add_alias(&mut slots, &slot, &alias);
        }
    }
    slots
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    Seed,
    Agent,
    Taught,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Seed => "seed",
            Origin::Agent => "agent",
            Origin::Taught => "taught",
        }
    }

    pub fn parse(text: &str) -> Option<Origin> {
        [Origin::Seed, Origin::Agent, Origin::Taught].into_iter().find(|origin| origin.as_str() == text)
    }
}

/// One row of the decision matrix: something the brain knows how to do.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub label: String,
    /// Shell steps run in order; `{name}` placeholders are filled from slots.
    pub steps: Vec<String>,
    /// When this skill applies. Rendered into the System 1 criteria.
    pub scenario: String,
    /// Phrasings that were accepted for this skill. This is the learning.
    pub examples: Vec<String>,
    /// Phrasings that were routed here and rejected.
    pub counter_examples: Vec<String>,
    pub slots: Vec<Slot>,
    pub risk: Risk,
    pub origin: Origin,
    pub uses: i64,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub created_host: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Path {
    /// routed instantly by the decision model
    #[serde(rename = "system1")]
    System1,
    /// legacy rows: the old "did you mean?" prompt
    #[serde(rename = "system1-confirmed")]
    System1Confirmed,
    /// escalated to the reasoning agent
    #[serde(rename = "system2")]
    System2,
    /// the user said no and typed the right command
    #[serde(rename = "taught")]
    Taught,
    /// nothing ran
    #[serde(rename = "none")]
    None,
}

impl Path {
    pub fn as_str(self) -> &'static str {
        match self {
            Path::System1 => "system1",
            Path::System1Confirmed => "system1-confirmed",
            Path::System2 => "system2",
            Path::Taught => "taught",
            Path::None => "none",
        }
    }

    pub fn parse(text: &str) -> Option<Path> {
        [Path::System1, Path::System1Confirmed, Path::System2, Path::Taught, Path::None]
            .into_iter()
            .find(|path| path.as_str() == text)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BrainEvent {
    pub id: i64,
    pub ts: String,
    pub host: String,
    pub cwd: String,
    pub query: String,
    pub path: Path,
    pub label: Option<String>,
    pub confidence: Option<f64>,
    pub command: Option<String>,
    pub exit_code: Option<i64>,
    pub accepted: bool,
    pub learned: Option<String>,
    pub ms_system1: Option<i64>,
    pub ms_system2: Option<i64>,
}

/// What kind of error stopped a run, as `--json` reports it in `error`. The set is closed.
pub const USAGE: &str = "usage";
pub const CONFIGURATION: &str = "configuration";
pub const MODEL: &str = "model";
pub const STORAGE: &str = "storage";
pub const INTERNAL: &str = "internal";

/// An error that carries its kind for `--json` callers. It displays as its message alone, so the
/// trace and `jli: …` read exactly as they did before errors had a kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coded {
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for Coded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Coded {}

pub fn coded(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Coded { code, message: message.into() })
}

/// The kind of an error: the first kind found along its chain, storage when libSQL failed, and
/// `internal` for anything nobody gave a kind.
pub fn error_code(error: &anyhow::Error) -> &'static str {
    if let Some(coded) = error.chain().find_map(|cause| cause.downcast_ref::<Coded>()) {
        return coded.code;
    }
    if error.chain().any(|cause| cause.is::<libsql::Error>()) {
        return STORAGE;
    }
    INTERNAL
}

/// Who does the teaching.
///   manual  you type phrase and command; they are stored as given
///   semi    the same, but a fast model rewrites what you taught into a general, checked matrix row
///   auto    as semi, and the system also learns from what happens: a "no" or a failed command
///           is diagnosed, alternatives are offered, and your pick is what gets learned
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Manual,
    Semi,
    Auto,
}

pub const DEFAULT_MODE: Mode = Mode::Semi;

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Manual, Mode::Semi, Mode::Auto];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Manual => "manual",
            Mode::Semi => "semi",
            Mode::Auto => "auto",
        }
    }

    pub fn parse(text: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|mode| mode.as_str() == text)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
