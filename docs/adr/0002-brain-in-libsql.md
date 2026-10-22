# 2. The brain is a libSQL database with a fixed schema, and rows are never replaced

- Status: accepted; superseded in part by [ADR 32](0032-durable-corrections.md), which makes explicit phrase choices durable;
  and by [ADR 15](0015-user-scope.md), which adds four tables
  for users other than the default and makes finding a free label safe for parallel writers
- Date: 2026-10-02
- Origin: the TypeScript prototype; the Rust port kept the schema byte for byte

## Context

Learning is writing rows ([ADR 1](0001-two-systems.md)), so the database is everything `jli` has
learned. One goal is that what one host learns, every host knows. Another is that a brain keeps
working across versions of `jli`, including brains written by the TypeScript prototype on hosts
that still have them.

## Decision

Use libSQL. One client covers three deployments:

| Setting | Meaning |
| --- | --- |
| default | a local file at `~/.jli/brain.db` |
| `JLI_DB_URL=file:/path` | a local file elsewhere |
| `JLI_DB_URL=libsql://…` | a remote database shared by all hosts |
| `JLI_SYNC_URL=libsql://…` | a local replica, at the `JLI_DB_URL` file, that syncs with the remote |
| `JLI_DB_TOKEN` | the auth token for the remote |

The schema is fixed:

```sql
CREATE TABLE IF NOT EXISTS skills (
  label            TEXT PRIMARY KEY,
  steps            TEXT NOT NULL,              -- JSON array of shell strings with {slot} placeholders
  scenario         TEXT NOT NULL,
  examples         TEXT NOT NULL DEFAULT '[]', -- JSON array, capped at 16, newest kept
  counter_examples TEXT NOT NULL DEFAULT '[]', -- JSON array, capped at 16
  slots            TEXT NOT NULL DEFAULT '[]', -- JSON array of { name, kind, description, default? }
  risk             TEXT NOT NULL DEFAULT 'safe',   -- safe | modifies | destructive
  origin           TEXT NOT NULL DEFAULT 'seed',   -- seed | agent | taught
  uses             INTEGER NOT NULL DEFAULT 0,
  created_at       TEXT NOT NULL,
  last_used_at     TEXT,
  created_host     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, host TEXT NOT NULL, cwd TEXT NOT NULL,
  query TEXT NOT NULL, path TEXT NOT NULL, label TEXT, confidence REAL, command TEXT,
  exit_code INTEGER, accepted INTEGER NOT NULL DEFAULT 0, learned TEXT,
  ms_system1 INTEGER, ms_system2 INTEGER
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

Rules for writing it:

- `meta` holds `seeded` and the settings `setting:confidence` and `setting:mode`.
- `events.path` is `system1`, `system2`, `taught` or `none`. Readers also accept the legacy
  `system1-confirmed`, left by an early "did you mean?" prompt.
- An empty brain is seeded once with nine skills (`src/seed.rs`): the matrix that was hard-coded in
  the first proof of concept. A seed that is forgotten does not come back.
- **Never replace a skill row.** Upserting a skill updates `steps`, `scenario`, `slots` and `risk`,
  and keeps `examples`, `counter_examples`, `uses`, `origin` and `created_at`.
- Examples and counter-examples are compared case-insensitively after trimming, and capped at 16
  with the newest kept.
- A label is a slug: lowercase, each run of other characters becomes one dash, outer dashes are
  trimmed, then the text is cut to 48 characters, so a cut can end in a dash. `-x` is appended
  until the label is unique.
- Timestamps are UTC ISO 8601 with milliseconds (`2026-10-02T12:00:00.000Z`). Each skill and event
  records the host it came from.
- A replica syncs when it is opened and after every command.

`rusqlite` was rejected. It is simpler and synchronous, but it would drop the remote and replica
modes. [ADR 10](0010-synchronous-rust.md) says how the async libSQL client is contained.

**Lesson paid for:** an early `INSERT OR REPLACE` wiped learned examples.

## Consequences

- Brains are interchangeable between the TypeScript prototype and the Rust version. On 2026-10-02,
  `skills`, `history`, `stats` and `config` printed byte-identical output from both versions on a
  brain written by the prototype, a brain written by Rust, and a brain written by both.
- Reads are lenient, because a brain may come from another version: a malformed JSON column reads
  as empty, an unknown risk as `modifies`, an unknown origin as `taught`, an unknown path as `none`.
- The schema cannot change without a migration for brains on every host.
- **Known gap:** the remote and replica modes have never been run against a live remote database.
- **Known gap:** libSQL refuses to open an embedded replica over a file that was not created by
  syncing, so pointing `JLI_SYNC_URL` at an existing local `brain.db` is expected to fail. This is
  untested, like the rest of the cloud setup.
