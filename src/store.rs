use std::collections::HashMap;
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
  CREATE TABLE IF NOT EXISTS scoped_skills (
     user_id          TEXT NOT NULL,
     project_id       TEXT NOT NULL DEFAULT '',
     label            TEXT NOT NULL,
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
     created_host     TEXT NOT NULL,
     PRIMARY KEY (user_id, project_id, label)
   );
  CREATE TABLE IF NOT EXISTS scoped_events (
     id          INTEGER PRIMARY KEY AUTOINCREMENT,
     user_id     TEXT NOT NULL,
     project_id  TEXT NOT NULL DEFAULT '',
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
  CREATE TABLE IF NOT EXISTS scoped_meta (
     user_id     TEXT NOT NULL,
     project_id  TEXT NOT NULL DEFAULT '',
     key         TEXT NOT NULL,
     value       TEXT NOT NULL,
     PRIMARY KEY (user_id, project_id, key)
   );
  CREATE TABLE IF NOT EXISTS shares (
     user_id     TEXT NOT NULL,
     project_id  TEXT NOT NULL DEFAULT '',
     label       TEXT NOT NULL,
     created_at  TEXT NOT NULL,
     from_user   TEXT NOT NULL,
     PRIMARY KEY (user_id, project_id, label, created_at)
   );
";

/// The user every brain has always had. Its rows are the tables a brain had before users existed.
pub const DEFAULT_USER: &str = "default";

/// A user id as typed: a short slug, so it reads the same in a flag, a variable and a message.
pub fn user_id(text: &str) -> Result<String> {
    let mut bytes = text.bytes();
    let starts_well = bytes.next().is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    let rest_well = bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_');
    if starts_well && rest_well && text.len() <= 32 {
        Ok(text.to_string())
    } else {
        Err(anyhow!(
            "a user id is 1 to 32 lowercase letters, digits, - or _, starting with a letter or digit, got \"{text}\""
        ))
    }
}

/// Whose rows a brain reads and writes. The default user, with no project, owns the tables every
/// brain has always had, so an older jli on the same brain sees exactly what it saw before. Every
/// other scope lives in the `scoped_*` tables, which an older jli never reads, keyed by user and
/// project. Nothing sets a project yet: `project` is always empty.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    user: String,
    project: String,
}

impl Scope {
    fn user(user: &str) -> Scope {
        Scope { user: user.to_string(), project: String::new() }
    }

    fn is_legacy(&self) -> bool {
        self.user == DEFAULT_USER && self.project.is_empty()
    }

    fn skills(&self) -> &'static str {
        if self.is_legacy() {
            "skills"
        } else {
            "scoped_skills"
        }
    }

    fn events(&self) -> &'static str {
        if self.is_legacy() {
            "events"
        } else {
            "scoped_events"
        }
    }

    /// The condition that keeps this scope's rows, to follow `WHERE`. Its values come from `args`.
    fn rows(&self) -> &'static str {
        if self.is_legacy() {
            "1"
        } else {
            "user_id = ? AND project_id = ?"
        }
    }

    /// The key columns an insert names first, and their placeholders. A conflict target starts with them too.
    fn key(&self) -> (&'static str, &'static str) {
        if self.is_legacy() {
            ("", "")
        } else {
            ("user_id, project_id, ", "?, ?, ")
        }
    }

    /// This scope's values for `rows` or `key`, followed by the statement's own.
    fn args(&self, rest: Vec<Value>) -> Vec<Value> {
        let mut args: Vec<Value> =
            if self.is_legacy() { vec![] } else { vec![self.user.clone().into(), self.project.clone().into()] };
        args.extend(rest);
        args
    }
}

/// How often a read-modify-write is tried while other writers keep changing the same row.
const WRITE_ATTEMPTS: usize = 16;
/// How many `-x` suffixes a new label may collect before giving up.
const LABEL_ATTEMPTS: usize = 64;

/// What `share` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shared {
    Copied,
    NoSuchSkill,
    SameUser,
    /// The other user already has a skill under this label.
    LabelTaken,
    /// The other user already has a skill, under the label given, that runs the same steps.
    AlreadyRuns(String),
}

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
    scope: Scope,
}

impl Drop for Brain {
    fn drop(&mut self) {
        let _inside = self.rt.enter();
        self.conn.take();
        self.db.take();
    }
}

impl Brain {
    /// The brain as the default user sees it.
    pub fn open(config: BrainConfig) -> Result<Brain> {
        Brain::open_as(config, DEFAULT_USER)
    }

    /// The brain as `user` sees it: their skills, events and settings, and nobody else's.
    pub fn open_as(config: BrainConfig, user: &str) -> Result<Brain> {
        let scope = Scope::user(&user_id(user)?);
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
        let brain = Brain { conn: Some(conn), db: Some(db), rt, config, scope };
        brain.block(brain.conn().execute_batch(SCHEMA)).context("could not create the brain's tables")?;
        brain.seed_once(&brain.scope)?;
        Ok(brain)
    }

    /// Whose skills, events and settings this brain reads and writes.
    pub fn user(&self) -> &str {
        &self.scope.user
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
        let place = match &self.config.sync_url {
            Some(sync_url) => format!("{} ⇄ {}", self.config.url, sync_url),
            None => self.config.url.clone(),
        };
        if self.scope.is_legacy() {
            place
        } else {
            format!("{place}, user {}", self.scope.user)
        }
    }

    pub fn is_shared(&self) -> bool {
        self.config.sync_url.is_some() || !self.config.url.starts_with("file:")
    }

    /// A new user starts from the seed skills, like a fresh brain, and a seed they forget does not come back.
    fn seed_once(&self, scope: &Scope) -> Result<()> {
        if self.meta(scope, "seeded")?.is_some() {
            return Ok(());
        }
        for skill in seed_skills() {
            self.insert_skill(scope, &skill, "DO NOTHING")?;
        }
        self.set_meta(scope, "seeded", &now_iso(), "DO NOTHING")?;
        Ok(())
    }

    /// A value in `meta` for the default user, or in `scoped_meta` for anyone else.
    fn meta(&self, scope: &Scope, key: &str) -> Result<Option<String>> {
        let sql = if scope.is_legacy() {
            "SELECT value FROM meta WHERE key = ?"
        } else {
            "SELECT value FROM scoped_meta WHERE user_id = ? AND project_id = ? AND key = ?"
        };
        let rows = self.query(sql, scope.args(vec![key.into()]), |row| text(row.get_value(0).unwrap_or(Value::Null)))?;
        Ok(rows.into_iter().next())
    }

    fn set_meta(&self, scope: &Scope, key: &str, value: &str, conflict: &str) -> Result<()> {
        let (table, key_columns) =
            if scope.is_legacy() { ("meta", "key") } else { ("scoped_meta", "user_id, project_id, key") };
        let (_, placeholders) = scope.key();
        self.execute(
            &format!("INSERT INTO {table} ({key_columns}, value) VALUES ({placeholders}?, ?) ON CONFLICT({key_columns}) {conflict}"),
            scope.args(vec![key.into(), value.into()]),
        )?;
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
        self.skills_of(&self.scope)
    }

    fn skills_of(&self, scope: &Scope) -> Result<Vec<Skill>> {
        let (table, rows) = (scope.skills(), scope.rows());
        self.query(
            &format!("SELECT {SKILL_COLUMNS} FROM {table} WHERE {rows} ORDER BY uses DESC, label ASC"),
            scope.args(vec![]),
            to_skill,
        )
    }

    pub fn skill(&self, label: &str) -> Result<Option<Skill>> {
        let (table, rows) = (self.scope.skills(), self.scope.rows());
        let rows = self.query(
            &format!("SELECT {SKILL_COLUMNS} FROM {table} WHERE {rows} AND label = ?"),
            self.scope.args(vec![label.into()]),
            to_skill,
        )?;
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
            &self.scope,
            &skill,
            "DO UPDATE SET steps = excluded.steps, scenario = excluded.scenario, slots = excluded.slots, risk = excluded.risk",
        )?;
        Ok(())
    }

    pub fn add_skill_if_missing(&self, skill: &NewSkill) -> Result<()> {
        self.insert_skill(&self.scope, skill, "DO NOTHING")?;
        Ok(())
    }

    /// Insert a new skill under its label, or under the label with `-x` appended until one is free.
    /// A label that another writer took after this one looked is never overwritten. Returns the
    /// label the skill was stored under.
    pub fn add_new_skill(&self, skill: &NewSkill) -> Result<String> {
        let mut skill = skill.clone();
        for _ in 0..LABEL_ATTEMPTS {
            if self.insert_skill(&self.scope, &skill, "DO NOTHING")? > 0 {
                return Ok(skill.label);
            }
            skill.label.push_str("-x");
        }
        Err(anyhow!("could not find a free label for {}", skill.label))
    }

    /// Returns how many rows were written: 0 when the label was taken and `conflict` does nothing.
    fn insert_skill(&self, scope: &Scope, skill: &NewSkill, conflict: &str) -> Result<u64> {
        let (table, (key_columns, placeholders)) = (scope.skills(), scope.key());
        self.execute(
            &format!(
                "INSERT INTO {table} ({key_columns}label, steps, scenario, examples, slots, risk, origin, created_at, created_host)
                 VALUES ({placeholders}?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT({key_columns}label) {conflict}"
            ),
            scope.args(vec![
                skill.label.clone().into(),
                serde_json::to_string(&skill.steps)?.into(),
                skill.scenario.clone().into(),
                serde_json::to_string(&skill.examples)?.into(),
                serde_json::to_string(&skill.slots)?.into(),
                skill.risk.as_str().into(),
                skill.origin.as_str().into(),
                now_iso().into(),
                host().into(),
            ]),
        )
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
        let label = self.add_new_skill(&NewSkill {
            label: slug(&wanted),
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

    /// This user's own setting. A user without one gets the built-in default, not the default user's.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.meta(&self.scope, &format!("setting:{key}"))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.set_meta(&self.scope, &format!("setting:{key}"), value, "DO UPDATE SET value = excluded.value")
    }

    pub fn forget(&self, label: &str) -> Result<bool> {
        let (table, rows) = (self.scope.skills(), self.scope.rows());
        Ok(self.execute(&format!("DELETE FROM {table} WHERE {rows} AND label = ?"), self.scope.args(vec![label.into()]))?
            > 0)
    }

    /// Copy one of this user's skills to another user, as it is now: its steps, scenario, slots
    /// with their aliases, risk and phrasings. Its counter-examples are not copied: they mark where
    /// it ends against this user's other skills. A skill the other user already has, by label or
    /// by its steps, is left alone.
    pub fn share(&self, label: &str, to: &str) -> Result<Shared> {
        let target = Scope::user(&user_id(to)?);
        if target == self.scope {
            return Ok(Shared::SameUser);
        }
        let Some(skill) = self.skill(label)? else { return Ok(Shared::NoSuchSkill) };
        self.seed_once(&target)?;
        let theirs = self.skills_of(&target)?;
        if let Some(same) = theirs.iter().find(|known| known.steps == skill.steps) {
            return Ok(Shared::AlreadyRuns(same.label.clone()));
        }
        if theirs.iter().any(|known| known.label == skill.label) {
            return Ok(Shared::LabelTaken);
        }
        let copy = NewSkill {
            label: skill.label.clone(),
            steps: skill.steps,
            scenario: skill.scenario,
            risk: skill.risk,
            origin: skill.origin,
            examples: skill.examples,
            slots: skill.slots,
        };
        if self.insert_skill(&target, &copy, "DO NOTHING")? == 0 {
            return Ok(Shared::LabelTaken);
        }
        let created_at = self
            .skills_of(&target)?
            .into_iter()
            .find(|known| known.label == copy.label)
            .map(|known| known.created_at)
            .unwrap_or_default();
        self.execute(
            "INSERT INTO shares (user_id, project_id, label, created_at, from_user) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id, project_id, label, created_at) DO NOTHING",
            vec![
                target.user.into(),
                target.project.into(),
                copy.label.into(),
                created_at.into(),
                self.scope.user.clone().into(),
            ],
        )?;
        Ok(Shared::Copied)
    }

    /// Who shared each of this user's skills with them. A skill counts only while it is the row that
    /// was shared: one forgotten and taught again under the same label has another `created_at`.
    pub fn shared_from(&self) -> Result<HashMap<String, String>> {
        let (table, rows) = (self.scope.skills(), self.scope.rows());
        let rows = self.query(
            &format!(
                "SELECT label, from_user FROM shares
                 WHERE user_id = ? AND project_id = ?
                   AND EXISTS (SELECT 1 FROM {table} WHERE {rows} AND label = shares.label AND created_at = shares.created_at)"
            ),
            [vec![self.scope.user.clone().into(), self.scope.project.clone().into()], self.scope.args(vec![])].concat(),
            |row| (text(row.get_value(0).unwrap_or(Value::Null)), text(row.get_value(1).unwrap_or(Value::Null))),
        )?;
        Ok(rows.into_iter().collect())
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

    /// Remember that `alias` is another name for a slot of `label`. Returns false if nothing changed.
    pub fn add_alias(&self, label: &str, slot: &str, alias: &str) -> Result<bool> {
        self.edit_column(label, "slots", |stored| {
            let mut slots: Vec<Slot> = json(stored);
            if !add_alias(&mut slots, slot, alias) {
                return Ok(None);
            }
            Ok(Some(serde_json::to_string(&slots)?))
        })
    }

    fn edit_list(&self, label: &str, column: ListColumn, edit: impl Fn(Vec<String>) -> Option<Vec<String>>) -> Result<bool> {
        self.edit_column(label, column.name(), |stored| {
            Ok(edit(json(stored)).map(|next| serde_json::to_string(&next)).transpose()?)
        })
    }

    /// Change one column of one skill without losing another writer's change to it. The new value
    /// is written only if the column still holds what was read; otherwise it is read and edited
    /// again. One conditional statement, so it holds on a file, a remote and a replica alike, and
    /// nothing is held open while `edit` runs. Returns false when `edit` changes nothing.
    fn edit_column(&self, label: &str, column: &str, edit: impl Fn(Value) -> Result<Option<String>>) -> Result<bool> {
        let (table, rows) = (self.scope.skills(), self.scope.rows());
        for _ in 0..WRITE_ATTEMPTS {
            let read = self.query(
                &format!("SELECT {column} FROM {table} WHERE {rows} AND label = ?"),
                self.scope.args(vec![label.into()]),
                |row| row.get_value(0).unwrap_or(Value::Null),
            )?;
            let Some(stored) = read.into_iter().next() else { return Ok(false) };
            let Some(next) = edit(stored.clone())? else { return Ok(false) };
            let written = self.execute(
                &format!("UPDATE {table} SET {column} = ? WHERE {rows} AND label = ? AND {column} IS ?"),
                [vec![next.into()], self.scope.args(vec![label.into(), stored])].concat(),
            )?;
            if written > 0 {
                return Ok(true);
            }
            // Another writer got there first. A replica reads its local copy, so catch up before reading again.
            self.sync()?;
        }
        Err(anyhow!("{label} kept changing while its {column} were being written, so nothing was written. Try again."))
    }

    pub fn record_use(&self, label: &str) -> Result<()> {
        let (table, rows) = (self.scope.skills(), self.scope.rows());
        self.execute(
            &format!("UPDATE {table} SET uses = uses + 1, last_used_at = ? WHERE {rows} AND label = ?"),
            [vec![now_iso().into()], self.scope.args(vec![label.into()])].concat(),
        )?;
        Ok(())
    }

    pub fn log_event(&self, event: &NewEvent) -> Result<i64> {
        let (table, (key_columns, placeholders)) = (self.scope.events(), self.scope.key());
        self.execute(
            &format!("INSERT INTO {table} ({key_columns}ts, host, cwd, query, path, label, confidence, command, exit_code, accepted, learned, ms_system1, ms_system2)
             VALUES ({placeholders}?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"),
            self.scope.args(vec![
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
            ]),
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    pub fn mark_rejected(&self, event_id: i64) -> Result<()> {
        let (table, rows) = (self.scope.events(), self.scope.rows());
        self.execute(
            &format!("UPDATE {table} SET accepted = 0, learned = 'corrected' WHERE {rows} AND id = ?"),
            self.scope.args(vec![event_id.into()]),
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn events(&self, limit: i64) -> Result<Vec<BrainEvent>> {
        let (table, rows) = (self.scope.events(), self.scope.rows());
        self.query(
            &format!("SELECT {EVENT_COLUMNS} FROM {table} WHERE {rows} ORDER BY id DESC LIMIT ?"),
            self.scope.args(vec![limit.into()]),
            to_event,
        )
    }

    pub fn last_executed(&self) -> Result<Option<BrainEvent>> {
        let (table, rows) = (self.scope.events(), self.scope.rows());
        let rows = self.query(
            &format!("SELECT {EVENT_COLUMNS} FROM {table} WHERE {rows} AND label IS NOT NULL AND accepted = 1 ORDER BY id DESC LIMIT 1"),
            self.scope.args(vec![]),
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

    impl Scratch {
        fn brain_as(&self, user: &str) -> Brain {
            Brain::open_as(BrainConfig::file(&self.0.join("brain.db")), user).unwrap()
        }
    }

    fn labels(brain: &Brain) -> Vec<String> {
        let mut labels: Vec<String> = brain.skills().unwrap().into_iter().map(|skill| skill.label).collect();
        labels.sort();
        labels
    }

    fn event(label: &str) -> NewEvent {
        NewEvent {
            cwd: "/tmp".into(),
            query: format!("run {label}"),
            path: Path::System1,
            label: Some(label.into()),
            confidence: Some(0.9),
            command: Some(label.into()),
            exit_code: Some(0),
            accepted: true,
            learned: None,
            ms_system1: Some(1),
            ms_system2: None,
        }
    }

    /// The schema every brain had before users existed, as raw SQL, so this test does not move with `SCHEMA`.
    const BEFORE_USERS: &str = "
      CREATE TABLE skills (
         label TEXT PRIMARY KEY, steps TEXT NOT NULL, scenario TEXT NOT NULL,
         examples TEXT NOT NULL DEFAULT '[]', counter_examples TEXT NOT NULL DEFAULT '[]',
         slots TEXT NOT NULL DEFAULT '[]', risk TEXT NOT NULL DEFAULT 'safe', origin TEXT NOT NULL DEFAULT 'seed',
         uses INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL, last_used_at TEXT, created_host TEXT NOT NULL);
      CREATE TABLE events (
         id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, host TEXT NOT NULL, cwd TEXT NOT NULL,
         query TEXT NOT NULL, path TEXT NOT NULL, label TEXT, confidence REAL, command TEXT,
         exit_code INTEGER, accepted INTEGER NOT NULL DEFAULT 0, learned TEXT, ms_system1 INTEGER, ms_system2 INTEGER);
      CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      INSERT INTO skills VALUES ('du-sh', '[\"du -sh .\"]', 'the user wants: disk use', '[\"disk use\"]',
         '[\"free space\"]', '[]', 'safe', 'taught', 3, '2026-10-01T09:00:00.000Z', '2026-10-02T09:00:00.000Z', 'old-host');
      INSERT INTO events (ts, host, cwd, query, path, label, confidence, command, exit_code, accepted, ms_system1)
         VALUES ('2026-10-02T09:00:00.000Z', 'old-host', '/w', 'disk use', 'system1', 'du-sh', 0.97, 'du -sh .', 0, 1, 240);
      INSERT INTO meta VALUES ('seeded', '2026-10-01T08:00:00.000Z');
      INSERT INTO meta VALUES ('setting:mode', 'manual');
    ";

    /// Run raw SQL against a brain file, with no `Brain` in between, and return every row as text.
    fn raw(path: &FsPath, sql: &str) -> Vec<String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let db = Builder::new_local(path).build().await.unwrap();
            let conn = db.connect().unwrap();
            let mut out = Vec::new();
            for statement in sql.split(';').map(str::trim).filter(|statement| !statement.is_empty()) {
                let mut rows = conn.query(statement, ()).await.unwrap();
                while let Some(row) = rows.next().await.unwrap() {
                    let values: Vec<String> =
                        (0..row.column_count()).map(|i| format!("{:?}", row.get_value(i).unwrap())).collect();
                    out.push(values.join(" | "));
                }
            }
            out
        })
    }

    /// Every row and table definition a brain had before users existed.
    const DUMP: &str = "SELECT name, sql FROM sqlite_master WHERE name IN ('skills', 'events', 'meta') ORDER BY name;
        SELECT * FROM skills ORDER BY rowid; SELECT * FROM events ORDER BY rowid; SELECT * FROM meta ORDER BY rowid";

    #[test]
    fn a_brain_from_before_users_opens_unchanged() {
        let scratch = Scratch::new("before-users");
        let path = scratch.0.join("brain.db");
        raw(&path, BEFORE_USERS);
        let as_written = raw(&path, DUMP);
        assert_eq!(as_written.len(), 3 + 1 + 1 + 2, "{as_written:#?}");

        let brain = scratch.brain();
        assert_eq!(brain.user(), DEFAULT_USER);
        assert_eq!(labels(&brain), ["du-sh"], "already seeded, so nothing is added");
        let skill = brain.skill("du-sh").unwrap().unwrap();
        assert_eq!((skill.uses, skill.counter_examples.as_slice()), (3, ["free space".to_string()].as_slice()));
        assert_eq!(brain.setting("mode").unwrap().as_deref(), Some("manual"));
        assert_eq!(brain.events(10).unwrap().len(), 1);
        assert_eq!(brain.last_executed().unwrap().unwrap().label.as_deref(), Some("du-sh"));
        assert!(!brain.location().contains(", user "), "the default user is not named");

        let agent = scratch.brain_as("agent");
        assert_eq!(agent.skills().unwrap().len(), 9, "a new user starts from the seeds");
        assert!(agent.skill("du-sh").unwrap().is_none());
        assert_eq!(agent.setting("mode").unwrap(), None, "settings are the user's own");
        assert!(agent.events(10).unwrap().is_empty() && agent.last_executed().unwrap().is_none());
        assert!(agent.location().ends_with(", user agent"));
        agent.learn_command("disk use", &["du -sh".into()], Risk::Safe).unwrap();
        agent.log_event(&event("du-sh")).unwrap();
        agent.set_setting("mode", "auto").unwrap();
        drop((agent, brain));

        assert_eq!(raw(&path, DUMP), as_written, "the default user's rows and tables are untouched");
    }

    #[test]
    fn each_user_sees_only_their_own_skills_events_and_settings() {
        let scratch = Scratch::new("isolation");
        let (default, a, b) = (scratch.brain(), scratch.brain_as("a"), scratch.brain_as("b"));
        let (label, created) = a.learn_command("disk use", &["du -sh .".into()], Risk::Safe).unwrap();
        assert_eq!((label.as_str(), created), ("du-sh", true));
        assert!(default.skill("du-sh").unwrap().is_none() && b.skill("du-sh").unwrap().is_none());
        assert!(!default.forget("du-sh").unwrap() && !b.forget("du-sh").unwrap(), "nobody else can forget it");
        assert!(!b.add_example("du-sh", "how big").unwrap() && !b.add_counter_example("du-sh", "how big").unwrap());
        assert!(!b.add_alias("du-sh", "x", "y").unwrap());
        b.record_use("du-sh").unwrap();
        let theirs = a.skill("du-sh").unwrap().unwrap();
        assert_eq!((theirs.examples.as_slice(), theirs.uses), (["disk use".to_string()].as_slice(), 0));

        // Two users can own one label, each with their own command.
        let (mine, _) = default.learn_command("disk use", &["du -sh ~".into()], Risk::Safe).unwrap();
        assert_eq!(mine, "du-sh");
        assert_eq!(a.skill("du-sh").unwrap().unwrap().steps, ["du -sh ."]);
        assert_eq!(default.skill("du-sh").unwrap().unwrap().steps, ["du -sh ~"]);
        assert!(b.forget("git-push").unwrap());
        assert!(default.skill("git-push").unwrap().is_some() && a.skill("git-push").unwrap().is_some());

        // Events have their own ids per user; one user's fix cannot reach another's.
        let theirs = a.log_event(&event("du-sh")).unwrap();
        assert!(default.last_executed().unwrap().is_none() && b.last_executed().unwrap().is_none());
        let own = default.log_event(&event("git-status")).unwrap();
        assert_eq!(own, theirs, "both tables count from 1");
        default.mark_rejected(own).unwrap();
        assert_eq!(a.last_executed().unwrap().unwrap().id, theirs);
        assert_eq!((a.events(10).unwrap().len(), default.events(10).unwrap().len(), b.events(10).unwrap().len()), (1, 1, 0));

        a.set_setting("confidence", "0.8").unwrap();
        assert_eq!(a.setting("confidence").unwrap().as_deref(), Some("0.8"));
        assert_eq!((default.setting("confidence").unwrap(), b.setting("confidence").unwrap()), (None, None));
        default.set_setting("mode", "manual").unwrap();
        assert_eq!(a.setting("mode").unwrap(), None, "no fallback to the default user's setting");
    }

    #[test]
    fn user_ids_are_short_slugs() {
        for good in ["default", "agent", "a", "claude-2", "ci_bot", &"x".repeat(32)] {
            assert_eq!(user_id(good).unwrap(), good);
        }
        for bad in ["", "Agent", "-agent", "_x", "a b", "a/b", "ä", &"x".repeat(33)] {
            assert!(user_id(bad).is_err(), "{bad:?}");
        }
        let scratch = Scratch::new("bad-user");
        assert!(Brain::open_as(BrainConfig::file(&scratch.0.join("brain.db")), "No Such").is_err());
    }

    #[test]
    fn sharing_copies_one_skill_as_it_is_now() {
        let scratch = Scratch::new("share");
        let (a, b) = (scratch.brain_as("a"), scratch.brain_as("b"));
        let slot = Slot {
            name: "directory".into(),
            kind: crate::types::SlotKind::File,
            description: "where".into(),
            default: None,
            aliases: vec!["dir".into()],
        };
        a.add_skill(&NewSkill {
            label: "count-files".into(),
            steps: vec!["ls {directory} | wc -l".into()],
            scenario: "the user wants the number of entries in a directory".into(),
            risk: Risk::Safe,
            origin: Origin::Agent,
            examples: vec!["how many files in src".into()],
            slots: vec![slot],
        })
        .unwrap();
        a.add_counter_example("count-files", "count the lines").unwrap();
        a.record_use("count-files").unwrap();

        assert_eq!(a.share("count-files", "a").unwrap(), Shared::SameUser);
        assert_eq!(a.share("no-such-skill", "b").unwrap(), Shared::NoSuchSkill);
        assert!(a.share("count-files", "Not A User").is_err());
        assert_eq!(a.share("count-files", "b").unwrap(), Shared::Copied);
        let copy = b.skill("count-files").unwrap().unwrap();
        assert_eq!(copy.steps, ["ls {directory} | wc -l"]);
        assert_eq!(copy.scenario, "the user wants the number of entries in a directory");
        assert_eq!((copy.risk, copy.origin, copy.uses), (Risk::Safe, Origin::Agent, 0));
        assert_eq!(copy.slots[0].aliases, ["dir"]);
        assert_eq!(copy.examples, ["how many files in src"]);
        assert!(copy.counter_examples.is_empty(), "counter-examples stay with the sharer");
        assert_eq!(b.shared_from().unwrap(), HashMap::from([("count-files".to_string(), "a".to_string())]));
        assert!(a.shared_from().unwrap().is_empty());

        // A copy, not a link.
        a.add_example("count-files", "entries in docs").unwrap();
        assert_eq!(b.skill("count-files").unwrap().unwrap().examples.len(), 1);
        assert_eq!(a.share("count-files", "b").unwrap(), Shared::AlreadyRuns("count-files".into()));

        // A label the other user already uses for something else is left alone.
        assert!(b.forget("count-files").unwrap());
        b.add_skill(&NewSkill {
            label: "count-files".into(),
            steps: vec!["find . -type f | wc -l".into()],
            scenario: "theirs".into(),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![],
        })
        .unwrap();
        assert!(b.shared_from().unwrap().is_empty(), "a row taught again is no longer the shared one");
        assert_eq!(a.share("count-files", "b").unwrap(), Shared::LabelTaken);
        assert_eq!(b.skill("count-files").unwrap().unwrap().scenario, "theirs");

        // A user who has never been used is seeded before anything is compared.
        assert_eq!(a.share("git-commit", "fresh").unwrap(), Shared::AlreadyRuns("git-commit".into()));
        assert_eq!(scratch.brain_as("fresh").skills().unwrap().len(), 9);

        // Sharing into the default user works the same way.
        assert_eq!(a.share("count-files", DEFAULT_USER).unwrap(), Shared::Copied);
        let default = scratch.brain();
        assert_eq!(default.skill("count-files").unwrap().unwrap().slots[0].aliases, ["dir"]);
        assert_eq!(default.shared_from().unwrap().get("count-files").map(String::as_str), Some("a"));
    }

    #[test]
    fn a_write_between_the_read_and_the_write_is_not_lost() {
        let scratch = Scratch::new("cas");
        let (mine, theirs) = (scratch.brain(), scratch.brain());
        let interleaved = std::cell::Cell::new(false);
        // The edit runs after the read: another connection writes then, exactly where the old code lost it.
        let added = mine
            .edit_list("list-files", ListColumn::Examples, |list| {
                if !interleaved.replace(true) {
                    assert!(theirs.add_example("list-files", "theirs").unwrap());
                }
                with(list, "mine")
            })
            .unwrap();
        assert!(added);
        assert_eq!(theirs.skill("list-files").unwrap().unwrap().examples, ["theirs", "mine"]);

        let interleaved = std::cell::Cell::new(false);
        let renamed = mine
            .edit_column("git-commit", "slots", |stored| {
                if !interleaved.replace(true) {
                    assert!(theirs.add_alias("git-commit", "message", "msg").unwrap());
                }
                let mut slots: Vec<Slot> = json(stored);
                add_alias(&mut slots, "message", "text");
                Ok(Some(serde_json::to_string(&slots)?))
            })
            .unwrap();
        assert!(renamed);
        assert_eq!(theirs.skill("git-commit").unwrap().unwrap().slots[0].aliases, ["msg", "text"]);
    }

    #[test]
    fn parallel_writers_keep_every_phrasing() {
        let scratch = Scratch::new("parallel");
        drop(scratch.brain());
        let path = scratch.0.join("brain.db");
        let start = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            for writer in 0..3 {
                let (path, start) = (&path, &start);
                scope.spawn(move || {
                    let brain = Brain::open(BrainConfig::file(path)).unwrap();
                    start.wait();
                    for n in 0..5 {
                        brain.add_example("git-status", &format!("writer {writer} phrase {n}")).unwrap();
                        if n < 2 {
                            assert!(brain.add_alias("git-commit", "message", &format!("w{writer}_{n}")).unwrap());
                        }
                    }
                });
            }
        });
        let brain = scratch.brain();
        let skill = brain.skill("git-status").unwrap().unwrap();
        assert_eq!(skill.examples.len(), 1 + 15, "the seed's own and every writer's: {:?}", skill.examples);
        assert_eq!(brain.skill("git-commit").unwrap().unwrap().slots[0].aliases.len(), 6);
    }

    #[test]
    fn a_label_another_writer_took_is_never_overwritten() {
        let scratch = Scratch::new("labels");
        let (mine, theirs) = (scratch.brain(), scratch.brain());
        let skill = |label: &str, step: &str| NewSkill {
            label: label.into(),
            steps: vec![step.into()],
            scenario: format!("runs {step}"),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: vec![],
            slots: vec![],
        };
        // Both looked when du-sh was free; theirs was written first.
        assert_eq!(theirs.add_new_skill(&skill("du-sh", "du -h")).unwrap(), "du-sh");
        assert_eq!(mine.add_new_skill(&skill("du-sh", "du -sh .")).unwrap(), "du-sh-x");
        assert_eq!(mine.skill("du-sh").unwrap().unwrap().steps, ["du -h"]);
        assert_eq!(mine.skill("du-sh-x").unwrap().unwrap().steps, ["du -sh ."]);

        // Four writers teaching four commands with one slug at once each keep their own command.
        drop((mine, theirs));
        let path = scratch.0.join("brain.db");
        let commands = ["du -s", "du -s .", "du  -s", "du -s ./"];
        let start = std::sync::Barrier::new(commands.len());
        let learned: Vec<String> = std::thread::scope(|scope| {
            let writers: Vec<_> = commands
                .iter()
                .map(|command| {
                    let (path, start) = (&path, &start);
                    scope.spawn(move || {
                        let brain = Brain::open(BrainConfig::file(path)).unwrap();
                        start.wait();
                        brain.learn_command(&format!("size via {command}"), &[command.to_string()], Risk::Safe).unwrap().0
                    })
                })
                .collect();
            writers.into_iter().map(|writer| writer.join().unwrap()).collect()
        });
        let brain = scratch.brain();
        for (label, command) in learned.iter().zip(commands) {
            let skill = brain.skill(label).unwrap().unwrap();
            assert_eq!(skill.steps, [command], "{label}");
            assert_eq!(skill.examples, [format!("size via {command}")]);
        }
        let mut distinct = learned.clone();
        distinct.sort();
        assert_eq!(distinct, ["du-s", "du-s-x", "du-s-x-x", "du-s-x-x-x"]);
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
