# 19. A flag is never a slot value, a literal skill is made general in place, and a label taken by the same command is joined

- Status: accepted; superseded in part by [ADR 30](0030-taught-as-given.md), under which part 1
  holds for a program as for a flag, and `flags_written_in` became `values_written_in`
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 6](0006-learning-modes.md), on step 1 of learning a phrase and on when the
  translator's `existingLabel` is believed; [ADR 7](0007-slot-templates.md), on matching a command
  back to a template; [ADR 13](0013-learning-fixes.md), on a taught command that a literal skill runs;
  [ADR 15](0015-user-scope.md), on what a writer does when its label is taken

## Context

Three defects, issues #17, #16 and #19, each found while building the work of
[ADR 13](0013-learning-fixes.md) and confirmed in the code on `dev` at `ad24a7c`:

1. **A flag was a slot value when a command was matched to a template.** `match_template` let each
   slot take any word. `jli teach "go back" -- cd -` joined `cd {directory}` with `{directory} = -`.
   [ADR 7](0007-slot-templates.md) says a flag is never a slot, and the translator's check enforced
   it; this path did not. The same match is how the router recognises a proposal a known skill
   already runs (`match_proposal`), how `jli fix` reads values back out of a command, and how
   `jli generalize` checks a proposal.
2. **A literal skill absorbed a template.** `learn_phrase` linked a taught command to the first skill
   whose template produced it, before any model. A literal skill (`git log --oneline -n 3`, stored in
   manual mode, from an agent's literal proposal, or with the translator out of reach) produces one
   command, so teaching that command in semi mode never reached the translator: the phrase joined the
   literal skill, the general form `git log --oneline -n {count}` was never written, and every later
   count ran `-n 3`. The same happened when both rows existed and the literal one was listed first
   (it was used more), and when the translator's `existingLabel` named the literal skill. ADR 13 fixed
   the router side (`match_proposal` refuses a literal standing in for a template) and left this side.
3. **Two writers teaching one new command at once got two skills.** [ADR 15](0015-user-scope.md)
   made the label claim safe: a writer whose label was taken appends `-x` and never overwrites. When
   both were teaching the same command the result was `du-sh` and `du-sh-x`, running one command and
   splitting its phrasings. ADR 15 recorded it as a known gap.

## Decision

### 1. A value that begins with `-` is not a match

`match_template` matches as before, then refuses the match if any slot's value, with its quotes
removed, begins with `-` (`slots::is_flag`, which the translator's check and `generalize` now use too).

| Template | Command | Before | Now |
| --- | --- | --- | --- |
| `cd {directory}` | `cd -` | `{directory} = -` | no match: `cd -` is a skill of its own |
| `ls {directory}` | `ls -la` | `{directory} = -la` | no match |
| `ls {directory}` | `ls '-la'` | `{directory} = -la` | no match: quoting a flag does not make it a value |
| `head -n {count} {file}` | `head -n -5 notes.txt` | `{count} = -5` | no match |
| `git commit -m {message}` | `git commit -m 'fix -x handling'` | matched | matched: the value begins with `f` |

What follows, by caller:

- `learn_phrase`: `cd -` taught beside `cd {directory}` makes its own skill. The translator's
  `existingLabel` is not believed when a value it gives is a flag either, so a translator that calls
  `cd -` the known `cd {directory}` with `-` does not bring the match back by another road.
- `match_proposal`, in the router and in `jli run` ([ADR 18](0018-agent-contract.md)): a proposal with
  a flag where a known skill has a slot is another command.
- The learned phrase and `jli fix` (ADR 13): a value that begins with `-` is not read back out of the
  command, so it is not hidden. The router and `jli fix` work the phrase out through the same function,
  so they still arrive at the same text.
- A correction (`is_another_command`): `cd -` typed after a route to `cd {directory}` is another
  command, so the request becomes a counter-example of the skill that was turned down.
- `generalize`: a proposal that slots a flag is still refused as `it made a flag a slot`, now worked
  out from the match itself.
- The agent's proposal in the router, added after this record was first written: the agent can give a
  flag as a value without any match, by naming a known skill as kind `existing` with `-` for
  `{directory}`, or by proposing that skill's template under another slot name (`cd {dir}`, `dir = -`),
  which `match_proposal` maps onto `cd {directory}`. Before the proposal is matched or checked against
  the rejected skills (`flags_written_in`), every value that is a flag is written into the steps it
  fills, and the slot is dropped: `cd {directory}` with `-` becomes the new proposal `cd -`, and
  `ls {options} {directory}` with `-la` becomes `ls -la {directory}`, keeping its other slot. The trace
  says `- is a flag, not a value for {directory} of go-to-directory, so cd - is a command of its own.`
  From there the proposal goes the way of any new one: if a known skill runs `cd -` it is that skill,
  if a rejected skill runs it, it is refused ([ADR 20](0020-write-back.md)), and otherwise it is learned
  as a new skill after a yes. A proposal named as a rejected template with a flag for its value is
  therefore not refused, since the command it renders is not the rejected one. The kind `existing`
  carries no scenario of its own, so an empty one becomes `the user wants: <request>` before System 1
  checks it, as `learn_phrase` falls back. Tested in
  `an_agent_proposal_that_gives_a_known_template_a_flag_is_a_command_of_its_own`, which fails without
  it: the route went to `list-directory`, ran `ls -la` and linked the phrasing there.

  This is the translator's rule applied to the agent. The other way to obey it, not believing the
  proposal and running nothing, was weighed and not taken: the agent has said which command fits the
  request, and only the reading of it as a known skill with a value is wrong. Teaching `cd -` beside
  `cd {directory}` makes `cd -` a skill of its own, so an approved route to it does the same.

**Where a flag can still reach a slot.** The rule holds wherever a command is read back into a
template: a taught command, a translator's draft, an agent's proposal, `jli fix` and `generalize`.
Once a skill is chosen, its slots are filled by `fill_slots` ([ADR 7](0007-slot-templates.md)), and
there a value is not checked for a flag:

- a value the person states (`directory=-`, a quoted `'-v'`), which [ADR 11](0011-stated-arguments.md)
  passes as written, by design, as are the values a caller gives `jli run`;
- a value the agent works out (`extract`) for a slot of a skill System 1 chose: `go back` routed by
  System 1 to `cd {directory}` with the agent giving `-` runs `cd -`, and a yes below 0.95 links the
  phrasing to `cd {directory}`;
- a System 1 pick from the directory's entries, when an entry's name begins with `-`;
- a slot's default, if a proposal or the translator wrote a flag there.

A number found in the request never begins with `-` (`numbers_in` reads digits and number words).

### 2. A literal skill never absorbs a template

A literal skill is one whose steps hold no placeholder. Step 1 of learning a phrase
([ADR 6](0006-learning-modes.md)) now tells the two apart:

| Skills that produce the taught command | Mode | What happens |
| --- | --- | --- |
| a template, with or without a literal skill beside it | any | the phrase joins the template, with no model; a template wins over a literal skill listed before it |
| only a literal skill | manual | the phrase joins it, with no model, as before |
| only a seed | any | the phrase joins it, with no model: a seed is a fixed command written by hand |
| only another literal skill | semi, auto | the translator is asked first |

Then, by the translator's draft:

- the draft is the command with no slots, or its slots do not reproduce the command: the phrase
  joins the literal skill;
- the draft is a template that reproduces the command: the literal skill is made general in place;
- the translator cannot be reached: the phrase joins the literal skill.

The translator's `existingLabel` is no longer believed when it names a literal skill and the draft is a
template, or when a value it gives is a flag (part 1). It is believed as before when the skill it names
is a template, or the draft is not.

**Made general in place.** The literal row keeps its label, examples, counter-examples, uses, origin
and `created_at`. Its steps, slots, scenario and risk become the template's: the translator's
scenario, and the more severe of the two risks unless `--risk` is given. Before anything is written,
the row gets the check every translated row gets (`verify_row`, [ADR 16](0016-measuring-the-matrix.md)),
in its own place in the matrix: the phrase must route to it, and the closest skills' phrasings must
stay where they were. If the check fails twice, or System 1 cannot be reached, the steps and slots are
still made general and the row keeps its own scenario, which already held for its phrasings.

Where the translator gave a slot no default, **the value the literal skill ran becomes the default**,
so a phrasing that named no value still runs what it ran:

| Request, after `git log --oneline -n 3` was made general | Before | Now |
| --- | --- | --- |
| `show the last 7 commits` | `git log --oneline -n 3` | `git log --oneline -n 7` |
| `show me the recent commits` | `git log --oneline -n 3` | `git log --oneline -n 3`, `{count} = 3 (from default)` |

The row is written with one conditional statement, as a list is ([ADR 15](0015-user-scope.md)):
`UPDATE … SET steps, slots, scenario, risk … WHERE label = ? AND steps IS ? AND slots IS ?`, the old
values being exactly what was read (`Brain::upgrade_skill`). A slot that keeps its name keeps its
aliases, as in an upsert. If another writer changed or forgot the row first, nothing is written to
it, and the template is stored as a new skill, which joins the row if it now runs the template (part 3).

`jli teach` says what happened:

```text
  git-log-oneline-n-3 ran only git log --oneline -n 3. It now runs git log --oneline -n {count}, and keeps its phrasings.
✚ learned "list the three newest commits" means git-log-oneline-n-3 (git log --oneline -n {count})
  added to an existing skill, which now takes {count} and has 2 phrasings
```

**Why in place, and not a second row.** The issue allowed either. The two were weighed:

| | The template as its own row, beside the literal skill | **The literal skill made general in place** |
| --- | --- | --- |
| rows | two, both able to run `-n 3` | one |
| routing | the literal skill keeps phrasings that name its value and a scenario that fits the whole family, so System 1 can still choose it for `show the last 5 commits` and run `-n 3` | every count reaches the one row; a phrasing that named the old value still runs it |
| the check before writing | the template is checked against a matrix that still holds the literal skill, which runs the very command being taught: the phrase and the literal skill's phrasings may go to either, so the check fails for a reason that is no fault of the row, and the row is stored with the phrase as its scenario | the row is checked in its own place, against the other skills |

[ADR 2](0002-brain-in-libsql.md) says a skill row is never replaced, and what that means: an upsert
updates steps, scenario, slots and risk, and keeps examples, counter-examples, uses, origin and
`created_at`. Making a literal skill general is that upsert, written as a compare-and-swap. Nothing
learned is lost, no column is added, and a share or a project's lesson, which names the row by its
`created_at` (ADR 15, [ADR 17](0017-project-scope.md)), still applies to it. ADR 13's decision is one
row per command; a second row would have carried its known gap, a proposal more general than a
literal skill becoming a row beside it, from the router to teaching as well.

**Inside a project** only a row the brain owns is made general. A global literal skill seen inside a
project belongs to every project, and ADR 17 keeps what is learned in one project from changing it, so
there the template is stored as the project's own skill. `jli teach --global` makes the global row
general.

### 3. A label taken by the same command is joined

The label claim (`Brain::add_new_skill`, ADR 15) is where every new skill is stored: `jli teach` in
every mode, a translated row and its fallback, a skill the agent proposed, and a project skill
`generalize` makes global. When the insert finds its label taken, the claim now reads the row that
holds it (inside a project, the global row that blocks the label). If that row's template produces
the new skill's (`match_proposal`, ADR 13: the same template under other slot names, or a command it
produces, never a literal standing in for a template), the new skill's phrasings are linked to that
row, each also lifted from its counter-examples, and nothing new is written. Only a different command
gets `-x`. Inside a project a global row joined this way gets the phrasing as the project's lesson,
not on the row (ADR 17).

`add_new_skill` now returns whether it wrote a row. When it did not, the router reports
`"how many lines are in …" means count-lines`, not `new skill count-lines-x`, and `jli teach` says
`added to an existing skill`.

**The label alone is not enough.** Two writers in manual mode slug one command to one label, which is
the case ADR 15 described. In semi mode, the default, the translator names the command, and live it
named one command differently for each writer in 3 rounds of 3 (`directory-size` and `disk-usage`,
both `du -sh {target}`): the labels never met, and two rows remained. So `learn_phrase` also reads the
matrix once more just before it writes, and a row that now runs the template is the skill, as step 1
would have found it had it been there. That narrows the window from the translator and the check, a
second or two, to the time between that read and the insert.

The claim does not compare every row, only the one under its label. After `jli fix` or a "no", the
router stores the agent's proposal as a new row on purpose when the skill just turned down runs it
(ADR 13), and joining that skill would undo the correction.

**The `-x` loops in the callers stay.** `learn_phrase`, the router and `generalize` pick a label that
is free in the matrix they read before the row is checked with System 1. The check puts the row into
that matrix by label (`with_row`), so a row that shared a skill's label would take that skill out of
the matrix it is checked against; and the router shows the label before "Run it?". Those loops choose
what is checked and shown. What is stored, and under which label, is decided by the claim.

### Measured on 2026-10-03

Live, with this branch's release binary, on scratch brains in scratch directories, Mercury 2.5 as the
translator and the agent. The directories were not git repositories: this session could not run
`git init` outside its own worktree.

| Step | What happened |
| --- | --- |
| `teach "go to the src folder" -- cd src`, then `teach "go back" -- cd -` | `change-directory` (`cd {directory}`, verified at 0.98), then a new skill `go-back-directory` (`cd -`, verified at 1.00); `change-directory` kept its one phrasing |
| `teach --mode=manual "show the last 3 commits" -- git log --oneline -n 3`, then `teach "list the three newest commits" -- git log --oneline -n 3` | the literal `git-log-oneline-n-3`; then the translator wrote `-n {count}`, the row was verified in its own place at 0.81, and `git-log-oneline-n-3` ran `git log --oneline -n {count}` with both phrasings |
| `-n show the last 7 commits` | `git-log-oneline-n-3` at 0.72: `git log --oneline -n 7` |
| `-n show me the recent commits` | at 0.60: `git log --oneline -n 3`, `{count} = 3 (from default)` |
| two `teach --mode=manual "disk use …" -- du -sh .` started at once, 5 rounds | one `du-sh` with both phrasings in every round |
| two `teach … -- du -sh .` in semi mode started at once, 3 rounds, with the claim alone | two rows in every round, under the translator's two labels |
| the same, 6 rounds, with the matrix read again before writing | one row with both phrasings in every round; the later writer said `disk-usage was taught this command while it was being checked, so the phrase joins it.` |

The manual rounds do not show whether the later writer joined through its first read or through the
claim. The offline test does: four threads on one brain behind a barrier, one row written, four
phrasings on it.

The acceptance checks of [ADR 9](0009-port-to-rust.md) were re-run on a fresh brain. Checks 1 to 8
held as recorded. Under `expect`: check 9 gave an `environment` diagnosis and no counter-example;
check 10 offered two numbered options this time, and picking one learned it and ran it; check 11 took
two requests with confirmations and left on ctrl-d. Check 12, `jli demo`, exited 0, answered
instantly 2 of 5 then 5 of 5, and spent 5.0 s deciding, then 1.3 s; a second run spent 65.0 s, then
1.4 s, on two agent calls of 31 s each. Check 13 needs the TypeScript version, which is not here.

### Measured on the merged build, 2026-10-03

ADRs 19 to 23 were built in parallel and first ran together on `dev` at `04da992`. With the release
binary of the integration branch (the router path above added), on fresh scratch brains in scratch
directories that were not git repositories:

| Step | What happened |
| --- | --- |
| `teach "go to the src folder" -- cd src`, then `teach "go back" -- cd -` | `change-directory` (`cd {directory}`, verified at 0.97), then a new skill `return-to-previous-dir` (`cd -`, verified at 1.00); `change-directory` kept its one phrasing |
| on that brain, `--agent --json -n return to the previous directory` | the agent named `return-to-previous-dir`: `cd -` |
| on a brain with only `change-directory`, `--agent -n` for "go back to the previous directory", "cd back to where I was" and "take me to the last directory I was in" | each time the agent proposed `cd -` as a new literal skill, not `cd {directory}` with `-`, so the router path above was not reached live. For "go back" it proposed `cd ..`, which `change-directory` runs with `{directory} = ..` |

The path in which the agent gives a flag as a known skill's value was not reproduced live; it is
covered by the offline test only.

The acceptance checks of ADR 9, on the same build: check 1 printed `ls -ltr`; check 2 `git commit -m
'first draft'`; check 3 was not a command, exit 0; check 4 gave `git add -A`, not run, exit 3; in check
5 the agent proposed `rm '*.log'`, destructive, refused with exit 3, and the three `.log` files were
untouched; check 6 printed valid `--json`, `path` `system1`, with the directory in `output`. Check 7:
`teach "show the last 3 commits" -- git log --oneline -n 3` made `git-log-latest`
(`git log --oneline -n {count}`, verified at 0.82), and `teach "last 10 commits please" -- git log
--oneline -n 10` with every API key unset joined it. Check 8 rendered `git log --oneline -n 7`, `{count}
= 7 (from pattern)`. Under `expect`: check 9 routed to `git-status` at 0.90, the command exited 128,
the diagnosis put the problem on where it ran, and there was no counter-example; check 10 offered three
numbered options after a no, and picking `ls -lt` learned it as a new skill and ran it; check 11 took
two requests with confirmations (`pwd`, then a new `tail -n 3 notes.txt`) and left on ctrl-d, exit 0.
Check 12, `jli demo`, exited 0, answered instantly 2 of 5 then 5 of 5, and spent 5.0 s deciding, then
1.4 s. Check 13 was not run: it needs the TypeScript version, which is not here.

## Consequences

- A command keeps one row in more of the cases that used to split it: a literal skill and the template
  that generalises it, and two writers teaching it at once.
- `cd -`, `ls -la` and other commands that differ from a known template by a flag are skills of their own.
- Teaching a command that a template or a seed runs still costs nothing.
- **Known gap:** teaching a command that a literal skill other than a seed runs now costs a translator
  call, about a second, even when the translator finds nothing to generalise. It cost nothing before.
- **Known gap:** a literal skill made general keeps its label, a slug of the old command:
  `git-log-oneline-n-3` runs `-n {count}`. A label cannot change without a new row.
- **Known gap:** with the old value as the default, a request that names no value runs that value
  without asking the agent, for a number, file or branch slot. For `rm build.log` made general as
  `rm {file}`, that is the file the literal skill removed.
- **Known gap:** only the literal skill's own command makes it general. A template taught with another
  value, `teach "the last 5 commits" -- git log --oneline -n 5` beside the literal
  `git log --oneline -n 3`, is still stored as a row beside it, and the two compete for the same
  phrasings, as ADR 13's gap says of the router.
- **Known gap:** inside a project a global literal skill is not made general. The template becomes the
  project's own skill beside it, and the two compete for the same phrasings there until
  `teach --global`.
- **Known gap:** the router does not make a literal skill general. An agent's proposal more general than
  a literal skill still becomes a row beside it (ADR 13).
- **Known gap:** a flag still reaches a slot through `fill_slots` once a skill is chosen: a value the
  agent works out for a skill System 1 picked, a System 1 pick of an entry whose name begins with `-`,
  and a default that is a flag. Only the first is likely, and it links the phrasing to the template as
  #17 did. A stated value is passed as written on purpose (ADR 11).
- **Known gap:** the router writing a flag the agent gives into its command was verified offline only.
  Live, the agent proposed `cd -` as a literal command each time, and the path was not reached.
- **Known gap:** the claim compares only the row under its label, and only `learn_phrase` reads the
  matrix again. Two agents' proposals for one command, named differently and stored at once, still make
  two rows, and so can two teaches whose last read and insert interleave within milliseconds.
- **Known gap:** a writer that joins a row drops its own scenario, risk and slot names with their
  aliases: the row stays as the first writer wrote it.
- **Known gap:** a value that begins with `-` is never read back out of a command. A negative number
  the agent gave (`head -n -5`) is not hidden in the learned phrase, and a correction to the same skill
  with another such value counts as another command. In the router it is written into the command:
  the agent's `head -n {count}` with `-5` becomes a literal `head -n -5`, a skill of its own.
- **Known gap:** a seed that names a value, `git push -u origin main`, is never made general by teaching.
- **Known gap:** the compare-and-swap that makes a literal skill general, like the rest of the store,
  has run only against local files.
