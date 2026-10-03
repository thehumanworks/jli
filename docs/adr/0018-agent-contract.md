# 18. A contract for agent callers: pinned plans, run in order, and corrections without a terminal

- Status: accepted; superseded in part by [ADR 20](0020-write-back.md), which adds
  `already_rejected` to the closed set of `blockedCode`; and by [ADR 21](0021-hand-edits.md), which
  adds seven error kinds
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 8](0008-corrections.md), on corrections, which no longer need a terminal;
  [ADR 5](0005-routing-loop.md), on what a dry run writes (a plan row, nothing
  else), on the CLI contract (`jli run`, `jli fix <eventId>`), on the `--json` outcome (fields
  appended) and on what `--json` prints for an error

## Context

jli is meant for agents as much as for people: `--json`, exit codes, no terminal needed. Three things
stood in an agent's way.

1. **It could not run what it reviewed.** `jli --json -n <request>` showed the commands; `jli --json
   -y <request>` then routed the request again from nothing: System 1 again, often the agent again (2
   seconds), and nothing made sure the same command came out. A dry run of a new proposal was paid for
   and thrown away. The owner put it as "akin to a prepared SQL statement", and confirmed: "sequential
   plan ids is fine, no DAG needed."
2. **It got prose where it needed data.** `blocked` was free text (`git-commit does not take msg=`,
   `could not work out a value for {x}`), exit code 3 covered six reasons, an error printed no JSON at
   all (it was printed only after `route()` returned), and `output` was stdout followed by all of
   stderr.
3. **It could not correct a route.** Every correction asks a question at a terminal, and
   `Prompt::confirm` and `ask` return nothing without one. `jli teach` links a phrase to the right
   skill but leaves no counter-example on the wrong one, and the caller never knew the hidden phrase
   that was learned (ADR 13).

## Decision

### 1. A plan is a route worked out once, and run later exactly as it was shown

**When one is made.** A dry run, and a run that stopped before it ran for lack of a yes, a value or an
argument the skill takes, store a plan and return its id as `planId`:

| Stopped at | `blockedCode` | Plan state |
| --- | --- | --- |
| `-n` | `dry_run` | `ready` |
| nobody to confirm | `needs_confirmation` | `ready` |
| destructive, no `--yolo`, nobody to confirm | `destructive` | `ready` |
| a slot with no value | `missing_value` | `incomplete` |
| `name=` that fits no slot | `unknown_argument` | `incomplete` |

A "no" at the prompt is an answer and stores nothing. Not a command stores nothing.

**What it pins**, as JSON in the row's `body`, so that running it needs no thinking: the request and
the phrase that will be learned (values hidden, ADR 13), the routing path and confidence, the skill
(its label, steps, scenario, risk and slots; for a new proposal this is the whole draft, which is not
in the brain), whether it is new, the effective risk the confirmation rule applies to, the rendered
commands, each slot's value with where it came from, the slots still missing, the arguments that fit
no slot, the slot names the request used (kept as aliases when it runs, ADR 12), and the time System 1
and the agent took to work it out. The row holds who made it (user and project, ADR 15), the host,
the absolute working directory, the state, when it was made and when it expires, and a hash of the
commands (FNV-1a, 64 bits; not Rust's `DefaultHasher`, which may change between versions).

**Where.** A new table, `plans`, with an `INTEGER PRIMARY KEY AUTOINCREMENT` id, keyed by `user_id`
and `project_id` as the `scoped_*` tables are. One table holds every user's plans, the default
user's included: an older jli never reads it, so ADR 15's reason for keeping the default user in the
old tables does not apply. It is `CREATE TABLE IF NOT EXISTS`, run on open, like everything since ADR
2; no row of any other table changes. Plan ids are global; another user's id reads as not found.

**A dry run still writes no skill and logs no event.** ADR 5 said a dry run "writes nothing and logs
nothing". It now writes one row, in `plans`. `skills` and `events` are untouched, as a test checks.
The alternative was to leave `-n` write-free and add `jli plan <request>`. It was rejected: an agent
already reviews with `-n --json`, and a separate command would have it route twice to get an id, or
learn a second habit for the same thing. A plan row is scratch state that expires, not learning.

**Time to live: one hour.** Long enough to review a plan and come back to it; short enough that the
directory and the brain it was worked out against are still the ones it runs against. An expired plan
is refused, never deleted: the row stays.

**`jli run <planId> [<planId> …] [name=value …]`** runs the plans in the order given, each exactly as
stored, with **no model call and no routing**, and stops at the first that fails or is refused. Before
each it checks, in this order, refusing with the code given:

| Check | `blockedCode` |
| --- | --- |
| the plan exists and belongs to this user | `plan_not_found` |
| it has not run, and is not running | `plan_already_run` |
| it has not expired | `plan_expired` |
| its commands still hash to the hash stored with it | `plan_changed` |
| its working directory still exists | `directory_missing` |
| every slot has a value, after the ones given | `missing_value` |
| no step must run in the calling shell (ADR 14) | `needs_caller_shell` |
| the confirmation rule | `destructive`, `declined` |
| the plan can still be claimed | `plan_already_run` |

- **The confirmation rule is re-applied.** Running a plan by id approves it, as `--yes` does: a safe
  or modifying plan runs. A destructive plan still needs `--yolo`, or a yes at the terminal when
  there is one. A plan refused this way is not used up and can be run again with `--yolo`.
- **A plan runs once.** It is claimed just before its commands run, with one conditional statement
  (`UPDATE plans SET state = 'running' … WHERE id = ? AND user_id = ? AND project_id = ? AND state = ?
  AND expires_at > ?`): of two callers, only one changes a row. Every check that can refuse comes
  before the claim. After the run its state is `ran`, whatever the exit code, with the event it
  logged.
- **The rest are reported, not run.** After a plan fails (exit code not 0), is refused, or hits an
  error, every later plan gets an outcome with `blockedCode: not_run`, its own `planId`, and, if it
  can be read, its label and commands. Those plans are untouched and can be run later.
- **Values for a missing slot** are given as `name=value` after the ids, and passed as written (ADR
  11). The name may be the slot's or one of its aliases; a number slot reads a number word, as in a
  request. A value fills only a slot that is missing: a value that is not missing in any of the plans
  given stops the whole run before anything runs (`error: usage`), because a pinned value is what was
  reviewed. With several plans, a value goes to each plan that is missing that slot. Its `source` is
  `caller`.
- **An unknown argument does not block the plan for good.** `fill_slots` stops at an argument no slot
  takes, before any model is asked, so such a plan usually has slots missing too, and `missing`
  lists them. Once each has a value it runs; the unknown argument was never in its commands, and the
  outcome lists it in `unknownArguments`.
- **What it learns** is what an approved run teaches, by the same code (`Route::write_back`): a new
  skill stored with the phrase as its first example, a phrasing added, an alias kept, the use recorded,
  the event logged. Running by id counts as `--yes`, so for a route System 1 picked, a phrasing is
  learned only on exit 0, and exit 1 with nothing on stderr does not teach a new proposal (ADR 13)
  unless a person said yes to a destructive one at the terminal. The event is logged with the plan's
  working directory, so a bare `jli fix` there finds it.
- **A command learned since the plan was made is that skill.** Before a new proposal is stored, it is
  checked without a model against every current skill (`match_proposal`, ADR 13). So two plans of
  the same new command, run one after the other, make one skill, not `label` and `label-x`.
- **A new proposal learned by `jli run` is stored unchecked.** ADR 16 checks a proposed row with
  System 1 while its command runs. `jli run` makes no model call, so the row is stored with the
  agent's scenario and `learned` says `… was not checked with System 1`. `jli eval` measures it later
  like any other row.
- **`msSystem1` and `msSystem2` on a plan's run are what working the plan out cost**, copied from the
  plan, not time spent by this run, which spends none. The event logs the same, so `jli stats` counts
  the route's cost once, where it ran.
- **The trace** names the plan, shows the command, and says what was learned; with `--json` it is
  off, as for any request.
- **`-n run` is refused** (`error: usage`, exit 1) before any plan is read. `-n` promises that nothing
  runs, and an agent that habitually adds it to preview must not run commands; the preview of a plan
  is the outcome that made it.

### 2. Every outcome says what happened as data

The `--json` outcome keeps every field, in order, and appends these after ADR 14's `callerShell` and
`handedOff`; each is always present, as `null` or `[]` when it does not apply:

| Field | What it holds |
| --- | --- |
| `eventId` | the event the outcome was logged as; `null` for a dry run, which logs none |
| `planId` | the plan that `jli run` runs, when one was stored |
| `phrase` | what was learned, or would be: the request with its values hidden |
| `values` | each slot's `name`, `value` and `source` (`request`, `pattern`, `system1`, `agent`, `default`, `caller`) |
| `blockedCode` | `blocked` as a code from a closed set, below |
| `missing` | the slots still without a value: `name`, `kind`, `description`, `aliases`, `default` |
| `unknownArguments` | the `name=` arguments no slot takes |
| `accepts` | every slot the chosen skill takes, in the same shape |
| `candidates` | System 1's labels with their probabilities, highest first (`label`, `probability`) |
| `stdout`, `stderr` | the command's two streams apart, when output is captured; `output` is unchanged, both one after the other |
| `diagnosis` | auto mode's `verdict`, `explanation` and `options` (`command`, `description`) after a failure |
| `error`, `errorMessage` | an error's kind and message; with `ran: true`, something failed after the command ran |

`blockedCode` is one of `dry_run`, `not_a_command`, `needs_confirmation`, `destructive`, `declined`,
`unknown_argument`, `missing_value`, `needs_caller_shell`, `plan_not_found`, `plan_already_run`,
`plan_expired`, `plan_changed`, `directory_missing` and `not_run`. `blocked` keeps its text, and the
exit codes of ADR 5 are unchanged: 0 for `dry_run` and `not_a_command`, 3 for the others.

**An error under `--json` is one JSON object** on stdout, `{ "error": <kind>, "errorMessage": … }`,
with exit code 1. The message also goes to stderr as before. The kinds are a closed set too:

| `error` | When |
| --- | --- |
| `usage` | a flag that does not parse |
| `configuration` | a missing API key, a refused key, or a bad `JLI_MODE`, `JLI_THRESHOLD`, `JLI_USER_ID` or stored setting |
| `model` | System 1 or a language model did not answer, or answered with something unreadable |
| `storage` | libSQL failed, or a write kept losing to other writers (ADR 15) |
| `internal` | anything else |
| `nothing_to_fix`, `event_not_found`, `event_not_run` | `jli fix` had nothing to act on |

An error carries its kind from where it is made (`src/system1.rs`, `src/llm.rs`, `src/store.rs`) in a
small error type that displays as its message alone, so the trace and `jli: …` read as they did. A
libSQL error anywhere in the chain is `storage`. `--json` is looked for before anything is parsed, so a
bad flag also answers in JSON.

**A command that ran is never lost to a later failure.** After the command runs, a failure to learn
(the write-back) or to log the event is caught: the outcome still says `ran: true` with the exit code,
and `error` and `errorMessage` say what failed, joined with `; ` if both did. The process exits with the
command's code. A test takes the brain's directory's write permission away from inside the command
and checks that the outcome comes back with `ran: true`, its stdout, and `error: storage`.

**`jli run` prints an array**, one outcome per plan id given, in order, the ones not run included,
even for a single id, so the shape does not depend on how many ids were given. Its exit code is 0
when every plan ran and exited 0; otherwise that of the plan that stopped the run: its own exit code
if it ran, 3 if it was refused, and 1 if it hit an error (including a command that ran and then failed
to be logged: the run stopped there).

### 3. A route can be corrected with no terminal

- **`jli fix <eventId>`** acts on that event of this user's, logged on any host and in any directory:
  another user's id is not found, as with plans. It does what bare `jli fix` does (ADR 13): the
  phrasing is unlearned, kept as a counter-example, the event is marked corrected, and the agent is
  asked again with the skill left out, in the event's directory if it still exists. Bare `jli fix`
  keeps ADR 13's rule: this host's last route in this directory.
- **`jli fix <eventId> -- <command>`**: that was wrong, this is right. It does what answering "no" and
  typing the command at the prompt does (`Route::correct`), through the same functions: a
  counter-example on the wrong skill, unless the command is that skill with other values
  (`match_template`), then the phrase is linked to the skill that runs the command, or a new one,
  through `learn_phrase`. The phrase is the one the route learned, worked out again from the event
  (`learned_phrase`, ADR 13), so a test that runs both paths finds the same examples and
  counter-examples on both skills.
- **It learns and does not run, by default.** A person at the prompt has just typed the command and
  sees it run; an agent correcting an event may be anywhere, later, and running it would be a second,
  unreviewed effect. So it returns a plan of the corrected command (`blockedCode: dry_run`, exit 0),
  which `jli run <planId>` runs with no model call. `--run` runs it at once, on the terms of `jli run`.
  Either way `--json` prints one outcome object, not `jli run`'s array, with what was learned first in
  `learned`. With `-n` nothing is learned and the outcome only shows the command.
- **`jli fix <eventId> --expected "<words>"`** unlearns the event and asks the agent again, with the
  skill left out and the words as the clarification (the path a "?" or an answer takes in auto mode).
  It returns a plan of the agent's answer; `--run` runs it on the terms of `jli run`.
- **`--pick=N` was not built.** A diagnosis's options are in the outcome as complete commands, so
  `jli fix <eventId> -- <option's command>` is the same correction with no state kept between the two
  calls. A pick by number would need the options stored, for no gain.
- **Who corrected is kept.** Every `jli fix` that changes the brain writes a row to a new table,
  `corrections`: the user and project, the event corrected, the host and time, `terminal` (1 when stdin
  was a terminal, 0 for an agent or a script), how (`unlearn`, `command` or `expected`), the command or
  words given, and the event the correction itself logged, if any. A separate table was chosen over a
  marker in `events`: no column can be added there (ADR 15), and `learned = 'corrected'` is matched
  exactly by `jli stats`.
- **No suggestion is paid for that nobody sees.** Without a terminal there is no "no", so the
  suggester's call after a "no" (auto mode) cannot happen. After a failure in auto mode the one call is
  needed for its verdict anyway, and its options used to be thrown away when nobody could answer. They
  are now returned in `diagnosis`.

### 4. What moved where, for the merge

| From | To |
| --- | --- |
| `Route::run`, from `// A human yes vouches …` to the end of the write-back `if`/`else` | `Route::write_back`, unchanged inside but for its opening lines, which unpack a `Ran` (the run's result, the diagnosis, whether a person was asked, whether stderr was seen, and ADR 16's `checked` and `proposed`) |
| `Route::correct`, the `was_wrong` test on `Correction::Steps` | `router::is_another_command` |
| `Route::correct`, `remove_example` and `add_counter_example` and their trace line | `router::unlearn_phrase` |
| `Route::correct`, `learn_phrase` and its trace line | `router::teach_instead` |
| `Route::finish` | the same, now `&mut self`, recording `eventId`, and keeping an outcome that ran when the log fails |
| `RunResult` | gains `captured_stdout` and `captured_stderr`, set only when output is captured |

New: `src/plan.rs` (the plan record, the hash, `jli run`), `router::run_plan` (a stored plan through
the same confirmation, execution and write-back), `Brain::save_plan`, `plan`, `claim_plan`,
`finish_plan`, `event`, `log_correction` and `corrections`.

**Measured on 2026-10-03**, live, with a scratch brain and a throwaway repository, every command with
stdin from `/dev/null`:

| Step | Result |
| --- | --- |
| `--json -n how many words are in notes.txt` | System 1 found nothing, the agent proposed `count-words-in-file`, `wc -w notes.txt` (`msSystem1` 316, `msSystem2` 1226); `blockedCode: dry_run`, `planId: 1`, `phrase: "how many words are in …"`, `values: [file = notes.txt from agent]` |
| `--json run 1` with `OPENROUTER_API_KEY` and `TYPESAFE_API_KEY` unset | exit 0, `ran`, stdout `6 notes.txt`, `learned: new skill count-words-in-file`, `eventId: 1`. With no key any model call fails with `error: configuration`, so the agent was called once in total, for the plan. `history` shows one `◆ system 2` route |
| `--json run 1` again | exit 3, `plan_already_run` |
| `-n show the newest files` (plan 2, System 1, `ls -ltr`) and `-n how many words are in other.txt` (plan 3, System 1 now, the skill just learned), then `run 2 3` with no keys | exit 0, both ran in order |
| plans 5 (`ls -ltr`), 4 (`cat missing.txt`, a new proposal) and 6 (`ls -ltr`), `run 5 4 6` | 5 ran, 4 ran and exited 1 with its stderr in `stderr`, 6 `not_run`; exit 1 |
| `--json -y what files are here` | System 1, `list-files`, `ls`, at 1.00, `eventId: 6` |
| `--json fix 6 -- ls -la` | exit 0; `learned: "what files are here" means list-files-details` (a new skill, written by the translator in semi mode), `planId: 7`, `blockedCode: dry_run`; `corrections` holds `(6, terminal 0, command, ls -la)` |
| `--json -n what files are here` | System 1, `list-files-details`, `ls -la`, at 0.84, `list-files` at 0.14 |
| `--json -n run 7`, then `--json run 7` | `{"error": "usage", …}`, exit 1, nothing run; then `ls -la` ran, path `taught`, exit 0 |
| `JLI_MODE=wild --json where am I`; `--json --mode=wild …`; `--json fix 99999`; `--json where am I` with no System 1 key | `{"error": "configuration", …}`, `usage`, `event_not_found`, `configuration`; exit 1 each |

The acceptance checks 1 to 8 of [ADR 9](0009-port-to-rust.md) were re-run on a fresh scratch brain and
held; each dry run and each run nobody could confirm now ends with `Stored as plan N`.

## Consequences

- An agent can review a route, then run exactly that, paying for System 1 and the agent once. A
  chain of reviewed steps runs in order and stops at the first failure, with an outcome for each.
- An agent can tell every reason nothing ran, and every error, apart without reading prose, and a
  failure after a command ran no longer hides that it ran.
- An agent can say "not that, this" about any of its own routes, and an audit can tell its
  corrections from a person's.
- Every brain opened by this version gains two tables, `plans` and `corrections`. A dry run, which
  wrote nothing, now writes a plan row.
- **Known gap:** a dry run needs a writable brain. On a read-only brain `-n` fails where it used to
  print the command.
- **Known gap:** plan rows are never deleted. An expired plan is refused but kept; there is no
  cleanup, and a brain used heavily by an agent grows by a row per dry run.
- **Known gap:** a plan does not notice that the brain changed after it was made. A skill forgotten or
  re-taught in between still runs the plan's pinned commands, and its phrasing is then added to
  whatever skill now has that label, or to nothing.
- **Known gap:** a proposal learned through `jli run` skips ADR 16's check with System 1, since a plan
  run asks no model. It is marked in `learned`, not in the brain.
- **Known gap:** `jli run` hands no step to a calling shell. A plan with a `cd` in it is refused with
  `needs_caller_shell`, even when `jli run` was started by the shell function.
- **Known gap:** a plan blocked on an unknown argument runs without it once its slots have values. The
  caller sees the argument named in `unknownArguments`, but nothing makes them acknowledge it.
- **Known gap:** with several plans, `name=value` goes to every plan missing that slot; there is no
  way to give two plans two different values for one slot name in one call.
- **Known gap:** `corrections` is not shown anywhere: `jli history` does not mark a correction made
  without a terminal. It is there to be queried.
- **Known gap:** `fix <eventId> --expected` and `fix <eventId>` without `--run` were run offline only;
  `fix <eventId> -- <command>` was run live once, as above.
- **Known gap:** the claim is one conditional `UPDATE`, which holds on a file, a remote and a replica
  alike (ADR 15), but like the rest of the cloud setup it has run only against local files.
