# 1. A decision model on every request, an agent only when it is unsure

- Status: accepted
- Date: 2026-10-02
- Origin: the TypeScript prototype; kept unchanged by the Rust port ([ADR 9](0009-port-to-rust.md))

## Context

`jli` takes a request in plain language, works out which shell command fulfils it, asks before
running it, and learns from the result. A reasoning model can answer any request, but it takes 3
to 6 seconds, which is too slow to sit in front of every command. A classifier answers in about
250 ms, but it can only pick among options it is given.

## Decision

Three models take part, and only the first is on the request path every time.

| Role | Model | Latency | When it runs |
| --- | --- | --- | --- |
| System 1, the decision model | TypeSafe `jev-latest` | about 250 ms | every request |
| System 2, the reasoning agent | `openai/gpt-5-mini` | 3 to 6 s | only when System 1 has no confident pick |
| Translator | `inception/mercury-2.5` | about 1 s | only when something is taught, declined, or fails |

**The decision matrix is a database table.** Each row is a skill: the shell steps it runs, a
scenario saying when it applies, the phrasings accepted for it and the phrasings rejected for it.
On every request the rows are rendered into the criteria of one System 1 `choice` question
([ADR 3](0003-system-1.md)). Learning means writing to those rows:

| What happened | What is stored |
| --- | --- |
| System 2 proposed a command and it ran | a new skill, with the request as its first example |
| The user said yes to a route | the phrasing, as an example of that skill |
| The user said no and gave the right command | a counter-example on the wrong skill, the phrasing on the right one |
| `jli teach` or `jli fix` | a phrasing, or a counter-example |

System 2 writes reusable skills, with anything specific to the request turned into a `{slot}`
([ADR 7](0007-slot-templates.md)), because what it returns is taught to System 1. The translator
never sits on the request path.

## Consequences

- Once System 1 knows a task, the task is answered in about 250 ms. The agent is paid for once per
  new task. `jli demo` measures this: in the Rust run of 2026-10-02, three tasks that needed the
  agent (about 4 s each) were answered by System 1 on the second pass, in different words, and the
  time spent deciding fell from 12.5 s to 1.4 s.
- What `jli` has learned can be read and edited as rows: `jli skills`, `jli forget`.
- A `choice` question accepts at most 255 labels, so the matrix is capped and shortlisted
  ([ADR 3](0003-system-1.md)).
- Both APIs cost money per call. The demo makes about 15 calls. One benchmark run made 194 System 1
  requests and a dozen translator calls.
