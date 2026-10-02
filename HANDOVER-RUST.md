# Handover: port `jli` to Rust

You are porting a working TypeScript proof of concept to Rust. The TypeScript in `src/` is the
specification. Read it before writing anything. This document tells you what the app does, what
must not change, the exact wire formats, and the things that were learned the hard way.

## What you are building

`jli` is a command-line tool. The user types a request in plain language, `jli` works out which
shell command fulfils it, asks for confirmation, runs it, and learns from the result.

```
$ jli show me the newest files
⚡ system 1 · jev-1.13.0 · 289 ms · 9 skills in the matrix
   list-files-by-time  ████████████████████  0.98
→ ls -ltr  risk: safe
? Run it? [Y/n]
```

Three models take part, and only the first is on the request path every time.

| Role | Model | Latency | When it runs |
| --- | --- | --- | --- |
| System 1, the decision model | TypeSafe `jev-latest` | about 250 ms | every request |
| System 2, the reasoning agent | `openai/gpt-5-mini` | 3 to 6 s | only when System 1 has no confident pick |
| Translator | `inception/mercury-2.5` | about 1 s | only when something is taught, declined, or fails |

The central idea: the decision matrix is a database table. Each row is a skill. Rows are rendered
into the criteria of a System 1 `choice` question on every request. Learning means writing to
those rows.

## Goal and definition of done

Produce a Rust binary named `jli` with the same behaviour as the TypeScript version.

1. Every command and flag listed under "CLI surface" works the same way.
2. The Rust binary opens a brain database created by the TypeScript version, and the reverse.
   Do not change the schema.
3. The two-pass demo (`jli demo`) shows requests that needed System 2 on the first pass being
   answered by System 1 on the second pass, in different wording.
4. The benchmark in `examples/bench-translator.ts` is ported and reproduces the shape of the
   recorded result: no difference on fixed commands, a large difference on commands with a value.
5. The acceptance checks at the end of this document pass.

Keep the TypeScript source in the repository until the Rust version passes all of the above.

## Source map

Port module by module. The file boundaries are sound and can become Rust modules as they are.

| File | Lines | Responsibility |
| --- | --- | --- |
| `src/types.ts` | 71 | `Skill`, `Slot`, `Risk`, `Path`, `Mode`, `BrainEvent` |
| `src/store.ts` | 338 | the brain: schema, skills, events, settings, sync |
| `src/seed.ts` | 73 | nine seed skills inserted into an empty brain |
| `src/context.ts` | 39 | cwd, git detection, directory entries, branches |
| `src/system1.ts` | 180 | builds the matrix, calls Jev, reads the answers. The only file that touches the TypeSafe SDK; in Rust, the only module that uses `typesafe-jev` |
| `src/llm.ts` | 149 | the single chat-completions call and shared JSON Schemas |
| `src/system2.ts` | 137 | agent: propose a skill, extract free-text slot values |
| `src/translator.ts` | 97 | turn a taught phrase and command into a general row |
| `src/learn.ts` | 157 | write a row and verify it against System 1 |
| `src/suggest.ts` | 87 | diagnose a failure, offer alternatives |
| `src/slots.ts` | 156 | fill, quote, render and match `{slot}` templates |
| `src/exec.ts` | 48 | run shell steps, optionally watching stderr |
| `src/router.ts` | 411 | the whole loop: decide, escalate, confirm, run, write back |
| `src/ui.ts` | 115 | trace output, bars, the shared stdin reader |
| `src/index.ts` | 330 | argument parsing, subcommands, interactive session |
| `src/demo.ts` | 97 | the two-pass learning demo |

**Port the system prompts verbatim.** The `INSTRUCTIONS` constants in `system2.ts`,
`translator.ts` and `suggest.ts`, and the question wording in `system1.ts`, were tuned against live
behaviour. Copy them character for character. Do the same for the JSON Schemas.

## Wire formats

The app makes two kinds of call. System 1 goes through the `typesafe-jev` Rust crate. The
language models have no SDK and use one plain HTTP request.

### System 1

**Use the `typesafe-jev` crate. Do not hand-roll this call.** It is our own crate
(`thehumanworks/jevgrep`, docs at https://docs.rs/typesafe-jev). It replaces `@typesafe-ai/sdk`,
which is imported only in `src/system1.ts`.

```toml
typesafe-jev = "0.2"
serde_json = "1"
```

Checked against version 0.2.0 (MIT or Apache-2.0, Rust 1.85). Read its README and
`examples/ask.rs` before coding.

| TypeScript | `typesafe-jev` |
| --- | --- |
| `new TypeSafeClient()` | `Client::from_env(Config { … })?`, reads `TYPESAFE_API_KEY` |
| `client.systemOne({ state, questions })` | `client.ask(&state, &questions)?` |
| the `questions` object | `Questions::new().with("id", question)`, or `insert` in a loop |
| `noul(instructions, { true, false })` | `Noul::new(instructions).yes(…).no(…)` |
| `choice(instructions, { label: description })` | `Choice::new(instructions, [(label, description), …])` |
| a label with a null description | `Choice::labels(…)`, or `.label(name)` on an existing choice |
| `score(instructions, [a, b, c])` | `Score::new(instructions, [a, b, c])` |
| `answers.skill` | `response.choice("skill")`: `choice`, `confidence`, `probabilities` |
| `answers.intentRisk.probabilities` | `response.score("intentRisk")`: `probabilities` is a `BTreeMap<u32, f64>` |
| `answers.isCommandRequest.noul` | `response.noul("isCommandRequest")`: `noul` |
| `result.model`, `result.usage` | `response.model`, `response.usage` |

Things to know about the crate:

- **It is blocking and has no async runtime.** It uses `ureq`. The client is `Send + Sync` with a
  shared connection pool, so concurrency is plain threads. This shapes the whole port. See
  "Suggested Rust shape".
- **The matrix is dynamic and that fits.** Build the skill question with `Choice::labels` or an
  empty `Choice`, then add each database row with `.option(label, content)`. Options keep the
  order they are added in.
- **Structured criteria use `Content`.** A `&str` converts with `into()`. A JSON object or array
  goes through `Content::try_from(json!({ "runs": …, "scenario": …, "examples": … }))`. Numbers,
  booleans and null are rejected.
- **Answer lookups return `Option`,** not `Result`. A missing or mistyped answer is `None`. Treat
  that as an error in the router.
- **Score probabilities are a `BTreeMap<u32, f64>`,** so `probabilities.get(&2)` is the lookup for
  the destructive level.
- **Choice probabilities keep wire order,** so sort them before showing the top three.
- **The defaults suit batch work, not an interactive CLI.** `Config::default()` has a 60 second
  timeout and 8 retries. Set roughly `timeout: 10s` and `max_retries: 2` to match the TypeScript
  behaviour, so a stalled call does not hang the prompt.
- **`base_url` is the full endpoint,** `https://api.typesafe.ai/v1/systemone`, not the origin.
- **Errors are typed:** `Auth`, `TokenLimit`, `InvalidRequest`, `InvalidConfig`, `Api`. Surface
  `Auth` with a message naming `TYPESAFE_API_KEY`.
- **Tests need no network.** `Client::with_transport` takes a closure from request body to
  `Reply`. Use it for the router's unit tests, and keep live calls to the acceptance checks.
- **Usage accounting is built in.** `client.usage()` gives requests, retries, tokens and
  `cost_usd()`. Consider showing it in `jli stats`. This is new relative to the TypeScript version.
- **There is no models listing call.** The TypeScript version does not use one either.
- Keep all use of the crate inside one `system1` module.

The raw wire format, for reference and for debugging:

```
POST https://api.typesafe.ai/v1/systemone
Authorization: Bearer $TYPESAFE_API_KEY
Content-Type: application/json
```

Request body:

```json
{
  "model": "jev-latest",
  "state": { "request": "show me the newest files",
             "context": { "directory": "jli", "insideGitRepository": true, "platform": "darwin" } },
  "questions": {
    "isCommandRequest": { "type": "noul", "instructions": ["..."], "criteria": { "true": {}, "false": {} } },
    "intentRisk":       { "type": "score", "instructions": "...", "criteria": ["...", "...", "..."] },
    "skill":            { "type": "choice", "instructions": "...", "criteria": { "<label>": { "runs": "...", "scenario": "...", "examples": [], "doesNotApplyTo": [] }, "none-of-these": { "scenario": "..." } } }
  }
}
```

Response body:

```json
{
  "model": "jev-1.13.0",
  "answers": {
    "isCommandRequest": { "type": "noul", "noul": 0.66 },
    "intentRisk": { "type": "score", "score": 0.1, "confidence": 0.9, "probabilities": { "0": 0.9, "1": 0.1, "2": 0.0 } },
    "skill": { "type": "choice", "choice": "list-files-by-time", "confidence": 0.97, "probabilities": { "list-files-by-time": 0.98, "none-of-these": 0.02 } }
  },
  "usage": { "input_tokens": 0, "output_tokens": 0 }
}
```

Facts about this API that shaped the design:

- `state`, `instructions` and every criterion accept a string, a JSON object, a JSON array or null.
- `confidence` is a separate statistic from the probability distribution. Route on `confidence`.
- Probabilities are rounded to two decimals and may not sum to one.
- A `choice` accepts at most 255 labels. A `score` accepts 2 to 10 levels.
- `noul` is the probability of true. It is not a confidence.
- Models are `jev-latest` and `jev-preview`.
- The TypeScript SDK retried twice on 408, 429 and 5xx with a 10 second timeout per attempt.

### Language models

One OpenAI-compatible call, defined in `src/llm.ts`:

```
POST {base}/chat/completions
Authorization: Bearer {key}
```

```json
{
  "model": "inception/mercury-2.5",
  "messages": [ { "role": "system", "content": "..." }, { "role": "user", "content": "<JSON input>" } ],
  "reasoning": { "enabled": false },
  "response_format": { "type": "json_schema", "json_schema": { "name": "...", "strict": true, "schema": {} } }
}
```

The reply is JSON text in `choices[0].message.content`. Strip a surrounding code fence before parsing.

Endpoint resolution, in order:

1. `JLI_LLM_API_KEY` with `JLI_LLM_BASE_URL`
2. `OPENROUTER_API_KEY`, with `OPENROUTER_BASE_URL` or `https://openrouter.ai/api/v1`
3. `AI_GATEWAY_API_KEY` with `https://ai-gateway.vercel.sh/v1`

| Caller | Model | Reasoning | Timeout | Attempts |
| --- | --- | --- | --- | --- |
| Agent `propose`, `extract` | `JLI_AGENT_MODEL`, default `openai/gpt-5-mini` | `{ "effort": "low" }` | 30 s | 2 |
| Translator | `JLI_TRANSLATOR_MODEL`, default `inception/mercury-2.5` | `{ "enabled": false }` | 8 s | 2 |
| Suggester | same as translator | `{ "enabled": false }` | 10 s | 2 |

Retry on 408, 429, 500, 502, 503, 504, on timeouts, and on an unparseable reply.

In strict mode every property must be listed in `required`, and optional fields are expressed as
`"type": ["string", "null"]`. In Rust, model these as `Option<T>` with serde.

## The brain

libSQL, which has a first-party Rust crate. One client covers three deployments.

| Setting | Meaning |
| --- | --- |
| default | local file at `~/.jli/brain.db` |
| `JLI_DB_URL=file:/path` | local file elsewhere |
| `JLI_DB_URL=libsql://…` | remote database shared by all hosts |
| `JLI_SYNC_URL=libsql://…` | local replica that syncs with the remote |
| `JLI_DB_TOKEN` | auth token for the remote |

Schema, to be kept exactly:

```sql
CREATE TABLE IF NOT EXISTS skills (
  label            TEXT PRIMARY KEY,
  steps            TEXT NOT NULL,              -- JSON array of shell strings with {slot} placeholders
  scenario         TEXT NOT NULL,
  examples         TEXT NOT NULL DEFAULT '[]', -- JSON array, capped at 16, newest kept
  counter_examples TEXT NOT NULL DEFAULT '[]', -- JSON array, capped at 16
  slots            TEXT NOT NULL DEFAULT '[]', -- JSON array of { name, kind, description, default? }
  risk             TEXT NOT NULL DEFAULT 'safe',   -- safe | modifies | destructive
  origin           TEXT NOT NULL DEFAULT 'seed',   -- seed | agent | taught
  uses             INTEGER NOT NULL DEFAULT 0,
  created_at       TEXT NOT NULL,
  last_used_at     TEXT,
  created_host     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, host TEXT NOT NULL, cwd TEXT NOT NULL,
  query TEXT NOT NULL, path TEXT NOT NULL, label TEXT, confidence REAL, command TEXT,
  exit_code INTEGER, accepted INTEGER NOT NULL DEFAULT 0, learned TEXT,
  ms_system1 INTEGER, ms_system2 INTEGER
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

- `meta` holds `seeded` and the settings `setting:confidence` and `setting:mode`.
- `events.path` is one of `system1`, `system2`, `taught`, `none`, plus the legacy `system1-confirmed`.
- Upserting a skill updates `steps`, `scenario`, `slots` and `risk` and keeps `examples`,
  `counter_examples` and `uses`. Never replace the row.
- Example comparison is case-insensitive and trimmed.

## The routing loop

`src/router.ts` is the heart of the app. Preserve this order.

1. **Decide.** One System 1 call with three questions. If there are more than 200 skills,
   shortlist by word overlap with the request, then by use count.
2. **Route.**
   - Chosen label is a skill and `confidence >= threshold`: use it.
   - No skill chosen and `isCommandRequest < 0.1`: say it is not a command and stop.
   - Otherwise: ask System 2, passing known skills, rejected skills and any clarification.
3. **Fill slots**, cheapest method first: a value supplied by the agent, a number or quoted
   string found in the request, a System 1 `choice` among real files or branches at confidence 0.6
   or above, the agent, then the slot's default.
4. **Confirm.** Always ask "Run it?" unless `--yes`. A destructive command asks even with `--yes`.
   With no terminal and no `--yes`, nothing runs and the exit code is 3.
5. **On no**, run the correction flow for the current mode.
6. **Run** each step with `bash -c`, stopping at the first failure.
7. **In auto mode, on failure**, diagnose before learning anything.
8. **Write back**, then log an event. A dry run writes nothing and logs nothing.

Constants: threshold default 0.5, not-a-command below 0.1, phrasing not stored when confidence is
0.95 or above, 16 examples per skill, 200 labels in the matrix, 8 neighbour checks.

Risk is read from the `score` distribution, not the expected score. Destructive when the
probability of level 2 is 0.3 or more. Modifies when levels 1 and 2 together are 0.5 or more.
A known skill uses its stored risk. A new proposal uses the more severe of the agent's risk and
the intent risk.

### Learning modes

| Mode | Who writes the row | On "no" | On a failed command |
| --- | --- | --- | --- |
| `manual` | phrase and command stored as given | user types the right command | nothing |
| `semi` (default) | translator generalises, System 1 verifies | user types the right command | nothing |
| `auto` | translator, from what happens | numbered options; pick one or state the expectation | diagnosed first |

Resolution order for both mode and threshold: flag, environment variable, brain setting, default.

### Learning a phrase, in `src/learn.ts`

1. If any known skill produces this exact command for some slot values, link the phrase to it.
   This check is local and uses no model.
2. In manual mode, store the pair as given.
3. Otherwise translate. Keep the translator's slots only if substituting its slot values back
   into its steps reproduces the taught command, and no slot value begins with `-`.
4. Write the row, then verify with System 1: the phrase must route to the new row at or above
   the threshold, and up to eight phrasings from other skills must not route to it. Skip any
   neighbour phrasing equal to the phrase being taught.
5. On failure, delete the row and retry once with the problem as feedback.
6. On a second failure, keep the steps and slots and store the raw phrase as the scenario.
7. If the translator is unreachable, fall back to manual and warn.

### Corrections

- Choosing a different command removes the phrase from the wrong skill's examples and adds it as
  a counter-example.
- Choosing the same command with a different slot value does neither. The skill was right.
- A stated expectation re-enters the loop with `agentOnly`, the clarification, and the rejected skill.
- In auto mode a failure gets one of three verdicts. Only `wrong_command` and `wrong_value` lead
  to a correction. `environment` explains and still records the route as correct.

## CLI surface

```
jli <request>                         route, run, learn
jli                                   interactive session (needs a terminal)
jli skills | stats | history
jli teach "<phrase>" [--risk=…] -- <command>
jli fix                               unlearn the last executed route and ask the agent
jli forget <label>
jli config [confidence <0..1> | mode <manual|semi|auto>]
jli sync
jli demo
jli help

-n, --dry-run   -y, --yes   -q, --quiet   --json   --agent
--confidence=N  --mode=M    --risk=R
```

- A subcommand is recognised only when it is the first word and the arguments fit its shape.
  Otherwise the whole line is a request.
- The trace goes to stderr. The command's own output goes to stdout. This must hold so `jli`
  composes in pipes.
- `--json` captures command output and prints one JSON object matching `Outcome` in `router.ts`.
- Exit codes: the command's own code when it ran, 0 for a dry run or a non-command, 3 when
  blocked, 1 on error.
- Colour is off when `NO_COLOR` is set or stderr is not a terminal.

## Lessons already paid for

Each of these cost a debugging round. Do not rediscover them.

- **The "none" label is sensitive to wording.** A strict instruction and a long description
  pulled known requests away from the right skill. Keep the neutral wording in `system1.ts`.
- **Do not gate on `isCommandRequest` at 0.5.** It scores question-form requests low. "where am I"
  scored 0.37 while the skill choice was 0.99. It is used only below 0.1 with no skill chosen.
- **Use one stdin reader.** The session prompt and the confirmation prompt each opened their own
  and collided. In Rust, own stdin in one place and pass a handle.
- **Mercury must run with reasoning disabled.** With reasoning on it stalled for 9 to 16 seconds.
- **Mercury is rejected by the Vercel gateway on this account** because of a zero-data-retention
  policy. The translator needs OpenRouter.
- **`gpt-5-mini` occasionally hangs on OpenRouter.** Hence the 30 second timeout and the retry.
- **Models turn flags into slots.** `date -I` came back as `date {format}`. The guard that
  rejects slot values beginning with `-` is required.
- **A failed command is not a wrong route.** `git status` outside a repository exits 128 and was
  the right command.
- **Placeholders must not match shell syntax.** `{name}` is a slot, `${name}` is not, and `{}`
  in `find -exec` is not. See `PLACEHOLDER` in `slots.ts`.
- **Shell-quote every slot value** unless it matches `^[A-Za-z0-9_\-.,/:=@%+]+$`.
- **Never replace a skill row.** An early `INSERT OR REPLACE` wiped learned examples.

## Suggested Rust shape

These are suggestions. Prefer what the crate ecosystem makes simple.

- `typesafe-jev` for System 1, as described above.
- **Write the app synchronously.** `typesafe-jev` blocks, the CLI is a prompt-and-wait loop, and
  nothing needs an event loop. The one place with concurrency is verification, which fans out up
  to nine System 1 calls. Use `std::thread::scope` there.
- `ureq` 3 with rustls for the one language model call. It is what `typesafe-jev` already uses,
  so no second HTTP stack is pulled in. Set the per-call timeouts from the table above.
- `serde` and `serde_json`. Write the JSON Schemas as `serde_json::json!` literals copied from the
  TypeScript, and the replies as plain structs.
- **Storage needs a decision.** The `libsql` crate is async and requires `tokio`. Two options:
  keep `libsql` behind a small current-thread runtime confined to the `store` module, which
  preserves the cloud and sync modes; or use `rusqlite` and drop them. Prefer the first, because
  sharing one brain across hosts is a stated goal. Do not let async leak out of `store`.
- `std::process::Command` with `bash -c`. For auto mode, pipe stderr, forward it live, and keep
  the last 4000 characters.
- Argument parsing can be hand-written. The grammar is small and the free-form request does not
  suit a declarative parser.
- The TypeScript SDK inferred answer types from question literals. `typesafe-jev` looks answers up
  by id with `noul("…")`, `choice("…")` and `score("…")`, each returning `None` if the id or type
  is wrong. Keep the three question names as constants shared by the request and the lookup.

## Known gaps to carry forward, not fix silently

- The cloud and sync storage modes have never been run against a live remote database.
- Correcting a phrase that strongly matches a seeded skill may not stick after one correction.
- Auto mode cannot learn from success alone. `ls` succeeds when the user wanted `ls -ltr`.
- The benchmark paraphrases were written by the benchmark's author, and it was run once.
- "State what you expected" after a failed command, and manual-mode correction, were not
  re-tested after the last refactor.

## Acceptance checks

Run these against the Rust binary with a fresh brain (`JLI_DB_URL=file:/tmp/…`) in a scratch git
repository. Never test in a repository you care about.

1. `jli -n show me files sorted by most recent change` prints `ls -ltr` and executes nothing.
2. `jli -n 'commit with message "first draft"'` prints `git commit -m 'first draft'`.
3. `jli how much is 2 + 2` reports that it is not a command and exits 0.
4. `jli stage everything` with no terminal and no `--yes` runs nothing and exits 3.
5. `jli -y delete all the log files` proposes a destructive command and refuses to run it.
6. `jli --json -y where am I` prints valid JSON with `path` equal to `system1` and the directory in `output`.
7. `jli teach "show the last 3 commits" -- git log --oneline -n 3` creates a skill whose steps contain a slot.
   Then `jli teach "last 10 commits please" -- git log --oneline -n 10` adds a phrasing to the same skill
   and makes no model call.
8. `jli -n give me the 7 most recent commits` renders `-n 7`.
9. In auto mode outside a git repository, a request routed to `git status` and approved gets an
   "environment" diagnosis and no counter-example.
10. In auto mode, declining a command shows numbered options, and picking one learns it and runs it.
11. An interactive session survives a confirmation prompt and accepts the next request.
12. `jli demo` exits 0 and reports more requests answered instantly on the second pass.
13. A brain written by the TypeScript version is readable by the Rust version: `jli skills` lists the same rows.

## Environment

| Variable | Purpose |
| --- | --- |
| `TYPESAFE_API_KEY` | System 1 |
| `OPENROUTER_API_KEY`, `OPENROUTER_BASE_URL` | language models; injected by fnox on the author's machine |
| `AI_GATEWAY_API_KEY` | fallback language model endpoint |
| `JLI_LLM_API_KEY`, `JLI_LLM_BASE_URL` | any other compatible endpoint |
| `JLI_AGENT_MODEL`, `JLI_TRANSLATOR_MODEL` | model overrides |
| `JLI_DB_URL`, `JLI_SYNC_URL`, `JLI_DB_TOKEN` | brain location |
| `JLI_CONFIDENCE`, `JLI_MODE` | per-shell settings |

Both APIs cost money per call. The demo makes about 15 calls and the benchmark about 100.
