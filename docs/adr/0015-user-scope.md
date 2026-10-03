# 15. What is learned belongs to a user, a skill is shared by copy, and parallel writers lose nothing

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 2](0002-brain-in-libsql.md), on the fixed schema (four tables are
  added) and on how a free label is found

## Context

The owner wants an AI agent to use jli in the way that suits the agent, on the same brain as the
owner, without either one's lessons changing what the other gets: "I would solve it simply by
letting the CLI accept a `--user-id` (also an env var), which by default should be a 'default'
user but if set as something else then all skills are scoped to that user. This is not about
authentication, it's about isolation." He also wants to share some of his skills with an agent,
"ad-hoc, but not all".

Three facts about the brain shaped the design:

- `label` is the primary key of `skills`, so two users cannot both own `git-commit` as the schema
  stands, and SQLite cannot change a primary key without rebuilding the table.
- [ADR 2](0002-brain-in-libsql.md) requires that several hosts and versions share one brain. An
  older jli will keep reading and writing a brain after this change. Its reads are unfiltered
  (`SELECT … FROM skills`, `… FROM events`), its upsert is `ON CONFLICT(label)`, and its `jli fix`
  acts on the newest accepted row of `events`.
- Parallel agents on one brain are the point, and a review found that they lose lessons. Adding a
  phrasing read the examples list and then wrote the whole list back unconditionally, so of two
  writers that read the same list one addition was lost. Adding an alias had the same shape. A new
  label was picked from a snapshot of the matrix and then upserted, so two writers whose commands
  made the same slug could each end up changing the other's command.

## Decision

### A user id on every invocation

- `--user-id=<id>`, or `JLI_USER_ID`; the flag wins; without either the user is `default`.
- An id is 1 to 32 lowercase letters, digits, `-` or `_`, starting with a letter or digit.
  Anything else stops the run: `a user id is 1 to 32 lowercase letters, digits, - or _, starting
  with a letter or digit, got "Agent"`. A bare `--user-id agent` is refused too
  (`--user-id needs a value: --user-id=<id>`), so the id is never read as words of the request.
- The id applies to every command that reads or writes the brain: routing, `teach`, `fix`,
  `forget`, `skills`, `stats`, `history`, `config`, `share`, the interactive session, and `demo`
  (which builds its sandbox brain as that user). `jli sync` syncs the whole brain, every user.
- This is isolation, not authentication. Anyone who can open the brain can pass any id.

### The storage: the default user keeps the old tables, every other user gets new ones

The options weighed, judged by what an older jli on the same brain would see and do:

| Option | What an older jli does | Verdict |
| --- | --- | --- |
| A `user_id` column on `skills`, default `'default'`, with labels made unique by some prefix | reads every user's rows into the default user's matrix, routes to them, and its `forget` and `fix` can change them | rejected: leaks and corrupts |
| Rebuild `skills` with a composite key `(user_id, label)` | every teach fails: its `ON CONFLICT(label)` no longer matches a key. The rebuild also rewrites every row | rejected: breaks old versions, and ADR 2 forbids rewriting rows |
| A `user_id` column on `events` | `jli history` and `stats` show other users' requests, and a bare `jli fix` can take another user's last run and add a counter-example to the default user's skill of that label | rejected: corrupts across users |
| **New tables for every user but the default, keyed by user** | never reads them: it sees exactly the default user's rows, as before | **chosen** |

So the tables every brain has are the default user's, unchanged, and every other user lives in
tables that only this version reads. All of it is `CREATE TABLE IF NOT EXISTS`, run on open: no
`ALTER`, no rebuild, no row rewritten, and nothing a remote or a replica has not already been asked
to do.

```sql
CREATE TABLE IF NOT EXISTS scoped_skills (
  user_id    TEXT NOT NULL,
  project_id TEXT NOT NULL DEFAULT '',
  …the columns of skills…,
  PRIMARY KEY (user_id, project_id, label)
);
CREATE TABLE IF NOT EXISTS scoped_events (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id, project_id, …the columns of events…);
CREATE TABLE IF NOT EXISTS scoped_meta (user_id, project_id, key, value, PRIMARY KEY (user_id, project_id, key));
CREATE TABLE IF NOT EXISTS shares (user_id, project_id, label, created_at, from_user,
  PRIMARY KEY (user_id, project_id, label, created_at));
```

`project_id` is always `''` today; it is there so that a second scoping dimension needs no third
set of tables (see below). In `src/store.rs` one `Scope` value, set when the brain is opened, says
which table and which condition every statement uses, so the router and the learning loop are
scoped without knowing it.

### What is scoped

- **Skills.** A user sees, routes against, teaches, corrects, forgets and adds aliases to their own
  skills only. System 1 and the agent are shown that user's matrix. Two users can each own
  `git-commit`, with different commands.
- **Events.** Each user's `history`, `stats` and `jli fix` read their own events. Event ids count
  separately per table, and `fix` marks an event rejected only within the user's own.
- **Settings.** `mode` and `confidence` are per user. **A user without a setting gets the built-in
  default** (semi, 0.5), not the default user's: a new user is a fresh brain, and the reason for
  users is that what suits the owner need not suit an agent. Flags and environment variables still
  come first, as before.
- **Seeds.** A new user starts from the nine seed skills, the first time they are used, and a seed
  they forget does not come back for them.

### Sharing: `jli share <label> --to <user>`

- It copies one of the current user's skills to another user, as it is at that moment: steps,
  scenario, slots with their aliases, risk, origin and the phrasings (examples), which are what make
  System 1 route to it. Uses start at 0, and the row records this host and time.
- **Counter-examples are not copied.** They mark where the skill ends against the sharer's other
  skills (`"newest files"` is not `list-files` because the sharer has `list-files-by-time`), and
  the other user's matrix is different.
- **A skill the other user already has is left alone, never merged.** If they have a skill that
  runs the same steps, under any label, the answer is `ci already runs the same command as
  git-commit, so nothing was shared.` If they have the label for something else, `ci already has a
  skill called deploy, so nothing was shared. They can drop theirs with: jli --user-id=ci forget
  deploy`. Merging phrasings was rejected: it would change a skill the other user had already
  vetted, which is the change isolation exists to prevent.
- The other user is seeded before anything is compared, so sharing a seed to a user who has never
  run jli is refused like sharing it to anyone else.
- Sharing is a copy, not a link: what either user teaches the skill afterwards stays theirs.
- **Where it came from** is kept in `shares`, which works for the default user's table as well
  without adding a column to it, and `jli skills` shows `shared by agent` next to the origin. A
  share counts only while the skill's `created_at` is the one that was shared, so a skill forgotten
  and taught again under the same label, by any version, is no longer shown as shared.

### Parallel writers

- **A list or slots column is written with a compare-and-swap.** Adding or removing a phrasing or a
  counter-example, and adding an alias, read the column, edit it in memory, and write it with
  `UPDATE … SET col = ? WHERE … AND label = ? AND col IS ?`, the old value being exactly what was
  read. If another writer changed it in between, nothing is written and the edit is done again on
  the new value, up to 16 times; after that the write fails with an error, never silently. One
  conditional statement was chosen over a transaction because it behaves the same on a file, a
  remote and a replica: a replica reads its local copy and sends writes to the primary, so a
  transaction there would read one database and write another. After a failed attempt a replica
  syncs before reading again. Nothing is held open while the edit runs, and no model is called
  inside it.
- **A new label is claimed, not picked.** A new skill is inserted with `ON CONFLICT DO NOTHING`; if
  no row was written the label was taken, and `-x` is appended and the insert tried again. A row
  another writer wrote is therefore never overwritten. This covers `jli teach` in every mode, a
  translated row and its fallback, and a skill the agent proposed. The router still shows the
  label it planned before "Run it?"; if another writer took it while the command ran, the skill is
  stored as the next free label and `--json`'s `label` and `learned` say so.
- The upsert that re-teaches a label (`Brain::add_skill`) still reads aliases and then writes. No
  command calls it any more: every path that creates a skill now claims a label instead.

### How project scope would attach

A later change is to add a project id read from a `.jlirc`. It would set `Scope.project`, and
every statement already filters and keys on `(user_id, project_id)`, so the default user inside a
project is just another scope in `scoped_skills`, and the default user outside any project stays in
the old tables. If a project's skills should be seen *alongside* a user's own rather than instead
of them, the matrix becomes the union of two scopes read the same way, with the narrower scope
winning a label both hold. Which of the two it should be is that change's decision.

**Measured on 2026-10-03**, live, with a scratch brain written first by the binary built from
`origin/dev` (`83db6b6`):

| Step | Result |
| --- | --- |
| `dev`: `teach --mode=manual "how big is this folder" -- du -sh .`, then `-y where am I` | a brain from before users |
| this version: `skills`, `history`, `stats`, `config` as the default user | byte-identical to the `dev` binary's output |
| `--user-id=agent teach "count the todo items" -- grep -c '^- ' todo.md` | translated to `grep -c '{pattern}' {file}` as `count-lines-matching-pattern`, verified at 0.93; `skills` shows 10 skills, `…, user agent` |
| `--user-id=agent --json -n count the open items in the todo list` | System 1, `count-lines-matching-pattern`, 0.93 |
| the same request as the default user | not in its matrix: System 1 found nothing, and the agent proposed a new `count-todo-items` (dry run, nothing kept) |
| `--user-id=agent share count-lines-matching-pattern --to default` | `Shared … with default.`; a second time: `default already runs the same command as count-lines-matching-pattern` |
| the request again as the default user | System 1, `count-lines-matching-pattern`, 0.95; `skills` shows `shared by agent` |
| `JLI_USER_ID=agent jli -y what is in this folder` | in the agent's `history` only; the default user's `history`, and the `dev` binary's, show only `where am I` |
| `-n fix` as the default user | `would unlearn that "where am I" means print-working-directory`, its own last run |
| `--user-id=agent config mode manual` | the agent's `config` shows manual; the default user's shows semi |
| the `dev` binary afterwards: `skills`, `-n where am I`, `history` (reads and a dry run, no write) | each worked; it saw the default user's 11 skills (its own, the seeds and the one shared to it) and none of the agent's, and its `skills` output differed from before only by the shared skill |
| `--user-id=Agent skills`, `--user-id agent skills` | both refused, exit 1 |

The acceptance checks 1 to 8 of [ADR 9](0009-port-to-rust.md) were re-run and held, once as the
default user and once with `JLI_USER_ID=agent`, each on a fresh brain. Check 12, `jli demo` as
`--user-id=agent`, exited 0: answered instantly 2 of 5, then 4 of 5, and the time spent deciding
fell from 6.9 s to 2.9 s. The lesson that stayed with the agent was the one with a number,
`git log -n 2 --oneline`, which went to the agent both times.

## Consequences

- An older jli on the same brain is the default user and nothing else. It never sees another
  user's skills, events or settings, so it cannot route to them, forget them or fix them. It does
  see a skill shared *to* the default user, as an ordinary skill.
- Every brain opened by this version gains four tables, empty until a second user or a share uses
  them.
- Two users holding one label each have their own row; nothing joins them. A lesson one user
  teaches never reaches another except through `jli share`.
- **Known gap:** an older jli does not know `--user-id`. `jli --user-id=agent skills` on it is a
  request, `--user-id=agent skills`, routed and possibly run; and `JLI_USER_ID` is ignored, so an
  agent running an older jli writes its lessons into the default user's brain.
- **Known gap:** two writers teaching the same new command at the same moment get two skills,
  `du-sh` and `du-sh-x`, running the same thing. Nothing is lost or overwritten, but the matrix is
  split until one is forgotten.
- **Known gap:** there is no list of users, no way to remove a share other than `forget`, and no
  merging when the other user has the label.
- **Known gap:** the compare-and-swap and the label claim have run only against local files, from
  separate connections and threads. Like the rest of the cloud setup they have not run against a
  live remote or replica, and the sync between attempts on a replica is untested.
- **Known gap:** under more than 16 conflicting writes in a row to one skill's column, the write
  fails with an error and the request reports it. Nothing is lost silently, but the lesson is not
  kept either.
