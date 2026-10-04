# 30. What `jli teach` is told is kept as given, with the person's name and description, and a program is never a slot value

- Status: accepted
- Date: 2026-10-04
- Origin: the Rust version; a report from the owner
- Supersedes in part: [ADR 6](0006-learning-modes.md), on who writes the row `jli teach` stores and
  which mode reaches it; [ADR 9](0009-port-to-rust.md), on acceptance check 7, which now teaches with
  `--mode=semi`; [ADR 19](0019-one-row-per-command.md), whose part 1 now holds for a program as it
  holds for a flag

## Context

On 2026-10-04 the owner set out to teach a skill called `pull`, to run `git pull` from origin. A few
times, they reported, the agent found no skill that matched, and yet the command was put with the
skill that opens a file in an editor, which has nothing to do with it. They asked three things:
whether the agent meant that, or creating skills is broken; for a way to name a skill; and that what
they teach be true by default: the command kept as taught, a model that generalises the description
without misreading the skill's purpose, and a description of their own to help it.

**What happened.** The owner's brain, read from a copy, holds the requests in the default user's
global event log:

| Event | Time | Request | Path | Skill | Command | Learned |
| --- | --- | --- | --- | --- | --- | --- |
| 37 | 13:52:11 | `pull` | system2 | `open-file-with-editor` | `git pull` | `"pull" means open-file-with-editor` |
| 38 | 13:52:26 | `pull` | system2 | `open-file-with-editor` | `git pull` | |
| 39 | 13:53:15 | `pull` | system2 | `open-file-with-specific-editor` | `git pull` | `"pull" means open-file-with-specific-editor` |

Both skills were written by the agent the day before, for `open file=Cargo.toml with editor=nvim`
and `… with editor=vim`, and both had been forgotten by the time of the copy. The commands they ran
for those requests, `nvim Cargo.toml` and `vim Cargo.toml`, make their template `{editor} {file}`: a
skill whose program is a slot.

Reproduced on a scratch brain holding that skill, the agent on `gpt-6-luna`:

```text
⚡ system 1 · jev-1.13.0 · 277 ms · 10 skills in the matrix
   none-of-these  ████████████████████  1.00
◆ system 2 · gpt-6-luna · using ChatGPT plan · no known skill fits
   proposes new skill git-pull · 3.7 s
   Pull updates the local branch and working tree from its configured upstream.
  open-file-with-editor already runs this command, so that skill is used.
→ git pull  risk: safe
  {editor} = git (from agent)
  {file} = pull (from agent)
```

The agent read the request right, and jli took it away. A new proposal that a known skill already
runs is that skill ([ADR 13](0013-learning-fixes.md), `match_proposal`), and the match lets each
slot take any word, so a template with no fixed word in it runs every command of two words.
Teaching had the same hole, with no model asked at all: `jli teach pull -- git pull` printed
`✚ learned "pull" means open-file-with-editor ({editor} {file})`. `git pull origin`, three words, did
not fit it and became a skill of its own.

**What a teach could change.** In semi mode, the default, the translator could turn a value of the
taught command into a slot, name a known skill as the same command (`existingLabel`), and choose the
label. Each is a place where a model makes the stored row differ from what was taught. A template it
writes from nothing has no default for its slot (only a literal skill made general keeps its value as
the default, ADR 19), so `git pull origin` written as `git pull {remote}` would ask the agent for a
remote on every later "pull". The person had no way to name the skill, and no way to say what it was
for beyond the phrase.

## Decision

### 1. A program is never a slot value

A slot in the place of a command's program, at the start of a step or right after `|`, `;`, `&`,
`(` or a backtick (`slots::program_slots`), takes no word of a command. `match_skill` refuses a match
that reads one into it, as it refuses a flag (ADR 19). Only another template's lone placeholder fits
there, so the same template under other names is still the same skill.

| Template | Command | Before | Now |
| --- | --- | --- | --- |
| `{editor} {file}` | `git pull` | `{editor} = git`, `{file} = pull` | no match |
| `{editor} {file}` | `nvim Cargo.toml` | matched | no match |
| `{editor} {file}` | the proposal `{program} {path}` | matched | matched, the slots renamed |
| `cat {file} \| {filter}` | `cat notes.txt \| sort` | matched | no match |
| `sudo {command}` | `sudo ls` | matched | matched: `ls` is an argument of `sudo` |

What follows, by caller, is what ADR 19 lists for a flag: a taught command (step 1, and the matrix
read again before writing), the label claim, a proposal that a known skill runs, in the router and in
`jli run`, refusing a rejected command again, a correction (`is_another_command`), and `generalize`.
The translator's `existingLabel` is not believed for a skill whose program is a slot either.

The agent can still give a program as a value without any match, by naming the skill as kind
`existing` with `git` for `{editor}`. Before the proposal is matched (`values_written_in`, which was
`flags_written_in`), a value for a program slot that the request does not have as a word of its own
is written into the command, and so is every other value, since what is left would be no template of
that command. The proposal becomes the new skill `git pull`, and the trace says
`git is a program the request does not name, not a value for {editor} of open-file-with-editor, so git pull is a command of its own.`
A program the request names (`editor=nvim`, "open it in nvim") stays a value. Live, the agent took
the first road each time; the second is covered by an offline test.

A skill whose program is a slot is still written, by the agent or by hand, and System 1 still routes
to it: the person who asks `open file=Cargo.toml with editor=nvim` names the program every time. It
only never takes a command it was not routed to.

### 2. `jli teach` keeps the command as given

`learn_phrase` now takes one of three ways of teaching (`learn::Teaching`):

| Way | Chosen by | The command | The scenario | The label |
| --- | --- | --- | --- | --- |
| as given | `jli teach --mode=manual`; a correction in manual mode | as written | `the user wants: <phrase>`, then the description | `--name`, or a slug of the command |
| **described** | `jli teach` with no `--mode` | as written | written by the translator, checked by System 1 | `--name`, or the translator's |
| generalised | `jli teach --mode=semi` or `--mode=auto`; a correction in semi or auto mode | the translator may make values slots | written by the translator, checked by System 1 | `--name`, or the translator's |

Only `--mode=` on the teach command line picks. `jli config mode` and `JLI_MODE` keep deciding
everything else they decided: what a "no", a failed command and `jli fix` teach.

A described teach, step by step:

1. A skill that runs the command, a template with some values or a literal skill, takes the phrase,
   with no model, as before.
2. Otherwise the command is stored as typed, with no slots but the placeholders the person wrote.
3. The translator writes the label, the scenario and the risk, from a new prompt and schema,
   `skill_scenario` (`translator::describe`). It is told that the command runs as written every
   time, so its values are fixed and the scenario names those that set it apart; that the
   description is the person's account of when and why to run it, to keep and never contradict; and
   that the name is a hint to what they mean. It has no field for steps or slots.
4. Every value the command runs with must be named in the scenario: each argument after the program
   with a digit, a dot, a slash, a colon or a `~` in it (`Cargo.toml`, `3`, `:3000`), as written or
   part by part, and a number also as its word (`3` is named by "the three most recent"). A scenario
   that leaves one out is sent back before System 1 is asked, with
   `the scenario does not name Cargo.toml, which the command runs with every time, so a request for another value would fit it`.
5. System 1 checks the row as it checks every translated row (`verify_row`, [ADR 16](0016-measuring-the-matrix.md)).
6. A failure gets one retry, with the problem as feedback. After a second, the scenario is the
   person's own words: `the user wants: <phrase>. <description>`.

Step 4 was added after the first runs of the benchmark below. Without it the default stole: the
translator wrote `count-lines-in-file` for `wc -l Cargo.toml` and `list-processes-on-port-3000` for
`lsof -i :3000`, with scenarios as broad as a template's, and on Mercury 6 of 9 requests for another
value reached those skills and would have run the taught value, `wc -l Cargo.toml` for the lines of
`README.md`. On `gpt-6-luna` it was 1 of 9 in one run and 3 of 9 in another. With it, none in either.

**Why not semi by default.** The owner asked for the command to be kept, and semi has the three
places, listed above, where a model changes what was taught. The cost is that a command with a value
is not made general, which is the gain semi mode was built for ([ADR 6](0006-learning-modes.md)).
The benchmark below measures it, and `--mode=semi` keeps it one flag away.

### 3. `--name`

`jli teach --name=<name>` (or `--name <name>`) gives the new skill its label. A name is made a label
as any is (`Pull Origin` becomes `pull-origin`), and one with no letter or digit is a usage error.
It is checked before any model is asked (`check_name`), and refused with exit 1:

- when a skill of that name runs another command:
  `pull is already a skill, which runs git pull --rebase. Choose another name, or forget that skill first: jli forget pull`
- when another skill already runs this command:
  `pull already runs git pull origin, so the phrase would join it, not a skill called sync. To add the phrase to pull, teach it without --name. To call the skill sync, forget pull first: jli forget pull`

Inside a project a global skill's name is refused too, and the way out named is `jli forget --global`.
A name that a skill running this command already holds is that skill, and the phrase joins it.

Renaming the other skill, and a second row beside it, were weighed and not taken: a label is what
events, plans, a project's lessons and `jli eval undo` name a skill by, and a command has one row
(ADR 19). A project skill may shadow a global one of the same label ([ADR 17](0017-project-scope.md)),
but an explicit name inside a project does not, to keep one rule.

### 4. `--description`

`jli teach --description=<words>` (or `--description "<words>"`) is the person's account of when and
why to run the command. It goes to the translator as `description`, in both prompts, under one rule
in each: keep its meaning and never contradict it, and where it is broader than the command,
describe what the command does. The fallback scenario quotes it after the phrase. It is not stored
apart from the scenario. When the phrase joins a skill that already runs the command, it is not used,
and the trace says so.

`--name` and `--description` are read only inside `jli teach`, from the words before `--`, so a
request that contains them is routed as it was.

### 5. The prompts

`matrix_row` gains the inputs `name` and `description`, null when not given, and the two bullets
above. `skill_scenario` is new. Both run on the translator's model with reasoning off, an 8 second
timeout and two attempts ([ADR 4](0004-language-models.md)).

### Measured on 2026-10-04

Live, with this branch's release binary, on scratch brains in scratch directories, with `gpt-6-luna`
on a ChatGPT plan through Codex's sign-in, copied to a scratch `JLI_CHATGPT_DIR` by
`jli login --codex`, unless said otherwise.

**The report.** On a brain holding `{editor} {file}`: `jli -n pull` gave System 1 none-of-these at
1.00, the agent proposed `git-pull`, and it stood as the new skill `git pull`, with no line about the
editor skill. `jli teach pull -- git pull` made a new skill `git-pull`, verified at 0.80. Then
`jli -n pull` routed to it at 0.80, and `jli -n open file=Cargo.toml with editor=nvim` still to
`open-file-with-editor` at 1.00, `nvim Cargo.toml`.

**A name and a description.** `jli teach --name=pull --description "bring the current branch up to
date with what origin has" pull -- git pull origin` made `pull`, running `git pull origin`, verified
at 0.58. In dry runs `pull` routed to it at 0.61, `update my branch from origin` at 0.95 and
`get the latest changes from origin` at 0.94, each `git pull origin`; `pull from upstream` reached it
too, at 0.85, and would have run `git pull origin`. The translator called the risk `destructive`.
Then `teach --name=pull sync -- git pull --rebase` and `teach --name=sync sync -- git pull origin`
were refused with the two messages above, exit 1, and `teach --description "before I start work"
"update from origin" -- git pull origin` joined `pull` with the note that the description was not
used. On a fresh brain, the README's example with `--risk=modifies` verified at 0.57, and `jli -n pull`
routed to it at 0.63.

**A value.** `teach "show the last 3 commits" -- git log --oneline -n 3` made `git-log-last-three`,
verified at 0.76, its scenario naming "exactly the three most recent commits". `show the last 3
commits` (0.78) and `the three newest commits` (0.74) ran `-n 3`. `show the last 10 commits` went to
the agent, none-of-these at 0.86, which proposed `git log --oneline -n 10`.

**The description between neighbours.** Two brains, each taught `--name=deploy-preview "deploy
preview" -- mise run deploy-preview` and `--name=deploy-shared-preview "deploy shared preview" --
mise run deploy-preview-shared`, the second brain with the descriptions "my own preview of the
website, which only I look at" and "the preview the whole team reviews before a release":

| Request | Without descriptions | With them |
| --- | --- | --- |
| deploy preview | deploy-preview 0.96 | deploy-preview 0.98 |
| deploy shared preview | deploy-shared-preview 1.00 | deploy-shared-preview 1.00 |
| put up the preview the team reviews | **deploy-preview 0.80** | deploy-shared-preview 0.98 |
| let me check my own preview build | deploy-preview 0.89 | deploy-preview 0.99 |
| publish the site for everyone to review | deploy-shared-preview 0.64 | deploy-shared-preview 0.98 |

**The benchmark.** `examples/bench-translator.rs` now teaches three brains, manual, described and
semi, and counts as "wrong" a request that reached its skill and rendered another command. Ten
commands, thirty requests; routed / right / wrong, in percent:

| Run | Fixed commands (21): manual, described, semi | Commands with a value (9): manual, described, semi | All (30): manual, described, semi |
| --- | --- | --- | --- |
| `gpt-6-luna`, without step 4 | 100/100/0, 100/100/0, 100/100/0 | 0/0/0, 11/0/11, 100/100/0 | 70/70/0, 70/70/0, 100/100/0 |
| `gpt-6-luna`, without step 4, a second run | 100/100/0, 100/100/0, 100/100/0 | 0/0/0, 33/0/33, 100/100/0 | 70/70/0, 80/70/10, 100/100/0 |
| Mercury 2.5 on OpenRouter, without step 4 | 100/100/0, 100/100/0, 100/100/0 | 0/0/0, 67/0/67, 100/100/0 | 70/70/0, 87/70/17, 100/100/0 |
| **`gpt-6-luna`, with step 4** | 100/100/0, 100/100/0, 100/100/0 | 0/0/0, 0/0/0, 100/100/0 | 70/70/0, 70/70/0, 100/100/0 |
| **Mercury 2.5, with step 4** | 100/100/0, 100/100/0, 100/100/0 | 0/0/0, 0/0/0, 100/100/0 | 73/70/3, 70/70/0, 100/100/0 |

The groups are routed separately, so a row's "all" can differ from its two groups by System 1's own
variation. In every run each model kept all ten rows it wrote, for described and semi alike. The
forgetting rate, with step 4: manual 0 of 93, described 2 of 101 on `gpt-6-luna` and 2 of 96 on
Mercury, semi 0 of 135. All four requests the described brains "forgot" asked for another count, so
what stopped was a route to `git log --oneline -n 3` that would have run the wrong count. The two
runs with `--forgetting` made 880 and 858 System 1 requests, about $0.057 and $0.054.

So on this benchmark the default is manual mode with a checked scenario: the same on fixed commands,
and a request for another value goes to the agent instead of running the taught one. The gain is
where two skills are close, as with the deploy previews above, which the benchmark does not have.

**The acceptance checks** of [ADR 9](0009-port-to-rust.md), on fresh brains, checks 1 to 8 and 12 in
a scratch git repository and 9 to 11 under `expect` in a directory outside any: check 1 printed
`ls -ltr`, exit 0; check 2 `git commit -m 'first draft'`; check 3 was not a command (97% sure), exit
0; check 4 gave `git add -A`, not run, exit 3; in check 5 the agent proposed `rm -- *.log`,
destructive, refused with exit 3, and the three `.log` files were untouched; check 6 printed valid
`--json`, `path` `system1`, with the directory in `output`. Check 7 now teaches with `--mode=semi`:
`git-log-oneline`, `git log --oneline -n {count}`, verified at 1.00, and `last 10 commits please`
joined it with every API key unset and an empty `JLI_CHATGPT_DIR`. Without `--mode` the first teach
kept `git log --oneline -n 3`, as `git-log-last-three`, verified at 1.00. Check 8 on the semi brain
rendered `git log --oneline -n 7`, `{count} = 7 (from pattern)`; on the default brain System 1 said
none-of-these at 0.80 and the agent proposed `git log --oneline -n 7` as a new skill. Check 9 routed
to `git-status` at 0.86, the command exited 128, the diagnosis put the problem on where it ran, and
there was no counter-example. Check 10 offered three numbered options after a no, and picking `ls
-lh` learned it and ran it. Check 11 took two requests with confirmations, `pwd` then `ls -ltr`, and
left on ctrl-d, exit 0. Check 12, `jli demo`, exited 0, answered instantly 2 of 5 then 5 of 5, and
spent 14.7 s deciding, then 1.5 s. Check 13 was not run: it needs the TypeScript version.

## Consequences

- "pull" taught or asked for beside `{editor} {file}` is a skill of its own, and the agent's reading
  of a request is no longer replaced by a template that fits any command.
- `jli teach pull -- git pull origin` stores `git pull origin`, under the name given, and every later
  "pull" runs it with no model. What a model writes is the sentence that decides when.
- Teaching a command no skill runs costs a translator call and a System 1 check by default, as in semi
  mode; `--mode=manual` costs nothing.
- **Known gap:** a request for another value still reaches a command kept as taught when that value is
  a plain word: `pull from upstream` reached `pull` at 0.85 and would have run `git pull origin`.
  Step 4 knows only values with a digit, a dot, a slash, a colon or a `~`; a remote, a branch or a
  task name is a word like any other.
- **Known gap:** a command with a value is not made general unless `--mode=semi` is given: on the
  benchmark the default rendered none of the 9 requests for another value, where semi rendered all 9.
  Those requests go to the agent, and after a yes its template is a row beside the literal skill, as
  ADR 19 records for the router.
- **Known gap:** step 4 misses a value spelled otherwise in the scenario (`Cargo.toml` as "the Cargo
  manifest") and a number above ten written as a word, and after two tries the person's own words are
  kept even when they do not name the value either.
- **Known gap:** the description is not kept apart from the scenario, so `jli eval --optimise` can
  rewrite it away, and a description given with a phrase that joins a known skill is dropped.
- **Known gap:** a name is checked before the model and claimed when the row is written. Another jli
  that takes it in between with another command leaves the new row as `<name>-x`.
- **Known gap:** inside a project a global skill's name is refused, never shadowed.
- **Known gap:** a template whose program is a slot recognises no command, not even its own program:
  `nvim notes.txt` taught or proposed beside `{editor} {file}` makes a row of its own.
- **Known gap:** a program counts as named only as a whole word of the request: "open it in neovim"
  does not name `nvim`, so that value is written in and makes a skill of its own.
- **Known gap:** a program's place is the start of a step or right after `|`, `;`, `&`, `(` or a
  backtick. In `xargs {tool}`, `env X=1 {tool}` or `sudo {command}` the slot reads as an argument.
- **Known gap:** the risk is still the translator's reading unless `--risk` is given: `git pull
  origin` came back `destructive` in one run, where `git pull` came back `modifies`.
- **Known gap:** a one-word phrase verifies close to the threshold: `pull` for `git pull origin`
  routed home at 0.57 to 0.63 in four runs.
- **Known gap:** `jli fix -- <command>` and a command typed after a "no" follow the configured mode,
  so in semi mode they can still turn a typed command into a template.
- **Known gap:** an older jli on the same brain still reads a command into a skill whose program is a
  slot, teaches in semi mode by default, and takes `--name` and `--description` as words of the phrase.
