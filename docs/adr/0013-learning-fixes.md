# 13. One row per command, the shape of a request, this host's own fix, and failures that are not the command's fault

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 5](0005-routing-loop.md), on when a new proposal becomes a skill;
  [ADR 8](0008-corrections.md), on which route `jli fix` unlearns; [ADR 11](0011-stated-arguments.md),
  on what is hidden in a learned phrasing

## Context

Four defects in how jli learns, each confirmed in the code and each now covered by an offline test:

1. **A "new" proposal could duplicate a known skill.** `learn_phrase` checks a taught command
   against every skill's template before calling any model ([ADR 6](0006-learning-modes.md)). The
   router did not. When System 1 was unsure and the agent answered kind `new` with
   `git log --oneline -n {n}` while `git log --oneline -n {count}` existed, the router wrote a second
   row, with `-x` added to its label if the names collided. The two rows then split the phrasings for
   one command between them.
2. **Learned phrasings kept their values.** ADR 11 hides values the request states by name or in
   quotes. A value found any other way stayed in the example: `how many lines are in notes.txt` was
   stored as it was typed. A skill keeps 16 examples, oldest dropped first, so a skill used daily
   filled up with the same request about different files.
3. **`jli fix` could unlearn someone else's route.** `Brain::last_executed` took the newest accepted
   event in the whole brain. On a brain shared between hosts ([ADR 2](0002-brain-in-libsql.md)), or
   with two agents working in two directories, bare `jli fix` unlearned whatever ran last anywhere.
4. **A new skill was learned only on exit 0.** `grep -rn TODO src` exits 1 when nothing matches, and
   `diff a b` exits 1 when the files differ. Both are answers, not errors. In auto mode a diagnosis
   of `environment` already counts as "the route is right" for a known skill
   ([ADR 8](0008-corrections.md)), yet a new proposal with that diagnosis was thrown away, so the next
   request paid for the agent again.

## Decision

### 1. A new proposal that a known skill already runs is that skill

Before a new proposal becomes a draft, the router checks it against every skill on offer, without
any model (`match_proposal` in `src/slots.rs`). The proposal matches a skill when the skill's
template produces the proposal's template:

| Known skill | Proposal | Result |
| --- | --- | --- |
| `git log --oneline -n {count}` | `git log --oneline -n {n}`, `n = 10` | the known skill, `{count} = 10` |
| `git log --oneline -n {count}` | `git log --oneline -n 5` | the known skill, `{count} = 5` |
| `ls -ltr` | `ls -ltr` | the known skill |
| `git log --oneline -n 3` | `git log --oneline -n {n}`, `n = 3` | **no match**: a new skill |
| `echo {text}` | `echo {a}-{b}` | no match |

On a match the plan is the known skill, on the System 2 path, with `is_new: false` and the known
skill's risk. Write-back is then what it is for any System 2 route to a known skill: the phrasing is
added as an example and the use is recorded. The trace says `<label> already runs this command, so
that skill is used.`

- **Values move by name.** A value the agent gave under its own slot name, `n = 10`, goes to the slot
  its placeholder lines up with, `{count}`. It is never rendered and read back, so a value with a
  quote in it arrives whole. A literal where the known skill has a slot is that slot's value. Any
  slot still open is filled the usual way ([ADR 11](0011-stated-arguments.md)).
- **A rejected skill is not on offer.** After `jli fix` or a "no", the skill just rejected is left
  out, so a proposal for the same command becomes a new row, as before.
- **A literal skill does not stand in for a template.** The brief for this work said to render the
  proposal with its values and check whether a known skill produces that command. That would also
  match `git log --oneline -n {n}` with `n = 3` against a literal `git log --oneline -n 3`, and then
  the template would be lost: every later request for 5 or 7 commits would route to the literal row
  and run `-n 3`. So the check is template against template, which catches the renamed placeholder
  and the literal proposal, and leaves a proposal more general than the known skill to become a new
  skill. `learn_phrase` still has the broader check, and a taught command that a literal skill runs
  still joins it.

### 2. A learned phrasing hides every value it was run with

After the slots are filled, every value the command runs with is hidden in the phrase as well,
wherever it appears verbatim as a whole word (`args::hide_values`). The phrase is what System 1
learns as an example and what a correction takes as a counter-example.

| Request | Values | Learned as |
| --- | --- | --- |
| `how many lines are in notes.txt` | `notes.txt` (agent) | `how many lines are in …` |
| `show the last 3 commits` | `3` (agent) | `show the last … commits` |
| `show the first three lines of the notes` | `3` (from the number word) | `show the first … lines of the notes` |
| `greet name="Ada L" greeting=hello 3 times` | stated, and `3` | `greet name="…" greeting=… … times` |
| `search every file here for the word FIXME` | `FIXME` (agent), `.` (default) | `search every file here for the word …` |

- **Whole words only.** A value is found where neither side continues the word: letters, digits,
  `_`, `-`, `/` and `.` continue it, except a `.` that ends a sentence. So `src` is not hidden in
  `src/main.rs`, `gitignore` is not hidden in `.gitignore`, `3` is not hidden in `13`, and
  `notes.txt.` at the end of a sentence is.
- **Verbatim and case-sensitive.** `README.md` picked by System 1 is not hidden in `show the
  readme.md`. A number is also hidden where it is written as a word from one to ten (`three`,
  `Three`), since that is how ADR 11 read it.
- **A value the request does not contain hides nothing.** A commit message the agent wrote, or a
  default, is not in the request, so there is nothing to hide.
- **Short values are not hidden.** A value is hidden only if it is a number or has at least two
  characters with a letter or digit among them. A default of `.` and a one-letter value such as `a`
  would otherwise cut words out of ordinary prose. A number of one digit is hidden: a number in the
  request is read into a number slot by the parser before any default is used, so a `1` in the
  phrase that is also the value is that value.
- **The rule does not depend on where a value came from**, which is what lets `jli fix` repeat it.
  `jli fix` has only the event log: the request and the command that ran. It reads the values back
  out of the command with the skill's template (`match_template`), hides them in the request the
  same way (`slots::learned_phrase`), and removes that phrasing, as well as the two forms it removed
  before. The router computes the learned phrase the same way, from the command it rendered, so both
  arrive at the same text. A test routes requests, reads the event back, and checks that the phrase
  worked out from the event is the stored example and that removing it succeeds.
- As with stated values, a request that would be nothing but hidden values is kept as it is.

What System 1 is sent to route a request is unchanged: values stated by name or in quotes are
hidden, nothing else is ([ADR 3](0003-system-1.md), ADR 11). Only what is stored changed, so System 1
now compares an unhidden request (`count the lines of todo.md`) with hidden examples (`how many lines
are in …`). ADR 11 listed this as unmeasured. It was measured, below.

### 3. Bare `jli fix` unlearns this host's last route in this directory

`Brain::last_executed` takes the working directory and returns the newest accepted route logged by
this host (`gethostname`) in that directory. A route run on another host, or in another directory on
this host, is not this one's last, and `jli fix` says there is nothing to fix if this directory has
none (`Nothing to fix: no command has been run in this directory on this host yet.`). The directory
is compared as the router logs it, the path as text. Events already in a brain
carry both fields, so nothing is migrated.

### 4. A new skill is learned when its failure is not the command's fault

A new proposal that ran is stored as a skill when any of these hold:

| Condition | Why it is enough |
| --- | --- |
| it exited 0 | as before |
| auto mode diagnosed the failure as `environment` | the diagnosis says the command fits the request and the problem is where it ran, which is what keeps a known skill's phrasing (ADR 8) |
| it exited 1 with nothing on stderr, and a person answered yes to "Run it?" | `grep` with no match and `diff` with a difference say so with exit 1 and no error message; a person saw the command before it ran |

Exit 1 with nothing on stderr does not count under `--yes` or `--yolo`. Nobody saw the command, and
ADR 5's rule that an unattended route needs a clean exit stays as it is. A destructive command under
`--yes` still asks, so its yes is a person's. Any other exit code, or exit 1 with an error on stderr,
is a failure as before: `cat missing.txt` exits 1 and is not learned.

**What stderr is seen, by mode.** The rule needs evidence that stderr was empty, and an empty string
is not that evidence when stderr was never read.

| How it runs | stderr before | stderr now |
| --- | --- | --- |
| `--json` (output captured) | captured | captured |
| auto mode | streamed and its tail kept | the same |
| manual or semi, a known skill | streamed to the terminal, not read | the same |
| manual or semi, a new proposal | streamed to the terminal, not read | streamed and its tail kept |

So a new proposal's stderr is now watched in every mode: it still reaches the terminal as it is
written, and its last 4000 characters are kept. The router decides from a flag that says stderr was
captured or watched, never from the string alone. If stderr was not seen, the exit-1 rule does not
apply and only exit 0 or an `environment` diagnosis teaches; with the watching above that case does
not arise for a new proposal, but the guard keeps the rule honest if the watching ever changes.

The trace says why a failed run was learned:
`The command did not exit 0, but it exited 1 with nothing on stderr, which is an answer and not an
error, so the proposal is learned.`

**Measured on 2026-10-03**, live, with scratch brains, `origin/dev` at `83db6b6` against this branch:

| Run | `origin/dev` | this branch |
| --- | --- | --- |
| `jli demo` | exit 0; instantly 2 of 5, then 5 of 5; deciding 5.1 s, then 1.4 s | exit 0; instantly 2 of 5, then 5 of 5; deciding 6.5 s, then 1.3 s |
| `bench-translator`, all 30 requests | manual 70% routed, 70% right; semi 100%, 100% | manual 70%, 70%; semi 100%, 100% |
| `bench-translator`, the 9 with a value | manual 0%; semi 100% at 0.99 | manual 0%; semi 100% at 1.00 |
| learn two skills with `-y`, then route the bench's six held-out value requests with `-n` | 6 of 6 routed by System 1 and rendered right, at 0.96 to 1.00 | 6 of 6, at 1.00 |
| what those two skills stored | `show the last 3 commits`, `count lines in Cargo.toml` | `show the last … commits`, `count lines in …` |

Each benchmark run made 194 System 1 requests, about $0.012. **The benchmark does not exercise this
change**: it teaches with `learn_phrase`, which stores the phrase as given, and calls `fill_slots`
only to render. It is reported as a control. `jli demo` does exercise it (its second and third
lessons learn a phrasing with a value), and so does the six-request comparison, which uses the
benchmark's held-out phrasings for `git log` and `wc -l`. Routing did not get worse in either, and
did not get better: both were at the ceiling before. The demo's time spent deciding differs by the
agent's latency, which varies from run to run.

Also run live on the branch, 2026-10-03:

| Request | Result |
| --- | --- |
| `-y count the lines in notes.txt` in a repository, `-y which directory am I in` in another directory, then `jli fix` in the first | fix unlearned `count the lines in notes.txt`, the first directory's route: example `count the lines in …` removed and added as a counter-example |
| `search every file here for the word FIXME, with line numbers`, answered yes in a terminal (`expect`), semi mode | `grep -rn FIXME .` exited 1 with nothing on stderr, and `search-file-for-word` was learned; the next phrasing went to it through System 1 at 1.00 |
| `--mode=auto list every local and remote git branch`, answered yes, outside a repository | `git branch -a` exited 128, the diagnosis was `environment`, and `git-list-all-branches` was learned |
| `--agent` with three requests for commands a taught skill runs (`ls -la`, `git log --oneline -n 3`, `wc -l a.txt`) | the agent picked the known skill twice; the third, `wc -l {file}`, was more general than the literal `wc -l a.txt` and became a new skill. The new check was not triggered live |

The acceptance checks 1 to 9 and 12 of [ADR 9](0009-port-to-rust.md) were re-run and held. Check 9
failed twice first, before slot filling, when the agent returned a reply with a `null` where a string
was required, in a brain that also held a taught skill; on a fresh brain it held.

## Consequences

- A command the brain already knows keeps one row, and the phrasings for it collect in one place.
- One example covers every file, count and branch the request could name, so the 16 examples a
  skill keeps are 16 ways of asking rather than one way asked 16 times.
- `jli fix` unlearns only what ran here. Two hosts or two agents can each fix their own last route.
- `grep`, `diff` and other commands that report "nothing found" or "different" with exit 1 are
  learned on their first approved run, and an auto-mode `environment` failure no longer costs a
  second agent call.
- A new proposal's stderr is a pipe, not the terminal, on its first run. A program that colours its
  errors or draws a progress bar only on a terminal does neither on that run. Its output on stdout is
  untouched, and the second run of the same skill streams as before.
- **Known gap:** the duplicate check was verified offline only. In four live attempts the agent chose
  the known skill itself or proposed something more general.
- **Known gap:** after `jli fix` rejects a skill, the agent can propose the same command again, and
  it becomes a second row (`count-lines-in-file-x`, seen live on 2026-10-03). The check leaves
  rejected skills out on purpose; what should happen instead is not decided.
- **Known gap:** a proposal more general than a known literal skill becomes a new row beside it, and
  the two can compete for the same phrasings.
- **Known gap:** hiding is verbatim and case-sensitive. `readme.md` typed for `README.md` stays.
- **Known gap:** a value that is also an ordinary word in the request is hidden there too: with
  `{branch} = main`, `push main to the main remote` becomes `push … to the … remote`.
- **Known gap:** when a correction teaches a phrase and the chosen command is stored literally, the
  phrase was hidden with the first skill's values, and `jli fix` reads none back from the literal
  command. It can then miss that example.
- **Known gap:** exit 1 with an empty stderr is also how some commands report a real failure without
  saying why. A person said yes to the command, and `jli fix` or `jli forget` removes it.
- **Known gap:** the routing of hidden examples against unhidden requests was measured on the demo
  and on six held-out requests, not on a benchmark built for it.
