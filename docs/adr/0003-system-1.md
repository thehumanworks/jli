# 3. System 1 answers three questions in one call, with frozen wording

- Status: accepted
- Date: 2026-10-02
- Origin: the TypeScript prototype; the Rust port moved the call to the `typesafe-jev` crate

## Context

System 1 is TypeSafe's Jev model. It does not generate text. It answers typed questions about a
state with calibrated probabilities: a `noul` (yes or no), a `choice` (one label out of a set) and a
`score` (a level on an ordered scale). Facts about the API that shaped this design:

- `state`, `instructions` and every criterion accept a string, a JSON object, a JSON array or null.
- `confidence` is a statistic separate from the probability distribution.
- Probabilities are rounded to two decimals and may not sum to one.
- A `choice` accepts at most 255 labels. A `score` accepts 2 to 10 levels.
- `noul` is the probability of true. It is not a confidence.
- The models are `jev-latest` and `jev-preview`.

## Decision

**One call per request, three questions, one state.** The state is
`{ request, context: { directory, insideGitRepository, platform } }`, with the platform named
`darwin`, `linux` or `win32`. The questions are:

| Id | Type | Asks |
| --- | --- | --- |
| `isCommandRequest` | noul | whether fulfilling the request needs a command |
| `intentRisk` | score | read-only; modifies and can be undone; deletes, rewrites history or changes remote systems |
| `skill` | choice | which known command fulfils the request |

Each skill row becomes one `skill` criterion: `{ runs, scenario, examples?, doesNotApplyTo? }`, with
`examples` and `doesNotApplyTo` left out when they are empty. Last comes the label `none-of-these`,
whose criterion is `{ "scenario": "None of the listed commands does what the user asked" }`.

- **The wording is frozen.** The instructions, the criteria and the `none-of-these` wording in
  `src/system1.rs` were tuned against live behaviour. Change them only with a measurement: the
  benchmark ([ADR 6](0006-learning-modes.md)) and `jli demo`.
- **Route on the choice's `confidence`**, never on its probability.
- **Read risk from the distribution**, not the expected score: destructive when the probability of
  level 2 is 0.3 or more, modifies when levels 1 and 2 together reach 0.5, safe otherwise. The
  expected score would average a "safe or destructive" split into "modifies".
- **Cap the matrix at 200 labels.** With more skills than that, shortlist: 10 points for each word
  shared with the request, plus `ln(1 + uses)`, best first.
- File and branch slots are filled by a second, smaller `choice` among the real directory entries
  (up to 200) or git branches, plus `none-of-these` ([ADR 7](0007-slot-templates.md)).
- **Use the `typesafe-jev` crate** (our own, `thehumanworks/jevgrep`), and only in
  `src/system1.rs`. The question ids are constants shared by the request and the lookup of its
  answers. A missing or mistyped answer is an error.
- **Use interactive limits.** A 10 second timeout per attempt and two retries. The crate's
  defaults, 60 seconds and 8 retries, suit batch work and would hang the prompt.
- Build the client on first use, so that commands that make no call need no key. An auth error
  names `TYPESAFE_API_KEY`.

The wire format, for debugging:

```
POST https://api.typesafe.ai/v1/systemone
Authorization: Bearer $TYPESAFE_API_KEY

{ "model": "jev-latest",
  "state": { "request": "show me the newest files",
             "context": { "directory": "jli", "insideGitRepository": true, "platform": "darwin" } },
  "questions": {
    "isCommandRequest": { "type": "noul", "instructions": ["..."], "criteria": { "true": {}, "false": {} } },
    "intentRisk":       { "type": "score", "instructions": "...", "criteria": ["...", "...", "..."] },
    "skill":            { "type": "choice", "instructions": "...",
                          "criteria": { "<label>": { "runs": "...", "scenario": "..." }, "none-of-these": { "scenario": "..." } } } } }

{ "model": "jev-1.13.0",
  "answers": {
    "isCommandRequest": { "type": "noul", "noul": 0.66 },
    "intentRisk": { "type": "score", "score": 0.1, "confidence": 0.9, "probabilities": { "0": 0.9, "1": 0.1, "2": 0.0 } },
    "skill": { "type": "choice", "choice": "list-files-by-time", "confidence": 0.97,
               "probabilities": { "list-files-by-time": 0.98, "none-of-these": 0.02 } } },
  "usage": { "input_tokens": 0, "output_tokens": 0 } }
```

**Lesson paid for:** the `none-of-these` label is sensitive to wording. A strict instruction and a
long description pulled known requests away from the right skill.

## Consequences

- A request costs one call of about 250 ms however many skills there are. Jev evaluates the
  questions in parallel, and more rows cost more input tokens, not more time.
- Rewording an instruction or a criterion is a behaviour change, not a refactor.
- The trace shows the three most probable labels. The crate keeps wire order, so `jli` sorts them.
- The client is `Send + Sync` with a shared connection pool, so verification can ask nine
  questions at once from plain threads ([ADR 10](0010-synchronous-rust.md)).
- Tests need no network: `System1::fake` wraps `Client::with_transport`.
