use std::collections::HashMap;
use std::future::Future;
use std::path::{Path as FsPath, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _, Result};
use libsql::{Builder, Connection, Database, Value};
use tokio::runtime::Runtime;

use crate::seed::seed_skills;
use crate::slots::{keep_settings, match_proposal};
use crate::types::{add_alias, coded, remove_alias, BrainEvent, Origin, Path, Risk, Skill, Slot, STORAGE};

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
  CREATE TABLE IF NOT EXISTS plans (
     id            INTEGER PRIMARY KEY AUTOINCREMENT,
     user_id       TEXT NOT NULL,
     project_id    TEXT NOT NULL DEFAULT '',
     created_at    TEXT NOT NULL,
     expires_at    TEXT NOT NULL,
     host          TEXT NOT NULL,
     cwd           TEXT NOT NULL,
     state         TEXT NOT NULL,
     body          TEXT NOT NULL,
     commands_hash TEXT NOT NULL,
     ran_at        TEXT,
     event_id      INTEGER
   );
  CREATE TABLE IF NOT EXISTS corrections (
     id              INTEGER PRIMARY KEY AUTOINCREMENT,
     user_id         TEXT NOT NULL,
     project_id      TEXT NOT NULL DEFAULT '',
     event_id        INTEGER NOT NULL,
     ts              TEXT NOT NULL,
     host            TEXT NOT NULL,
     terminal        INTEGER NOT NULL,
     how             TEXT NOT NULL,
     detail          TEXT,
     result_event_id INTEGER
   );
";

/// What a project has learned about the user's global skills (ADR 17): phrasings and counter-examples
/// that hold inside that project only. A lesson is about one row: it names the row's `created_at`,
/// so a global skill forgotten and taught again does not inherit what was learned about the old one.
const PROJECT_SCHEMA: &str = "
  CREATE TABLE IF NOT EXISTS project_lessons (
     user_id          TEXT NOT NULL,
     project_id       TEXT NOT NULL,
     label            TEXT NOT NULL,
     created_at       TEXT NOT NULL,
     examples         TEXT NOT NULL DEFAULT '[]',
     counter_examples TEXT NOT NULL DEFAULT '[]',
     PRIMARY KEY (user_id, project_id, label, created_at)
   );
";

/// A scenario a project keeps for one of the user's global skills (ADR 22): `jli eval --optimise` measured
/// the rewrite inside that project, so it holds there and nowhere else. Keyed like a lesson, by the
/// global row's `created_at`, so a global skill forgotten and taught again starts clean.
const SCENARIO_SCHEMA: &str = "
  CREATE TABLE IF NOT EXISTS project_scenarios (
     user_id     TEXT NOT NULL,
     project_id  TEXT NOT NULL,
     label       TEXT NOT NULL,
     created_at  TEXT NOT NULL,
     scenario    TEXT NOT NULL,
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
/// project. `project` is empty outside every project, and is the `.jlirc` id inside one (ADR 17).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    user: String,
    project: String,
}

impl Scope {
    fn user(user: &str) -> Scope {
        Scope { user: user.to_string(), project: String::new() }
    }

    fn in_project(user: &str, project: &str) -> Scope {
        Scope { user: user.to_string(), project: project.to_string() }
    }

    fn has_project(&self) -> bool {
        !self.project.is_empty()
    }

    /// The same user outside every project: whose global skills and settings these are.
    fn base(&self) -> Scope {
        Scope::user(&self.user)
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

/// Where `promote` puts a project skill.
#[derive(Debug, Clone, PartialEq)]
pub enum Promote {
    /// A new global skill. Its label is claimed like any new label.
    New(NewSkill),
    /// The global skill of this label, which already runs the command: it gains the project's phrasings.
    Into(String),
}

/// What a project learned about one of the user's global skills.
#[derive(Debug, Clone, Default, PartialEq)]
struct Lesson {
    examples: Vec<String>,
    counter_examples: Vec<String>,
}

/// A global row as one project sees it: a counter-example learned there hides the same phrasing in the
/// row's examples, and a phrasing learned there lifts the same counter-example. The newest are kept.
fn with_lesson(skill: &mut Skill, lesson: &Lesson) {
    let holds = |list: &[String], phrase: &str| list.iter().any(|entry| same(entry, phrase));
    let merge = |row: &[String], hide: &[String], add: &[String]| {
        let mut out: Vec<String> = row.iter().filter(|entry| !holds(hide, entry)).cloned().collect();
        for entry in add {
            if !holds(&out, entry) {
                out.push(entry.clone());
            }
        }
        let excess = out.len().saturating_sub(MAX_EXAMPLES);
        out.drain(..excess);
        out
    };
    let examples = merge(&skill.examples, &lesson.counter_examples, &lesson.examples);
    let counter_examples = merge(&skill.counter_examples, &lesson.examples, &lesson.counter_examples);
    (skill.examples, skill.counter_examples) = (examples, counter_examples);
}

/// The phrasings and scenarios one project keeps about the user's global rows, by label and `created_at`.
struct Kept {
    lessons: HashMap<(String, String), Lesson>,
    scenarios: HashMap<(String, String), String>,
}

impl Kept {
    /// A global row as the project sees it: with its lesson about the row, and its own scenario if it keeps one.
    fn apply(&self, skill: &mut Skill) {
        let key = (skill.label.clone(), skill.created_at.clone());
        if let Some(lesson) = self.lessons.get(&key) {
            with_lesson(skill, lesson);
        }
        if let Some(scenario) = self.scenarios.get(&key) {
            skill.scenario = scenario.clone();
        }
    }
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
    iso_after(Duration::ZERO)
}

/// The same format, `later` from now. Text in this format sorts as the times it holds.
pub fn iso_after(later: Duration) -> String {
    let millis = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default() + later).as_millis() as i64;
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

/// A stored plan, as `jli run` reads it back. `body` is the plan itself, as JSON written by `plan.rs`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRow {
    pub id: i64,
    pub created_at: String,
    pub expires_at: String,
    pub cwd: String,
    /// `ready`, `incomplete`, `running` once claimed, `ran` once its event is logged.
    pub state: String,
    pub body: String,
    pub commands_hash: String,
}

/// One `jli fix`, kept beside the event log so an audit can tell who corrected what.
#[derive(Debug, Clone, PartialEq)]
pub struct Correction {
    /// The event that was corrected.
    pub event_id: i64,
    /// Whether a terminal was there to answer: false for an agent or a script.
    pub terminal: bool,
    /// `unlearn`, `command` or `expected`.
    pub how: String,
    /// The command given, or the words of what was expected.
    pub detail: Option<String>,
    /// The event the correction itself logged, if it logged one.
    pub result_event_id: Option<i64>,
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
        Brain::open_in(config, user, None)
    }

    /// The brain as `user` sees it inside `project`: the project's own skills and events, and the
    /// user's global skills and settings. With no project it is `open_as`.
    pub fn open_in(config: BrainConfig, user: &str, project: Option<&str>) -> Result<Brain> {
        let user = user_id(user)?;
        let scope = match project {
            Some(project) => Scope::in_project(&user, project),
            None => Scope::user(&user),
        };
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
        brain.block(brain.conn().execute_batch(PROJECT_SCHEMA)).context("could not create the brain's tables")?;
        brain.block(brain.conn().execute_batch(SCENARIO_SCHEMA)).context("could not create the brain's tables")?;
        // A project starts from the user's own skills, not from a copy of the seeds.
        brain.seed_once(&brain.scope.base())?;
        Ok(brain)
    }

    /// Whose skills, events and settings this brain reads and writes.
    pub fn user(&self) -> &str {
        &self.scope.user
    }

    /// The project this brain is opened in, if any.
    pub fn project(&self) -> Option<&str> {
        self.scope.has_project().then_some(self.scope.project.as_str())
    }

    /// Run `act` against the user's global skills, as if outside every project: `teach --global`.
    pub fn globally<T>(&mut self, act: impl FnOnce(&Brain) -> T) -> T {
        let project = std::mem::take(&mut self.scope.project);
        let done = act(self);
        self.scope.project = project;
        done
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
        let place = if self.scope.base().is_legacy() { place } else { format!("{place}, user {}", self.scope.user) };
        match self.project() {
            Some(project) => format!("{place}, project {project}"),
            None => place,
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

    /// The matrix: outside a project the user's skills; inside one, the project's own skills and the
    /// user's global skills, each global one with what this project learned about it.
    pub fn skills(&self) -> Result<Vec<Skill>> {
        self.view(&self.scope)
    }

    fn skills_of(&self, scope: &Scope) -> Result<Vec<Skill>> {
        let (table, rows) = (scope.skills(), scope.rows());
        self.query(
            &format!("SELECT {SKILL_COLUMNS} FROM {table} WHERE {rows} ORDER BY uses DESC, label ASC"),
            scope.args(vec![]),
            to_skill,
        )
    }

    /// What a scope routes against. A project row shadows a global row of the same label: the
    /// narrower scope wins. Ordered as `skills_of` orders one table.
    fn view(&self, scope: &Scope) -> Result<Vec<Skill>> {
        if !scope.has_project() {
            return self.skills_of(scope);
        }
        let mut skills = self.skills_of(scope)?;
        let kept = self.kept_by(scope)?;
        for mut skill in self.skills_of(&scope.base())? {
            if skills.iter().any(|own| own.label == skill.label) {
                continue;
            }
            kept.apply(&mut skill);
            skills.push(skill);
        }
        skills.sort_by(|a, b| b.uses.cmp(&a.uses).then_with(|| a.label.cmp(&b.label)));
        Ok(skills)
    }

    pub fn skill(&self, label: &str) -> Result<Option<Skill>> {
        if let Some(own) = self.row(&self.scope, label)? {
            return Ok(Some(own));
        }
        if !self.scope.has_project() {
            return Ok(None);
        }
        let Some(mut global) = self.row(&self.scope.base(), label)? else { return Ok(None) };
        self.kept_by(&self.scope)?.apply(&mut global);
        Ok(Some(global))
    }

    /// One scope's own row of `label`, as stored.
    fn row(&self, scope: &Scope, label: &str) -> Result<Option<Skill>> {
        let (table, rows) = (scope.skills(), scope.rows());
        let rows = self.query(
            &format!("SELECT {SKILL_COLUMNS} FROM {table} WHERE {rows} AND label = ?"),
            scope.args(vec![label.into()]),
            to_skill,
        )?;
        Ok(rows.into_iter().next())
    }

    /// The project's own skills, without the global ones. Empty outside a project.
    pub fn project_skills(&self) -> Result<Vec<Skill>> {
        if !self.scope.has_project() {
            return Ok(vec![]);
        }
        self.skills_of(&self.scope)
    }

    /// The user's global skills as stored, without any project's lessons.
    pub fn global_skills(&self) -> Result<Vec<Skill>> {
        self.skills_of(&self.scope.base())
    }

    /// Every project this user has skills in, by id.
    pub fn projects(&self) -> Result<Vec<String>> {
        self.query(
            "SELECT DISTINCT project_id FROM scoped_skills WHERE user_id = ? AND project_id != '' ORDER BY project_id",
            vec![self.scope.user.clone().into()],
            |row| text(row.get_value(0).unwrap_or(Value::Null)),
        )
    }

    /// One project's own skills, for this user.
    pub fn skills_in(&self, project: &str) -> Result<Vec<Skill>> {
        self.skills_of(&Scope::in_project(&self.scope.user, project))
    }

    /// The matrix this user routes against inside `project`.
    pub fn view_of(&self, project: &str) -> Result<Vec<Skill>> {
        self.view(&Scope::in_project(&self.scope.user, project))
    }

    /// The global skills this project has lessons about, with how many phrasings and counter-examples.
    pub fn project_lessons(&self) -> Result<Vec<(String, usize, usize)>> {
        if !self.scope.has_project() {
            return Ok(vec![]);
        }
        let lessons = self.lessons_of(&self.scope)?;
        let mut out: Vec<(String, usize, usize)> = self
            .global_skills()?
            .into_iter()
            .filter_map(|skill| {
                let lesson = lessons.get(&(skill.label.clone(), skill.created_at))?;
                let counts = (lesson.examples.len(), lesson.counter_examples.len());
                (counts != (0, 0)).then_some((skill.label, counts.0, counts.1))
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// A project's lessons, by the label and `created_at` of the global row each is about.
    fn lessons_of(&self, scope: &Scope) -> Result<HashMap<(String, String), Lesson>> {
        let rows = self.query(
            "SELECT label, created_at, examples, counter_examples FROM project_lessons WHERE user_id = ? AND project_id = ?",
            vec![scope.user.clone().into(), scope.project.clone().into()],
            |row| {
                let mut c = Columns::new(row);
                let key = (text(c.take()), text(c.take()));
                (key, Lesson { examples: json(c.take()), counter_examples: json(c.take()) })
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    /// The scenarios a project keeps for global rows, by the label and `created_at` of each row.
    fn scenarios_of(&self, scope: &Scope) -> Result<HashMap<(String, String), String>> {
        let rows = self.query(
            "SELECT label, created_at, scenario FROM project_scenarios WHERE user_id = ? AND project_id = ?",
            vec![scope.user.clone().into(), scope.project.clone().into()],
            |row| {
                let mut c = Columns::new(row);
                ((text(c.take()), text(c.take())), text(c.take()))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    fn kept_by(&self, scope: &Scope) -> Result<Kept> {
        Ok(Kept { lessons: self.lessons_of(scope)?, scenarios: self.scenarios_of(scope)? })
    }

    /// Inside a project with no row of `label`, the user's global row of it, as stored.
    fn global_row(&self, label: &str) -> Result<Option<Skill>> {
        if !self.scope.has_project() || self.row(&self.scope, label)?.is_some() {
            return Ok(None);
        }
        self.row(&self.scope.base(), label)
    }

    /// The scenario this project keeps for the global skill `label` (ADR 22). None outside a project,
    /// for a project's own skill, and for a global skill seen here with the global row's scenario.
    pub fn project_scenario(&self, label: &str) -> Result<Option<String>> {
        let Some(global) = self.global_row(label)? else { return Ok(None) };
        Ok(self.scenarios_of(&self.scope)?.remove(&(global.label, global.created_at)))
    }

    /// The global skills this project sees with a scenario of its own, by label.
    pub fn project_scenarios(&self) -> Result<Vec<String>> {
        if !self.scope.has_project() {
            return Ok(vec![]);
        }
        let own: Vec<String> = self.skills_of(&self.scope)?.into_iter().map(|skill| skill.label).collect();
        let scenarios = self.scenarios_of(&self.scope)?;
        let mut out: Vec<String> = self
            .global_skills()?
            .into_iter()
            .filter(|skill| !own.contains(&skill.label))
            .filter(|skill| scenarios.contains_key(&(skill.label.clone(), skill.created_at.clone())))
            .map(|skill| skill.label)
            .collect();
        out.sort();
        Ok(out)
    }

    /// Change when a skill applies, as this brain sees it. Inside a project, a skill only the user's
    /// global skills hold gets a scenario of the project's own, and the global row is not written: a
    /// rewrite measured in one project is that project's (ADR 22). Otherwise the row's own scenario
    /// changes. Returns false when there is no such skill or it already reads that way here.
    ///
    /// The global row's own scenario is never kept as the project's: that would stop a later rewrite
    /// of the global row from reaching the project. Asked for it, the project drops its own instead.
    pub fn set_scenario(&self, label: &str, scenario: &str) -> Result<bool> {
        let Some(global) = self.global_row(label)? else {
            return self.edit_column_in(&self.scope, label, "scenario", |stored| {
                Ok((text(stored) != scenario).then(|| scenario.to_string()))
            });
        };
        if global.scenario == scenario {
            return self.clear_project_scenario(label);
        }
        let kept = self.scenarios_of(&self.scope)?.remove(&(global.label.clone(), global.created_at.clone()));
        if kept.as_deref() == Some(scenario) {
            return Ok(false);
        }
        self.execute(
            "INSERT INTO project_scenarios (user_id, project_id, label, created_at, scenario) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id, project_id, label, created_at) DO UPDATE SET scenario = excluded.scenario",
            vec![
                self.scope.user.clone().into(),
                self.scope.project.clone().into(),
                global.label.into(),
                global.created_at.into(),
                scenario.into(),
            ],
        )?;
        Ok(true)
    }

    /// Let this project see the global scenario of `label` again. Returns false when it kept none.
    pub fn clear_project_scenario(&self, label: &str) -> Result<bool> {
        let Some(global) = self.global_row(label)? else { return Ok(false) };
        let removed = self.execute(
            "DELETE FROM project_scenarios WHERE user_id = ? AND project_id = ? AND label = ? AND created_at = ?",
            vec![
                self.scope.user.clone().into(),
                self.scope.project.clone().into(),
                global.label.into(),
                global.created_at.into(),
            ],
        )?;
        Ok(removed > 0)
    }

    /// The scope that holds the row this brain sees for `label`: its own, or inside a project the
    /// user's global row when the project has none of that label.
    fn row_scope(&self, label: &str) -> Result<Scope> {
        if self.scope.has_project() && self.row(&self.scope, label)?.is_none() {
            return Ok(self.scope.base());
        }
        Ok(self.scope.clone())
    }

    /// Insert a skill, or change what an existing label runs. Re-teaching a label keeps
    /// what was learned about it: examples, counter-examples and uses are never replaced,
    /// and a slot that keeps its name keeps its aliases and what was set on it by hand (ADR 24).
    ///
    /// No command calls it (ADR 22): a new skill is stored with `add_new_skill`, a literal skill is
    /// made general with `upgrade_skill`, and a rewritten scenario goes through `set_scenario`. It is
    /// kept for tests, which build their brains with it. Inside a project it still writes a label
    /// only the user's global skills hold to the global row, the write ADR 22 took away from
    /// `jli eval --optimise`, so a command run inside a project must not use it.
    pub fn add_skill(&self, skill: &NewSkill) -> Result<()> {
        let mut skill = skill.clone();
        let known = self.skill(&skill.label)?.map(|known| known.slots).unwrap_or_default();
        keep_settings(&mut skill.slots, &known, &skill.steps);
        let global = self.scope.base();
        let only_global = self.scope.has_project()
            && self.row(&self.scope, &skill.label)?.is_none()
            && self.row(&global, &skill.label)?.is_some();
        self.insert_skill(
            if only_global { &global } else { &self.scope },
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
    /// A label that another writer took after this one looked is never overwritten. If the row that
    /// holds a label runs the same command, the skill is that row: its phrasings are added to it and
    /// nothing new is written. Returns the label, and whether a new row was written.
    pub fn add_new_skill(&self, skill: &NewSkill) -> Result<(String, bool)> {
        self.claim(&self.scope, skill)
    }

    /// Inside a project a label the user's global skills hold is taken too, so a new project skill
    /// never hides a global one by accident.
    ///
    /// Two writers teaching one command at once both pick its label; the second finds it taken by the
    /// same command and joins it, so the phrasings collect on one row instead of `du-sh` and `du-sh-x`
    /// (ADR 19). "The same command" is `match_proposal`: the row's template produces this skill's, so a
    /// literal row never stands in for a template.
    fn claim(&self, scope: &Scope, skill: &NewSkill) -> Result<(String, bool)> {
        let mut skill = skill.clone();
        for _ in 0..LABEL_ATTEMPTS {
            let held_globally = scope.has_project() && self.row(&scope.base(), &skill.label)?.is_some();
            if !held_globally && self.insert_skill(scope, &skill, "DO NOTHING")? > 0 {
                return Ok((skill.label, true));
            }
            let holder = match self.row(scope, &skill.label)? {
                Some(own) => Some(own),
                None if scope.has_project() => self.row(&scope.base(), &skill.label)?,
                None => None,
            };
            if holder
                .is_some_and(|holder| match_proposal(&holder.steps, &holder.slots, &skill.steps, &HashMap::new()).is_some())
            {
                for phrase in &skill.examples {
                    self.link_in(scope, &skill.label, phrase)?;
                }
                return Ok((skill.label, false));
            }
            skill.label.push_str("-x");
        }
        Err(coded(STORAGE, format!("could not find a free label for {}", skill.label)))
    }

    /// Add a phrasing to the row `scope` sees under `label`, and drop it from that row's counter-examples.
    /// In this brain's own scope that is `link_phrase`, so inside a project a global row gets a lesson;
    /// in another scope (`promote` writing the global skills) the row itself is written.
    fn link_in(&self, scope: &Scope, label: &str, phrase: &str) -> Result<()> {
        if scope == &self.scope {
            self.link_phrase(label, phrase)?;
            return Ok(());
        }
        let edit = |list: Vec<String>| with(list, phrase);
        self.edit_column_in(scope, label, ListColumn::Examples.name(), |stored| {
            Ok(edit(json(stored)).map(|next| serde_json::to_string(&next)).transpose()?)
        })?;
        let edit = |list: Vec<String>| without(list, phrase);
        self.edit_column_in(scope, label, ListColumn::CounterExamples.name(), |stored| {
            Ok(edit(json(stored)).map(|next| serde_json::to_string(&next)).transpose()?)
        })?;
        Ok(())
    }

    /// True when this brain's own scope holds `label`: outside a project any skill, inside one a
    /// project skill and not a global one seen there.
    pub fn owns(&self, label: &str) -> Result<bool> {
        Ok(self.row(&self.scope, label)?.is_some())
    }

    /// Make a literal skill general in place (ADR 19): it runs `to`'s steps with `to`'s slots, scenario
    /// and risk, and keeps everything learned about it, as an upsert does (ADR 2): examples,
    /// counter-examples, uses, origin, `created_at`, and the aliases and hand settings of a slot that
    /// keeps its name (ADR 24). Written only in this brain's own scope, and only if the row still runs `from`, with one
    /// conditional statement on its steps and slots, as a list is written (ADR 15). Returns false when
    /// the row is not this scope's, has gone, or another writer changed it first.
    pub fn upgrade_skill(&self, label: &str, from: &[String], to: &NewSkill) -> Result<bool> {
        let (table, rows) = (self.scope.skills(), self.scope.rows());
        let read = self.query(
            &format!("SELECT steps, slots FROM {table} WHERE {rows} AND label = ?"),
            self.scope.args(vec![label.into()]),
            |row| (row.get_value(0).unwrap_or(Value::Null), row.get_value(1).unwrap_or(Value::Null)),
        )?;
        let Some((steps, slots)) = read.into_iter().next() else { return Ok(false) };
        if json::<Vec<String>>(steps.clone()) != from {
            return Ok(false);
        }
        let mut next = to.slots.clone();
        keep_settings(&mut next, &json::<Vec<Slot>>(slots.clone()), &to.steps);
        let written = self.execute(
            &format!(
                "UPDATE {table} SET steps = ?, slots = ?, scenario = ?, risk = ?
                 WHERE {rows} AND label = ? AND steps IS ? AND slots IS ?"
            ),
            [
                vec![
                    serde_json::to_string(&to.steps)?.into(),
                    serde_json::to_string(&next)?.into(),
                    to.scenario.clone().into(),
                    to.risk.as_str().into(),
                ],
                self.scope.args(vec![label.into(), steps, slots]),
            ]
            .concat(),
        )?;
        Ok(written > 0)
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
        self.add_new_skill(&NewSkill {
            label: slug(&wanted),
            steps: steps.to_vec(),
            scenario: format!("the user wants: {phrase}"),
            risk,
            origin: Origin::Taught,
            examples: vec![phrase.to_string()],
            slots: vec![],
        })
    }

    /// Add a phrasing to a skill, and drop it from that skill's counter-examples if it was one.
    /// Returns false when it was already an example and not a counter-example: nothing changed.
    pub fn link_phrase(&self, label: &str, phrase: &str) -> Result<bool> {
        let added = self.add_example(label, phrase)?;
        let lifted = self.edit_list(label, ListColumn::CounterExamples, |list| without(list, phrase))?;
        Ok(added || lifted)
    }

    /// This user's own setting. A user without one gets the built-in default, not the default user's.
    /// Settings belong to the user, so they are the same inside every project.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.meta(&self.scope.base(), &format!("setting:{key}"))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.set_meta(&self.scope.base(), &format!("setting:{key}"), value, "DO UPDATE SET value = excluded.value")
    }

    /// Write a `meta` entry for this brain's user, replacing the value under the same key.
    pub fn put_meta(&self, key: &str, value: &str) -> Result<()> {
        self.set_meta(&self.scope, key, value, "DO UPDATE SET value = excluded.value")
    }

    /// Every `meta` entry of this brain's user whose key starts with `prefix`, in key order.
    pub fn meta_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let sql = if self.scope.is_legacy() {
            "SELECT key, value FROM meta WHERE substr(key, 1, length(?)) = ? ORDER BY key"
        } else {
            "SELECT key, value FROM scoped_meta WHERE user_id = ? AND project_id = ? AND substr(key, 1, length(?)) = ? ORDER BY key"
        };
        self.query(sql, self.scope.args(vec![prefix.into(), prefix.into()]), |row| {
            (text(row.get_value(0).unwrap_or(Value::Null)), text(row.get_value(1).unwrap_or(Value::Null)))
        })
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
    ///
    /// Inside a project, a project skill goes to the other user's copy of the same project, and a
    /// global skill to their global skills, as stored: what this project learned about it stays here.
    pub fn share(&self, label: &str, to: &str) -> Result<Shared> {
        let to = user_id(to)?;
        if to == self.scope.user {
            return Ok(Shared::SameUser);
        }
        let (skill, target) = match self.row(&self.scope, label)? {
            Some(own) => (Some(own), Scope::in_project(&to, &self.scope.project)),
            None if self.scope.has_project() => (self.row(&self.scope.base(), label)?, Scope::user(&to)),
            None => (None, Scope::user(&to)),
        };
        let Some(skill) = skill else { return Ok(Shared::NoSuchSkill) };
        self.seed_once(&target.base())?;
        let theirs = self.view(&target)?;
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
        let created_at = self.row(&target, &copy.label)?.map(|known| known.created_at).unwrap_or_default();
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
    /// Inside a project it covers the project's skills and the global skills it sees.
    pub fn shared_from(&self) -> Result<HashMap<String, String>> {
        let own = self.shared_into(&self.scope)?;
        if !self.scope.has_project() {
            return Ok(own);
        }
        let shadowed: Vec<String> = self.skills_of(&self.scope)?.into_iter().map(|skill| skill.label).collect();
        let mut from = self.shared_into(&self.scope.base())?;
        from.retain(|label, _| !shadowed.contains(label));
        from.extend(own);
        Ok(from)
    }

    fn shared_into(&self, scope: &Scope) -> Result<HashMap<String, String>> {
        let (table, rows) = (scope.skills(), scope.rows());
        let rows = self.query(
            &format!(
                "SELECT label, from_user FROM shares
                 WHERE user_id = ? AND project_id = ?
                   AND EXISTS (SELECT 1 FROM {table} WHERE {rows} AND label = shares.label AND created_at = shares.created_at)"
            ),
            [vec![scope.user.clone().into(), scope.project.clone().into()], scope.args(vec![])].concat(),
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

    pub fn remove_counter_example(&self, label: &str, phrase: &str) -> Result<bool> {
        self.edit_list(label, ListColumn::CounterExamples, |list| without(list, phrase))
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

    /// Write what a person set on one slot of `label` (`jli slot`, ADR 24): its kind, description,
    /// default, whether it is optional, its flag and its choices. Its aliases stay as the row holds them,
    /// so another writer's new name is not lost. A placeholder no slot declares gets this slot. It is
    /// written where `add_alias` writes: the row that holds the skill. Returns false if nothing changed.
    pub fn set_slot(&self, label: &str, slot: &Slot) -> Result<bool> {
        self.edit_column(label, "slots", |stored| {
            let mut slots: Vec<Slot> = json(stored);
            match slots.iter_mut().find(|known| known.name == slot.name) {
                Some(known) => {
                    let next = Slot { aliases: known.aliases.clone(), ..slot.clone() };
                    if *known == next {
                        return Ok(None);
                    }
                    *known = next;
                }
                None => slots.push(slot.clone()),
            }
            Ok(Some(serde_json::to_string(&slots)?))
        })
    }

    /// Take `alias` from a slot of `label`, by hand (`jli alias --remove`, ADR 21). It is written where
    /// `add_alias` writes: the row that holds the skill. Returns false if nothing changed.
    pub fn remove_alias(&self, label: &str, slot: &str, alias: &str) -> Result<bool> {
        self.edit_column(label, "slots", |stored| {
            let mut slots: Vec<Slot> = json(stored);
            if !remove_alias(&mut slots, slot, alias) {
                return Ok(None);
            }
            Ok(Some(serde_json::to_string(&slots)?))
        })
    }

    /// Inside a project, a phrasing or counter-example for a global skill is a lesson of the project's
    /// own: the global row is never written, so another project never sees it (ADR 17).
    fn edit_list(&self, label: &str, column: ListColumn, edit: impl Fn(Vec<String>) -> Option<Vec<String>>) -> Result<bool> {
        let scope = self.row_scope(label)?;
        if scope != self.scope {
            let Some(seen) = self.skill(label)? else { return Ok(false) };
            // Judged on the row as this project sees it: a phrasing the global row already has is known here.
            let list = match column {
                ListColumn::Examples => seen.examples.clone(),
                ListColumn::CounterExamples => seen.counter_examples.clone(),
            };
            if edit(list).is_none() {
                return Ok(false);
            }
            return self.edit_lesson(&self.scope, label, &seen.created_at, column, edit);
        }
        self.edit_column(label, column.name(), |stored| {
            Ok(edit(json(stored)).map(|next| serde_json::to_string(&next)).transpose()?)
        })
    }

    /// Change one list of a project's lesson about a global row, with the same compare-and-swap as
    /// `edit_column`. The lesson is created the first time something is written to it.
    fn edit_lesson(
        &self,
        scope: &Scope,
        label: &str,
        created_at: &str,
        column: ListColumn,
        edit: impl Fn(Vec<String>) -> Option<Vec<String>>,
    ) -> Result<bool> {
        let column = column.name();
        let key = || -> Vec<Value> {
            vec![scope.user.clone().into(), scope.project.clone().into(), label.into(), created_at.into()]
        };
        for _ in 0..WRITE_ATTEMPTS {
            let read = self.query(
                &format!(
                    "SELECT {column} FROM project_lessons WHERE user_id = ? AND project_id = ? AND label = ? AND created_at = ?"
                ),
                key(),
                |row| row.get_value(0).unwrap_or(Value::Null),
            )?;
            let written = match read.into_iter().next() {
                Some(stored) => {
                    let Some(next) = edit(json(stored.clone())) else { return Ok(false) };
                    self.execute(
                        &format!(
                            "UPDATE project_lessons SET {column} = ?
                             WHERE user_id = ? AND project_id = ? AND label = ? AND created_at = ? AND {column} IS ?"
                        ),
                        [vec![serde_json::to_string(&next)?.into()], key(), vec![stored]].concat(),
                    )?
                }
                None => {
                    let Some(next) = edit(vec![]) else { return Ok(false) };
                    self.execute(
                        &format!(
                            "INSERT INTO project_lessons (user_id, project_id, label, created_at, {column}) VALUES (?, ?, ?, ?, ?)
                             ON CONFLICT(user_id, project_id, label, created_at) DO NOTHING"
                        ),
                        [key(), vec![serde_json::to_string(&next)?.into()]].concat(),
                    )?
                }
            };
            if written > 0 {
                return Ok(true);
            }
            self.sync()?;
        }
        Err(anyhow!("what this project learned about {label} kept changing while it was being written, so nothing was written. Try again."))
    }

    /// Change one column of one skill without losing another writer's change to it. The new value
    /// is written only if the column still holds what was read; otherwise it is read and edited
    /// again. One conditional statement, so it holds on a file, a remote and a replica alike, and
    /// nothing is held open while `edit` runs. Returns false when `edit` changes nothing.
    ///
    /// Inside a project with no row of `label`, the row is the user's global one: an alias names a
    /// value, not a route, so it is kept where the skill is.
    fn edit_column(&self, label: &str, column: &str, edit: impl Fn(Value) -> Result<Option<String>>) -> Result<bool> {
        self.edit_column_in(&self.row_scope(label)?, label, column, edit)
    }

    fn edit_column_in(
        &self,
        scope: &Scope,
        label: &str,
        column: &str,
        edit: impl Fn(Value) -> Result<Option<String>>,
    ) -> Result<bool> {
        let (table, rows) = (scope.skills(), scope.rows());
        for _ in 0..WRITE_ATTEMPTS {
            let read = self.query(
                &format!("SELECT {column} FROM {table} WHERE {rows} AND label = ?"),
                scope.args(vec![label.into()]),
                |row| row.get_value(0).unwrap_or(Value::Null),
            )?;
            let Some(stored) = read.into_iter().next() else { return Ok(false) };
            let Some(next) = edit(stored.clone())? else { return Ok(false) };
            let written = self.execute(
                &format!("UPDATE {table} SET {column} = ? WHERE {rows} AND label = ? AND {column} IS ?"),
                [vec![next.into()], scope.args(vec![label.into(), stored])].concat(),
            )?;
            if written > 0 {
                return Ok(true);
            }
            // Another writer got there first. A replica reads its local copy, so catch up before reading again.
            self.sync()?;
        }
        Err(coded(
            STORAGE,
            format!("{label} kept changing while its {column} were being written, so nothing was written. Try again."),
        ))
    }

    /// Counted on the row that ran, global or the project's: a use is not a lesson.
    pub fn record_use(&self, label: &str) -> Result<()> {
        let scope = self.row_scope(label)?;
        let (table, rows) = (scope.skills(), scope.rows());
        self.execute(
            &format!("UPDATE {table} SET uses = uses + 1, last_used_at = ? WHERE {rows} AND label = ?"),
            [vec![now_iso().into()], scope.args(vec![label.into()])].concat(),
        )?;
        Ok(())
    }

    /// Make one of `project`'s skills global (`jli generalize`, ADR 17). The global row is written
    /// first: a new skill with the project row's phrasings, or the project's phrasings added to the
    /// global skill named. The project row's counter-examples mark where it ends against that
    /// project's other skills, so they become that project's lesson about the global row. Then the
    /// project row is deleted. Returns the global label, or None when the project has no such skill.
    pub fn promote(&self, project: &str, label: &str, to: &Promote) -> Result<Option<String>> {
        let scope = Scope::in_project(&self.scope.user, project);
        let base = scope.base();
        let Some(row) = self.row(&scope, label)? else { return Ok(None) };
        let global = match to {
            Promote::New(skill) => self.claim(&base, skill)?.0,
            Promote::Into(global) => {
                if self.row(&base, global)?.is_none() {
                    return Err(anyhow!("there is no global skill called {global}"));
                }
                for phrase in &row.examples {
                    self.link_in(&base, global, phrase)?;
                }
                global.clone()
            }
        };
        let created_at = self.row(&base, &global)?.map(|skill| skill.created_at).unwrap_or_default();
        for phrase in &row.counter_examples {
            self.edit_lesson(&scope, &global, &created_at, ListColumn::CounterExamples, |list| with(list, phrase))?;
        }
        let (table, rows) = (scope.skills(), scope.rows());
        self.execute(&format!("DELETE FROM {table} WHERE {rows} AND label = ?"), scope.args(vec![label.into()]))?;
        Ok(Some(global))
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

    /// The newest route this user ran on this host in `cwd`. Other hosts and other directories share
    /// the brain, and their last route is not the one the person here means.
    pub fn last_executed(&self, cwd: &str) -> Result<Option<BrainEvent>> {
        let (table, rows) = (self.scope.events(), self.scope.rows());
        let rows = self.query(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM {table} WHERE {rows} AND label IS NOT NULL AND accepted = 1 AND host = ? AND cwd = ?
                 ORDER BY id DESC LIMIT 1"
            ),
            self.scope.args(vec![host().into(), cwd.into()]),
            to_event,
        )?;
        Ok(rows.into_iter().next())
    }

    /// One of this user's events by id. Another user's id is not found: event ids count per table.
    pub fn event(&self, id: i64) -> Result<Option<BrainEvent>> {
        let (table, rows) = (self.scope.events(), self.scope.rows());
        let rows = self.query(
            &format!("SELECT {EVENT_COLUMNS} FROM {table} WHERE {rows} AND id = ?"),
            self.scope.args(vec![id.into()]),
            to_event,
        )?;
        Ok(rows.into_iter().next())
    }

    /// Keep a plan for `ttl`. One table holds every user's plans, keyed by user and project as the
    /// `scoped_*` tables are; an older jli never reads it. Returns the plan's id.
    pub fn save_plan(&self, cwd: &str, state: &str, body: &str, commands_hash: &str, ttl: Duration) -> Result<i64> {
        self.execute(
            "INSERT INTO plans (user_id, project_id, created_at, expires_at, host, cwd, state, body, commands_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                self.scope.user.clone().into(),
                self.scope.project.clone().into(),
                now_iso().into(),
                iso_after(ttl).into(),
                host().into(),
                cwd.into(),
                state.into(),
                body.into(),
                commands_hash.into(),
            ],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// One of this user's plans. Another user's plan is not found.
    pub fn plan(&self, id: i64) -> Result<Option<PlanRow>> {
        let rows = self.query(
            "SELECT id, created_at, expires_at, cwd, state, body, commands_hash FROM plans
             WHERE id = ? AND user_id = ? AND project_id = ?",
            vec![id.into(), self.scope.user.clone().into(), self.scope.project.clone().into()],
            |row| {
                let mut c = Columns::new(row);
                PlanRow {
                    id: opt_int(c.take()).unwrap_or(0),
                    created_at: text(c.take()),
                    expires_at: text(c.take()),
                    cwd: text(c.take()),
                    state: text(c.take()),
                    body: text(c.take()),
                    commands_hash: text(c.take()),
                }
            },
        )?;
        Ok(rows.into_iter().next())
    }

    /// Take a plan for running. One conditional statement: of two callers only one sees true, and a
    /// plan that expired, ran, or changed state since it was read is not taken.
    pub fn claim_plan(&self, id: i64, from_state: &str) -> Result<bool> {
        let now = now_iso();
        let written = self.execute(
            "UPDATE plans SET state = 'running', ran_at = ?
             WHERE id = ? AND user_id = ? AND project_id = ? AND state = ? AND expires_at > ?",
            vec![
                now.clone().into(),
                id.into(),
                self.scope.user.clone().into(),
                self.scope.project.clone().into(),
                from_state.into(),
                now.into(),
            ],
        )?;
        Ok(written > 0)
    }

    /// A claimed plan has run; `event_id` is the event its run logged.
    pub fn finish_plan(&self, id: i64, event_id: Option<i64>) -> Result<()> {
        self.execute(
            "UPDATE plans SET state = 'ran', event_id = ? WHERE id = ? AND user_id = ? AND project_id = ?",
            vec![event_id.into(), id.into(), self.scope.user.clone().into(), self.scope.project.clone().into()],
        )?;
        Ok(())
    }

    /// Let a plan's time run out, for tests.
    #[cfg(test)]
    pub fn expire_plan(&self, id: i64) -> Result<()> {
        self.execute("UPDATE plans SET expires_at = ? WHERE id = ?", vec![now_iso().into(), id.into()])?;
        Ok(())
    }

    pub fn log_correction(&self, correction: &Correction) -> Result<()> {
        self.execute(
            "INSERT INTO corrections (user_id, project_id, event_id, ts, host, terminal, how, detail, result_event_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                self.scope.user.clone().into(),
                self.scope.project.clone().into(),
                correction.event_id.into(),
                now_iso().into(),
                host().into(),
                i64::from(correction.terminal).into(),
                correction.how.clone().into(),
                correction.detail.clone().into(),
                correction.result_event_id.into(),
            ],
        )?;
        Ok(())
    }

    /// This user's corrections, oldest first.
    pub fn corrections(&self) -> Result<Vec<Correction>> {
        self.query(
            "SELECT event_id, terminal, how, detail, result_event_id FROM corrections
             WHERE user_id = ? AND project_id = ? ORDER BY id",
            vec![self.scope.user.clone().into(), self.scope.project.clone().into()],
            |row| {
                let mut c = Columns::new(row);
                Correction {
                    event_id: opt_int(c.take()).unwrap_or(0),
                    terminal: opt_int(c.take()) == Some(1),
                    how: text(c.take()),
                    detail: opt_text(c.take()),
                    result_event_id: opt_int(c.take()),
                }
            },
        )
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
    use crate::types::SlotKind;

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
        assert_eq!(brain.events(10).unwrap()[0].label.as_deref(), Some("du-sh"));
        assert!(!brain.location().contains(", user "), "the default user is not named");

        let agent = scratch.brain_as("agent");
        assert_eq!(agent.skills().unwrap().len(), 9, "a new user starts from the seeds");
        assert!(agent.skill("du-sh").unwrap().is_none());
        assert_eq!(agent.setting("mode").unwrap(), None, "settings are the user's own");
        assert!(agent.events(10).unwrap().is_empty() && agent.last_executed("/tmp").unwrap().is_none());
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
        assert!(default.last_executed("/tmp").unwrap().is_none() && b.last_executed("/tmp").unwrap().is_none());
        let own = default.log_event(&event("git-status")).unwrap();
        assert_eq!(own, theirs, "both tables count from 1");
        default.mark_rejected(own).unwrap();
        assert_eq!(a.last_executed("/tmp").unwrap().unwrap().id, theirs);
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
            aliases: vec!["dir".into()],
            ..Slot::default()
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
        assert_eq!(theirs.add_new_skill(&skill("du-sh", "du -h")).unwrap(), ("du-sh".into(), true));
        assert_eq!(mine.add_new_skill(&skill("du-sh", "du -sh .")).unwrap(), ("du-sh-x".into(), true));
        assert_eq!(mine.skill("du-sh").unwrap().unwrap().steps, ["du -h"]);
        assert_eq!(mine.skill("du-sh-x").unwrap().unwrap().steps, ["du -sh ."]);

        // Four writers teaching four commands with one slug at once each keep their own command.
        // (`du  -s` would not do here: it is `du -s` with another space, the same command, and joins it.)
        drop((mine, theirs));
        let path = scratch.0.join("brain.db");
        let commands = ["du -s", "du -s .", "du -s /", "du -s ./"];
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
    fn a_label_taken_by_the_same_command_is_joined() {
        let scratch = Scratch::new("label-join");
        let (mine, theirs) = (scratch.brain(), scratch.brain());
        let template = |label: &str, step: &str, slot: &str, examples: &[&str]| NewSkill {
            slots: vec![Slot { name: slot.into(), kind: SlotKind::Number, ..Slot::default() }],
            ..new_skill(label, step, examples)
        };
        // Both looked when du-sh was free, and both are teaching du -sh .; theirs was written first.
        theirs.add_new_skill(&new_skill("du-sh", "du -sh .", &["disk use"])).unwrap();
        theirs.add_counter_example("du-sh", "how big is this").unwrap();
        assert_eq!(
            mine.add_new_skill(&new_skill("du-sh", "du  -sh  .", &["how big is this"])).unwrap(),
            ("du-sh".into(), false)
        );
        let joined = mine.skill("du-sh").unwrap().unwrap();
        assert_eq!(joined.steps, ["du -sh ."], "the row is theirs, as written");
        assert_eq!(joined.examples, ["disk use", "how big is this"]);
        assert!(joined.counter_examples.is_empty(), "joining links the phrase, so it is no longer a counter-example");
        assert!(mine.skill("du-sh-x").unwrap().is_none());

        // The same template under other slot names, and a command a template produces, are the same command.
        theirs.add_new_skill(&template("recent", "git log --oneline -n {count}", "count", &["last commits"])).unwrap();
        let renamed = template("recent", "git log --oneline -n {n}", "n", &["latest commits"]);
        assert_eq!(mine.add_new_skill(&renamed).unwrap(), ("recent".into(), false));
        let five = new_skill("recent", "git log --oneline -n 5", &["five commits"]);
        assert_eq!(mine.add_new_skill(&five).unwrap(), ("recent".into(), false));
        assert_eq!(mine.skill("recent").unwrap().unwrap().examples, ["last commits", "latest commits", "five commits"]);

        // A literal row never stands in for a template: the template gets the next free label.
        theirs.add_new_skill(&new_skill("three", "git log --oneline -n 3", &["three commits"])).unwrap();
        let general = template("three", "git log --oneline -n {count}", "count", &["some commits"]);
        assert_eq!(mine.add_new_skill(&general).unwrap(), ("three-x".into(), true));
        assert_eq!(mine.skill("three").unwrap().unwrap().examples, ["three commits"]);

        // Inside a project, a global row of the label that runs the same command gets the phrase as the
        // project's lesson; the global row is not written (ADR 17).
        let shop = scratch.in_project(DEFAULT_USER, "shop");
        assert_eq!(
            shop.add_new_skill(&new_skill("du-sh", "du -sh .", &["size of the shop"])).unwrap(),
            ("du-sh".into(), false)
        );
        assert!(shop.project_skills().unwrap().is_empty());
        assert!(shop.skill("du-sh").unwrap().unwrap().examples.contains(&"size of the shop".to_string()));
        assert_eq!(theirs.skill("du-sh").unwrap().unwrap().examples, ["disk use", "how big is this"]);
    }

    #[test]
    fn writers_teaching_one_command_at_once_get_one_skill() {
        let scratch = Scratch::new("label-race-same");
        let path = scratch.0.join("brain.db");
        let writers = 4;
        let start = std::sync::Barrier::new(writers);
        let learned: Vec<(String, bool)> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..writers)
                .map(|n| {
                    let (path, start) = (&path, &start);
                    scope.spawn(move || {
                        let brain = Brain::open(BrainConfig::file(path)).unwrap();
                        start.wait();
                        brain.learn_command(&format!("disk use {n}"), &["du -sh .".into()], Risk::Safe).unwrap()
                    })
                })
                .collect();
            threads.into_iter().map(|thread| thread.join().unwrap()).collect()
        });
        assert!(learned.iter().all(|(label, _)| label == "du-sh"), "{learned:?}");
        assert_eq!(learned.iter().filter(|(_, created)| *created).count(), 1, "one writer wrote the row: {learned:?}");
        let brain = scratch.brain();
        assert_eq!(labels(&brain).iter().filter(|label| label.starts_with("du-sh")).count(), 1);
        let mut examples = brain.skill("du-sh").unwrap().unwrap().examples;
        examples.sort();
        assert_eq!(examples, (0..writers).map(|n| format!("disk use {n}")).collect::<Vec<_>>());
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
    fn removing_the_last_alias_writes_the_slot_as_it_was_before_aliases() {
        let scratch = Scratch::new("alias-remove");
        let brain = scratch.brain();
        let stored = |brain: &Brain| {
            brain
                .query("SELECT slots FROM skills WHERE label = 'git-commit'", vec![], |row| text(row.get_value(0).unwrap()))
                .unwrap()
                .remove(0)
        };
        let before = stored(&brain);
        assert!(brain.add_alias("git-commit", "message", "msg").unwrap());
        assert!(brain.add_alias("git-commit", "message", "text").unwrap());
        assert!(brain.remove_alias("git-commit", "message", "MSG").unwrap(), "compared as names are");
        assert!(!brain.remove_alias("git-commit", "message", "msg").unwrap(), "already gone");
        assert!(!brain.remove_alias("git-commit", "subject", "text").unwrap(), "no such slot");
        assert!(!brain.remove_alias("no-such-skill", "message", "text").unwrap());
        assert_eq!(brain.skill("git-commit").unwrap().unwrap().slots[0].aliases, ["text"]);
        assert!(brain.remove_alias("git-commit", "message", "text").unwrap());
        assert_eq!(stored(&brain), before, "byte for byte, so an older jli reads it as it always did");
    }

    #[test]
    fn re_teaching_a_label_keeps_its_slots_aliases() {
        let scratch = Scratch::new("alias-upsert");
        let brain = scratch.brain();
        assert!(brain.add_alias("git-commit", "message", "msg").unwrap());
        let slot = |name: &str, aliases: &[&str]| Slot {
            name: name.into(),
            kind: crate::types::SlotKind::Text,
            aliases: aliases.iter().map(|alias| alias.to_string()).collect(),
            ..Slot::default()
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
    fn a_slots_settings_are_written_beside_its_aliases_and_leave_a_plain_slot_as_it_was() {
        let scratch = Scratch::new("slot-settings");
        let brain = scratch.brain();
        let stored = |brain: &Brain| {
            brain
                .query("SELECT slots FROM skills WHERE label = 'git-commit'", vec![], |row| text(row.get_value(0).unwrap()))
                .unwrap()
                .remove(0)
        };
        assert_eq!(
            stored(&brain),
            r#"[{"name":"message","kind":"text","description":"the commit message"}]"#,
            "a slot with no settings is written as before ADR 24"
        );
        assert!(brain.add_alias("git-commit", "message", "msg").unwrap());
        let choices = vec![crate::types::Choice { value: "wip".into(), text: "wip".into() }];
        let set = Slot {
            name: "message".into(),
            description: "the commit message".into(),
            default: Some("wip".into()),
            optional: true,
            flag: Some("-m".into()),
            choices: choices.clone(),
            ..Slot::default()
        };
        assert!(brain.set_slot("git-commit", &set).unwrap());
        assert!(!brain.set_slot("git-commit", &set).unwrap(), "nothing changed");
        assert_eq!(
            stored(&brain),
            r#"[{"name":"message","kind":"text","description":"the commit message","default":"wip","aliases":["msg"],"optional":true,"flag":"-m","choices":[{"value":"wip","text":"wip"}]}]"#,
            "the alias another writer added is kept, and the new fields come last"
        );

        // Re-teaching the label keeps what was set by hand, as it keeps the aliases.
        let mut retaught = new_skill("git-commit", "git commit -m {message}", &[]);
        retaught.slots = vec![Slot { name: "message".into(), ..Slot::default() }];
        brain.add_skill(&retaught).unwrap();
        let slot = brain.skill("git-commit").unwrap().unwrap().slots.remove(0);
        assert_eq!((slot.optional, slot.flag.as_deref(), slot.default.as_deref()), (true, Some("-m"), Some("wip")));
        assert_eq!((slot.choices, slot.aliases), (choices, vec!["msg".to_string()]));

        // A placeholder no slot declares is declared.
        assert!(brain.set_slot("git-commit", &Slot { name: "author".into(), ..Slot::default() }).unwrap());
        assert_eq!(brain.skill("git-commit").unwrap().unwrap().slots.len(), 2);
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
        // Newer routes run in another directory, and on another host in this one, are not this one's last.
        brain.log_event(&NewEvent { cwd: "/elsewhere".into(), ..event.clone() }).unwrap();
        let other_host = brain.log_event(&event).unwrap();
        brain.execute("UPDATE events SET host = 'another-host' WHERE id = ?", vec![other_host.into()]).unwrap();
        let last = brain.last_executed("/tmp").unwrap().unwrap();
        assert_eq!(last.id, first);
        brain.mark_rejected(first).unwrap();
        assert!(brain.last_executed("/tmp").unwrap().is_none());
        assert!(brain.last_executed("/elsewhere").unwrap().is_some());
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

    // ── Projects (ADR 17) ───────────────────────────────────────────────────────

    impl Scratch {
        fn in_project(&self, user: &str, project: &str) -> Brain {
            Brain::open_in(BrainConfig::file(&self.0.join("brain.db")), user, Some(project)).unwrap()
        }
    }

    fn new_skill(label: &str, step: &str, examples: &[&str]) -> NewSkill {
        NewSkill {
            label: label.into(),
            steps: vec![step.into()],
            scenario: format!("runs {step}"),
            risk: Risk::Safe,
            origin: Origin::Taught,
            examples: examples.iter().map(|example| example.to_string()).collect(),
            slots: vec![],
        }
    }

    #[test]
    fn a_project_sees_its_own_skills_and_the_users_global_ones() {
        let scratch = Scratch::new("project-view");
        let global = scratch.brain();
        let mut shop = scratch.in_project(DEFAULT_USER, "shop");
        assert_eq!(labels(&shop), labels(&global), "a project keeps the user's global skills");
        assert!(shop.project_skills().unwrap().is_empty(), "and gets no copy of the seeds");
        assert_eq!((shop.project(), global.project()), (Some("shop"), None));
        assert!(shop.location().ends_with(", project shop") && !shop.location().contains(", user "));

        // What is learned in the project is the project's.
        let (label, created) = shop.learn_command("run the tests", &["cargo test".into()], Risk::Safe).unwrap();
        assert_eq!((label.as_str(), created), ("cargo-test", true));
        assert!(global.skill("cargo-test").unwrap().is_none());
        assert!(scratch.in_project(DEFAULT_USER, "blog").skill("cargo-test").unwrap().is_none());
        assert_eq!((shop.project_skills().unwrap().len(), shop.skills().unwrap().len()), (1, 10));
        shop.record_use("cargo-test").unwrap();
        assert_eq!(shop.skills().unwrap()[0].label, "cargo-test", "most used first, across both scopes");

        // A use of a global skill is counted on the global row: it is not a lesson.
        shop.record_use("git-status").unwrap();
        assert_eq!(global.skill("git-status").unwrap().unwrap().uses, 1);

        // A new label never hides a global one.
        global.learn_command("disk use", &["du -sh .".into()], Risk::Safe).unwrap();
        assert_eq!(shop.learn_command("size of it", &["du  -sh".into()], Risk::Safe).unwrap().0, "du-sh-x");
        assert_eq!(shop.skill("du-sh").unwrap().unwrap().steps, ["du -sh ."]);

        // Re-teaching a global skill from inside a project changes the global row, as an alias would.
        shop.add_skill(&new_skill("list-files-with-sizes", "ls -lhS", &[])).unwrap();
        assert_eq!(global.skill("list-files-with-sizes").unwrap().unwrap().steps, ["ls -lhS"]);
        assert!(shop.project_skills().unwrap().iter().all(|skill| skill.label != "list-files-with-sizes"));

        // A clash that happens anyway, by another writer or an older jli, is won by the project.
        shop.add_skill_if_missing(&new_skill("list-files", "ls -1", &[])).unwrap();
        assert_eq!(shop.skill("list-files").unwrap().unwrap().steps, ["ls -1"]);
        assert_eq!(shop.skills().unwrap().iter().filter(|skill| skill.label == "list-files").count(), 1);
        assert_eq!(global.skill("list-files").unwrap().unwrap().steps, ["ls"]);

        // Forgetting inside a project reaches the project's rows only.
        assert!(shop.forget("list-files").unwrap());
        assert_eq!(shop.skill("list-files").unwrap().unwrap().steps, ["ls"], "the global one is seen again");
        assert!(!shop.forget("git-status").unwrap());
        assert!(global.skill("git-status").unwrap().is_some());
        assert!(shop.globally(|brain| brain.forget("git-push")).unwrap());
        assert!(global.skill("git-push").unwrap().is_none());
        assert_eq!(shop.project(), Some("shop"), "back in the project afterwards");
    }

    #[test]
    fn a_lesson_learned_in_one_project_stays_in_that_project() {
        let scratch = Scratch::new("lessons");
        let global = scratch.brain();
        global.learn_command("run the tests", &["cargo test".into()], Risk::Safe).unwrap();
        let (rust, node) = (scratch.in_project(DEFAULT_USER, "rust"), scratch.in_project(DEFAULT_USER, "node"));
        let as_stored = global.skill("cargo-test").unwrap().unwrap();

        // The correction the router makes in the node project.
        assert!(
            !node.remove_example("cargo-test", "run the tests").unwrap(),
            "the global row's phrasing is not the project's"
        );
        assert!(node.add_counter_example("cargo-test", "run the tests").unwrap());
        assert_eq!(node.learn_command("run the tests", &["npm test".into()], Risk::Safe).unwrap().0, "npm-test");
        let seen = node.skill("cargo-test").unwrap().unwrap();
        assert!(seen.examples.is_empty(), "the counter-example hides the same phrasing");
        assert_eq!(seen.counter_examples, ["run the tests"]);
        assert_eq!(node.skills().unwrap().into_iter().find(|skill| skill.label == "cargo-test").unwrap(), seen);
        assert_eq!(node.project_lessons().unwrap(), [("cargo-test".to_string(), 0, 1)]);

        // The global row, and so every other project, is as it was.
        assert_eq!(global.skill("cargo-test").unwrap().unwrap(), as_stored);
        assert_eq!(rust.skill("cargo-test").unwrap().unwrap(), as_stored);
        assert!(rust.skill("npm-test").unwrap().is_none() && global.skill("npm-test").unwrap().is_none());
        assert!(!rust.add_example("cargo-test", "Run the tests").unwrap(), "already known there, so nothing to keep");
        assert!(rust.project_lessons().unwrap().is_empty());

        // A phrasing learned in a project lifts the same counter-example there, and only there.
        assert!(rust.add_counter_example("cargo-test", "test it").unwrap());
        rust.link_phrase("cargo-test", "test it").unwrap();
        let seen = rust.skill("cargo-test").unwrap().unwrap();
        assert_eq!(
            (seen.examples.as_slice(), seen.counter_examples.len()),
            (["run the tests".to_string(), "test it".into()].as_slice(), 0)
        );
        node.link_phrase("cargo-test", "run the tests").unwrap();
        let seen = node.skill("cargo-test").unwrap().unwrap();
        assert_eq!((seen.examples.as_slice(), seen.counter_examples.len()), (["run the tests".to_string()].as_slice(), 0));
        assert_eq!(global.skill("cargo-test").unwrap().unwrap(), as_stored);

        // A lesson is about one row: forgotten and taught again, the global skill starts clean.
        node.add_counter_example("cargo-test", "check it").unwrap();
        assert!(global.forget("cargo-test").unwrap());
        std::thread::sleep(Duration::from_millis(5));
        global.learn_command("run the tests", &["cargo test".into()], Risk::Safe).unwrap();
        assert!(node.skill("cargo-test").unwrap().unwrap().counter_examples.is_empty());
        assert!(node.project_lessons().unwrap().is_empty());
    }

    #[test]
    fn settings_and_seeds_belong_to_the_user_not_the_project() {
        let scratch = Scratch::new("project-settings");
        let shop = scratch.in_project("agent", "shop");
        assert!(shop.project_skills().unwrap().is_empty());
        assert_eq!(scratch.brain_as("agent").skills().unwrap().len(), 9, "a new user's seeds are global");
        shop.set_setting("mode", "auto").unwrap();
        assert_eq!(scratch.brain_as("agent").setting("mode").unwrap().as_deref(), Some("auto"));
        assert_eq!(scratch.in_project("agent", "blog").setting("mode").unwrap().as_deref(), Some("auto"));
        assert_eq!(scratch.brain().setting("mode").unwrap(), None, "and still the user's alone");
    }

    #[test]
    fn a_projects_skills_and_events_belong_to_one_user() {
        let scratch = Scratch::new("project-users");
        let (mine, theirs) = (scratch.in_project(DEFAULT_USER, "shop"), scratch.in_project("agent", "shop"));
        mine.learn_command("run the tests", &["cargo test".into()], Risk::Safe).unwrap();
        assert!(theirs.skill("cargo-test").unwrap().is_none());
        mine.add_counter_example("git-status", "what is new").unwrap();
        assert!(theirs.skill("git-status").unwrap().unwrap().counter_examples.is_empty());
        assert!(scratch.brain().skill("git-status").unwrap().unwrap().counter_examples.is_empty());

        // A project's requests are its own history: elsewhere, and for another user, they are not there.
        mine.log_event(&event("cargo-test")).unwrap();
        assert_eq!(mine.events(10).unwrap().len(), 1);
        assert!(theirs.events(10).unwrap().is_empty() && scratch.brain().events(10).unwrap().is_empty());
        assert!(mine.last_executed("/tmp").unwrap().is_some());
        assert!(scratch.brain().last_executed("/tmp").unwrap().is_none(), "so a fix outside it cannot reach it");
    }

    #[test]
    fn sharing_a_project_skill_copies_it_into_the_same_project() {
        let scratch = Scratch::new("project-share");
        let global = scratch.brain();
        let mine = scratch.in_project(DEFAULT_USER, "shop");
        mine.learn_command("run the tests", &["cargo test".into()], Risk::Safe).unwrap();
        mine.add_counter_example("cargo-test", "test the docs").unwrap();
        assert_eq!(mine.share("cargo-test", "agent").unwrap(), Shared::Copied);
        let theirs = scratch.in_project("agent", "shop");
        let copy = theirs.skill("cargo-test").unwrap().unwrap();
        assert_eq!((copy.examples.as_slice(), copy.counter_examples.len()), (["run the tests".to_string()].as_slice(), 0));
        assert_eq!(theirs.project_skills().unwrap().len(), 1);
        assert!(scratch.brain_as("agent").skill("cargo-test").unwrap().is_none(), "not one of their global skills");
        assert_eq!(theirs.shared_from().unwrap().get("cargo-test").map(String::as_str), Some(DEFAULT_USER));

        // A global skill shared from inside a project is shared as stored, to their global skills.
        global.learn_command("disk use", &["du -sh .".into()], Risk::Safe).unwrap();
        mine.add_counter_example("du-sh", "size of the repo").unwrap();
        assert_eq!(mine.share("du-sh", "agent").unwrap(), Shared::Copied);
        let copy = scratch.brain_as("agent").skill("du-sh").unwrap().unwrap();
        assert!(copy.counter_examples.is_empty() && copy.examples == ["disk use"]);
        assert_eq!(theirs.shared_from().unwrap().get("du-sh").map(String::as_str), Some(DEFAULT_USER));
        assert_eq!(mine.share("du-sh", DEFAULT_USER).unwrap(), Shared::SameUser);
        assert_eq!(mine.share("cargo-test", "agent").unwrap(), Shared::AlreadyRuns("cargo-test".into()));
    }

    #[test]
    fn a_brain_from_before_projects_opens_unchanged() {
        let scratch = Scratch::new("before-projects");
        let path = scratch.0.join("brain.db");
        raw(&path, BEFORE_USERS);
        let as_written = raw(&path, DUMP);

        let shop = scratch.in_project(DEFAULT_USER, "shop");
        assert_eq!(labels(&shop), ["du-sh"], "the user's global skills, and no seeds added");
        assert_eq!(shop.setting("mode").unwrap().as_deref(), Some("manual"));
        assert!(shop.events(10).unwrap().is_empty(), "the project's history starts empty");
        shop.learn_command("run the tests", &["cargo test".into()], Risk::Safe).unwrap();
        assert!(shop.add_example("du-sh", "how big").unwrap());
        assert!(shop.add_counter_example("du-sh", "size of the repo").unwrap());
        shop.log_event(&event("cargo-test")).unwrap();
        let seen = shop.skill("du-sh").unwrap().unwrap();
        assert_eq!(seen.examples, ["disk use", "how big"]);
        assert_eq!(seen.counter_examples, ["free space", "size of the repo"]);
        drop(shop);

        assert_eq!(raw(&path, DUMP), as_written, "the global rows and tables are untouched");
        let outside = scratch.brain();
        assert_eq!(labels(&outside), ["du-sh"]);
        assert_eq!(outside.skill("du-sh").unwrap().unwrap().counter_examples, ["free space"]);
        assert_eq!(outside.events(10).unwrap().len(), 1);
    }

    /// Every table a brain had before project scenarios (ADR 22), with its definition and rows.
    const DUMP_BUT_SCENARIOS: &str = "SELECT name, sql FROM sqlite_master WHERE name != 'project_scenarios' ORDER BY name;
        SELECT * FROM skills ORDER BY rowid; SELECT * FROM scoped_skills ORDER BY rowid;
        SELECT * FROM project_lessons ORDER BY rowid; SELECT * FROM meta ORDER BY rowid; SELECT * FROM scoped_meta ORDER BY rowid";

    #[test]
    fn a_project_keeps_its_own_scenario_for_a_global_skill() {
        let scratch = Scratch::new("project-scenario");
        let path = scratch.0.join("brain.db");
        let global = scratch.brain();
        global.learn_command("disk use", &["du -sh .".into()], Risk::Safe).unwrap();
        let (mut shop, blog) = (scratch.in_project(DEFAULT_USER, "shop"), scratch.in_project(DEFAULT_USER, "blog"));
        shop.add_counter_example("du-sh", "size of the repo").unwrap();
        let scenario = |brain: &Brain| brain.skill("du-sh").unwrap().unwrap().scenario;
        let listed =
            |brain: &Brain| brain.skills().unwrap().into_iter().find(|skill| skill.label == "du-sh").unwrap().scenario;
        let (stored, checkout, folder) =
            ("the user wants: disk use", "the user wants the size of this checkout", "the user wants the size of a folder");

        // Written in its own table: every other table, and every row in them, is as it was.
        let before = raw(&path, DUMP_BUT_SCENARIOS);
        assert!(!shop.set_scenario("du-sh", stored).unwrap(), "the global row's own scenario is not kept twice");
        assert!(shop.set_scenario("du-sh", checkout).unwrap());
        assert!(!shop.set_scenario("du-sh", checkout).unwrap());
        assert_eq!(raw(&path, DUMP_BUT_SCENARIOS), before);
        assert_eq!((scenario(&shop), listed(&shop)), (checkout.to_string(), checkout.to_string()));
        assert_eq!(shop.skill("du-sh").unwrap().unwrap().counter_examples, ["size of the repo"], "the lesson still holds");
        assert_eq!(shop.project_scenarios().unwrap(), ["du-sh"]);
        for elsewhere in [&global, &blog, &scratch.in_project("agent", "shop")] {
            assert_eq!(elsewhere.project_scenario("du-sh").unwrap(), None);
            assert!(elsewhere.skill("du-sh").unwrap().is_none_or(|skill| skill.scenario == stored));
        }

        // A project's own skill, and a skill outside every project, change their own row.
        shop.learn_command("run the tests", &["cargo test".into()], Risk::Safe).unwrap();
        assert!(shop.set_scenario("cargo-test", "the user wants the crate tested").unwrap());
        assert_eq!(shop.project_skills().unwrap()[0].scenario, "the user wants the crate tested");
        assert!(global.set_scenario("du-sh", folder).unwrap());
        assert_eq!(scenario(&blog), folder);
        assert_eq!(scenario(&shop), checkout, "the project's own still wins there");

        // `teach --global` works on the global row as stored, and a share copies it as stored.
        assert_eq!(shop.globally(|brain| scenario(brain)), folder);
        assert_eq!(shop.share("du-sh", "agent").unwrap(), Shared::Copied);
        assert_eq!(scenario(&scratch.brain_as("agent")), folder);

        // `generalize` merging a project skill into the global row keeps the row, and so the project's scenario.
        shop.promote("shop", "cargo-test", &Promote::Into("du-sh".into())).unwrap();
        assert_eq!(scenario(&shop), checkout);

        // A project row of the same label is seen instead; forgotten, the global row comes back with the scenario.
        shop.add_skill_if_missing(&new_skill("du-sh", "du -sh", &[])).unwrap();
        assert_eq!((scenario(&shop), shop.project_scenarios().unwrap().len()), ("runs du -sh".into(), 0));
        assert_eq!(shop.project_scenario("du-sh").unwrap(), None);
        assert!(shop.forget("du-sh").unwrap());
        assert_eq!(scenario(&shop), checkout);

        // Cleared, the project sees the global scenario again.
        assert!(shop.clear_project_scenario("du-sh").unwrap());
        assert!(!shop.clear_project_scenario("du-sh").unwrap());
        assert_eq!(scenario(&shop), folder);

        // Set to the global row's own text, the project keeps none: a later global rewrite still reaches it.
        shop.set_scenario("du-sh", checkout).unwrap();
        assert!(shop.set_scenario("du-sh", folder).unwrap());
        assert_eq!(shop.project_scenario("du-sh").unwrap(), None);
        global.set_scenario("du-sh", "the user wants a folder's size").unwrap();
        assert_eq!(scenario(&shop), "the user wants a folder's size");

        // The scenario is about one row: forgotten everywhere and taught again, the global skill starts clean.
        shop.set_scenario("du-sh", checkout).unwrap();
        assert!(shop.globally(|brain| brain.forget("du-sh")).unwrap());
        std::thread::sleep(Duration::from_millis(5));
        global.learn_command("disk use", &["du -sh .".into()], Risk::Safe).unwrap();
        assert_eq!(scenario(&shop), stored);
        assert!(shop.project_scenarios().unwrap().is_empty());
    }

    #[test]
    fn promoting_moves_a_project_skill_to_the_global_skills() {
        let scratch = Scratch::new("promote");
        let global = scratch.brain();
        let rust = scratch.in_project(DEFAULT_USER, "rust");
        rust.learn_command("run the tests", &["cargo test".into()], Risk::Safe).unwrap();
        rust.add_example("cargo-test", "test everything").unwrap();
        rust.add_counter_example("cargo-test", "test the docs").unwrap();

        let new = new_skill("cargo-test", "cargo test", &["run the tests", "test everything"]);
        assert_eq!(rust.promote("rust", "cargo-test", &Promote::New(new.clone())).unwrap().as_deref(), Some("cargo-test"));
        let row = global.skill("cargo-test").unwrap().unwrap();
        assert_eq!((row.examples.len(), row.counter_examples.len()), (2, 0));
        assert!(rust.project_skills().unwrap().is_empty(), "the project row is gone");
        assert_eq!(rust.skill("cargo-test").unwrap().unwrap().counter_examples, ["test the docs"], "kept as a lesson");
        assert!(scratch.in_project(DEFAULT_USER, "node").skill("cargo-test").unwrap().unwrap().counter_examples.is_empty());
        assert_eq!(rust.promote("rust", "cargo-test", &Promote::New(new)).unwrap(), None, "nothing left to promote");

        // Into a global skill that already runs it: the project's phrasings join it.
        global.add_skill(&new_skill("git-recent-commits", "git log --oneline -n {count}", &["last commits"])).unwrap();
        global.add_counter_example("git-recent-commits", "the last five").unwrap();
        let node = scratch.in_project(DEFAULT_USER, "node");
        node.add_skill(&new_skill("five-commits", "git log --oneline -n 5", &["the last five"])).unwrap();
        assert!(node.promote("node", "five-commits", &Promote::Into("no-such-skill".into())).is_err());
        assert!(node.skill("five-commits").unwrap().is_some(), "a failed promotion keeps the project row");
        let into = Promote::Into("git-recent-commits".into());
        assert_eq!(node.promote("node", "five-commits", &into).unwrap().as_deref(), Some("git-recent-commits"));
        let row = global.skill("git-recent-commits").unwrap().unwrap();
        assert_eq!(
            (row.examples.as_slice(), row.counter_examples.len()),
            (["last commits".to_string(), "the last five".into()].as_slice(), 0)
        );
        assert!(node.skill("five-commits").unwrap().is_none());
    }
}
