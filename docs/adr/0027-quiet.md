# 27. A quiet run hides the trace, keeps its warnings, and still shows what a question asks about

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version, from feedback on `--quiet`
- Supersedes in part: [ADR 5](0005-routing-loop.md), on what `--quiet` and `--json` hide, and adds
  `JLI_QUIET` to the CLI contract beside `JLI_YES` and `JLI_YOLO`

## Context

The feedback: "JLI_QUIET=1 or --quiet / -q should run jli without the probability table being
printed. Exception: if it requires prompting the user, then the user prompt must show."

`-q` already hid the decision trace, System 1's table with it. Three things were wrong:

- There was no `JLI_QUIET`, so quiet could not be set once for a shell, as `JLI_YES` and `JLI_YOLO`
  can.
- The trace also carried what each question was about. "Run it?" follows `→ git add -A  risk:
  modifies`, which is a trace line, so a quiet run asked `? Run it? [Y/n]` without saying what it
  would run. "Pick a number" came without its numbered options, and "Make it global?" without the
  proposal. `--json` hides the trace for the same reason, and had the same gap with someone at the
  terminal.
- Quiet hid the warnings with the trace. A quiet run nobody could confirm exited 3 without a word.

## Decision

### `JLI_QUIET`

`JLI_QUIET` set to `1` or `true`, in any case, is `--quiet`. It is read where `JLI_YES` and
`JLI_YOLO` are, by the same rule: anything else is ignored, and the environment never turns a flag
off.

### What each run shows

| | The trace: System 1's table, the agent, notes, what was learned | Warnings | What a question asks about |
| --- | --- | --- | --- |
| no flag | shown | shown | in the trace, above the question |
| `-q`, `--quiet`, `JLI_QUIET=1` | hidden | shown | shown with the question |
| `--json` | hidden | hidden: the outcome's `blocked`, `blockedCode` and `exitCode` say the same | shown with the question |

The trace and the warnings still go to stderr, and the command's output to stdout
([ADR 5](0005-routing-loop.md)). `jli teach`, `jli eval` and `jli generalize` have no JSON outcome
and keep their trace under `--json`, as before; there `--quiet` hides the trace and keeps the
warnings.

### A question shows what it asks about

`Prompt::confirm` and `Prompt::ask` take the lines a question is about. The terminal prints them
just before the question, and only once it asks: with no terminal nothing is asked, so nothing is
printed. `Ui::about` hands the lines over only when the trace is off, so a line is never printed
twice, and with the trace on the output is what it was.

| Question | What it shows when the trace is hidden |
| --- | --- |
| "Run it?", for a route and for a destructive plan under `jli run` | the commands and the risk, as the trace writes them: `→ git add -A  risk: modifies` |
| "Pick a number, or say what you expected" | the numbered options; after a failed run, the diagnosis before them |
| "What should it run instead?" | nothing more: only a "no" to "Run it?" leads here, and that question showed the command |
| "Make it global?" | the global row as it would be written, and the "If approved: …" line |

The notes about each value, such as `{count} is left out with -n`, are not shown: the command shows
the values.

```
$ JLI_QUIET=1 jli list the files here
→ ls  risk: safe
? Run it? [Y/n] y
a.txt		b.txt		notes.md
$ JLI_QUIET=1 jli list the files here </dev/null
! Not run: nobody to confirm. Re-run with --yes to approve.
```

### Warnings

Under `--quiet` a warning still prints: "Not run: nobody to confirm", "Not run: it is
destructive", "could not work out a value for …", "… does not take …", "exited with code N", a plan
that is refused, and `jli generalize`'s "Not offered", "Rejected by System 1" and "Not written". A
quiet run that does nothing says why in one line, instead of exiting 3 in silence. Under `--json`
they stay hidden: an agent reads the outcome, and stdout is that outcome alone.

**Measured on 2026-10-03**, live, with the debug binary on fresh scratch brains, under `expect` for
the terminal:

- `JLI_QUIET=1 jli list the files here`: no System 1 table; `→ ls  risk: safe` just before `Run it?`;
  `y` ran `ls`, exit 0. With stdin from `/dev/null`: the one warning line, exit 3. With the trace on:
  the table, then `→ ls  risk: safe` once, then `Run it?`. `-q`, answered `n`: the command, `Run
  it?`, then "What should it run instead?"; enter skipped it, exit 3. `--json` at a terminal: the
  command before `Run it?`, then the JSON outcome on stdout.
- The acceptance checks of [ADR 9](0009-port-to-rust.md) that ask or refuse a confirmation, each with
  the trace on and again under `JLI_QUIET=1` (or `-q`). 4: `git add -A` not run with nobody to
  confirm, exit 3; quiet printed only the warning. 5: `rm '*.log'` refused as destructive, the files
  untouched; quiet printed only the warning. 9: `show the git status` outside a repository, in auto
  mode, approved: exit 128, an `environment` diagnosis with the trace on, no counter-example either
  way; quiet showed the command before `Run it?` and then only `! exited with code 128`, since nothing
  was asked after it. 10: auto mode, `ls` declined: three numbered options, shown under quiet too;
  picking 1 learned `ls -lh` as `list-files-with-sizes` and ran it. 11: an interactive session took
  two confirmed requests (`ls`, `pwd`) and left on ctrl-d, exit 0; under quiet each `Run it?` came
  after its command and no table was printed.

## Consequences

- `JLI_QUIET=1` in a shell's profile gives jli without the table, and every question it still asks
  says what it is about.
- A quiet run with nobody at the terminal prints at most a warning on stderr, and the command's
  output on stdout.
- `--json` at a terminal now shows the command before "Run it?". Before, it asked without it.
- **Known gap:** `-q -n` prints nothing: a dry run asks nothing, and the command it would run is a
  trace line. `--json -n` gives the command.
- **Known gap:** under `--quiet`, a run nobody could confirm warns, but says neither the command nor
  the plan it stored; `--json` gives both.
- **Known gap:** the agent's answer to a request that is not a command is a trace line, so `--quiet`
  hides it, and a request System 1 calls not a command ends with nothing printed and exit 0.
- **Known gap:** `jli fix` prints its "✚ learned … does not mean" and dry-run lines outside the
  `Ui`, so they show under `--quiet` and `--json`.
