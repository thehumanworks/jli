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
| System 2 proposed a command and it ran | a new skill with the request as its first example | System 1 answers next time |
| You said yes to "Run it?" | the phrasing, as an example of that skill | that phrasing becomes more confident |
| You said no and typed the right command | a counter-example on the wrong skill, and the phrasing on the right one | it routes to your command next time |
| You ran `jli teach` or `jli fix` | a phrasing, or a counter-example | same as above, by hand |
| You wrote `dir=src` for a slot called `directory` | `dir` as another name for that slot | `dir=` is read with no model next time |

One command can hold many phrasings. Teaching a phrase for a command that a skill already runs adds
the phrase to that skill.

## Learning modes

Set with `jli config mode <manual|semi|auto>`, `JLI_MODE`, or `--mode=`. The default is `semi`.

| Mode | Who writes the matrix row | What a "no" does | What a failed command does |
| --- | --- | --- | --- |
| `manual` | you: phrase and command are stored as given | asks you to type the right command | nothing |
| `semi` | the translator generalises what you teach | asks you to type the right command | nothing |
| `auto` | the translator, from what happens | offers numbered alternatives; pick one or say what you expected | diagnoses it, and offers alternatives only if the command was wrong |

**Translated rows are checked, not trusted.** After the translator writes a row, System 1 is asked
to route your original phrase and up to eight phrasings that belong to other skills. The row is kept
only if your phrase lands on it and none of the others do. It gets one retry with that feedback, and
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
| `jli -n <request>` | dry run: show the command, execute and learn nothing |
| `jli -y <request>` | run without asking; never approves destructive commands (also `--yes`, `JLI_YES=1`) |
| `jli -Y <request>` | run without asking, destructive commands included (also `--yolo`, `JLI_YOLO=1`) |
| `jli config confidence <0..1>` | set the confidence threshold |
| `jli config mode <manual\|semi\|auto>` | set the learning mode |
| `jli --json <request>` | machine-readable outcome with captured output, for agents |
| `jli skills` | every skill, with learned phrasings and counter-examples |
| `jli stats` | routing mix, latency, and the learning curve |
| `jli history` | recent requests and how each was routed |
| `jli teach "<phrase>" -- <command>` | link a phrase to a command; repeat to add more phrasings |
| `jli fix` | the last command was wrong: unlearn it and ask the agent |
| `jli forget <label>` | delete a skill |

The decision trace goes to stderr and the command's output to stdout, so `jli` composes in pipes.

## Slots

Skills are templates such as `git log -n {count}` or `git commit -m {message}`. A value you state
is passed as written, by a parser and no model:

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

## Layout

| File | Role |
| --- | --- |
| `src/system1.rs` | builds the matrix from the database and asks Jev; the only module that uses `typesafe-jev` |
| `src/system2.rs` | the agent: structured proposals, free-text slot filling, and naming an unknown argument's slot |
| `src/translator.rs` | turns a taught phrase and command into a general matrix row |
| `src/learn.rs` | writes a row and verifies it against System 1, one thread per check |
| `src/suggest.rs` | auto mode: diagnoses a failure and offers alternatives |
| `src/llm.rs` | the one HTTP call every language model request goes through |
| `src/router.rs` | the loop: decide, escalate, confirm, run, write back |
| `src/store.rs` | the brain: skills and events in libSQL, behind a runtime that never leaves the module |
| `src/args.rs` | reads the arguments a request states: `name=value`, labels and quoted text |
| `src/slots.rs` | fills and shell-quotes `{slot}` values |
| `src/seed.rs` | the original hard-coded matrix, now only the seed for an empty brain |
| `src/exec.rs` | runs the steps with `bash -c`, watching stderr in auto mode |
| `src/cli.rs` | argument parsing, subcommands and the interactive session |
| `src/ui.rs` | the trace on stderr, and the one owner of stdin |
| `src/demo.rs` | the two-pass learning demo |
| `src/tests.rs` | the router and learning loop, offline, against fake models and a scripted prompt |
| `examples/bench-translator.rs` | manual versus semi on held-out paraphrases |
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

