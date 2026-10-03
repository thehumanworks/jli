# jli

Say what you want in plain language. `jli` runs the right command, and learns how you say it.

```
$ jli show me the newest files
⚡ system 1 · jev-1.13.0 · 289 ms · 9 skills in the matrix
   list-files-by-time  ████████████████████  0.98
→ ls -ltr  risk: safe
```

## How it works

Two models, the way people think about fast and slow thinking.

- **System 1** is TypeSafe's `jev` decision model. One call, about 250 ms, answers three typed
  questions against the request: a `choice` over every known skill, a `noul` on whether a command is
  wanted at all, and a `score` on how risky the intent is.
- **System 2** is a reasoning agent, Mercury 2.5 by default (about 2 s). It is only called when
  System 1 has no confident answer. It proposes a reusable skill, not a one-off command.
- **The translator** is a fast model, Mercury 2.5 by default (under 1 s). It never sits on the
  request path. It rewrites what you teach into a general matrix row, and in auto mode it diagnoses
  failures and suggests alternatives.
- **The brain** is a SQLite database. Each row is a skill. The rows are rendered into the `choice`
  criteria on every request, so the decision matrix *is* the database.

Learning is writing to that database:

| What happened | What is stored | Effect |
| --- | --- | --- |
| System 2 proposed a command and it ran, with exit 0 or a failure that was not the command's fault ([ADR 13](docs/adr/0013-learning-fixes.md)) | a new skill with the request as its first example, its values hidden, once System 1 has checked it; a command a skill already runs adds the phrasing to that skill instead | System 1 answers next time |
| You said yes to "Run it?" | the phrasing, as an example of that skill | that phrasing becomes more confident |
| You said no and typed the right command | a counter-example on the wrong skill, and the phrasing on the right one | it routes to your command next time |
| You ran `jli teach` or `jli fix` | a phrasing, or a counter-example | same as above, by hand |
| You wrote `dir=src` for a slot called `directory` | `dir` as another name for that slot | `dir=` is read with no model next time |

One command can hold many phrasings. Teaching a phrase for a command that a skill already runs adds
the phrase to that skill. If that skill runs only the one command, as one taught in manual mode
does, and the translator finds a value in it, the skill is made general in place and keeps its
phrasings: teaching `git log --oneline -n 3` again turns it into `git log --oneline -n {count}`, with
3 as the count's default. Two jli processes teaching one command at once also end up with one skill.
See [ADR 19](docs/adr/0019-one-row-per-command.md).

## Learning modes

Set with `jli config mode <manual|semi|auto>`, `JLI_MODE`, or `--mode=`. The default is `semi`.

| Mode | Who writes the matrix row | What a "no" does | What a failed command does |
| --- | --- | --- | --- |
| `manual` | you: phrase and command are stored as given | asks you to type the right command | nothing |
| `semi` | the translator generalises what you teach | asks you to type the right command | nothing |
| `auto` | the translator, from what happens | offers numbered alternatives; pick one or say what you expected | diagnoses it, and offers alternatives only if the command was wrong |

**Translated rows are checked, not trusted.** After the translator writes a row, and before it is
stored, System 1 is asked to route your original phrase with the row in the matrix, then up to eight
phrasings of the skills closest to it: those System 1 ranked just behind it, then those that share
the most words. The row is kept only if your phrase lands on it, none of the others do, and none
that routed home without it stops doing so. A skill the agent proposes gets the same check while
its command runs. It gets one retry with that feedback, and
then falls back to your raw phrase. Slots are kept only if filling them back in reproduces the exact
command you taught.

**A failure is not a wrong route.** `git status` outside a repository fails, and it was still the
right command. In auto mode the translator reads stderr and gives one of three verdicts: wrong
command, wrong value, or an environment problem. Only the first two lead to a correction.

`cargo run --release --example bench-translator`, run from the repository root, compares manual
and semi on held-out paraphrases. One run on 2026-10-02, ten commands, thirty requests written by
the author of the benchmark:

| | manual routed | manual right command | semi routed | semi right command |
| --- | --- | --- | --- | --- |
| fixed commands (21 requests) | 100% | 100% | 100% | 100% |
| commands with a value (9 requests) | 0% | 0% | 100% | 100% |
| all (30 requests) | 73% | 70% | 100% | 100% |

The gain is entirely in commands that carry a value, such as a count, a file or a port. A raw phrase
already generalises well for fixed commands. A run costs about 200 System 1 requests (about $0.012)
and a dozen translator calls. Earlier runs, including the TypeScript prototype's, are in
[ADR 6](docs/adr/0006-learning-modes.md).

## Measuring the matrix

The learning curve in `jli stats` counts a confident wrong route as a win. `jli eval` measures
whether routes are right, from the brain's own evidence and with the real System 1:

- every stored example is routed with itself held out of the matrix, and should come home;
- every accepted request that was not declined or corrected later is routed again, and should
  reach the skill that was accepted.

Each phrasing comes **home**, is **stolen** by another skill, or **escalates** below the threshold.
The report lists, per skill, the least confident phrasings, even those that came home, and who
stole what. The number of System 1 calls and their price are printed before anything is spent,
about $0.00006 a call. `jli stats` shows the trend, and how often a request was right the first
time, with declined and corrected attempts counted.

`jli eval --optimise` ranks skills by expected gain: how much each is used and how often the
person accepted its routes, times how far its phrasings fall short of coming home. System 2 then
rewrites the top rows. A rewrite is kept only if the measured score of the skill and its
competitors rises and no other skill loses a phrasing. Nobody is asked: the measurement is the
approval. Each kept change is recorded, and `jli eval undo` puts it back. With `-n` nothing at all
is written. On a scratch brain of 16 skills on 2026-10-03, a measurement took 22 System 1 calls
($0.0014). An optimisation run took 80 System 1 and 6 System 2 calls, kept one rewrite of six, and
cut the steal rate from 9% to 5%: the phrasing it freed
now escalates to the agent instead, so the share that came home stayed at 91%. See [ADR 16](docs/adr/0016-measuring-the-matrix.md).

## Confirmation

`jli` shows the command and asks "Run it?" every time, unless you approve up front. There are two levels, each a flag or an environment variable set to `1` or `true`:

- `-y`, `--yes` or `JLI_YES`: run without asking, except destructive commands, which still ask.
- `-Y`, `--yolo` or `JLI_YOLO`: run without asking, destructive commands included.

Answer no and it asks what it should have run: type the command, type `?` to ask the agent, or
press enter to skip.

System 1's pick is used when its confidence reaches the threshold, 0.5 by default. Below that the
agent is asked. Set the threshold with `jli config confidence 0.7` (stored in the brain, so shared
across hosts), `JLI_THRESHOLD=0.7` (`JLI_CONFIDENCE` also works), or `--confidence=0.7` for one run.

## Try it

```sh
export TYPESAFE_API_KEY=…      # System 1
export OPENROUTER_API_KEY=…    # System 2 and the translator (here it is injected by fnox)

cargo build --release          # the binary is target/release/jli
cargo install --path .         # optional: puts `jli` on your PATH
jli demo                       # watch a fresh brain learn in a throwaway git repo
jli which branch am I on
jli                            # interactive session
jli help
cargo test                     # offline: System 1 and the language models are faked
```

The demo asks five things, then asks them again in different words. In a real run, three requests
that needed the agent the first time (4 to 6 s each) were answered by System 1 in about 250 ms the
second time, and the time spent deciding fell from 12.5 s to 1.4 s.

## Commands

| Command | What it does |
| --- | --- |
| `jli <request>` | route, run, learn |
| `jli -n <request>` | dry run: show the command, execute and learn nothing, and keep it as a plan for `jli run` |
| `jli -y <request>` | run without asking; never approves destructive commands (also `--yes`, `JLI_YES=1`) |
| `jli -Y <request>` | run without asking, destructive commands included (also `--yolo`, `JLI_YOLO=1`) |
| `jli config confidence <0..1>` | set the confidence threshold |
| `jli config mode <manual\|semi\|auto>` | set the learning mode |
| `jli --json <request>` | machine-readable outcome with captured output, for agents; an error is one JSON object too |
| `jli run <planId> [<planId> …] [name=value …]` | run stored plans exactly as shown, in order, with no model call; stop at the first that fails |
| `jli skills` | every skill, with learned phrasings and counter-examples |
| `jli stats` | routing mix, latency, the learning curve, how often routes were right first time, and the `jli eval` trend |
| `jli eval [<label>] [--limit N]` | route every stored example with itself held out, and replay accepted requests; priced before it spends |
| `jli eval --optimise` | the agent rewrites the weakest rows; a rewrite is kept only if the measurement improves |
| `jli eval -n [--optimise]` | measure, and show the rewrites and their effect, but write nothing |
| `jli eval undo [<label>]` | put back what the last optimisation changed |
| `jli history` | recent requests and how each was routed |
| `jli teach "<phrase>" -- <command>` | link a phrase to a command; repeat to add more phrasings |
| `jli teach --global "<phrase>" -- <command>` | the same, from inside a project, for every project |
| `jli fix` | the last command run here was wrong: unlearn it and ask the agent |
| `jli fix <eventId>` | the same for one of your events, from any directory (an event made in a project, from inside that project) |
| `jli fix <eventId> -- <command>` | that was wrong, this is right: learn it as a "no" and the command at the prompt would, and return a plan of it (`--run` runs it) |
| `jli fix <eventId> --expected "<words>"` | unlearn it and ask the agent with what you expected; returns a plan (`--run` runs it) |
| `jli forget <label>` | delete a skill; inside a project only the project's own, `--global` for a global one |
| `jli project` | the project this directory is in, from the nearest `.jlirc`, and how many skills are its own |
| `jli project init [<id>]` | make this directory a project: what jli learns here stays with it |
| `jli generalize [<label>]` | the agent proposes how to make project skills global; each is written only once you approve (`--all-projects`, `--approve-all`, `-n`) |
| `jli shell-init [zsh\|bash\|fish]` | print the shell function that lets `cd`, `export` and `source` change your shell |
| `jli --user-id=<id> …` | any command, as another user with their own skills, history and settings (also `JLI_USER_ID`) |
| `jli share <label> --to <user>` | give another user a copy of one of your skills |

The decision trace goes to stderr and the command's output to stdout, so `jli` composes in pipes.

## For agents

An agent drives `jli` with `--json` and no terminal. It can review a route, then run exactly that:

```sh
jli --json -n how many words are in notes.txt   # {"commands": ["wc -w notes.txt"], "planId": 1, "blockedCode": "dry_run", …}
jli --json run 1                                 # [{"ran": true, "exitCode": 0, "stdout": "6 notes.txt\n", "eventId": 1, …}]
jli --json run 2 3 4                             # in order; after the first failure the rest say "not_run"
jli --json run 5 message="fix the parser"        # a value the plan was missing (see "missing" in its outcome)
jli --json fix 6 -- ls -la                       # event 6 was wrong, ls -la was right: learned, and a plan of it returned
```

A dry run, and a run that stopped for lack of a yes or a value, stores a plan for an hour. `jli run`
asks no model and routes nothing; running a plan by id approves it as `--yes` does, so a destructive
plan still needs `--yolo`, and a plan runs once. Every outcome says why nothing ran as a code
(`blockedCode`), gives the event and plan ids, the phrase that is learned, each slot's value and
source, System 1's runners-up, stdout and stderr apart, and auto mode's diagnosis. An error is
`{"error": "<kind>", "errorMessage": …}`, and a failure after the command ran is reported beside
`ran: true`, never instead of it. See [ADR 18](docs/adr/0018-agent-contract.md).

## Commands that change your shell

jli runs commands in a shell of its own, so on its own a `cd` or an `export` would change nothing
you can see. Install the shell function once:

```sh
eval "$(jli shell-init zsh)"        # in ~/.zshrc; bash: the same with bash, in ~/.bashrc
jli shell-init fish | source        # in ~/.config/fish/config.fish
```

Then a step such as `cd`, `pushd`, `export`, `unset`, `source` or `alias`, and every step after it,
is handed to your shell, which runs it once jli exits: `jli go to src` leaves you in `src`. jli
passes it through a temp file, not stdout, so output still goes down a pipe, and the function
returns jli's exit code, or the status of what your shell ran. A plain `cd` to a directory that does
not exist fails in jli, with the shell's own message, before anything is handed over. A ctrl-c while
jli runs leaves no temp file behind.
Without the function such a step is not run and nothing is learned from it: jli says how to install
the function in the shell it was started from (a bash started from zsh gets `~/.bashrc`) and exits 3,
and `--json` lists the steps in `callerShell` for an agent to run itself.
See [ADR 14](docs/adr/0014-shell-integration.md) and [ADR 23](docs/adr/0023-shell-fixes-and-number-words.md).

## Slots

Skills are templates such as `git log -n {count}` or `git commit -m {message}`. A flag is never a
value: `cd -` is a skill of its own, not `cd {directory}` with `-`. A value you state is passed as
written, by a parser and no model:

```sh
jli commit message="fix the parser"            # name=value, for the slot called message
jli commit this with a message: 'fix the parser'   # the slot's name as a label
jli commit with message: fix the parser        # unquoted, a label takes the rest of the request
jli commit "fix the parser"                    # a quoted string goes to the next open text slot
jli tag this name=v1.2.0 message="first cut"   # several arguments, each to its own slot
```

The trace shows `{message} = fix the parser (from request)`. A stated value is hidden from routing,
so the words of a commit message cannot change which command is picked.

You do not have to remember what a slot is called. Each slot answers to other names as well, such
as `dir` and `path` for `{directory}`. The model that writes a skill suggests some. When you use a
name the skill does not know, `msg=wip` for `{message}`, the agent is asked once which slot it
means; the value stays yours, and after the run the name is kept, so next time the parser reads it.
A name that fits no slot stops the run and lists the command's arguments. `jli skills` shows each
slot as `{name}` in the command, with its other names below it. See
[ADR 12](docs/adr/0012-slot-aliases.md).

Anything not stated is filled by the cheapest method that works: a number in the request, a System 1
`choice` among the real files or branches, the agent, and last the slot's default. See
[ADR 11](docs/adr/0011-stated-arguments.md).

A file value has a leading `~` and any `$NAME` or `${NAME}` expanded by jli before it is quoted, so
`jli list dir=~/notes` runs `ls /Users/you/notes`, and the trace says
`(from request, ~/notes expanded)`. Nothing is ever run to expand a value, an unset variable is left
as written, and a text value such as a commit message is never expanded. A path named anywhere in
the request, not only in the working directory, can be picked for a file slot. See
[ADR 14](docs/adr/0014-shell-integration.md).

What is learned is the shape of a request, not the instance: every value the command ran with is
hidden in the stored phrasing wherever it appears as a whole word, so `how many lines are in
notes.txt` is kept as `how many lines are in …`. See [ADR 13](docs/adr/0013-learning-fixes.md).

## One brain for every host

Storage goes through libSQL, so the same code runs on a local file or a cloud database.

```sh
# default: local file at ~/.jli/brain.db
export JLI_DB_URL=libsql://your-db.turso.io   # every host reads and writes one cloud brain
export JLI_DB_TOKEN=…

# or keep a local replica and sync it, for local-speed reads
export JLI_SYNC_URL=libsql://your-db.turso.io
```

What one host learns, every host knows. Each skill and event records the host it came from.
The cloud modes are wired but have not been run against a live Turso database yet.

## Users

One brain can hold several users, so an agent can learn its own way of using jli without changing
yours. This is isolation, not authentication.

```sh
jli --user-id=agent teach "count the todo items" -- grep -c '^- ' todo.md   # or JLI_USER_ID=agent
jli share count-lines-matching-pattern --to default   # as agent: give the default user a copy
```

Without `--user-id` or `JLI_USER_ID` the user is `default`, which is everything a brain held before
users existed. A user sees, routes against, teaches, fixes and forgets only their own skills; their
history, stats and settings are their own; a new user starts from the seed skills with the built-in
settings. `jli share` copies one skill as it is now, with its phrasings and slot names but not its
counter-examples, and refuses if the other user already has that label or that command. Several
jli processes can write one brain at once without losing a lesson, and two that teach the same
command at once get one skill ([ADR 19](docs/adr/0019-one-row-per-command.md)). An older jli on the same brain
sees only the default user. See [ADR 15](docs/adr/0015-user-scope.md).

## Projects

"Run the tests" is `cargo test` in one repository and `npm test` in the next. Make each one a
project, and what jli learns in one stays there:

```sh
jli project init                     # writes .jlirc: project = shop-api-4k2x9q (commit it)
jli teach "run the tests" -- cargo test     # kept for this project
jli generalize                       # the agent proposes which project skills could be global; you approve each
```

A project is named by the id in the nearest `.jlirc` at or above the working directory (up to
`$HOME`), never by a path, so every checkout is the same project. Inside one, System 1 sees the
project's skills and your global ones together, and a project skill wins a label both hold. A new
skill is the project's; a phrasing or a counter-example for a global skill is kept as the project's
own lesson about it, so correcting `cargo test` to `npm test` in one project never reaches another.
`teach --global` and `forget --global` reach the global skills from inside a project. Settings stay
yours; history, stats, and the event and plan ids that `jli fix` and `jli run` take are the
project's. `jli generalize` checks each proposal with System 1 in
the project, among the global skills and against your other projects, and writes it only when you
say yes. Without a `.jlirc` nothing changes. See [ADR 17](docs/adr/0017-project-scope.md).

## Layout

| File | Role |
| --- | --- |
| `src/system1.rs` | builds the matrix from the database and asks Jev; the only module that uses `typesafe-jev` |
| `src/system2.rs` | the agent: structured proposals, free-text slot filling, naming an unknown argument's slot, rewriting a weak row, and proposing how a project skill could be global |
| `src/translator.rs` | turns a taught phrase and command into a general matrix row |
| `src/learn.rs` | writes a row once System 1 has verified it, or makes a literal skill general in place, and settles a skill the agent proposed |
| `src/eval.rs` | measures the matrix against its own examples and history, optimises it, and checks a new row against its closest skills |
| `src/suggest.rs` | auto mode: diagnoses a failure and offers alternatives |
| `src/llm.rs` | the one HTTP call every language model request goes through |
| `src/router.rs` | the loop: decide, escalate, confirm, run, write back; and a stored plan through the same confirmation and write-back |
| `src/plan.rs` | plans: what a route pins, and `jli run` |
| `src/store.rs` | the brain: skills, events, settings, plans and corrections per user and project in libSQL, behind a runtime that never leaves the module |
| `src/project.rs` | finds, reads and writes the `.jlirc` that names a project |
| `src/generalize.rs` | `jli generalize`: checks the agent's proposal for a project skill and writes it once approved |
| `src/args.rs` | reads the arguments a request states: `name=value`, labels and quoted text |
| `src/slots.rs` | fills, expands and shell-quotes `{slot}` values, and matches a command back to its template |
| `src/shell.rs` | the shell function, which steps it must run in your shell, and which shell jli is running under |
| `src/seed.rs` | the original hard-coded matrix, now only the seed for an empty brain |
| `src/exec.rs` | runs the steps with `bash -c`, watching stderr in auto mode and for a new proposal, tries builtins before they are handed over, and reads the process table to find the shell jli was started from |
| `src/cli.rs` | argument parsing, subcommands, the interactive session, and errors as JSON |
| `src/ui.rs` | the trace on stderr, and the one owner of stdin |
| `src/demo.rs` | the two-pass learning demo |
| `src/tests.rs` | the router and learning loop, offline, against fake models and a scripted prompt |
| `src/tests/eval_tests.rs` | `jli eval`, the optimiser, undo, and the check a new row gets, offline |
| `examples/bench-translator.rs` | manual versus semi on held-out paraphrases, and with `--forgetting` the forgetting rate |
| `examples/bench-context.rs` | one phrase taught to two commands in two projects: whether a correction in one undoes the other |
| `docs/adr/` | the design decisions, and the lessons and known gaps behind them |

## How it talks to the world

The app makes two kinds of HTTP call and uses one database. It is synchronous throughout.

- **Language models:** `POST {base}/chat/completions` in the OpenAI-compatible shape, with a
  `response_format` of type `json_schema`, sent with `ureq`. Schemas are plain JSON Schema objects
  in the source. `JLI_LLM_BASE_URL` with `JLI_LLM_API_KEY` is used when both are set, then
  OpenRouter when `OPENROUTER_API_KEY` is set, then the Vercel AI Gateway.
- **System 1:** `POST https://api.typesafe.ai/v1/systemone` with `{ model, state, questions }`,
  through the `typesafe-jev` crate, with a 10 second timeout and two retries.
- **Storage:** libSQL through its first-party crate. It is async, so `store.rs` owns a
  current-thread Tokio runtime and blocks on it; nothing outside that module is async.

Models are set with `JLI_AGENT_MODEL` (default `inception/mercury-2.5`) and `JLI_TRANSLATOR_MODEL`
(default `inception/mercury-2.5`).

## Contributing

`AGENTS.md` has the rules for changing this code, for people and for coding agents. `mise install`
then `hk install` sets up the git hooks: format, strict clippy, the `ast-grep` architecture rules in
`rules/`, and the offline tests before a push. `mise run check` runs all of it.

## Design decisions

The reasons behind all of this are in [docs/adr](docs/adr/README.md): why there are two systems,
why the prompts are frozen, why rows are never replaced, how slots are quoted, and what is known
not to work yet. `jli` started as a TypeScript prototype; [ADR 9](docs/adr/0009-port-to-rust.md)
records the port, and the acceptance checks to re-run after changing routing, learning or storage.

