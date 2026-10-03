use std::future::Future;
use std::path::{Path as FsPath, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _, Result};
use libsql::{Builder, Connection, Database, Value};
use tokio::runtime::Runtime;

use crate::seed::seed_skills;
use crate::types::{add_alias, BrainEvent, Origin, Path, Risk, Skill, Slot};

/// Where the brain lives. One libSQL client covers every deployment:
///
///   (default)                         local SQLite file at ~/.jli/brain.db
///   JLI_DB_URL=file:/path/brain.db    local SQLite file somewhere else
///   JLI_DB_URL=libsql://x.turso.io    remote cloud database, shared by all hosts
///   JLI_SYNC_URL=libsql://x.turso.io  local replica that syncs with the cloud
///                                     (local-speed reads, shared learning)
///   JLI_DB_TOKEN=...                  auth token for the remote database
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrainConfig {
    pub url: String,
    pub sync_url: Option<String>,
    pub auth_token: Option<String>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

impl BrainConfig {
    pub fn from_env() -> BrainConfig {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        BrainConfig {
            url: env("JLI_DB_URL").unwrap_or_else(|| format!("file:{}", home.join(".jli").join("brain.db").display())),
            sync_url: env("JLI_SYNC_URL"),
            auth_token: env("JLI_DB_TOKEN"),
        }
    }

    /// A local file brain.
    pub fn file(path: &FsPath) -> BrainConfig {
        BrainConfig { url: format!("file:{}", path.display()), sync_url: None, auth_token: None }
    }

    /// The file path of a `file:` URL, if it is one.
    fn local_path(&self) -> Option<PathBuf> {
        let rest = self.url.strip_prefix("file:")?;
        // file:///abs/path is the URL form of /abs/path.
        let rest = rest.strip_prefix("//").unwrap_or(rest);
        Some(PathBuf::from(rest))
    }
}

pub const MAX_EXAMPLES: usize = 16;

static HOST: LazyLock<String> = LazyLock::new(|| gethostname::gethostname().to_string_lossy().into_owned());

/// This machine's name, as stored with every skill and event.
pub fn host() -> &'static str {
    &HOST
}

const SCHEMA: &str = "
  CREATE TABLE IF NOT EXISTS skills (
     label            TEXT PRIMARY KEY,
     steps            TEXT NOT NULL,
     scenario         TEXT NOT NULL,
     examples         TEXT NOT NULL DEFAULT '[]',
     counter_examples TEXT NOT NULL DEFAULT '[]',
     slots            TEXT NOT NULL DEFAULT '[]',
     risk             TEXT NOT NULL DEFAULT 'safe',
     origin           TEXT NOT NULL DEFAULT 'seed',
     uses             INTEGER NOT NULL DEFAULT 0,
     created_at       TEXT NOT NULL,
     last_used_at     TEXT,
     created_host     TEXT NOT NULL
   );
  CREATE TABLE IF NOT EXISTS events (
     id          INTEGER PRIMARY KEY AUTOINCREMENT,
     ts          TEXT NOT NULL,
     host        TEXT NOT NULL,
     cwd         TEXT NOT NULL,
     query       TEXT NOT NULL,
     path        TEXT NOT NULL,
     label       TEXT,
     confidence  REAL,
     command     TEXT,
     exit_code   INTEGER,
     accepted    INTEGER NOT NULL DEFAULT 0,
     learned     TEXT,
     ms_system1  INTEGER,
     ms_system2  INTEGER
   );
  CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

const SKILL_COLUMNS: &str =
    "label, steps, scenario, examples, counter_examples, slots, risk, origin, uses, created_at, last_used_at, created_host";
const EVENT_COLUMNS: &str =
    "id, ts, host, cwd, query, path, label, confidence, command, exit_code, accepted, learned, ms_system1, ms_system2";

// ── Reading rows leniently: a brain may have been written by another jli ────

fn text(value: Value) -> String {
    match value {
        Value::Text(text) => text,
        Value::Integer(n) => n.to_string(),
        Value::Real(n) => n.to_string(),
        Value::Blob(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Value::Null => String::new(),
    }
}

fn opt_text(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        other => Some(text(other)),
    }
}

fn opt_real(value: Value) -> Option<f64> {
    match value {
        Value::Null => None,
        Value::Integer(n) => Some(n as f64),
        Value::Real(n) => Some(n),
        Value::Text(text) => text.trim().parse().ok(),
        Value::Blob(_) => None,
    }
}

fn opt_int(value: Value) -> Option<i64> {
    opt_real(value).map(|n| n as i64)
}

/// A JSON column, or the fallback when it does not parse.
fn json<T: serde::de::DeserializeOwned + Default>(value: Value) -> T {
    serde_json::from_str(&text(value)).unwrap_or_default()
}

/// Takes the columns of one row in order.
struct Columns {
    row: libsql::Row,
    next: i32,
}

impl Columns {
    fn new(row: libsql::Row) -> Columns {
        Columns { row, next: 0 }
    }

    fn take(&mut self) -> Value {
        let value = self.row.get_value(self.next).unwrap_or(Value::Null);
        self.next += 1;
        value
    }
}

fn to_skill(row: libsql::Row) -> Skill {
    let mut c = Columns::new(row);
    Skill {
        label: text(c.take()),
        steps: json(c.take()),
        scenario: text(c.take()),
        examples: json(c.take()),
        counter_examples: json(c.take()),
        slots: json::<Vec<Slot>>(c.take()),
        risk: Risk::parse(&text(c.take())).unwrap_or(Risk::Modifies),
        origin: Origin::parse(&text(c.take())).unwrap_or(Origin::Taught),
        uses: opt_int(c.take()).unwrap_or(0),
        created_at: text(c.take()),
        last_used_at: opt_text(c.take()),
        created_host: text(c.take()),
    }
}

fn to_event(row: libsql::Row) -> BrainEvent {
    let mut c = Columns::new(row);
    BrainEvent {
        id: opt_int(c.take()).unwrap_or(0),
        ts: text(c.take()),
        host: text(c.take()),
        cwd: text(c.take()),
        query: text(c.take()),
        path: Path::parse(&text(c.take())).unwrap_or(Path::None),
        label: opt_text(c.take()),
        confidence: opt_real(c.take()),
        command: opt_text(c.take()),
        exit_code: opt_int(c.take()),
        accepted: opt_int(c.take()) == Some(1),
        learned: opt_text(c.take()),
        ms_system1: opt_int(c.take()),
        ms_system2: opt_int(c.take()),
    }
}

/// UTC with millisecond precision, `2026-10-02T12:00:00.000Z`: the format every brain holds.
pub fn now_iso() -> String {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
    let (seconds, millis) = (millis.div_euclid(1000), millis.rem_euclid(1000));
    let (days, of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z", of_day / 3600, of_day % 3600 / 60, of_day % 60)
}

fn same(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewSkill {
    pub label: String,
    pub steps: Vec<String>,
    pub scenario: String,
    pub risk: Risk,
    pub origin: Origin,
    pub examples: Vec<String>,
    pub slots: Vec<Slot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
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

#[derive(Clone, Copy)]
enum ListColumn {
    Examples,
    CounterExamples,
}

impl ListColumn {
    fn name(self) -> &'static str {
        match self {
            ListColumn::Examples => "examples",
            ListColumn::CounterExamples => "counter_examples",
        }
    }
}

/// The persistent decision matrix plus the log it learns from.
///
/// libSQL is async; this is the only place that knows. Every method blocks on a
/// current-thread runtime that the brain owns, so the rest of the app stays synchronous.
pub struct Brain {
    // Declared before the runtime, and dropped inside it (see Drop).
    conn: Option<Connection>,
    db: Option<Database>,
    rt: Runtime,
    pub config: BrainConfig,
}

impl Drop for Brain {
    fn drop(&mut self) {
        let _inside = self.rt.enter();
        self.conn.take();
        self.db.take();
    }
}

impl Brain {
    pub fn open(config: BrainConfig) -> Result<Brain> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start the database runtime")?;
        let local = config.local_path();
        if let Some(path) = &local {
            if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
            }
        }
        let token = config.auth_token.clone().unwrap_or_default();
        let db = rt
            .block_on(async {
                match (&config.sync_url, &local) {
                    (Some(sync_url), Some(path)) => {
                        let db = Builder::new_remote_replica(path, sync_url.clone(), token).build().await?;
                        db.sync().await?;
                        Ok(db)
                    }
                    (Some(_), None) => {
                        Err(anyhow!("JLI_SYNC_URL needs JLI_DB_URL to be a file: path for the local replica"))
                    }
                    (None, Some(path)) => Ok(Builder::new_local(path).build().await?),
                    (None, None) => Ok(Builder::new_remote(config.url.clone(), token).build().await?),
                }
            })
            .with_context(|| format!("could not open the brain at {}", config.url))?;
        let conn = db.connect()?;
        if local.is_some() && config.sync_url.is_none() {
            // Two jli processes at once wait for each other instead of failing.
            let _ = conn.busy_timeout(Duration::from_secs(5));
        }
        let brain = Brain { conn: Some(conn), db: Some(db), rt, config };
        brain.block(brain.conn().execute_batch(SCHEMA)).context("could not create the brain's tables")?;
        brain.seed_once()?;
        Ok(brain)
    }

    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("the connection lives as long as the brain")
    }

    fn block<F: Future>(&self, future: F) -> F::Output {
        self.rt.block_on(future)
    }

    fn execute(&self, sql: &str, args: Vec<Value>) -> Result<u64> {
        Ok(self.block(self.conn().execute(sql, args))?)
    }

    fn query<T>(&self, sql: &str, args: Vec<Value>, map: impl Fn(libsql::Row) -> T) -> Result<Vec<T>> {
        self.block(async {
            let mut rows = self.conn().query(sql, args).await?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await? {
                out.push(map(row));
            }
            Ok(out)
        })
    }

    /// Where this brain is stored, for display.
    pub fn location(&self) -> String {
        match &self.config.sync_url {
            Some(sync_url) => format!("{} ⇄ {}", self.config.url, sync_url),
            None => self.config.url.clone(),
        }
    }

    pub fn is_shared(&self) -> bool {
        self.config.sync_url.is_some() || !self.config.url.starts_with("file:")
    }

    fn seed_once(&self) -> Result<()> {
        let seeded = self.query("SELECT value FROM meta WHERE key = 'seeded'", vec![], |_| ())?;
        if !seeded.is_empty() {
            return Ok(());
        }
        for skill in seed_skills() {
            self.add_skill_if_missing(&skill)?;
        }
        self.execute("INSERT OR IGNORE INTO meta (key, value) VALUES ('seeded', ?)", vec![now_iso().into()])?;
        Ok(())
    }

    /// Pull and push the local replica. A no-op without `JLI_SYNC_URL`.
    pub fn sync(&self) -> Result<bool> {
        let Some(db) = (self.config.sync_url.is_some()).then_some(self.db.as_ref()).flatten() else {
            return Ok(false);
        };
        self.block(db.sync())?;
        Ok(true)
    }

    pub fn skills(&self) -> Result<Vec<Skill>> {
        self.query(&format!("SELECT {SKILL_COLUMNS} FROM skills ORDER BY uses DESC, label ASC"), vec![], to_skill)
    }

    pub fn skill(&self, label: &str) -> Result<Option<Skill>> {
        let rows =
            self.query(&format!("SELECT {SKILL_COLUMNS} FROM skills WHERE label = ?"), vec![label.into()], to_skill)?;
        Ok(rows.into_iter().next())
    }

    /// Insert a skill, or change what an existing label runs. Re-teaching a label keeps
    /// what was learned about it: examples, counter-examples and uses are never replaced,
    /// and a slot that keeps its name keeps its aliases.
    pub fn add_skill(&self, skill: &NewSkill) -> Result<()> {
        let mut skill = skill.clone();
        for known in self.skill(&skill.label)?.map(|known| known.slots).unwrap_or_default() {
            for alias in &known.aliases {
                add_alias(&mut skill.slots, &known.name, alias);
            }
        }
        self.insert_skill(
            &skill,
            "DO UPDATE SET steps = excluded.steps, scenario = excluded.scenario, slots = excluded.slots, risk = excluded.risk",
        )
    }

    pub fn add_skill_if_missing(&self, skill: &NewSkill) -> Result<()> {
        self.insert_skill(skill, "DO NOTHING")
    }

    fn insert_skill(&self, skill: &NewSkill, conflict: &str) -> Result<()> {
        self.execute(
            &format!(
                "INSERT INTO skills (label, steps, scenario, examples, slots, risk, origin, created_at, created_host)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(label) {conflict}"
            ),
            vec![
                skill.label.clone().into(),
                serde_json::to_string(&skill.steps)?.into(),
                skill.scenario.clone().into(),
                serde_json::to_string(&skill.examples)?.into(),
                serde_json::to_string(&skill.slots)?.into(),
                skill.risk.as_str().into(),
                skill.origin.as_str().into(),
                now_iso().into(),
                host().into(),
            ],
        )?;
        Ok(())
    }

    /// Link a phrase to a command. If a skill already runs exactly this command the
    /// phrase is added to it, so one command can collect many phrasings.
    pub fn learn_command(&self, phrase: &str, steps: &[String], risk: Risk) -> Result<(String, bool)> {
        let skills = self.skills()?;
        let wanted = steps.join(" && ");
        if let Some(existing) = skills.iter().find(|skill| skill.steps.join(" && ") == wanted) {
            self.link_phrase(&existing.label, phrase)?;
            return Ok((existing.label.clone(), false));
        }
        let mut label = slug(&wanted);
        while skills.iter().any(|skill| skill.label == label) {
            label.push_str("-x");
        }
        self.add_skill(&NewSkill {
            label: label.clone(),
            steps: steps.to_vec(),
            scenario: format!("the user wants: {phrase}"),
            risk,
            origin: Origin::Taught,
            examples: vec![phrase.to_string()],
            slots: vec![],
        })?;
        Ok((label, true))
    }

    /// Add a phrasing to a skill, and drop it from that skill's counter-examples if it was one.
    pub fn link_phrase(&self, label: &str, phrase: &str) -> Result<()> {
        self.add_example(label, phrase)?;
        self.edit_list(label, ListColumn::CounterExamples, |list| without(list, phrase))?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let rows = self.query("SELECT value FROM meta WHERE key = ?", vec![format!("setting:{key}").into()], |row| {
            text(row.get_value(0).unwrap_or(Value::Null))
        })?;
        Ok(rows.into_iter().next())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.execute(
            "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            vec![format!("setting:{key}").into(), value.into()],
        )?;
        Ok(())
    }

    /// Write a `meta` entry, replacing the value under the same key.
    pub fn put_meta(&self, key: &str, value: &str) -> Result<()> {
        self.execute(
            "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            vec![key.into(), value.into()],
        )?;
        Ok(())
    }

    /// Every `meta` entry whose key starts with `prefix`, in key order.
    pub fn meta_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        self.query(
            "SELECT key, value FROM meta WHERE substr(key, 1, length(?)) = ? ORDER BY key",
            vec![prefix.into(), prefix.into()],
            |row| (text(row.get_value(0).unwrap_or(Value::Null)), text(row.get_value(1).unwrap_or(Value::Null))),
        )
    }

    pub fn forget(&self, label: &str) -> Result<bool> {
        Ok(self.execute("DELETE FROM skills WHERE label = ?", vec![label.into()])? > 0)
    }

    /// Remember that `phrase` means `label`. Returns false if it was already known.
    pub fn add_example(&self, label: &str, phrase: &str) -> Result<bool> {
        self.edit_list(label, ListColumn::Examples, |list| with(list, phrase))
    }

    pub fn remove_example(&self, label: &str, phrase: &str) -> Result<bool> {
        self.edit_list(label, ListColumn::Examples, |list| without(list, phrase))
    }

    /// Remember that `phrase` does NOT mean `label`.
    pub fn add_counter_example(&self, label: &str, phrase: &str) -> Result<bool> {
        self.edit_list(label, ListColumn::CounterExamples, |list| with(list, phrase))
    }

    pub fn remove_counter_example(&self, label: &str, phrase: &str) -> Result<bool> {
        self.edit_list(label, ListColumn::CounterExamples, |list| without(list, phrase))
    }

    /// Remember that `alias` is another name for a slot of `label`. Returns false if nothing changed.
    pub fn add_alias(&self, label: &str, slot: &str, alias: &str) -> Result<bool> {
        let Some(mut skill) = self.skill(label)? else { return Ok(false) };
        if !add_alias(&mut skill.slots, slot, alias) {
            return Ok(false);
        }
        self.execute(
            "UPDATE skills SET slots = ? WHERE label = ?",
            vec![serde_json::to_string(&skill.slots)?.into(), label.into()],
        )?;
        Ok(true)
    }

    fn edit_list(
        &self,
        label: &str,
        column: ListColumn,
        edit: impl FnOnce(Vec<String>) -> Option<Vec<String>>,
    ) -> Result<bool> {
        let column = column.name();
        let rows = self.query(&format!("SELECT {column} FROM skills WHERE label = ?"), vec![label.into()], |row| {
            row.get_value(0).unwrap_or(Value::Null)
        })?;
        let Some(list) = rows.into_iter().next() else { return Ok(false) };
        let Some(next) = edit(json(list)) else { return Ok(false) };
        self.execute(
            &format!("UPDATE skills SET {column} = ? WHERE label = ?"),
            vec![serde_json::to_string(&next)?.into(), label.into()],
        )?;
        Ok(true)
    }

    pub fn record_use(&self, label: &str) -> Result<()> {
        self.execute(
            "UPDATE skills SET uses = uses + 1, last_used_at = ? WHERE label = ?",
            vec![now_iso().into(), label.into()],
        )?;
        Ok(())
    }

    pub fn log_event(&self, event: &NewEvent) -> Result<i64> {
        self.execute(
            "INSERT INTO events (ts, host, cwd, query, path, label, confidence, command, exit_code, accepted, learned, ms_system1, ms_system2)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                now_iso().into(),
                host().into(),
                event.cwd.clone().into(),
                event.query.clone().into(),
                event.path.as_str().into(),
                event.label.clone().into(),
                event.confidence.into(),
                event.command.clone().into(),
                event.exit_code.into(),
                i64::from(event.accepted).into(),
                event.learned.clone().into(),
                event.ms_system1.into(),
                event.ms_system2.into(),
            ],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    pub fn mark_rejected(&self, event_id: i64) -> Result<()> {
        self.execute("UPDATE events SET accepted = 0, learned = 'corrected' WHERE id = ?", vec![event_id.into()])?;
        Ok(())
    }

    /// Newest first.
    pub fn events(&self, limit: i64) -> Result<Vec<BrainEvent>> {
        self.query(&format!("SELECT {EVENT_COLUMNS} FROM events ORDER BY id DESC LIMIT ?"), vec![limit.into()], to_event)
    }

    pub fn last_executed(&self) -> Result<Option<BrainEvent>> {
        let rows = self.query(
            &format!("SELECT {EVENT_COLUMNS} FROM events WHERE label IS NOT NULL AND accepted = 1 ORDER BY id DESC LIMIT 1"),
            vec![],
            to_event,
        )?;
        Ok(rows.into_iter().next())
    }
}

/// The list with `phrase` appended, newest kept, or None if it was already there.
fn with(list: Vec<String>, phrase: &str) -> Option<Vec<String>> {
    if list.iter().any(|entry| same(entry, phrase)) {
        return None;
    }
    let mut next = list;
    next.push(phrase.to_string());
    let excess = next.len().saturating_sub(MAX_EXAMPLES);
    next.drain(..excess);
    Some(next)
}

/// The list without `phrase`, or None if it was not there.
fn without(list: Vec<String>, phrase: &str) -> Option<Vec<String>> {
    let before = list.len();
    let next: Vec<String> = list.into_iter().filter(|entry| !same(entry, phrase)).collect();
    (next.len() != before).then_some(next)
}

/// A label made from free text: lowercase, dashes, at most 48 characters.
pub fn slug(text: &str) -> String {
    let mut dashed = String::new();
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            dashed.push(ch);
        } else if !dashed.ends_with('-') {
            dashed.push('-');
        }
    }
    // Trimmed before the cut, so a cut can still end in a dash: labels in existing brains were made this way.
    let slug: String = dashed.trim_matches('-').chars().take(48).collect();
    if slug.is_empty() {
        "skill".into()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("jli-store-{name}-{}-{}", std::process::id(), now_iso().replace(':', "")));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn brain(&self) -> Brain {
            Brain::open(BrainConfig::file(&self.0.join("brain.db"))).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_fresh_brain_is_seeded_once() {
        let scratch = Scratch::new("seed");
        let brain = scratch.brain();
        let skills = brain.skills().unwrap();
        assert_eq!(skills.len(), 9);
        assert!(brain.forget("git-push").unwrap());
        drop(brain);
        let again = scratch.brain();
        assert_eq!(again.skills().unwrap().len(), 8, "a forgotten seed must not come back");
        let commit = again.skill("git-commit").unwrap().unwrap();
        assert_eq!(commit.slots[0].name, "message");
        assert_eq!(commit.risk, Risk::Modifies);
    }

    #[test]
    fn upserting_keeps_what_was_learned() {
        let scratch = Scratch::new("upsert");
        let brain = scratch.brain();
        assert!(brain.add_example("list-files", "what is here").unwrap());
        assert!(brain.add_counter_example("list-files", "newest first").unwrap());
        brain.record_use("list-files").unwrap();
        brain
            .add_skill(&NewSkill {
                label: "list-files".into(),
                steps: vec!["ls -1".into()],
                scenario: "changed".into(),
                risk: Risk::Safe,
                origin: Origin::Taught,
                examples: vec!["ignored".into()],
                slots: vec![],
            })
            .unwrap();
        let skill = brain.skill("list-files").unwrap().unwrap();
        assert_eq!(skill.steps, ["ls -1"]);
        assert_eq!(skill.scenario, "changed");
        assert_eq!(skill.examples, ["what is here"]);
        assert_eq!(skill.counter_examples, ["newest first"]);
        assert_eq!(skill.uses, 1);
        assert_eq!(skill.origin, Origin::Seed, "the origin is not updated");
    }

    #[test]
    fn examples_are_deduplicated_loosely_and_capped() {
        let scratch = Scratch::new("examples");
        let brain = scratch.brain();
        assert!(brain.add_example("pwd-less", "x").is_ok_and(|added| !added), "no such skill");
        assert!(brain.add_example("print-working-directory", "Where am I").unwrap());
        assert!(!brain.add_example("print-working-directory", "  where am i ").unwrap());
        for n in 0..20 {
            brain.add_example("print-working-directory", &format!("phrase {n}")).unwrap();
        }
        let skill = brain.skill("print-working-directory").unwrap().unwrap();
        assert_eq!(skill.examples.len(), MAX_EXAMPLES);
        assert_eq!(skill.examples.last().unwrap(), "phrase 19");
        assert_eq!(skill.examples.first().unwrap(), "phrase 4");
        assert!(brain.remove_example("print-working-directory", "PHRASE 19").unwrap());
        assert!(!brain.remove_example("print-working-directory", "phrase 19").unwrap());
    }

    #[test]
    fn learning_a_known_command_links_the_phrase() {
        let scratch = Scratch::new("learn");
        let brain = scratch.brain();
        brain.add_counter_example("list-files-by-time", "newest files").unwrap();
        let (label, created) = brain.learn_command("newest files", &["ls -ltr".into()], Risk::Safe).unwrap();
        assert_eq!((label.as_str(), created), ("list-files-by-time", false));
        let skill = brain.skill(&label).unwrap().unwrap();
        assert!(skill.examples.contains(&"newest files".to_string()));
        assert!(skill.counter_examples.is_empty(), "linking drops the counter-example");

        let (label, created) = brain.learn_command("disk use", &["du -sh .".into()], Risk::Safe).unwrap();
        assert_eq!((label.as_str(), created), ("du-sh", true));
        let (again, _) = brain.learn_command("other", &["du  -sh".into()], Risk::Safe).unwrap();
        assert_eq!(again, "du-sh-x");
    }

    #[test]
    fn a_slot_collects_aliases() {
        let scratch = Scratch::new("aliases");
        let brain = scratch.brain();
        // A row written before aliases existed reads as it always did, and is written back unchanged.
        let stored = |brain: &Brain| {
            brain
                .query("SELECT slots FROM skills WHERE label = 'git-commit'", vec![], |row| text(row.get_value(0).unwrap()))
                .unwrap()
                .remove(0)
        };
        assert_eq!(stored(&brain), r#"[{"name":"message","kind":"text","description":"the commit message"}]"#);

        assert!(brain.add_alias("git-commit", "message", "Msg").unwrap());
        assert!(!brain.add_alias("git-commit", "message", "msg").unwrap(), "already known");
        assert!(!brain.add_alias("git-commit", "message", "message").unwrap(), "the name itself");
        assert!(!brain.add_alias("git-commit", "message", "-m").unwrap(), "a flag is never a name");
        assert!(!brain.add_alias("git-commit", "message", "two words").unwrap());
        assert!(!brain.add_alias("git-commit", "subject", "s").unwrap(), "no such slot");
        assert!(!brain.add_alias("no-such-skill", "message", "m").unwrap());
        assert_eq!(brain.skill("git-commit").unwrap().unwrap().slots[0].aliases, ["msg"]);
        assert_eq!(
            stored(&brain),
            r#"[{"name":"message","kind":"text","description":"the commit message","aliases":["msg"]}]"#
        );
    }

    #[test]
    fn re_teaching_a_label_keeps_its_slots_aliases() {
        let scratch = Scratch::new("alias-upsert");
        let brain = scratch.brain();
        assert!(brain.add_alias("git-commit", "message", "msg").unwrap());
        let slot = |name: &str, aliases: &[&str]| Slot {
            name: name.into(),
            kind: crate::types::SlotKind::Text,
            description: String::new(),
            default: None,
            aliases: aliases.iter().map(|alias| alias.to_string()).collect(),
        };
        brain
            .add_skill(&NewSkill {
                label: "git-commit".into(),
                steps: vec!["git commit --author {msg} -m {message}".into()],
                scenario: "changed".into(),
                risk: Risk::Modifies,
                origin: Origin::Taught,
                examples: vec![],
                slots: vec![slot("message", &["text"]), slot("author", &["by"])],
            })
            .unwrap();
        let slots = brain.skill("git-commit").unwrap().unwrap().slots;
        assert_eq!(slots[0].aliases, ["text", "msg"]);
        assert_eq!(slots[1].aliases, ["by"]);
    }

    #[test]
    fn settings_and_events_round_trip() {
        let scratch = Scratch::new("events");
        let brain = scratch.brain();
        assert_eq!(brain.setting("mode").unwrap(), None);
        brain.set_setting("mode", "auto").unwrap();
        brain.set_setting("mode", "manual").unwrap();
        assert_eq!(brain.setting("mode").unwrap().as_deref(), Some("manual"));

        let event = NewEvent {
            cwd: "/tmp".into(),
            query: "where am I".into(),
            path: Path::System1,
            label: Some("print-working-directory".into()),
            confidence: Some(0.99),
            command: Some("pwd".into()),
            exit_code: Some(0),
            accepted: true,
            learned: None,
            ms_system1: Some(250),
            ms_system2: None,
        };
        let first = brain.log_event(&event).unwrap();
        let second = brain.log_event(&NewEvent { accepted: false, label: None, ..event.clone() }).unwrap();
        assert!(second > first);
        let events = brain.events(10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, second, "newest first");
        assert_eq!(events[1].confidence, Some(0.99));
        assert_eq!(events[1].path, Path::System1);
        assert_eq!(events[1].ms_system2, None);
        assert_eq!(events[1].ts.len(), "2026-10-02T12:00:00.000Z".len());
        let last = brain.last_executed().unwrap().unwrap();
        assert_eq!(last.id, first);
        brain.mark_rejected(first).unwrap();
        assert!(brain.last_executed().unwrap().is_none());
        assert_eq!(brain.events(1).unwrap().len(), 1);
    }

    #[test]
    fn slugs_trim_before_the_cut() {
        assert_eq!(slug("git log --oneline -n {count}"), "git-log-oneline-n-count");
        assert_eq!(slug("  --Hello, World!--  "), "hello-world");
        assert_eq!(slug("!!!"), "skill");
        assert_eq!(slug(&"ab ".repeat(30)), "ab-".repeat(16), "trimmed before the cut");
    }

    #[test]
    fn timestamps_are_utc_with_milliseconds() {
        let ts = now_iso();
        assert_eq!(ts.len(), 24, "{ts}");
        assert!(ts.ends_with('Z') && ts.as_bytes()[10] == b'T', "{ts}");
    }
}
