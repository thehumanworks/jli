# 23. The install line follows the running shell, the function cleans up after ctrl-c, and a hyphenated number word is not a count

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version; issues #14 and #22
- Supersedes in part: [ADR 14](0014-shell-integration.md), on which shell the install line names
  and on what the shell function does when it is interrupted; [ADR 7](0007-slot-templates.md), on
  which words count as a number

## Context

Three small defects, each found on 2026-10-03 while building the work in #13.

1. **A number word inside a hyphenated word was a count** (#14). `numbers_in` in `src/args.rs`
   split the request on every character that is not a letter or a digit, so `one-line log of the
   newest 4 commits` gave the numbers `1` and `4`, and the first went to `{count}`: the command was
   `git log --oneline -n 1`.
2. **The install line named the login shell** (#22). When a step such as `cd` needs the calling
   shell and the function is not installed, jli says which line to add to which file
   ([ADR 14](0014-shell-integration.md)). It read the shell from `$SHELL`, which is the login shell,
   so inside a bash started from zsh it said `~/.zshrc`.
3. **Ctrl-c left the hand-off file behind** (#22). The function makes an empty file with `mktemp`
   before it runs jli and removes it afterwards. Measured on 2026-10-03 under `expect`, with a fake
   `jli` that sleeps, ctrl-c pressed while it ran, in `zsh -f -i +Z` (5.9) and
   `bash --norc --noprofile -i` (3.2):

   | Shell | INT trap of the user's own | Before: files left | After: files left |
   | --- | --- | --- | --- |
   | zsh | none | 1 (the function stopped before its `rm`) | 0, status 130 |
   | zsh | `trap 'echo user-trap' INT` | 0 (a trap lets the function go on) | 0, status 130, the user's trap still set |
   | bash | none | 0 (bash 3.2 goes on after the interrupted command) | 0, status 130 |
   | bash | `trap 'echo user-trap' INT` | 0 | 0, status 130, the user's trap still set |

   ZLE was turned off in zsh (`+Z`) because it dropped lines typed ahead by `expect`; ctrl-c reaches
   the foreground command through the terminal driver either way.

## Decision

### Number words

- **A word from one to ten counts as a number only when it is not joined by `-` to another word.**
  `one-line`, `two-column`, `three-way` and `twenty-one` give no number; `one line`, `four commits`
  and a dash standing alone (`- one`) still do. A word is joined when the character next to it is
  a `-` and the one after that is a letter or a digit.
- **Digits are unchanged.** `the last 5-10 commits` still gives `5` and `10`, and `HEAD~3` gives
  `3`. A range or a date has no single right reading, and nothing asked for one.
- Tested in `a_number_word_joined_by_a_hyphen_is_not_a_number` (`src/args.rs`), which fails on the
  old `numbers_in` with `["1", "4"]`.

### Which shell the install line names

- **The shell jli is running under** (`Shell::running` in `src/shell.rs`): the shell in
  `JLI_HANDOFF_SHELL` when it is set; else the nearest of jli's ancestors that is zsh, bash or fish;
  else the shell in `$SHELL`; else, as before, the lines for all three.
- **The nearest ancestor, not only the parent.** jli started through a shim such as mise's, or
  through `env`, `sudo` or `time`, has that program as its parent. jli looks up to eight processes
  up for a shell. In the usual case, a shell running `jli`, the parent is the shell.
- **The process table comes from `ps -A -o pid=,ppid=,comm=`**, started in `src/exec.rs`, the one
  place that starts the user's steps ([rule](../../rules/processes-start-in-known-places.yml)).
  Reading the table and walking it is done in `src/shell.rs` and tested offline with a fake table:
  a program's path may contain spaces, and a login shell shows as `-zsh`. `ps` is only run when
  the install line is about to be shown. If it cannot be run, `$SHELL` is used.
- **`jli shell-init` with no argument still reads `$SHELL`**, as ADR 14 says. The install line
  names the shell explicitly (`jli shell-init bash`), so the line it prints is right for the file
  it names.

### The function after ctrl-c

- **zsh and bash set an INT trap for as long as the function runs**, which removes the file:
  `trap 'command rm -f -- "$__jli_handoff"' INT`, set before jli is started. With a trap set, zsh
  goes on with the function after the interrupted command instead of stopping it, so the function
  reaches its own clean-up: jli's status is 130, nothing is sourced, the file is removed, and 130 is
  returned. The trap does not `return`, so that clean-up always runs.
- **The user's own INT trap is kept.** bash saves it with `$(trap -p INT)` before setting its own,
  and after the clean-up resets INT and `eval`s what it saved. zsh sets `local_traps` while it sets
  the trap, which puts the previous trap back when the function returns, and then unsets the
  option again if it was not already set. `local_options` is not used: it would also undo a `setopt`
  that jli handed over. Checked on 2026-10-03: a handed-over `export FOO=bar; setopt autocd` and a
  `trap … USR1` outlast the function in zsh, `shopt -s cdspell` does in bash, no INT trap is left,
  and `local_traps` is off afterwards.
- **fish removes the file once the command line is done.** fish runs no signal handler for a
  ctrl-c that reaches only the command in the foreground (its documentation for `function
  --on-signal` says so), and stops the function. So the fish function adds its file to a global
  list, `__jli_handoffs`, and a second function, `__jli_remove_handoffs`, run on the
  `fish_postexec` event, removes every file on the list and clears it. The function's own `rm` still
  removes the file on a normal exit.
- The wrappers are tested offline as text (`an_interrupted_function_still_removes_its_file` in
  `src/shell.rs`): each trap is set before jli runs, and bash restores the user's trap after.

### Measured live on 2026-10-03

With the release build and a scratch brain. System 1 answered every request with HTTP 401 from
TypeSafe (the key in the environment was rejected), so the paths through System 1 could not be run;
these went through the agent with `--agent`:

| Request | Result |
| --- | --- |
| `SHELL=/bin/zsh bash -c 'jli -y --agent go into the directory sub'` | refused, exit 3, `add this line to ~/.bashrc`, `eval "$(jli shell-init bash)"` |
| `SHELL=/bin/bash zsh -f -c 'jli -y --agent go into the directory sub'` | refused, exit 3, `add this line to ~/.zshrc`, `eval "$(jli shell-init zsh)"` |
| `jli -y --agent one-line log of the newest 4 commits`, with `git log --oneline -n {count}` taught | `git log --oneline -n 4`; the count came from the agent, not from `numbers_in`, and the learned phrasing is `one-line log of the newest … commits` |

## Consequences

- `one-line log of the newest 4 commits` fills `{count}` with 4 when System 1 picks the skill.
- The install line names the file of the shell the user is typing in, and a ctrl-c leaves no
  `jli-handoff.*` file in `$TMPDIR` in zsh or bash.
- **Known gap:** fish has still not been run. Whether `fish_postexec` is emitted after a command
  line that ctrl-c cancelled is from fish's behaviour as described, not measured. A `jli` left
  running in the background when the line ends has its file removed early.
- **Known gap:** when no ancestor is a shell, as when an editor or an agent starts jli directly, the
  install line still names the shell in `$SHELL`. A `bash -c` an agent runs counts as bash.
- **Known gap:** digits joined by `-` are still numbers: `a 3-way diff` gives 3. `the last 5-10
  commits` gives 5 to the first number slot.
- **Known gap:** `jli shell-init` with no argument still reads `$SHELL`.
- **Known gap:** a sourced hand-off that sets its own INT trap in bash has it replaced by the user's
  earlier trap when the function returns.
- **Known gap:** the number-word fix was not exercised live through System 1, whose key was rejected
  on 2026-10-03; it is covered by the offline test only.
