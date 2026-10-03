# 24. A slot can be left out with its flag, a person can give it a default, and choices say how each value is written

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 7](0007-slot-templates.md), on a slot with no value blocking the run, on
  the order in which values are found, on every value being quoted, and on matching a command back to
  a template; [ADR 11](0011-stated-arguments.md), on how quoted strings are handed out and on a stated
  value always being taken; [ADR 18](0018-agent-contract.md), on the closed set of `error` kinds, to
  which three are added, and on what `accepts` and `missing` say about a slot

## Context

Every `{slot}` had to get a value. Teaching `git log --oneline -n 3` made `git log --oneline -n
{count}`, and from then on "show the recent commits" needed a count from somewhere: the request named
none, System 1 does not pick numbers, so the agent wrote one, in 2 to 3 seconds, or the run blocked.
Yet `git log` does the same job without `-n`. A value given with a flag is often not part of the job
at all, and the models that learn commands had no way to say so.

A person could not say what an unsaid value should be either. A default came only from a model's
draft, or from the value a literal skill ran when [ADR 19](0019-one-row-per-command.md) made it
general.

And the words a person uses for a value are not always the text the command needs. To a person,
Cargo's profiles are `release` and `debug`; to `cargo build` they are `--release` and nothing at all.
The only ways to write that were two skills, or a slot that took `--release` as its value, which ADR 19
forbids: a flag is never a slot value.

## Decision

### A slot has three more fields

| Field | Meaning | Example |
| --- | --- | --- |
| `optional` | With no value and no default, the placeholder and its flag are left out of the command instead of blocking the run. | `{count}` of `git log --oneline -n {count}` |
| `flag` | The flag written just before the placeholder that goes with the value. It must sit there at every use. | `-n` in `-n {count}`, `--profile=` in `--profile={profile}` |
| `choices` | The only values the slot takes: each is a word a person says and the text it puts in the command, as the person wrote it. An empty text puts nothing, and the flag goes with it. | `release` → `--release`, `debug` → nothing |

They are stored in the slot's JSON and skipped when unset, so a slot without them is written byte for
byte as before ([ADR 2](0002-brain-in-libsql.md)). The template stays a real command:
`git log --oneline -n {count}`, with no new syntax. An older jli reads it as it always did.

### What a command becomes

| Template | Slot | Value | Command |
| --- | --- | --- | --- |
| `git log --oneline -n {count}` | optional, flag `-n` | none | `git log --oneline` |
| | | 5 | `git log --oneline -n 5` |
| `cargo build --profile={profile}` | optional, flag `--profile=` | none | `cargo build` |
| `cargo build {profile}` | `release` → `--release`, `debug` → nothing; default `release` | none | `cargo build --release` |
| | | `debug` | `cargo build` |
| `ls -la {dir}` | optional, no flag | none | `ls -la` |

Leaving a slot out takes away the space before it and its flag, and nothing else. A value is quoted
as before ([ADR 7](0007-slot-templates.md)). A choice's text is not: a person wrote it as command text
with `jli slot`, and a request can only pick a choice, never write one. So a slot value still cannot
inject shell syntax.

### How a value is found

This replaces the order of [ADR 7](0007-slot-templates.md) and [ADR 11](0011-stated-arguments.md):

1. A value stated by name, alias or label. For a slot with choices it must name one, compared
   without case and kept as the choice spells it; a label there takes one word. A value that names
   none blocks the run with `missing_value`: `{profile} takes release or debug, not fast`.
2. A value the agent gave with its proposal; for a slot with choices, only one of them.
3. Values with no name, in template order. Quoted strings go to text slots when there is one for each
   open text slot; with fewer, but enough for the text slots that are not optional, those take them.
   Numbers go to number slots, optional ones too. A choice is taken when the request names exactly one
   of them as a word of its own: "a debug build", not "debugging".
4. System 1: a pick among files or branches as before, and among a slot's choices when the slot has
   no default and is not optional.
5. The agent, as before, for a text slot and a slot with no default, but never for an optional slot
   or a slot with choices: what an unsaid value means there is the person's to decide, and they did.
6. The default. A default wins over leaving a slot out.
7. An optional slot with nothing is left out. Any other slot blocks the run, as before.

So an optional slot the request does not name costs no model call.

### What the models may say

The slot schema of the translator, the agent and `jli generalize` gains `optional` and `flag`. The
translator is told:

> A value given with a flag is optional when the command does the same job without the flag: `git log`
> still lists commits without `-n 3`, and `cargo build` still builds without `--profile release`. Then
> set optional to true and flag to the flag as it is written, keep the flag in steps right before its
> placeholder, and give no default. A value the command cannot do its job without is not optional: the
> file of `wc -l notes.txt`, the path of `cargo install --path .`.

The agent is told the same in its own words, and to leave such a value out of `slotValues` when the
request does not name it. The schema says it once more on the field: "true only when the command does
the same job without this value and its flag … false when the command needs it, as cargo install
needs --path {path}".

What is kept is checked in code (`slots::checked_slots`), whatever the model said:

- a flag is kept only if it is written just before every use of its placeholder;
- a slot is optional only with such a flag, so no model leaves out a value the command takes without
  one, such as the directory of `ls {directory}`; that is a person's call;
- an optional slot from a model has no default, since a default would always fill it. A literal skill
  made general ([ADR 19](0019-one-row-per-command.md)) still gets the value it ran as its default, so
  its phrasings run what they ran;
- no model writes choices.

The check that the template gives back the taught command is unchanged: every slot needs its value
there, so a model cannot add an optional flag the taught command did not have. The agent's prompts
that fill slots and name arguments are shown each slot with the fields they were measured with, never
its flag or choices.

### `jli slot`

```sh
jli slot cargo-build profile --choice=release=--release --choice=debug= --default=release
jli slot git-log count --optional --flag=-n
jli slot list-largest-files count --default=5
```

`jli slot [--global] <label> <slot> [--default=<value>] [--optional | --required] [--flag=<flag>]
[--choice=<value>[=<text>] … | --no-choices] [--kind=<kind>]`. Each setting is written with `=`, so
`--flag=-n` stays one word (`-n` on its own is jli's dry run). `--choice=fast` puts `fast` in the
command; the choices given replace the slot's. `--default=` and `--flag=` with nothing take the
setting away. A placeholder the steps use that no slot declares is declared: teach `cargo build
{profile}` in manual mode, and give it choices.

It is a hand edit in the sense of [ADR 21](0021-hand-edits.md): written where `jli alias` writes, so
inside a project a global skill's slot changes in every project; `-n` writes nothing; a setting that
is already so exits 0 with `changed: false`; a slot keeps the aliases the row holds. A refusal writes
nothing and exits 1, and adds three kinds to ADR 18's closed set: `flag_invalid` (the flag is not
written just before the placeholder, or the slot is made optional where its placeholder is joined to
a word and no flag is given), `choice_invalid` (a choice that is not one word, begins with `-`, or is
given twice), and `default_invalid` (a default that is not one of the choices).
`skill_not_found` and `slot_not_found` are ADR 21's. Under `--json` the outcome is `label`, `slot`,
`declared`, `changed`, `dryRun`, `kind`, `default`, `optional`, `flag`, `choices`, `writtenTo`,
`project`. `jli skills` shows each slot's settings: `{count}: optional, left out with -n; default 5`.

### What else reads a slot

- `--json`'s `accepts` and `missing` describe each slot with `optional`, `flag` and `choices`,
  appended after `default` and always present.
- A plan leaves out an optional slot and does not wait for it. `jli run <plan> profile=Release`
  takes a choice as a request does, and refuses one that is none, saying which it takes.
- A command is read back against the skill's slots: `git log --oneline` is `git log --oneline -n
  {count}` without the count, and `cargo build` is `cargo build {profile}` with `debug`. So teaching
  `git log --oneline` joins that skill with no model call, a "no" followed by it is not a different
  command, and the learned phrasing hides a choice as it hides any value: "build the app in … mode".
- What a person set on a slot survives a write that replaces a skill's slots, as its aliases do.
  `jli generalize` keeps a project skill's choices, optional and flag on the global skill, but its
  default only when that is one of the choices: otherwise it is usually a value of the project.

**Measured on 2026-10-03**, live, with the release binary on scratch brains, mostly in a scratch git
repository with nine commits. The prompts above are the third wording. With the first (no "does the
same job", no example of a needed flag), `teach "install the app" -- cargo install --path .
--profile release` made `--path {directory}` optional too, and "install the app" rendered `cargo
install`, which fails. With the second (the prompt lines, not the schema description) one run did the
same. With the third, on five fresh brains, each teaching the same five commands:

| Taught | What the translator made |
| --- | --- |
| `git log --oneline -n 3` | `-n {count}`, optional: 5 of 5 |
| `head -n 5 notes.txt` | `-n {count}` optional, `{file}` required: 4 of 5; both required: 1 |
| `cargo build --profile release` | `--profile {profile}`, optional: 3 of 5; the command as it is: 2 |
| `cargo install --path . --profile release` | the command as it is: 3 of 5; `--path {path}` required with `--profile {profile}` optional: 1; both required: 1. `--path` optional: 0 of 5 |
| `git commit -m "fix the parser"` | joined the seeded `git commit -m {message}`, required: 5 of 5 |

On one more fresh brain with the second wording, `wc -l notes.txt`, `lsof -i :3000` and
`curl -s -o out.html https://example.com` kept every slot required.

The agent, with `--agent -y` on two fresh brains, then the same skill without the number:

| Request | What the agent proposed | Then, with no number |
| --- | --- | --- |
| "show the 4 most recent commits" | `git log -n {count}` and `git log --oneline -n {count}`, optional both times | `git log`, `git log --oneline`, `{count}` left out, System 1 alone |
| "show the 2 biggest files in this directory" | `… \| head -n {count}`, optional once and required once | `… \| head` the first time, `… \| head -n 5` the second |
| "the first 2 lines of notes.txt" | `head -n {count} {file}`, required both times | |
| "install this rust project from its folder" | `cargo install --path repo`, then `--path .`, with no slot | |

On the skill taught from `git log --oneline -n 3` (`{count}` optional): "show the recent commits"
routed at 0.93, "list the latest commits" at 0.96 and `-y what are the most recent commits` at 0.98,
each to `git log --oneline` with `{count} is left out with -n`, and no language model call; the last
listed all nine commits. `teach "the commit history in one line each" -- git log --oneline` joined the
skill with no model call. "give me the 7 most recent commits" rendered `-n 7`. "show me the git log"
went to the agent at 0.32, because the scenario the translator wrote said "limited to a specific
count".

The cargo profiles: `--mode=manual teach "build the app" -- cargo build {profile}` stored the command
as given, and "build the app" blocked on `{profile}`. `jli slot cargo-build-profile profile
--choice=release=--release --choice=debug= --default=release` declared the slot. Then "build the app"
rendered `cargo build --release` (`{profile} = release, written --release (from default)`) at 0.99;
"build the app in debug mode" rendered `cargo build` (`from pattern`) at 0.98; `--json -n build the
app profile=fast` stopped with `missing_value`, `{profile} takes release or debug, not fast`, and the
slot's choices in `missing`. `jli slot … --default=fast` was refused with `default_invalid`; `jli slot
… --default release` printed the usage line; `jli slot list-largest-files count --default=5` made "show
the biggest files in this directory" render `head -n 5`. Inside a project, `--json slot git-commit
message --default=wip` wrote to the global row (`writtenTo: global`, `project: shop-api`).

`jli generalize -n` in a scratch project: `head -n {count} {file}`, its `{count}` optional, was
proposed global with the count still optional, and verified at 0.99; `cargo build --profile {profile}`
was kept in the project in one run and proposed global in another.

The translator benchmark of [ADR 6](0006-learning-modes.md), 30 requests: semi 100% routed and 100%
right, as on 2026-10-02; manual 70% and 70% (73% and 70% then). 176 System 1 requests, about $0.012.

The acceptance checks of [ADR 9](0009-port-to-rust.md), on a fresh brain: 1 to 8 and 12 with no
terminal, 9 to 11 under `expect`. All held: `ls -ltr`; `git commit -m 'first draft'`; not a command,
exit 0; `git add -A` not run with nobody to confirm, exit 3; `rm '*.log'` refused as destructive, the
files untouched; valid `--json` on the `system1` path with the directory in `output`; the second teach
joining `git log --oneline -n {count}` with every API key unset; `-n 7`; for `show the git status`
outside a repository an `environment` diagnosis and no counter-example; three numbered options after a
no, the first learned as a new skill and run; an interactive session of two confirmed requests left
with ctrl-d, exit 0; `jli demo` exit 0, answered instantly 2 of 5 then 5 of 5, deciding 6.9 s then
1.4 s. Check 13 was not run: the TypeScript version is not available. A first try at check 9 asked
"anything changed in this repo", which the agent answered as not a command outside a repository.

## Consequences

- A value given with a flag that the request leaves unsaid is left out, at System 1's speed, instead
  of being written by the agent.
- What an unsaid value means is the person's to decide: a default, leaving it out, or a block.
- Words a person says can stand for command text they chose, without a skill per value and without a
  flag ever being a slot's value.
- **Known gap:** an older jli reads an optional slot as one that must have a value, as before, and a
  slot with choices as a plain one: for `cargo build {profile}` it renders `cargo build release`. A
  brain shared with an older jli should give choices only to skills that jli does not route.
- **Known gap:** the models mark a value given with a flag optional unevenly. With the third wording,
  of the slots made for a flag the command does without (`-n` of `git log` and `head`, `--profile`),
  the translator marked 13 of 15 optional and the agent 3 of 6. A slot left required works as before;
  `jli slot --optional` changes it.
- **Known gap:** a model can still call optional a flag the command needs. The first wording did so for
  `cargo install --path`; the third did not in five runs. The command is shown before it runs, and
  `jli slot … --required` undoes it.
- **Known gap:** a number word fills an optional number slot even when it counts something else:
  "the history of commits one line each" rendered `git log --oneline -n 1`. As for any number slot
  ([ADR 7](0007-slot-templates.md)), `count=` is the exact way.
- **Known gap:** a choice is found in a request only by the word that names it. "An optimised build"
  does not name `release`; the default, or `profile=release`, does.
- **Known gap:** only `jli slot --optional` leaves out a value with no flag, and nothing checks that
  the command does its job without it. A choice's text is the person's command text and, like a
  taught command, is not checked either.
- **Known gap:** the translator still writes the scenario of the instance: "show me the git log" did
  not reach a skill whose scenario said "limited to a specific count", though the skill runs it.
- **Known gap:** `jli slot` inside a project was run live once, and offline only through the code it
  shares with `jli alias`.
