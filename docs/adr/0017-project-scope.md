# 17. What is learned inside a project stays with it, global skills are still used there, and a project skill becomes global only with approval

- Status: accepted; superseded in part by [ADR 22](0022-project-scenarios.md), which keeps a
  scenario that `jli eval --optimise` rewrites inside a project for that project, instead of writing
  the global row; and by [ADR 26](0026-project-flag.md), under which `--project` names the project
  from any directory instead of the nearest `.jlirc`, and `jli skills --project` lists only the
  project's own skills
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 15](0015-user-scope.md), on `project_id` (now set, from a `.jlirc`), on
  where settings and seeds live inside a project, and on what `jli share` copies from inside one

## Context

Skills were global to a user. "Run the tests" taught as `cargo test` in a Rust repository routed
there confidently in a Node repository, and correcting it there to `npm test` put "run the tests" as
a counter-example on the cargo skill, which broke the Rust repository: two projects undid each
other's corrections. Four independent design reviews raised it.

Reproduced live on 2026-10-03 with the binary built from `dev` (`d7a1da8`), a scratch brain and two
scratch git repositories, `api` (a cargo crate) and `web` (an npm package):

| Step | What happened |
| --- | --- |
| in `api`: `teach "run the tests" -- cargo test` | translated to `run-tests`, verified at 0.97 |
| in `web`: `-y run the tests` | System 1 picked `run-tests` at 0.88: `cargo test`, exit 101 |
| in `web`: `-y fix` | "run the tests" became a counter-example of `run-tests`; the agent proposed `npm test`, kept as `run-tests-x` |
| in `api`: `-n run the tests` | **`npm test`**, `run-tests-x` at 0.85: the Rust project's own lesson was undone |
| in `api`: `-y run the tests`, then `-y fix` | back to `cargo test`, and now `run-tests-x` holds "run the tests" as a counter-example |
| `skills` | both rows list "run the tests" as a counter-example, and `run-tests` lists it as an example too; `web` and `api` each route right at only 0.73 and 0.71 |

The owner's decision on what identifies a project: "it should not be a file path, it likely should
be a .rc file in the project that references an identifier that is sent with the tool (like
.jlirc)". And it "should also likely be built alongside a 'jli generalize' command that would let a
user point a system 2 model to a project scoped skill (or all skills) and ask the system 2 model to
propose generalizations that make the skills global (this proposal must be user approved before
written)".

[ADR 15](0015-user-scope.md) left the key ready: `scoped_skills`, `scoped_events` and `scoped_meta`
are keyed by `(user_id, project_id)`, and every statement in `src/store.rs` already filters on both.
It left one choice open: whether a project's skills are seen instead of the user's own or beside
them.

## Decision

### A project is named by a `.jlirc`, and the name is not a path

```text
# jli project: what jli learns in this directory and below it is kept for this project.
# The id is not a path, so every checkout of the project is the same project. Safe to commit.
project = shop-api-4k2x9q
```

- **Format.** `key = value` lines; blank lines and `#` comments are skipped; the value may be
  quoted. Only `project` is read. Other keys are ignored, so a later jli can add some without an
  older one stopping. A second `project` line is an error.
- **The id** follows the user id rule: 1 to 32 lowercase letters, digits, `-` or `_`, starting with
  a letter or digit. Anything else is refused: `a project id is 1 to 32 lowercase letters, …, got
  "Shop API"`. So a path cannot be an id, and the same project on another host, in another checkout
  or after a move is the same project.
- **Finding it.** The nearest `.jlirc` at or above the working directory wins, so a nested project
  is its own. Inside `$HOME` the search checks `$HOME` itself and stops there: a stray file in
  `/Users` would otherwise make every directory one project. Outside `$HOME` (a CI runner, `/tmp`)
  it goes up to `/`.
- **A file that is there but cannot be read stops every command** that opens the brain, with the
  file's path and the reason: `…/.jlirc is not a valid .jlirc: it has no project = <id> line`.
  Ignoring it would write the project's lessons as global ones, which is the defect this record
  exists to fix. `--json` reports it as `"error": "configuration"`. `jli help`, `jli demo` and
  `jli shell-init` open no brain and still work, so a broken file does not break a shell's startup;
  `jli project init` does open it, so under a broken parent file that file must be fixed first.
- **It should be committed.** It holds only the id, no secret and no path, and committing it is
  what makes every checkout, and every collaborator on a shared brain, the same project.
- **No `.jlirc`, no project**: every command behaves exactly as before. This was checked
  byte for byte (see the measurements).
- `jli project init [<id>]` writes `.jlirc` in the working directory. Without an id it makes one
  from the directory's name and six random characters (`shop-api-4k2x9q`), so two directories
  called `api` do not meet. It never overwrites a file: `There is already a .jlirc here, for project
  shop-api. Nothing was changed.`, exit 1. Inside another project it says this directory is now its
  own.
- `jli project` shows the id, the file it came from, how many skills are scoped to it and how many
  global skills it has lessons about. With no project it says so and how to start one.

### Inside a project the matrix is the project's skills and the user's global ones

A project must not lose `ls` and `git status`, so its skills are seen **beside** the user's, not
instead of them: the matrix is the union of the two scopes, read the same way, ordered as one table
is (most used first, then by label). Where both hold a label, the project's row is the one seen.

Every write inside a project goes by who owns the row, decided in `src/store.rs`, so the router,
the learning loop, `jli fix` and the translator are scoped without knowing it:

| What happens inside a project | Where it is written |
| --- | --- |
| a new skill: from the agent, `teach`, a correction, the translator's fallback | a project row |
| a phrasing or counter-example for a project skill | that project row |
| **a phrasing or counter-example for a global skill** | **a lesson the project keeps about it (`project_lessons`); the global row is never written** |
| `jli teach --global …` | the user's global skills, as if outside every project |
| another name for a slot (`dir=` for `{directory}`) | the row that holds the skill, global or the project's: a name for a value does not steer a route |
| a use | the row that ran |
| an upsert of a label only the global skills hold (`jli eval --optimise` rewriting a scenario) | the global row, for the same reason as an alias |
| `mode` and `confidence` | the user's: a project is not a new user |
| the seed skills | the user's global skills only; a project gets no copy |
| requests (`history`, `stats`, `jli fix`) | the project's own events |
| plans and corrections ([ADR 18](0018-agent-contract.md)) | the project's: ADR 18 keys them by user and project, so `jli run <planId>` and `jli fix <eventId>` find a plan or an event made in a project from inside that project |
| `jli eval` records | the project's (they are kept in its `scoped_meta`) |

**The ping-pong.** A lesson about a global row holds a list of phrasings and a list of
counter-examples. The project sees the global row with its lesson applied:

- examples: the row's, without any the lesson calls a counter-example, then the lesson's own;
- counter-examples: the row's, without any the lesson calls an example, then the lesson's own;
- each kept to the newest 16, as a row is.

So in `web` System 1 is shown `cargo test` with "run the tests" under `doesNotApplyTo` and not
under `examples`, and `npm test` with it under `examples`; in `api` it is shown exactly the global
row. The System 1 question is unchanged ([ADR 3](0003-system-1.md)); only the rows, examples and
counter-examples sent for this project differ. A correction in one project never reaches another,
because it is never written where another project reads. A lesson is about one row: it names the
row's `created_at`, as a share does, so a global skill forgotten and taught again starts clean in
every project.

A phrasing is "already known" in a project when the project already sees it, so a route to a global
skill at less than 0.95 that the global row already lists teaches nothing, as before.

**Label clashes.** A new project skill never takes a label the global skills hold: it gets the
next free one (`du-sh-x`), as a label another writer took does ([ADR 15](0015-user-scope.md)). A clash
can still arise, from another writer, a later `teach --global` or an older jli; then the project's
row is the one seen, and `jli skills` ends with `Hidden here by a project skill of the same name:
the global …`.

**Commands inside a project.**

- `jli skills` lists the union and tags each skill `project shop-api` or `global`, with `+N −M
  learned here` when the project has a lesson about it. Outside a project the listing is as before.
- `jli forget <label>` forgets the project's skill; if a global skill of that label was hidden, it
  is seen again (`Forgot run-tests in project shop-api. The global run-tests is used here again.`).
  A global skill is not forgotten from inside a project: `run-tests is a global skill, used in
  every project, so it was not forgotten. To forget it everywhere: jli forget --global run-tests`,
  exit 1.
- `jli fix` reads the project's events and unlearns where the lesson belongs, by the table above.
- `jli stats` and `jli history` show the union's skills and the project's requests; the brain's
  location reads `…, project shop-api`.

**`jli share` and projects.** The project id is not a path, so another user who works in the same
repository is in the same project. Sharing a project skill copies it into the other user's copy of
that project (`Shared run-web-tests with agent, in project shop-web.`). Sharing a global skill from
inside a project copies the global row as stored, to their global skills: the project's lessons
about it stay here, for the reason counter-examples are not copied. The comparisons that refuse a
share (same label, same command) are made against what the other user would see in that scope.

**Users.** A project's rows, lessons and events are keyed by user and project, so the agent's
`shop-api` and the default user's `shop-api` are as separate as their global skills.

### Storage

One table is added, `CREATE TABLE IF NOT EXISTS` on open like the others, kept in its own
statement so that it does not touch `SCHEMA`:

```sql
CREATE TABLE IF NOT EXISTS project_lessons (
  user_id, project_id, label,
  created_at,                     -- the global row the lesson is about
  examples, counter_examples,     -- JSON lists, as in skills
  PRIMARY KEY (user_id, project_id, label, created_at));
```

Project skills and events live in `scoped_skills` and `scoped_events`, which ADR 15 created, with
`project_id` set. No column is added, no row is rewritten, and a lesson list is written with the
same compare-and-swap as a row's ([ADR 15](0015-user-scope.md)). A brain from before this change
opens unchanged: a test opens one written as raw SQL inside a project, learns a project skill and a
lesson about a global one, logs a request, and finds every row and table definition of the old
tables as they were.

### `jli generalize [<label>]`: a project skill becomes global only once the user approves

With a label it looks at that project skill; without one, at every skill of the current project;
`--all-projects` looks at every project skill of the user, in every project, and works outside a
project too. For each skill, one at a time:

1. **The agent is asked** (a new strict-JSON call, `generalization`, in `src/system2.rs`, through
   `src/llm.rs`). It is given the project skill (steps, scenario, slots with aliases, phrasings,
   risk); the skills in the user's other projects that look like the same request (same label, or
   two words or more in common outside a short stop list; up to ten); and the user's global skills.
   It answers `promote` (global as it is), `parameterize` (the project's values become `{slot}`s,
   with aliases), `merge` (a global skill already runs it, named in `mergeInto`) or `keep` with a
   reason. The instructions say that System 1 knows nothing of a project's language, so a global
   `cargo test` would be offered in a Node project too, and to keep when unsure.
2. **The proposal is checked without any model.** Its template must give back the project's
   command (`match_template`), declare every placeholder, and not make a flag a slot. A command a
   global skill already runs, or a template one already produces, is that skill: the proposal
   becomes a merge into it ([ADR 13](0013-learning-fixes.md)). A merge the agent names must be into a
   global skill that runs the command. A proposal that fails is `Not offered:` with the reason.
3. **It is shown**: the skill before and after (steps, slots with their aliases, scenario,
   phrasings and counter-examples), and what approving does: `If approved: get-directory-size
   becomes a global skill, with the 1 phrasing of get-directory-size. The project skill
   get-directory-size is then removed from shop-api.`
4. **System 1 checks it before anything is written**, with the matrices as they would be:
   - the check every new row gets (`eval::verify_row`, [ADR 16](0016-measuring-the-matrix.md)) in the
     project, without its own row and with its counter-examples as its lesson about the global row;
   - the same check among the global skills, which every other project sees;
   - the project's other phrasings (up to seven more) must reach the row there;
   - the newest phrasings of the user's other projects' own skills (up to eight) must not land on it
     in those projects. This is where promoting one project's "run the tests" would take another's.
5. **It is written only on approval.** `Make it global? [Y/n]` per proposal. With nobody at the
   keyboard nothing is written (`Not written: nobody to approve it. Re-run with --approve-all to
   write what passes the checks.`). `--approve-all` writes each proposal that passed every check
   without asking; it is a flag of its own, set by no environment variable, because `JLI_YES` and
   `JLI_YOLO` are exported in shells and must not approve a change to every project. `-n` shows the
   proposals and their checks and writes nothing.

**What happens to the project row.** Once approved, the global row is written first: a new skill,
claimed like any new label, with the project row's phrasings, steps, slots and origin, and the more
severe of the two risks; or, for a merge, the project's phrasings added to the global skill (each
lifted from its counter-examples there). The project row's counter-examples become the project's
lesson about the global row: they mark where the skill ends against that project's other skills.
Then the project row is deleted, so the two do not compete. Nothing is deleted silently: the trace
says `get-directory-size is global now, as get-directory-size. The project skill get-directory-size
was removed from shop-api.`, and the project's history records it as a request that ran nothing,
with the old command and `generalized: project skill … of shop-api is now global skill …, and the
project row was removed`. It carries no label, so `stats` and `jli fix` pass over it.

### Measured on 2026-10-03

**The ping-pong, live, with this branch's binary**: the same scratch repositories, each with a
`.jlirc` (`shop-api`, `shop-web`), on a fresh brain.

| Step | What happened |
| --- | --- |
| in `api`: `teach "run the tests" -- cargo test` | `run-tests`, verified at 0.97, `new skill · kept for project shop-api` |
| in `web`: `-y run the tests` | System 1: none of 9 skills fits (1.00); the agent proposed `npm test`, which ran and was kept in `shop-web` |
| `-n run the tests` in each | `api`: `cargo test` at 0.97. `web`: `npm test` at 1.00 |

The harder case, a global skill corrected in one project, on another fresh brain:

| Step | What happened |
| --- | --- |
| in `api`: `teach --global "run the tests" -- cargo test` | `run-tests`, `new skill · global, for every project` |
| in `web`: `-y run the tests` | `run-tests` at 0.95: `cargo test`, exit 101 |
| in `web`: `-y fix` | the counter-example went to `shop-web`'s lesson; the agent proposed `npm test`, kept as `run-web-tests` in `shop-web` |
| `-n run the tests` in each | `api`: `cargo test` at 0.98. `web`: `npm test` at 1.00 |
| `skills` in `web` | `run-tests … global, +0 −1 learned here` with `− "run the tests"`; `run-web-tests … project shop-web` |
| the `dev` binary, `-n run the tests` in `web` | `cargo test`: an older jli does not read `.jlirc` (a known gap below) |

Repeated after rebasing on `dev` with ADR 18 merged (`fa42b27`), on a fresh brain: `run-tests` at
0.88 in `web` ran `cargo test`; `-y fix` kept the counter-example in `shop-web` and learned
`npm test` as `run-tests-x` there; then `api` routed `cargo test` at 0.95 and `web` `npm test` at
0.98, and outside both projects `skills` listed the user's 10 global skills only.

**`examples/bench-context.rs`**, run twice. Five phrases ("run the tests", "build the project",
"start the app", "lint the code", "format the code"), each taught to a cargo command in a Rust
project and an npm command in a Node project, three held-out paraphrases each, both projects seen
from a directory called `app`, semi mode. The Rust project learns first; the Node project routes
each phrase and is corrected as the router corrects a "no" where it is wrong; then the Rust project
is corrected back where it went wrong. Right command at or above 0.5, phrases out of 5 and
paraphrases out of 15, second run:

| Arm | Node phrases that ran cargo before Node learned | After Node's corrections: Rust | Node | Rust corrections needed | After them: Rust | Node |
| --- | --- | --- | --- | --- | --- | --- |
| one scope (no `.jlirc`, as on `dev`) | 5/5 | 0/5, 2/15 | 5/5, 11/15 | 5 | 5/5, 11/15 | **0/5, 0/15** |
| two projects | 0/5 | 5/5, 15/15 | 5/5, 15/15 | 0 | 5/5, 15/15 | 5/5, 15/15 |
| two projects, the cargo commands taught as global skills | 5/5 | 5/5, 15/15 | 5/5, 10/15 | 0 | 5/5, 15/15 | 5/5, 11/15 |

Before any correction each arm's Rust project routed 5/5 and 15/15. The "one scope" arm is this
binary with both projects opened without a project, which is what a brain without a `.jlirc` does:
the outputs of `skills`, `history`, `stats` and `config` from the `dev` binary and this one, on a
brain written by `dev`, were byte-identical. In the first run the numbers were the same in shape:
one scope fell to 0/5 and 1/15 for Rust after Node's corrections and to 0/5 and 1/15 for Node
after Rust's; in the two-project arm the Rust project scored 4/5 and 12/15 once after Node's
corrections, which cannot reach it, so that is System 1's own variance between calls; in the global
arm Node reached 4/5 and 6/15. 522 System 1 requests in the second run, about $0.03.

The third arm shows the limit of a lesson. It blocks the phrasing that was corrected, so the other
project is untouched, but a paraphrase in the correcting project can still go to the global skill,
whose scenario fits it as well as the project's own: 10 or 11 of 15 against 15 of 15 when the
skill was never global. That is why a command that belongs to one kind of project should be taught
inside it, and why `generalize` prefers `keep` for one.

**`jli generalize`, live**, on the second brain, with Mercury 2.5:

| Request | The agent | Then |
| --- | --- | --- |
| `-n generalize` in `api` (two project skills) | `cargo clippy --all-targets -- -D warnings`: promote, as `run-cargo-clippy-strict`; `du -sh {directory}`: promote, as it was | both verified (in the project, among the global skills, and 1 phrasing of `shop-web` still home); `Dry run: nothing was written.` |
| `generalize` in `api` under `expect`, answering n, then y | the same two promotions (the clippy one named `run-clippy-strict` this time) | the first `Left in project shop-api. Nothing was written.`; the second written: global `get-directory-size`, the project row gone, the event in `history` |
| the request in `web` afterwards, `-n how big is the src folder` | | `get-directory-size` at 1.00: `du -sh src` |
| `generalize` in `api`, no terminal | promote the clippy skill | verified, then `Not written: nobody to approve it…`; still a project skill |
| `-n generalize run-web-tests` in `web` | keep: "npm … is specific to JavaScript projects" | `stays in project shop-web` |
| `-n generalize run-tests` in `api`, where `shop-web`'s `run-tests` is `npm test` | keep: "shop-api uses cargo (Rust) while shop-web uses npm" | nothing written; the same for both with `--all-projects` from outside a project |

The acceptance checks 1 to 8 of [ADR 9](0009-port-to-rust.md) were re-run inside a project (a
fresh brain, a scratch git repository with a `.jlirc`) and held: `ls -ltr` for check 1, `git commit
-m 'first draft'`, not a command, exit 3 with no terminal, a destructive proposal refused with the
files untouched, valid `--json` on the `system1` path, the second teach joining
`git log --oneline -n {count}` with every API key unset (`kept for project acc-check`), and `-n 7`.
Checks 9 to 12 were not re-run: nothing in this change touches the terminal, auto mode's diagnosis
or the demo, which opens no project.

## Consequences

- Inside a project, a correction, a new phrasing or a new skill changes that project only. Outside
  every project nothing changed.
- Global skills now change from inside a project only by `teach --global`, `forget --global`, an
  alias, a use, an upsert, or an approved `generalize`.
- Every brain opened by this version gains one table, empty until a project learns something about
  a global skill.
- **Known gap:** an older jli does not read `.jlirc`. Inside a project directory it routes against,
  and teaches, the global skills, and it does not see the project's skills or lessons.
- **Known gap:** a lesson only blocks the phrasing it was learned from. A paraphrase in the same
  project can still route to the global skill it corrected (the third arm above).
- **Known gap:** what a project learns about a global skill has no way to become global except
  `teach --global` by hand; `generalize` looks at project skills, not at lessons.
- **Known gap:** if jli stops between writing the global row and deleting the project row, both
  remain; with the same label the project's row hides the global one in that project.
- **Known gap:** a project's id cannot be renamed: a changed id is a new project, and the old one's
  skills stay under the old id, reachable with `--all-projects` and by putting the old id back.
  There is no list of projects other than `generalize --all-projects`.
- **Known gap:** which skills of other projects the agent is shown is a word overlap, not a model's
  judgement, and `generalize`'s check replays at most eight phrasings of other projects.
- **Known gap:** there are no per-project settings; `mode` and `confidence` are the user's.
- **Known gap:** `jli eval --optimise` inside a project measures the project's matrix but rewrites
  a global row's scenario in place, for every project.
- **Known gap:** the `generalization` prompt was measured on the six live requests above, not on a
  benchmark built for it, and the store has run only against local files.
