# 31. A slot's choices name values unless one of them writes a flag, and a value stated for such a slot is taken as written

- Status: accepted
- Date: 2026-10-06
- Origin: the Rust version; a report from the owner
- Supersedes in part: [ADR 24](0024-optional-slots.md), on choices being the only values a slot
  takes, on a stated value that names no choice blocking the run, on a label taking one word for a
  slot with choices, and on reading a command back into a slot with choices; [ADR 18](0018-agent-contract.md),
  on what `accepts` and `missing` say about a slot, to which `onlyChoices` is appended

## Context

On 2026-10-06 the owner taught a skill for `xai tts`, gave its three placeholders choices and
defaults, and asked for speech with an output file and a text of their own:

```text
❯ jli teach --global --mode=manual "tts voice=carina output=output.mp3 text='Hello, my name is Tomas'" --name="tts" --description="… Default voice is carina, default output is output.mp3. If the user does not pass a 'text' value, keep the prompt at the end empty …" --risk=safe -- xai tts --voice {voice} --normalize -o {output} {text}
❯ jli slot tts output --choice=output=output.mp3 --default=output
{output} in tts is now a slot: output → output.mp3; default output.
❯ jli slot tts voice --choice=carina --choice=eva --default=carina
{voice} in tts is now a slot: carina, eva; default carina.
❯ jli slot tts text --choice=empty= --default=empty
{text} in tts is now a slot: empty → nothing; default empty.
❯ jli "tts output=hello.mp3 text='Hello, my name is Tomas'"
→ xai tts --voice {voice} --normalize -o {output} {text}  risk: safe
! {output} takes output, not hello.mp3; {text} takes empty, not Hello, my name is Tomas.
  Stored as plan 8: jli run 8 runs exactly this, with no model call.
```

Under ADR 24 a slot's choices were the only values it took, so both stated values were refused, and
the plan's note offered a run that could not happen without them.

The owner meant the choices as names for values: `output` is `output.mp3` unless the request says
otherwise, `empty` is no text unless it gives one. Nothing else that `jli slot` offers says that
with no model. A plain `--default` on a text slot is the agent's last resort, not the person's answer
([ADR 7](0007-slot-templates.md)): with nothing quoted, the agent is asked first and told to write a
sensible value. Choices were the only way to have an unsaid value mean what the person said, and they
closed the slot to everything else.

ADR 24 closed the slot for one reason, which its own example shows: Cargo's profiles are `--release`
and nothing at all, so `cargo build fast` means nothing, and `profile=fast` should stop with the
choices it takes. That holds where a choice writes a flag. Where every choice writes a value, or
nothing, the placeholder holds a value, and a value the person states is what it is for.

## Decision

### The rule

A slot whose choices include one that writes a flag (its text begins with `-`) takes only its
choices, as before. Any other slot with choices takes a value that names none of them, when the
person states it, as written and quoted like the value of a slot with no choices
(`Slot::takes_only_choices`).

| Template | Choices | Stated | Command |
| --- | --- | --- | --- |
| `xai tts --voice {voice} --normalize -o {output} {text}` | `output` → `output.mp3`; `carina`, `eva`; `empty` → nothing | `output=hello.mp3 text='Hello, my name is Tomas'` | `xai tts --voice carina --normalize -o hello.mp3 'Hello, my name is Tomas'` |
| | | nothing | `xai tts --voice carina --normalize -o output.mp3` |
| | | `voice=ara` | `xai tts --voice ara --normalize -o output.mp3` |
| `cargo install --path . --profile {profile}` | `release`, `dev` | `profile=bench` | `cargo install --path . --profile bench` |
| `cargo build {profile}` | `release` → `--release`, `debug` → nothing | `profile=fast` | stops: `{profile} takes release or debug, not fast` |

A value that names a choice, compared without case, is still that choice, so `output=OUTPUT` writes
`output.mp3`. A choice's text is still the person's and unquoted; another value is quoted, so a
request still cannot put shell syntax in the command.

### Where it holds

- A value stated by name or alias, and one whose name the agent placed
  ([ADR 12](0012-slot-aliases.md)): the value is the person's.
- `jli run <plan> name=value` for a plan waiting on that slot.
- A label: `text: good morning to you` gives a text slot whose choices write no flag the rest of the
  request, as a text slot with no choices gets. A slot that takes only its choices still takes one
  word.
- Reading a command back into the skill (`match_skill`): such a slot reads a choice's text as that
  choice and any other word or quoted string as the value itself. So
  `jli teach "say good morning as ara" -- xai tts --voice ara --normalize -o morning.mp3 'good morning'`
  joins `tts` with no model call, its phrase kept as given ([ADR 30](0030-taught-as-given.md)).

Where it does not:

- A value the agent gives with its proposal is taken only if it names a choice, as before: what an
  unsaid value means there is the person's, and they said it with the default.
- A quoted string with no name is not handed to a slot with choices, as before.
- A word in the request names only a choice ("in the eva voice"); System 1 picks only among the
  choices; the agent is never asked for a slot with choices.
- A default of a slot with choices must still be one of them (`default_invalid`).

### What is shown

`jli skills` and `jli slot` say that such a slot takes other values:
`{voice}: carina, eva, or another value; default carina`. A slot that takes only its choices is
shown as before. `jli help slot` says the rule.

`--json`'s `accepts` and `missing` append `onlyChoices` to each slot, after `choices` and always
present: `true` when a value that names no choice is refused, `false` otherwise, and for a slot
with no choices.

Two notes in the trace change with it. A choice that puts its own name in the command no longer says
so twice (`{voice} = carina`, not `{voice} = carina, written carina`). A plan that waits on a value
names it: `Stored as plan 9: jli run 9 profile=<value> runs it with that value, with no model call.`

Nothing stored changes: the rule reads the choices a slot already has.

### Measured on 2026-10-06

Live, with this branch's release binary, on scratch brains in scratch directories, System 1
`jev-1.13.0`, the models Mercury 2.5 on OpenRouter.

**The report**, replayed word for word: the teach and the three `jli slot` commands, then dry runs.

| Request | Command | Values |
| --- | --- | --- |
| `tts output=hello.mp3 text='Hello, my name is Tomas'` | `xai tts --voice carina --normalize -o hello.mp3 'Hello, my name is Tomas'` | output and text from the request, voice from the default |
| `tts` | `xai tts --voice carina --normalize -o output.mp3` | each from its default; `{text} = empty, which writes nothing` |
| `tts voice=ara text='Good morning'` | `xai tts --voice ara --normalize -o output.mp3 'Good morning'` | |
| `tts with the eva voice` | `xai tts --voice eva --normalize -o output.mp3` | voice from the pattern |
| `tts voice=eva text: good morning to you all` | `xai tts --voice eva --normalize -o output.mp3 'good morning to you all'` | |

Each routed to `tts` at 0.87 to 0.99 with no language model call, and `--json` gave `onlyChoices:
false` for all three slots. The teach above joined `tts` with no model call. On the same brain,
`teach --mode=manual --name=build-app "build the app" -- cargo build {profile}` with
`--choice=release=--release --choice=debug= --default=release`: "build the app" rendered
`cargo build --release`, and "build the app profile=fast" stopped with exit 3,
`{profile} takes release or debug, not fast`, and `onlyChoices: true` in `missing`.

**The acceptance checks** of [ADR 9](0009-port-to-rust.md), on fresh brains, checks 1 to 8 and 12 in
a scratch git repository with nine commits and 9 to 11 under `expect` in a directory outside any:
check 1 printed `ls -ltr`, exit 0; check 2 `git commit -m 'first draft'`; check 3 was not a command
(98% sure), exit 0; check 4 gave `git add -A`, not run, exit 3; in check 5 the agent proposed
`rm -f '*.log'`, destructive, refused with exit 3, and the three `.log` files were untouched; check 6
printed valid `--json`, `path` `system1`, with the directory in `output`. Check 7, with
`--mode=semi` as ADR 30 runs it, made `git-log-recent`, `git log --oneline -n {count}`, verified at
1.00, and `last 10 commits please` joined it with every API key unset and an empty `JLI_CHATGPT_DIR`.
Check 8 rendered `git log --oneline -n 7`, `{count} = 7 (from pattern)`. Check 9 routed to
`git-status` at 0.90, the command exited 128, the diagnosis put the problem on where it ran, and there
was no counter-example. Check 10 offered three numbered options after a no, and picking `ls -lh`
learned it and ran it. Check 11 took two requests with confirmations, `pwd` then `ls -ltr`, and left
on ctrl-d, exit 0. Check 12, `jli demo`, exited 0, answered instantly 2 of 5 then 5 of 5, and spent
6.9 s deciding, then 1.3 s. Check 13 was not run: it needs the TypeScript version.

`jli run <plan> voice=ara` for a plan waiting on a voice ran offline only.

## Consequences

- The owner's commands work as typed: a choice says what an unsaid value means, with no model, and
  a value they state is passed as written ([ADR 11](0011-stated-arguments.md)).
- A slot whose choices write a flag stops a value it cannot use, as ADR 24 measured.
- **Known gap:** a slot whose choices write no flag cannot be closed. `voice=carinna` runs
  `--voice carinna`, and the command's own error is the only check.
- **Known gap:** only a text that begins with `-` counts as a flag. A choice written `x --fast`
  leaves the slot open.
- **Known gap:** a quoted string with no name is still not handed to a slot with choices.
  `jli "tts 'Hello'"` runs with `{text}` at its default, which writes nothing, and `xai tts` then
  reads its input. `text='Hello'` is the way.
- **Known gap:** a plain `--default` on a text slot is still the agent's last resort
  ([ADR 7](0007-slot-templates.md)), so the way to an unsaid text value that no model writes is a
  choice, as the owner found.
- **Known gap:** a default of a slot with choices must still be one of them, so `--default=output.mp3`
  beside `--choice=short=s.mp3` is refused.
- **Known gap:** an older jli on the same brain still refuses a value that names no choice, and one
  older than ADR 24 reads a slot with choices as a plain one.
