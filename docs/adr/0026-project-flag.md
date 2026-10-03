# 26. `--project` names the project a command acts on, from any directory

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version; feedback from the owner
- Supersedes in part: [ADR 17](0017-project-scope.md), on how a project is named (the nearest
  `.jlirc`, unless `--project` names another) and on what `jli skills` lists

## Context

The feedback: "There needs to be a way to pass --project {{project_id}} when calling 'jli skills' so
the user only sees project-local skills. Similarly, any other command that can be acted on in the
project, should accept a --project flag."

[ADR 17](0017-project-scope.md) names a project only by the nearest `.jlirc` at or above the working
directory. Two things followed:

- **A project could be reached only from inside one of its checkouts.** To see or change what jli
  learned for `shop-api`, a person or an agent had to `cd` there first. A checkout on another host, one
  that was deleted, or a CI job running elsewhere could not reach it. Only `jli generalize
  --all-projects` read every project, and ADR 17 lists "there is no list of projects other than
  `generalize --all-projects`" as a gap.
- **`jli skills` inside a project lists the union**: the project's own skills and every global skill,
  each tagged `project shop-api` or `global`. With the nine seeds and whatever was taught globally,
  the project's own skills were a few lines among many, and there was no way to list them alone.

The scoping itself was already there. The brain keys every row by user and project, and every
command that acts in a project reads the project from the brain it was opened in (`Brain::project`, in
`src/cli.rs`, `src/hand.rs`, `src/eval.rs` and `src/generalize.rs`). Only the choice of project was tied
to the directory.

## Decision

### The flag

- `--project=<id>` or `--project <id>`, anywhere before `--`. After `--` it belongs to the command:
  in `jli teach x -- ls --project=y`, `--project=y` is part of the command taught.
- In the space form the id is the next word, unless that word starts with `-`.
- Either way the id is held to the rule a `.jlirc` id follows (`project::project_id`, [ADR
  17](0017-project-scope.md)), so a word that cannot be an id stops the run as a usage error instead of
  scoping into a project nobody meant:

  ```
  $ jli --project Demo history
  jli: a project id is 1 to 32 lowercase letters, digits, - or _, starting with a letter or digit, got "Demo". --project takes the word after it as the id: for this directory's project, put --project last
  ```

  `jli teach --project "run the tests" -- cargo test` fails the same way: the phrase is not an id.
- `--project` on its own, as the last word before `--` or the end, or followed by another flag, means
  the project of this directory's `.jlirc`. Outside every project that is a usage error, exit 1:
  `Not inside a project: there is no .jlirc here or above. Name one with --project=<id>.` Under
  `--json` it is `{"error": "usage", "errorMessage": …}`, as any other bad flag is.
- Given twice, the last one wins, as `--user-id` does.

### What it means: as if jli ran in a directory of that project

One function decides the project of a run, `scope_of` in `src/cli.rs`: the project `--project` names,
else the one this directory's `.jlirc` names, else none. `App::open` opens the brain in it. Every
command that acts in a project already follows the brain's project, so each of them follows the flag
with no change of its own: a request, `-n` and the plan it keeps, `jli run`, `teach`, `fix` (bare, the
last route run in this directory in that project; with an event id, that project's event), `forget`,
`forget-phrase`, `alias`, `slot`, `eval` and `eval undo`, `share`, `generalize`, `history`, `stats`
and the interactive session.

- `--global` still means the user's global skills: `jli teach --global --project=x …` teaches a
  global skill, as `teach --global` does inside a project.
- Settings stay the user's, as inside a project ([ADR 17](0017-project-scope.md)).
- `jli help`, `jli demo` and `jli shell-init` open no brain and ignore it. A malformed id still stops
  them, as a malformed `--mode=` does.

Three commands read the flag themselves:

| Command | With `--project` |
| --- | --- |
| `jli skills` | only the project's own skills, `1 project skills in …, project demo-app`, each tagged `project demo-app`. No global skill is listed. What the project learned about global skills, a lesson or a scenario of its own ([ADR 22](0022-project-scenarios.md)), is named in one line, since it is not a skill of the project's: `Not listed: what project shop-api learned about the global git-status. Inside the project, jli skills lists them with the global skills.` With no skills of its own: `Project demo-ap has no skills of its own. Projects with skills of their own: demo-app.`, or `…, and no project has any yet.` |
| `jli project` | the project named, `named with --project; this directory is in project shop-web (…/.jlirc)` or `… is in no project`, then its counts as before |
| `jli project init` | refused, exit 1: `jli project init makes this directory a project and names it, so it takes no --project. To choose the id: jli project init <id>` |

`jli skills` without `--project` is as it was: inside a project the union, tagged; outside every project
the user's global skills. `jli generalize` outside a project now says `Run it in a project, name one
with --project=<id>, or add --all-projects.`

### A project jli has nothing for gets a note, not a refusal

A named id that this user has nothing for in the brain (no skill, no lesson or scenario about a global
skill, no request, no plan, no `jli eval` record) may be a typo. Before the command runs, one line goes
to stderr, and then the command runs:

```
$ jli --json --project=demo-ap -n run the tests
! jli has nothing for project demo-ap yet. Projects with skills of their own: demo-app.
{ "query": "run the tests", … }
```

It is a note because an empty project is also how every project starts: teaching into a project from
outside it, as an agent or a CI job would, is the use the flag is for. Refusing would need a second flag
to say "yes, a new one". The note is left out under `--quiet`; for `jli skills`, whose listing says the
same; for `jli project init`, which refuses the flag; and for the project of this directory's `.jlirc`,
which is no typo even when `jli project init` has just made it. Under `--json` stdout stays one JSON
value. The check is `Brain::knows_project`, one query of six `EXISTS` over the tables keyed by user and
project. Nothing is added to the schema.

## Measured

Offline, with the fakes in `src/tests.rs` and the tests of `src/cli.rs`:

- `the_project_flag_names_a_project_or_asks_for_this_directorys`: both forms, bare at the end, bare
  before `--json`, bare before `--`, the flag after `--` left to the command, and `--project=Shop`,
  `--project=` and `teach --project "run the tests"` refused as usage errors.
- `the_project_comes_from_the_flag_then_the_jlirc`: the flag wins over a `.jlirc`, and bare
  `--project` outside a project is a `usage` error with the sentence above.
- `taught_with_the_project_flag_from_outside_every_project_it_is_that_projects`: `teach --mode=manual
  --project=demo-app` from a directory in no project writes a skill of `demo-app` and no global skill;
  `skills --project=demo-app` lists it and none of the seeds; `skills` lists the seeds and not it.
- `a_request_with_the_project_flag_routes_against_that_projects_matrix`: System 1 is shown the
  project's skill beside the global ones, and outside the project it is not shown.
- `a_bare_project_flag_means_this_directorys_project_and_outside_one_it_is_a_usage_error`: from a
  subdirectory of a project, `skills --project` lists its own skills only; `project init` with either
  form is refused and writes no `.jlirc`.
- `a_named_project_jli_has_nothing_for_gets_a_note_and_carries_on`: the note, with and without other
  projects to name; none once the project has a skill, a lesson or a request; none for the
  `.jlirc`'s own project; and the command runs after it.

**Live, on 2026-10-03**, with this branch's debug binary, a scratch brain, and a scratch directory in
no project (`HOME` set to the scratch directory, so the search for a `.jlirc` stopped there):

| Request | What happened |
| --- | --- |
| `teach --mode=manual --project=demo-app "run the tests" -- echo tests` | `! jli has nothing for project demo-app yet.`, then `new skill · kept for project demo-app` |
| `skills --project=demo-app` | `1 project skills in file:…/brain.db, project demo-app`, and only `echo-tests … project demo-app` |
| `skills` | the 9 global seeds, without `echo-tests` |
| `--project=demo-app -n run the tests` | System 1 over 10 skills, `echo-tests` at 0.71: `echo tests` |
| `-n run the tests`, without the flag | 9 skills, none fits (1.00); the agent proposed `pytest` |
| `--project=demo-app -y run the tests` | `echo-tests` at 0.66, ran, printed `tests` |
| `history --project=demo-app`, then `history` | the request above; then `No requests yet.` |
| `skills --project`, and the same with `--json` | `Not inside a project: …`, exit 1; the `usage` error object |
| `skills --project=demo-ap` | `Project demo-ap has no skills of its own. Projects with skills of their own: demo-app.` |
| `--json --project=demo-ap -n run the tests` | the note on stderr, and one JSON outcome on stdout |
| `project --project=demo-app` | `named with --project; this directory is in no project`, `1 skill scoped to it` |
| `--project=Demo history`, `--project Demo history` | the id rule, exit 1; the second with the hint about the space form |
| `project init --project=other` | the refusal, exit 1, with no note above it |

The acceptance checks 1 to 8 of [ADR 9](0009-port-to-rust.md), on a fresh brain in a scratch git
repository, without the flag, held: `ls -ltr`; `git commit -m 'first draft'`; not a command, exit 0;
`git add -A` not run with nobody to confirm, exit 3; `rm '*.log'` refused as destructive, the files
untouched; valid `--json` on the `system1` path with the directory in `output`; the second teach
joining `git log --oneline -n {count}` with every API key unset; `-n 7`. From the directory above the
repository, in no project, with `--project=acc-check`: check 1 gave `ls -ltr` at 0.99 after the note;
check 7's teach added its phrasing to the global `git log --oneline -n {count}` for that project,
which already saw it, so no lesson was written; and check 8, in the space form, gave `-n 7`. Checks 9
to 12 were not run: nothing here touches the terminal, auto mode's diagnosis or the demo, which ignores
the flag. Check 13 was not run: the TypeScript version is not available.

## Consequences

- A project can be seen and changed from any directory and any host, with no checkout of it: `jli
  skills --project=shop-api`, or an agent teaching into a project from CI.
- `jli skills --project` lists a project's own skills alone, inside the project or from anywhere.
- Without `--project` nothing changed: the project is the `.jlirc`'s, through the same function.
- **Known gap:** the space form takes the next word as the id. `jli --project deploy the app` scopes
  into a project called `deploy` and routes "the app"; `jli --project skills` opens the session, or
  prints the help with no terminal, in a project called `skills`. The note on stderr is the only guard,
  and only until that project has something. `--project=<id>`, or `--project` last, avoids it.
- **Known gap:** a mistyped id that names another real project (`shop` for `shop-api`) gets no note.
- **Known gap:** `jli skills --project=<id>` names the global skills the project has lessons or a
  scenario about, but does not show them; the union a project routes against is listed only inside it.
- **Known gap:** the projects the note and the listing name are those with skills of their own; a
  project with only lessons or requests is not named.
- **Known gap:** an id is not a path, so `jli project --project=<id>` cannot say where the project's
  checkouts are.
- **Known gap:** an older jli does not know the flag. It reads `--project=x` as words of the request:
  `jli skills --project=x` is routed as a request, and `jli teach --project=x "run the tests" -- …`
  teaches the global skills the phrase "--project=x run the tests", as an older jli does with
  `--user-id` ([ADR 15](0015-user-scope.md)).
