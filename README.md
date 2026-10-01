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
- **System 2** is an OpenAI model. It is slow (3 to 6 s) and is only called when System 1 has no
  confident answer. It proposes a reusable skill, not a one-off command.
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

`bun examples/bench-translator.ts` compares manual and semi on held-out paraphrases. One run, ten
commands, thirty requests written by the author of the benchmark:

| | manual routed | manual right command | semi routed | semi right command |
| --- | --- | --- | --- | --- |
| fixed commands (21 requests) | 100% | 100% | 100% | 100% |
| commands with a value (9 requests) | 22% | 0% | 100% | 100% |
| all (30 requests) | 77% | 70% | 100% | 100% |

The gain is entirely in commands that carry a value, such as a count, a file or a port. A raw phrase
already generalises well for fixed commands.

## Confirmation

There is one rule. `jli` shows the command and asks "Run it?" every time, unless you pass `-y`.
Destructive commands ask even with `-y`. Answer no and it asks what it should have run: type the
command, type `?` to ask the agent, or press enter to skip.

System 1's pick is used when its confidence reaches the threshold, 0.5 by default. Below that the
agent is asked. Set the threshold with `jli config confidence 0.7` (stored in the brain, so shared
across hosts), `JLI_CONFIDENCE=0.7`, or `--confidence=0.7` for one run.

## Try it

```sh
export TYPESAFE_API_KEY=…      # System 1
export OPENROUTER_API_KEY=…    # System 2 and the translator (here it is injected by fnox)

bun run demo                   # watch a fresh brain learn in a throwaway git repo
bun run build                  # compiles a standalone binary to dist/jli
bun link                       # optional: puts `jli` on your PATH
jli which branch am I on
jli                            # interactive session
jli help
```

The demo asks five things, then asks them again in different words. In a real run, three requests
that needed the agent the first time (4 to 6 s each) were answered by System 1 in about 250 ms the
second time, and the time spent deciding fell from 16.4 s to 1.4 s.

## Commands

| Command | What it does |
| --- | --- |
| `jli <request>` | route, run, learn |
| `jli -n <request>` | dry run: show the command, execute and learn nothing |
| `jli -y <request>` | run without asking; never approves destructive commands |
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

Skills are templates such as `git log -n {count}` or `wc -l {file}`. Values are filled by the
cheapest method that works: a number or quoted string found in the request, a System 1 `choice`
among the real files or branches, the slot's default, and only then the agent.

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
| `src/system1.ts` | builds the matrix from the database and makes the typed `systemOne` call |
| `src/system2.ts` | the agent: structured proposals and free-text slot filling |
| `src/translator.ts` | turns a taught phrase and command into a general matrix row |
| `src/learn.ts` | writes a row and verifies it against System 1 |
| `src/suggest.ts` | auto mode: diagnoses a failure and offers alternatives |
| `src/llm.ts` | the one HTTP call every language model request goes through |
| `src/router.ts` | the loop: decide, escalate, confirm, run, write back |
| `src/store.ts` | the brain: skills and events in libSQL |
| `src/slots.ts` | fills and shell-quotes `{slot}` values |
| `src/seed.ts` | the original hard-coded matrix, now only the seed for an empty brain |
| `src/demo.ts` | the two-pass learning demo |
| `examples/original-matrix.ts` | the original proof of concept, untouched |
| `examples/bench-translator.ts` | manual versus semi on held-out paraphrases |

## Porting

There is no model SDK to port. The app makes two kinds of HTTP call and uses one database.

- **Language models:** `POST {base}/chat/completions` in the OpenAI-compatible shape, with a
  `response_format` of type `json_schema`. Schemas are plain JSON Schema objects in the source.
  OpenRouter is used when `OPENROUTER_API_KEY` is set, the Vercel AI Gateway otherwise, or any
  compatible endpoint through `JLI_LLM_BASE_URL` and `JLI_LLM_API_KEY`.
- **System 1:** `POST https://api.typesafe.ai/v1/systemone` with `{ model, state, questions }`.
  The TypeScript SDK is a thin wrapper around this one endpoint and is only imported in
  `src/system1.ts`.
- **Storage:** libSQL, which has a first-party Rust crate.

Models are set with `JLI_AGENT_MODEL` (default `openai/gpt-5-mini`) and `JLI_TRANSLATOR_MODEL`
(default `inception/mercury-2.5`).

