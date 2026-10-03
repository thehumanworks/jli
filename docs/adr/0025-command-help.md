# 25. `--help` before `--` is jli's own help, each command has its own, and a choice is shown by its name

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version, from two reports on 2026-10-03
- Supersedes in part: [ADR 5](0005-routing-loop.md), on a line being a request whenever it is not a
  subcommand in its own shape: a line that asks for help before `--` is never a request.
  Clarifies [ADR 24](0024-optional-slots.md) on how a choice is written, without changing it

## Context

Two reports from using jli on 2026-10-03.

1. **`jli skills --help` routed "skills --help" through the models.** jli knew `--help` and `-h`
   only as the first word, and `help` as a command. Anywhere else they were words of a request:
   a subcommand is recognised only in its own shape ([ADR 5](0005-routing-loop.md)), `skills` takes
   no words, so the line went to System 1 and then the agent. The same held for every command
   (`jli slot --help`, `jli eval -h`), and `jli --mode=wild --help` stopped at the bad flag before
   help was looked for.
2. **`jli slot install-rust-app-from-path-x profile --choice=release=release --choice=debug=debug
   --default=release`.** The person meant `--choice=release --choice=debug --default=release`, and
   that already worked: `slot_shape` gives a choice written without a second `=` its own name as its
   text, and ADR 24 says `--choice=fast` puts `fast` in the command. They wrote the long form because
   the help showed only `--choice=<value>=<text>`. `jli skills` then showed `release → release`,
   which reads as if the text had to be given.

## Decision

### `--help` and `-h` before `--` are always help

jli reads the command line for help first: before its flags are parsed, before the brain is opened
and before any model is asked. A line asks for help when `--help` or `-h` is one of its words before
`--`, or when its first word, after jli's flags, is `help`. After `--` the words belong to the
command, so `jli teach "list" -- ls --help` still teaches `ls --help`.

The help given depends on the first word that is not one of jli's flags (a word that begins with
`-`):

| Line | Help |
| --- | --- |
| `jli skills --help`, `jli --json skills -h`, `jli help skills` | the `skills` command's |
| `jli --help`, `jli -h`, `jli help`, `jli help --help`, `jli help me find big files` | the full help |
| `jli deploy the app --help`, `jli show me the skills -h` | the full help: the first word names no command |
| `jli --mode=wild --help` | the full help, though `--mode=wild` alone is refused |
| `jli teach x -- ls --help` | none: it teaches |

The word after a bare `--project` is skipped as the project's id unless it names a command, so
`jli --project shop skills --help` is the `skills` command's help (`--project` is ADR 26's). Help
goes to stdout and exits 0, `--json` or not.

`run` asks before anything else. `dispatch` asks again about the words it is given, so that no way
into it routes a line that asks for help; a test sends `skills --help`, `slot -h`, `deploy the app
--help` and `help eval` through it, with System 1 and every language model set to panic on any call,
and checks that no event was logged.

### The help is written once

`src/help.rs` holds a table of the commands, each with its lines and the options that matter to it,
a table of the options and one of the environment variables. `jli --help` prints all of it; `jli
<command> --help` prints that command's lines, the options it names, and a line saying that `jli
--help` has the rest. A test checks that each command's lines are in the full help as they are in
its own. Every command has an entry: `skills`, `stats`, `history`, `teach`, `config`, `fix`, `run`,
`forget`, `forget-phrase`, `alias`, `slot`, `eval`, `share`, `project`, `generalize`, `sync`,
`shell-init`, `demo` and `help`.

```
$ jli slot --help
Usage
  jli slot <label> <slot> <setting>…   how a slot is filled and written; each setting is written with =
      --default=<value>                the value it takes when the request names none (--default= for none)
      --optional | --required          whether it, and its --flag=<flag>, may be left out of the command
      --choice=<value>[=<text>] …      the only values it takes. A choice puts its own name in the command,
                                       or <text> when one is given: --choice=release=--release.
                                       --choice=debug= puts nothing; --no-choices takes them away
      --kind=file|branch|number|text   what kind of value it takes, and so where one is looked for

Options
  -n, --dry-run         show what would run or change, and do neither; a request is kept as a plan for jli run
      --json            print the outcome as JSON (for agents); command output is captured

jli --help lists every command, the options they all take, and the environment.
```

The full help also gained lines it lacked: `-h, --help`, `JLI_MODE` (always read, never listed),
teach's `--risk=R` and slot's `--kind=`, and, ahead of the records that implement them,
`--project[=<id>]` (ADR 26) and `JLI_QUIET` (ADR 27). Its sections are now Usage, Options, Arguments,
Commands and Environment; the last two were "The brain" and "Storage". `-n` is described so that it
holds for a request and for a hand edit alike: "show what would run or change, and do neither".

### A choice is shown by its name

Nothing about choices changes in what is stored or run. `--choice=<value>` puts the value itself in
the command, `--choice=<value>=<text>` puts `<text>`, and `--choice=<value>=` puts nothing;
`--choice=release` and `--choice=release=release` store the same choice. The help, the refusal of
a bad choice (`choice_invalid`) and the README now lead with the short form:

```sh
jli slot install-app profile --choice=release --choice=dev --default=release
jli slot cargo-build-profile profile --choice=release=--release --choice=debug= --default=release
```

`jli skills`, `jli slot` and `jli generalize` show a choice whose text is its own name as the name
alone: `{profile}: release, dev; default release`. A choice with other text reads as before:
`release → --release, debug → nothing`. A test gives a `{profile}` placeholder the short form with
`jli slot` and routes "install the app" to `echo install release` and "a debug install" to `echo
install debug`.

**Run on 2026-10-03** with the debug binary, `JLI_DB_URL` set to a scratch file and no API key:
`jli skills --help`, `jli slot --help`, `jli --help`, `jli help eval`, `jli deploy it --help`,
`jli --mode=wild --help` and `jli --json skills -h` each printed the help described above and exited
0, and the scratch brain file was never created. No prompt, schema or System 1 wording changed, and
the router, learning and storage are as they were, so nothing was measured against a model and the
acceptance checks of [ADR 9](0009-port-to-rust.md) were not re-run.

## Consequences

- `--help` after any command is answered at once, with no key, no brain and no model, and an agent
  can read one command's usage the same way.
- The full help and a command's help come from one table, so they cannot drift apart.
- **Known gap:** a request with `-h` or `--help` as a word of its own before `--` is now help: `jli
  du -h of this folder` prints the help. The way round is to quote the request as one argument
  (`jli "du -h of this folder"`), or to put those words after `--` with at least one word before
  it: `jli show disk usage -- -h` routes "show disk usage -h". `jli -- du -h` does not, since with
  no word before `--` it is the interactive session, or the full help with no terminal.
- **Known gap:** help is plain text even with `--json`.
- **Known gap:** the word after a bare `--project` is read as its id unless it names a command. That
  is a guess at ADR 26's reading of the flag, and it only decides which command's help is shown.
- **Known gap:** the interactive session has no commands and routes what is typed, `skills --help`
  as much as `skills`.
- **Known gap:** the usage line a command prints when its words do not fit its shape (`Usage: jli
  teach …`) is written apart from the table.
- **Known gap:** this record was written before ADRs 26 and 27 were merged. Until they are,
  `--project` is a word of the request and `JLI_QUIET` is ignored, though the help lists both.
