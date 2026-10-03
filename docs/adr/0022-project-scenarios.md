# 22. A scenario rewritten inside a project is kept for that project, and the global row is left as it was

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 16](0016-measuring-the-matrix.md), on how a kept rewrite is written and
  what `jli eval undo` puts back inside a project; [ADR 17](0017-project-scope.md), on where
  `jli eval --optimise` writes the rewritten scenario of a global skill from inside a project

## Context

[ADR 17](0017-project-scope.md) says that what is learned inside a project stays with it. One path
did not hold to it (issue #21, a known gap of ADR 17). Inside a project, `jli eval --optimise`
measures the project's matrix: the project's own skills, and the user's global skills with the
project's lessons about them. When a rewrite of a global skill passed the gate, `write` stored its
scenario through `add_skill`'s upsert. For a label only the global skills hold, that upsert writes
the global row (ADR 17's table: "an upsert of a label only the global skills hold … | the global
row"). So a scenario chosen because it helped one project's phrasings became the scenario every
other project, and every directory outside a project, was shown. Nothing measured it there.

The upsert was sent to the global row on purpose: the alternative at the time was a project copy of
the whole row, which would hide the global skill's later phrasings, slots and uses from the project.
The scenario alone was never considered as something a project could keep.

Reproduced offline on `dev` at `ad24a7c`: in `a_rewrite_measured_in_a_project_stays_in_that_project`,
two global skills are optimised from project `a` with a fake System 1 and a fake agent. The rewrite is
kept, and then, outside every project and in project `b`, `disk-usage` read "the user wants the total
size of the current directory" instead of "the user wants disk use". The test fails on `dev` at that
assertion.

A second defect came to light in the same path. Inside a project, `remove_example` on a global skill
does not touch the global row. It removes the phrasing from the project's lesson, and only if the
lesson holds it ([ADR 17](0017-project-scope.md): "the global row's phrasing is not the project's").
So when a rewrite dropped one of the global row's own phrasings, the gate measured the matrix without
it, the record said `drops "…"`, and the matrix kept it. Counter-examples were already right: one for
a global competitor becomes the project's lesson about that competitor.

The issue named two ways out:

| Option | Cost | What other projects see |
| --- | --- | --- |
| **Keep the rewrite in the project** | one table; no extra System 1 call | nothing changes for them |
| Gate the rewrite on the global matrix and on every other project's phrasings as well, then write the global row | the gate's probes again among the global skills and in each other project's matrix | a rewrite that passes changes the wording for every project, though only one was measured as better; a rewrite that helps one project but costs another a phrasing is lost for both |

## Decision

### A rewrite measured inside a project is kept for that project

This is the direct reading of ADR 17. From inside a project a global skill changes only by an explicit
act: `teach --global`, `forget --global`, or a `generalize` the user approves. `--optimise` asks
nobody, because the measurement is the approval ([ADR 16](0016-measuring-the-matrix.md)), and that
measurement covered one project. A better scenario for every project is found where the global
matrix is measured: run `jli eval --optimise` outside every project.

A project keeps **only the scenario**, not a copy of the row. It still sees the global row's steps,
slots and aliases, phrasings, risk and uses, and its own lessons on top. A phrasing taught to the
global skill later still reaches the project.

### Storage

One table, added on open with `CREATE TABLE IF NOT EXISTS` in a statement of its own:

```sql
CREATE TABLE IF NOT EXISTS project_scenarios (
  user_id, project_id, label,
  created_at,   -- the global row the scenario is for
  scenario,
  PRIMARY KEY (user_id, project_id, label, created_at));
```

- No column is added and no existing row is written. An older jli never reads the table. It ignores
  `.jlirc` anyway, so inside a project it sees the global scenario, as before.
- Like a lesson, the scenario names the global row's `created_at`. A global skill that is forgotten
  and taught again starts clean in every project.
- It holds one value, written with an upsert (`ON CONFLICT … DO UPDATE SET scenario`) as a setting
  is. Of two writers the later one wins. There is no list in the row that a second writer could lose.
- `Brain::set_scenario` writes nothing when the scenario already reads that way in the project, and
  it never stores a copy of the global row's scenario, which would stop later global rewrites from
  reaching the project. Asked for the global row's own text, it removes the project's scenario
  instead.

### Where it is seen

`Brain::skills()` and `Brain::skill()` inside the project apply the project's lesson to a global row,
then its scenario. Every reader goes through those two:

- **System 1** is shown the project's scenario as that skill's `scenario` criterion. The
  questions, the instructions and the shape of the state are unchanged ([ADR 3](0003-system-1.md)):
  a scenario is per-skill data, and only the rows sent for this project differ.
- **The agent** is shown the matrix it routes against, so it sees the same scenario.
- **`jli eval`** measures with it. `--optimise` starts from it, so a second rewrite in the same
  project builds on the first one.
- **`jli skills`** lists the skill with that scenario, tagged `global, scenario rewritten here`
  (after `+N −M learned here` when the project also has a lesson). **`jli project`** adds a line,
  `its own scenario for 1 global skill, from jli eval --optimise`, when there is one.
- **`jli generalize`** reads each project's matrix through `view_of`, so its checks measure a
  project's own scenario in that project, and the global row's scenario among the global skills.

Outside the project, in every other project and for every other user, the global row's scenario is
the one seen.

### How a kept rewrite is written

| Part of the rewrite | A skill of the project's own, or outside every project | A global skill, inside a project |
| --- | --- | --- |
| the scenario | the row's `scenario` column | **the project's own scenario**; the global row is not written |
| a dropped example | removed from the row | only a phrasing the project learned itself (its lesson) can be dropped. A global row's own phrasing is taken out of the rewrite **before** the gate measures it, so what is measured is what is written |
| a counter-example for a competitor | the competitor's row | the project's lesson about the competitor, as before. In the gate it also hides the phrasing among that competitor's examples, as a lesson does |

If dropping a global phrasing was all a rewrite changed, it is discarded as `the rewrite changed
nothing that can be kept here: "space used here" is a phrasing of the global disk-usage, which a
project cannot drop`, and that problem is the feedback for the agent's one retry.

The scenario goes through `Brain::set_scenario` in every case. Outside a project, or for a project's
own skill, it updates the `scenario` column with the compare-and-swap that list columns use
([ADR 15](0015-user-scope.md)), where it used `add_skill`'s upsert of the whole row. The result is the
same. No command calls `add_skill` any more.

**The record.** As before, the change is stored under `optimised:<ts>:<label>`, in the `meta` of the
scope it was made in. Inside a project that is the project's `scoped_meta` (ADR 17 keeps eval records
there). Two fields are appended to the record and to each change in the `--json` report, and are left
out when empty: `project`, the project whose own scenario a rewrite of a global skill was kept as, and
`projectHadScenario`, true when that project already had a scenario of its own. The trace and the
report say `kept for project shop-api` and `for project shop-api only` beside a change kept that way.

### `jli eval undo`

A change is undone where it was made: inside a project, `undo` finds that project's records only;
outside every project, only the global ones. For a change kept as a project's scenario:

- if the project had no scenario of its own before, `undo` removes the project's scenario, and the
  project sees the global scenario again, whatever it says now. Writing the old text back would fix
  the project on a scenario that a later rewrite outside every project would no longer reach;
- if it had one, `undo` puts that one back.

It says `Put back the scenario of disk-usage in project shop-api: …`, and `--json` adds `project`.
The refusal when the scenario has changed since the rewrite stays as it was. Removing a project's
scenario is the one row `undo` deletes; the record keeps both texts.

### What the other commands do with a project's scenario

Each of these is covered by `a_project_keeps_its_own_scenario_for_a_global_skill` in `src/store.rs`:

| Command | What happens to the project's scenario |
| --- | --- |
| `jli teach --global` | nothing: it works on the global skills as if outside every project, and never reads the project's scenario. A phrasing it adds to the global skill keeps the row, so the project's scenario still holds there |
| a rewrite outside every project | changes the global row; the project's own scenario still wins in that project |
| `jli generalize`, a merge into the global skill | keeps the global row, and so the project's scenario. A promotion as a new skill makes a new row, with no project scenario. `generalize` never takes a project's scenario to the global row |
| `jli share` of the global skill from inside the project | copies the global row as stored: the project's scenario stays here, as its lessons do |
| a project row of the same label (a clash, ADR 17) | the project row is seen and the scenario is not applied; once that row is forgotten, the global row comes back with the project's scenario |
| `jli forget <label>` inside the project | refuses a global skill, as before, and leaves its scenario |
| `jli forget --global <label>` | deletes the global row. The project's scenario names that row's `created_at` and is never read again; the label taught again starts clean |

## Measured

Offline, with the fakes in `src/tests.rs`:

- `a_rewrite_measured_in_a_project_stays_in_that_project` (`src/tests/eval_tests.rs`): a rewrite
  kept in project `a` is what System 1 is shown there, and the global row is untouched. Outside every
  project and in project `b`, System 1 is shown the old scenario and the counter-example is absent.
  `undo` from outside and from `b` finds nothing, and `undo` in `a` brings back the global scenario
  there. On `dev` the test fails because the global row was rewritten.
- `a_project_cannot_drop_a_global_phrasing_and_undo_puts_back_its_own_scenario`: a rewrite that only
  drops a global phrasing is discarded with the problem above, which reaches the agent's retry. The
  retry is kept over the project's earlier scenario, and `undo` puts that scenario back, not the
  global one.
- `a_project_keeps_its_own_scenario_for_a_global_skill` (`src/store.rs`): the table above. Every other
  table, with its definition and rows, is byte-identical before and after a project's scenario is
  written.
- `a_rewrite_is_measured_as_a_project_would_keep_it` (`src/eval.rs`): the gate's in-memory matrix,
  outside and inside a project.

**Live, on 2026-10-03**, with this branch's release binary, a scratch brain and a scratch git
repository. Outside every project, `du-sh` (`du -sh .`) was taught in manual mode with four
phrasings, so its scenario was "the user wants: how big is this folder". `jli project init` made two
projects in subdirectories, `shop-api` and `shop-web`. In `shop-api`, two `-y` requests routed to
`du-sh` (at 0.83 and 1.00).

| Step | What happened |
| --- | --- |
| in `shop-api`: `eval --optimise --max-skills=2` | 12 phrasings measured (10 held-out examples, 2 replays); `du-sh` ranked first at 0.12. Rewrite 1 was **kept**: score 0.933 → 0.962, 9 of 9 phrasings home before and after, scenario "the user wants to know how much disk space a folder or directory currently uses", and the counter-example "how much disk space this folder uses" on `git-status`. The trace said `kept for project shop-api`. 21 System 1 calls ($0.0012) and 1 System 2 call |
| `skills` in `shop-api` | `du-sh … global, +1 −0 learned here, scenario rewritten here`, with the new scenario; `git-status … global, +0 −1 learned here`, with the counter-example |
| `skills` in `shop-web`, and outside every project | `du-sh` with "the user wants: how big is this folder"; `git-status` with no counter-example |
| `project` in `shop-api` | `its own scenario for 1 global skill, from jli eval --optimise` |
| `eval undo -n` in `shop-api`, in `shop-web`, outside | `Would put back the scenario of du-sh in project shop-api: the user wants: how big is this folder`, and the counter-example; in `shop-web` and outside, `No optimisation to undo.`, exit 1 |
| `eval undo` in `shop-api` | `du-sh` back on the global scenario there, without the tag; `git-status` without the counter-example |
| in `shop-api` again: `eval --optimise --max-skills=1` | kept again, 0.928 → 0.969, with another scenario: "the user wants to know the total disk usage of a specific path or directory" ([ADR 16](0016-measuring-the-matrix.md): rewrites are not repeatable) |
| a binary built earlier that day from the main checkout, without `project_scenarios`, reading the same brain | `skills` outside every project and in `shop-web` byte-identical to this binary's; in `shop-api` the same except the tag and `du-sh`'s scenario, where it shows the global one, as an older jli is expected to |

The acceptance checks of [ADR 9](0009-port-to-rust.md), on a fresh brain in a scratch git repository:

- **Held:** checks 1 to 6 and 9 to 12. Check 1 gave `ls -ltr`. Check 2 gave `git commit -m 'first
  draft'`. Check 3 was not a command, exit 0. Check 4 gave `git add -A`, not run, exit 3. In check 5
  the agent proposed `rm '*.log'`, destructive, refused with exit 3, and the files were untouched.
  Check 6 printed valid `--json`, `path` `system1`, with the directory in `output`. Checks 9 to 11
  were driven with `expect`. In check 9, auto mode outside a repository routed to `git-status` at
  0.89; after the yes the command exited 128, the diagnosis was "the command fits the request; the
  problem is where it ran", and there was no counter-example. In check 10, after a no, three numbered
  options were shown; picking `ls -lt` learned it and ran it. Check 11 took two requests with
  confirmations and left on ctrl-d. Check 12, `jli demo`, exited 0: answered instantly 2 of 5, then
  5 of 5, and time spent deciding fell from 35.4 s to 1.5 s, because one agent call took 31.4 s (its
  first attempt timed out).
- **Run as a variant:** checks 7 and 8. The agent's sandbox refused any command line that named `git`,
  even as text for `jli teach` to store, so the same mechanism was run with `tail`. `teach "show the 3
  newest lines of the log" -- tail -n 3 app.log` became `tail -n {lineCount} {file}`, verified at 1.00.
  An earlier phrasing was stored literally, because the translator's slots did not give back the
  command, so it was forgotten and taught again. `teach "the final 10 lines of the log please" -- tail
  -n 10 app.log` then joined the template with every API key unset. `-n give me the 7 newest lines of
  app.log` rendered `tail -n 7 app.log`.
- **Not run:** check 13, because the TypeScript version is not available.

## Consequences

- Inside a project, `jli eval --optimise` changes that project only: its own skills, its own
  scenario for global skills, and its lessons. Outside every project it works as before.
- What the gate measures is what is written, in a project too: a drop that a project cannot make is
  not measured.
- Every brain opened by this version gains one table, empty until a rewrite is kept inside a project.
- **Known gap:** a project's scenario has no path to the global row. `generalize` looks at project
  skills, not at a project's scenarios or lessons. A scenario that would suit every project must be
  found again by `jli eval --optimise` outside every project.
- **Known gap:** a project that keeps a scenario of its own does not see later rewrites of the global
  scenario. The only ways to drop it are `jli eval undo` (the newest change still in force for that
  label) and `jli forget --global`. There is no command to drop it by hand, though `jli skills` shows
  which skills have one.
- **Known gap:** inside a project a rewrite cannot drop a phrasing of a global row, only phrasings
  the project learned. A phrasing taught to a global skill by mistake is dropped only outside every
  project, where ADR 16's gap still applies: the gate cannot score such a drop as a gain.
- **Known gap:** a counter-example for a global competitor is measured as hiding the phrasing among
  that competitor's examples, as a lesson does. If the project's lesson already lists the phrasing as
  one of the competitor's examples, the written view shows it in both lists, which the gate did not
  measure.
- **Known gap:** live, a rewrite was kept in a project twice, on one skill of one scratch brain. The
  drop that a project cannot make and `undo` over a project's earlier scenario were verified
  offline only.
