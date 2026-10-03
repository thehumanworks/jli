# 14. Steps such as `cd` are handed to the calling shell, and paths are expanded before they are quoted

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 5](0005-routing-loop.md), on where a step runs, the `--json` outcome and
  the exit codes; [ADR 7](0007-slot-templates.md), on what happens to a file value before it is quoted

## Context

**Builtins.** jli runs every step with `bash -c` in a child process (`src/exec.rs`). A builtin
whose whole effect is on the shell that runs it, such as `cd`, `export`, `source` or `alias`, exits
0 there and changes nothing the user can see. jli still reports a success and learns from it. The
owner's brain has a skill `cd {directory}`. On 2026-10-03, in a scratch brain, the agent proposed
`cd {directory}` for "go into the directory …" and it was learned after a clean exit, with three
more phrasings on later requests; `jli -y go to the sub directory` (a taught `cd sub`) exited 0 and
the calling shell stayed where it was. Each step also starts in the original directory, so in a
skill `cd build`, `make`, the `make` never sees the `cd`.

**`~` and `$HOME`.** `shell_quote` in `src/slots.rs` single-quotes any value with a character
outside `[A-Za-z0-9_\-.,/:=@%+]`, and `~` and `$` are outside it. That is the guarantee of ADR 7,
and it is also why nothing is expanded. Measured on 2026-10-03 with the release build:

| Request | Command | Result |
| --- | --- | --- |
| `--agent 'go into the directory dir=~/notes'` | `cd '~/notes'` | exit 1 |
| `--agent 'list the files in dir=$HOME/notes'` | `ls '$HOME/notes'` | exit 1 |
| `--agent 'count the lines of ~/notes/n.txt'` | `wc -l '~/notes/n.txt'` | exit 1, the value came from the agent |
| `count the lines in the file /…/home/notes/n.txt`, with `n.txt` in the working directory | `wc -l n.txt` | System 1 picked the wrong file |
| `count the lines in the file '~/notes/n.txt'` | `wc -l n.txt` | the same |

A value reaches jli unexpanded more often than it seems: zsh does not expand `~` after `=`, so
`jli go to dir=~/notes` typed at a zsh prompt passes `~/notes` (seen live under `expect`); a line
typed in jli's own session never meets a shell; an agent calls jli with argument vectors; and the
agent and System 1 write values such as `~/notes.txt` from the words of a request. The last two
rows are a separate defect: the System 1 file pick (`list_entries` in `src/context.rs`) is offered
only the top level of the working directory, so a path anywhere else could not be picked, and a
similar name in the working directory could be picked instead.

## Decision

### The shell function

- **`jli shell-init [zsh|bash|fish]`** prints a shell function named `jli`. With no argument the
  shell is read from `$SHELL`; an unknown shell is an error that lists the three. It needs no brain
  and no keys. It is installed with `eval "$(jli shell-init zsh)"` in `~/.zshrc` (`bash` in
  `~/.bashrc`; `jli shell-init fish | source` in `config.fish`).
- **The side channel is a file.** The function makes an empty file with `mktemp`, runs
  `command jli "$@"` with its path in `JLI_HANDOFF` and the shell's name in `JLI_HANDOFF_SHELL`, and
  afterwards, if jli exited 0 and the file is not empty, sources it in the calling shell and deletes
  it. It returns jli's exit code, or, when something was sourced, the status of what was sourced.
  Arguments pass through as `"$@"` (fish: `$argv`) and are never re-read.
- **Why a file.** Stdout carries the command's output and must keep composing in pipes
  ([ADR 5](0005-routing-loop.md)), so `eval "$(jli …)"` is out. Stderr is the trace. A file
  descriptor such as fd 3 would have to be opened by the function around jli and read back while
  jli writes, which a pipeline and fish make awkward. A file made by `mktemp` is private to the user
  (mode 0600), works the same in all three shells, and needs no reading until jli has exited.
- **jli never creates the file.** It opens the path for appending and fails if it does not exist,
  so a stray `JLI_HANDOFF` cannot make jli write anywhere new. Both variables are removed from every
  step's environment (`env_remove` in `src/exec.rs`), so a step that itself runs `jli` cannot write
  into the calling shell's file.

### Which steps need the calling shell

- **The rule** (`needs_caller` in `src/shell.rs`): a step needs the calling shell when one of its
  top-level commands is `cd`, `chdir`, `pushd`, `popd`, `export`, `unset`, `source`, `.`, `alias`,
  `unalias`, `set`, `setopt`, `unsetopt`, `shopt`, `umask` or `ulimit`, or is nothing but
  `NAME=value` assignments. Top-level commands are the parts of the step between `&&`, `||`, `;`,
  `|`, `&` and newlines, read with quotes respected; a leading `{`, `!` or `builtin` is skipped.
  Text in quotes, in `( … )` or `$( … )`, in backticks and after `#` is not split: `(cd src && make)`
  runs in a subshell anyway, and `git commit -m 'x; cd /'` is one command. So `git clone x && cd x`
  needs the calling shell; `EDITOR=vim git commit` does not.
- **The rule is read on the rendered step.** Slot values are quoted before it is read, so a value
  cannot make a step a builtin (`a_quoted_value_cannot_make_a_step_a_builtin`).
- **Never handed over:** a step with `exit`, `logout` or `exec` as a top-level command runs in
  jli's shell as before. Sourced into the user's shell, it would end or replace that shell.
- **Not recognised:** `declare`, `typeset`, `readonly` and `local` (inside the function they would
  make variables local to it), `eval`, and shell functions such as `nvm use` or `conda activate`.
  They run in jli's shell as before.

### Where the steps run

- **The steps before the first that needs the calling shell run in jli's shell**, as before. That
  step and **every step after it** are handed over, so the later steps see the `cd`:
  `mkdir -p {directory}`, `cd {directory}`, `touch inside` runs `mkdir` in jli's shell and
  `cd … && touch inside` in the user's.
- **Tried first when it is safe.** When every command handed over is `cd`, `pushd`, `popd`,
  `export`, `unset`, `alias`, `unalias`, `umask` or an assignment, which touch nothing outside the
  shell and mean the same in bash and zsh, jli runs them once in a shell of its own with stdout
  dropped. If that fails, as `cd /nowhere` does, nothing is handed over and the run is a failure
  with the shell's own message and exit code, which learning and auto mode's diagnosis treat like
  any failure. Anything else (`source`, `setopt`, a later `make`) is not tried: it would run twice,
  or mean something else in bash 3.2 than in the user's zsh.
- **The text handed over is exactly the rendered steps.** One step is written as it is. Several
  are each kept verbatim in a group, joined so each runs only if the one before succeeded:
  `{ cd x\n} && { make\n}` for bash and zsh, `begin … end` joined by `and` for fish. A `;`, `||` or
  `# comment` in one step cannot reach the next.
- **Quoting for the shell that reads it.** Values are quoted by `shell_quote` as in ADR 7, which
  holds for bash and zsh, with one change: a value that starts with `=` is now quoted, because zsh
  reads an unquoted `=name` as the path of the command `name`. For fish, values are quoted by
  `fish_quote`: inside fish's single quotes `\'` and `\\` are escapes, so `it's` is `'it\'s'` and
  `a\b` is `'a\\b'`, and `%` is quoted too. jli's own shell is still bash, so `commands` in the
  outcome and the trace keep the bash quoting; `callerShell` holds what the calling shell reads.

### Without the function

- **Such a step is refused**, before "Run it?": nobody is asked to approve what cannot work. The
  trace says
  `Not run: cd sub has to run in your own shell, and jli can only run commands in a shell of its
  own, where this would change nothing.`, gives the install line for the shell in `$SHELL` (all
  three when it is unknown), and says nothing was learned. `blocked` is
  `"must run in the calling shell"` and the exit code is 3, as for anything else that did not run.
- **Nothing is learned**: no skill, no phrasing, no alias, no use. A command typed after a "no"
  is refused the same way, before it is learned. The counter-example from the "no" is kept: that
  lesson is about the skill that was declined.
- **An agent with no function and no terminal** gets the same outcome: `--json` returns
  `blocked: "must run in the calling shell"`, exit code 3, and `callerShell` with the steps it must
  apply in its own shell, quoted for bash. Nothing ran, including any steps before the first one in
  `callerShell`, so `commands` is the whole list to apply.

### Confirmation, dry runs, `--json`, exit codes and learning

- Confirmation and risk are unchanged: a handed-over step is shown and asked about like any other,
  and `--yes` still never approves a destructive one.
- A dry run (`-n`) runs and hands over nothing, fills `callerShell`, and says whether the steps
  would run in the calling shell or would need the function. Its exit code stays 0.
- **`--json` appends two fields**, after `msSystem2`: `callerShell`, the steps that had to run in
  the calling shell (empty when none), and `handedOff`, whether they were written to the function.
  With the function, the outcome is printed and then the calling shell runs the steps.
- **A hand-over is a run.** `ran` is true, and `exitCode` and jli's own exit code are those of the
  steps that ran in jli's shell, or of the trial, which is 0 when anything was handed over. The
  function then returns the status of what the calling shell ran. Learning follows the existing
  rules on that exit code: a phrasing after a human yes or a clean exit, a new proposal after a
  clean exit.

### Expansion of `~` and `$NAME` in paths

- **A file value is expanded in jli, then quoted** (`expand_path` in `src/slots.rs`). Expanded:
  a leading `~` alone or followed by `/`, to `$HOME`; `$NAME` and `${NAME}` anywhere, to that
  variable in jli's own environment. Nothing else: `~user`, a `~` that is not leading, `${NAME:-x}`,
  `$1`, `$$` and `$?` stay as written, and `$(…)` and backticks are never run. The result is quoted
  like any value, so a variable whose value is `my notes; rm -rf /` reaches the command as one
  quoted word. ADR 7's guarantee holds: a slot value still cannot inject shell syntax.
- **Only file slots.** A text value is passed as written: a commit message that says "costs $5"
  or "fix $PATH handling" means those characters, and expanding it would put the user's
  environment into their history. A branch never holds a path, and a number has no `$`. In three
  live proposals on 2026-10-03 the agent gave a path outside the working directory the kind `file`
  (`cd {directory}`, `ls {directory}`, `wc -l {file}`), so the prompts are unchanged.
- **Every source.** The value is expanded however it was found: stated, from the agent, from a
  System 1 pick or a default.
- **An unset or empty variable is left as written**, and the trace says
  `$NOPE is not set, so {directory} is passed as written.` Expanding an empty `$PROJECT` in
  `$PROJECT/build` to `/build` is the case this guards against.
- **A value that names an existing path as written is kept**, so a file really called `$RECYCLE`
  or `~` in the working directory is still reachable.
- **The trace shows it**: `{directory} = /Users/ada/notes (from request, ~/notes expanded)`, and
  the command line above it shows the expanded value. An expanded value is shown even when it came
  from the agent for a new skill, where values are otherwise not listed.
- **The phrasing is shaped after expansion**, so the values it hides are those of the command that
  is logged, which `jli fix` reads back ([ADR 13](0013-learning-fixes.md)).

### Paths elsewhere in the file pick

- A file slot's System 1 pick is offered the top level of the working directory, as before, and
  also every word of the request that is a path: one that contains `/`, or is `~`, or starts with
  `~/` or `$NAME`, and contains no `=` and no `://`. The question's wording is unchanged
  ([ADR 3](0003-system-1.md)); the words are options, like directory entries. The picked word is
  then expanded like any file value.

### Elsewhere on the way to execution

Templates were checked: a `$HOME` or `~` written in a step is not a placeholder
([ADR 7](0007-slot-templates.md)) and is expanded by the shell that runs the step, which is right.
A command taught with `jli teach … -- cd ~/x` reaches jli already expanded by the user's shell.
No other place on the path from request to execution quotes or drops a `~` or `$`.

**Measured on 2026-10-03**, live, with a scratch `HOME`, a scratch brain, and the function installed
from a `.zshrc` and a `.bashrc` that `eval` it, in `zsh -i` (5.9) and `bash -i` (3.2):

| In the calling shell | Result, the same in zsh and bash |
| --- | --- |
| `jli -y go to the sub directory; pwd` (a taught `cd sub`) | `cd sub runs in your shell as soon as jli exits.`; exit 0; the shell is in `sub` |
| `jli -y go into the directory "dir=~/notes"` | `cd /…/home/notes`, `(from request, ~/notes expanded)`; the shell is there |
| `jli -y go into the directory 'dir=$HOME/my notes'` | `cd '/…/home/my notes'`; the shell is there |
| `jli -y set my editor to vim; echo $EDITOR` (a taught `export EDITOR=vim`) | `vim`, after `nvim` before |
| `jli -y go into the directory dir=/nowhere/at/all; echo $?` | the shell's own `No such file or directory`, exit 1, nothing handed over, the shell stays |
| `jli -y show me the newest files \| grep -c .` | `6`, the lines of `ls -ltr`: output still goes down the pipe |
| `jli -y -q count the lines in the file "~/notes/n.txt" \| awk '{print $1}'` | `1` |
| `jli -y -q go to the sub directory \| cat; pwd` | the shell stays: the function runs in a subshell in a pipe |
| `command jli -y go to the sub directory` | refused with the install line, exit 3, nothing learned |
| `command jli --json -y go to the sub directory` | `blocked: "must run in the calling shell"`, `callerShell: ["cd sub"]`, `handedOff: false` |
| `jli --json -y go to the sub directory > out.json` | `ran: true, exitCode: 0, handedOff: true`; the shell is in `sub` |
| `jli -y make a scratch dir and enter it` (`mkdir -p tmpx`, `cd tmpx`, `touch inside`) | the shell is in `tmpx`, and `inside` is there |
| `jli -n go to the sub directory` | `cd sub would run in your shell, once jli has exited.`; exit 0 |
| `jli -n --json commit "message=it's \$5 and \"quoted\""` | `git commit -m 'it'\''s $5 and "quoted"'`: arguments pass through the function untouched |
| `jli -y --agent "list the files in dir=~/notes"` | `ls /…/home/notes`, lists `n.txt`, learned as a new skill |
| `jli -n count the lines in the file "~/notes/n.txt"`, with `n.txt` in the working directory | System 1 picked `~/notes/n.txt`: `wc -l /…/home/notes/n.txt` (before: `wc -l n.txt`) |
| `jli -n go into the directory "dir=\$NOPE_NOT_SET/x"` | `cd '$NOPE_NOT_SET/x'`, and the trace says it is not set |
| `jli -n go into the directory "dir=\$(touch …/pwned)"` | quoted as written; no file was created |

The acceptance checks 1 to 8 and 12 of [ADR 9](0009-port-to-rust.md) were re-run with a fresh
brain and held. `jli demo`, whose requests contain no builtin and no `~`, answered 2 of 5 then 4 of
5 instantly on one run (deciding 5.8 s, then 2.8 s) and 2 of 5 then 5 of 5 on the next (5.5 s, then
1.4 s).

Under `expect`, in `zsh -i` at a real terminal: `jli go to the sub directory`, answered `y` at
"Run it?", left the shell in `sub` with exit 0; `jli go into the directory dir=~/notes`, answered
`n` and then enter, exited 3 and the shell did not move.

## Consequences

- `jli go to src` moves the user's shell, once the function is installed, and a skill such as
  `cd {directory}` is learned like any other. Without the function, jli says what is missing instead
  of reporting a success that did nothing.
- `~/notes` and `$HOME/src` work as file values, wherever they come from, and a path elsewhere can
  be picked by System 1.
- **Known gap:** jli cannot see what happens to steps it handed over without trying them first
  (`source`, `setopt`, or any step after the first that needs the calling shell, such as the `make`
  in `cd build`, `make`). For `--json` and for learning they count as a clean exit, their output is
  not captured, and auto mode cannot diagnose them.
- **Known gap:** the trial runs in bash, which on macOS is 3.2, whatever the calling shell is. A
  builtin that means something else in zsh or fish can pass the trial and then fail, or the other
  way round; fish has no `unset`, so a handed-over `unset` passes the trial and fails in fish.
- **Known gap:** fish has not been run: it is not installed on the machine this was built on. The
  fish function and `fish_quote` are covered by offline tests only. A template written with syntax
  fish lacks, such as `${NAME}` or `[[ … ]]`, fails in fish with fish's own error.
- **Known gap:** in jli's own session (`jli` with no arguments) handed-over steps are kept in order
  and run when the session ends, and the session itself stays in its directory.
- **Known gap:** the install line names the shell in `$SHELL`, the login shell. In a bash started
  from zsh it says `~/.zshrc` (seen live).
- **Known gap:** interrupting jli with ctrl-c can leave the function's empty temp file behind.
- **Known gap:** a sourced file's `typeset` or `declare` without `-g` makes a variable local to the
  function, so it is gone when the function returns.
- **Known gap:** `~user` is not expanded, there is no escape for a literal `$` in a file value other
  than the file existing, and a quoted path with spaces in the request is not offered to the pick.
- **Known gap:** a `~` path that System 1 or the agent found, rather than one stated by name, stays
  visible in the learned phrasing, because the phrasing hides the values of the logged command,
  which are expanded. `jli fix` agrees with what was learned.
- **Known gap:** whether a model gives a path outside the working directory the kind `file` was seen
  in three proposals, not measured; the slot schema still describes `file` as "a path in the working
  directory". A path in a text slot is not expanded.
