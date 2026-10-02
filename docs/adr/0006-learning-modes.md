# 6. Three learning modes, and translated rows are verified by System 1 before they are kept

- Status: accepted
- Date: 2026-10-02
- Origin: the TypeScript prototype; kept unchanged by the Rust port

## Context

A taught pair, a phrase and a command, stored exactly as given generalises well for fixed
commands. It does badly for commands that carry a value: "show the last 3 commits" stored
literally does not help "give me the 7 most recent commits". A fast model can rewrite the pair into
a general row with slots ([ADR 7](0007-slot-templates.md)). But a model's row can also be too
broad, and steal phrasings that belong to other skills.

## Decision

| Mode | Who writes the row | On "no" | On a failed command |
| --- | --- | --- | --- |
| `manual` | the phrase and command, stored as given | the user types the right command | nothing |
| `semi` (default) | the translator generalises, System 1 verifies | the user types the right command | nothing |
| `auto` | the translator, from what happens | numbered options; pick one or state the expectation | diagnosed first ([ADR 8](0008-corrections.md)) |

The mode is resolved like the threshold: flag, `JLI_MODE`, the brain's setting, then `semi`.

**Learning a phrase**, in `src/learn.rs`:

1. If a known skill produces exactly this command for some slot values, link the phrase to it.
   This check is local and uses no model.
2. In manual mode, store the pair as given. The label is a slug of the command, the scenario is
   "the user wants: <phrase>", and the risk is `--risk` or `modifies`.
3. Otherwise, translate. If the translator says a known skill matches, believe it only if
   rendering that skill with the translator's values reproduces the command. Keep the translator's
   slots only if rendering its template reproduces the taught command and no slot value begins
   with `-`.
4. Write the row, then verify it with System 1. The phrase must route to the new row at or above
   the threshold. Up to eight phrasings of other skills (the two newest from each) must not route
   to it; a neighbour phrasing equal to the phrase being taught is skipped. The checks run
   concurrently.
5. On failure, delete the row and retry once, giving the translator the problem as feedback.
6. On a second failure, keep the steps and slots, and store the raw phrase as the scenario.
7. If the translator cannot be reached, fall back to manual mode and warn.

**The benchmark**, `examples/bench-translator.rs`, teaches the same ten commands to a manual brain
and a semi brain, then routes 30 held-out paraphrases through both. "Routed" means it reached the
taught skill above the threshold; "right" means the rendered command was exactly the expected one.

| Run | Fixed commands (21): manual / semi | Commands with a value (9): manual / semi | All (30): manual / semi |
| --- | --- | --- | --- |
| TypeScript prototype | 100% / 100% routed, 100% / 100% right | 22% / 100% routed, 0% / 100% right | 77% / 100% routed, 70% / 100% right |
| Rust, same cases | 100% / 100% routed, 100% / 100% right | 33% / 100% routed, 0% / 100% right | 77% / 100% routed, 70% / 100% right |
| Rust, file cases moved to `Cargo.toml`, `Cargo.lock`, `.gitignore` | 100% / 100% routed, 100% / 100% right | 0% / 100% routed, 0% / 100% right | 73% / 100% routed, 70% / 100% right |

The Rust runs were on 2026-10-02. Each made 194 System 1 requests (about $0.012) and a dozen
translator calls. In each, semi mode kept all ten translated rows.

## Consequences

- The gain is entirely in commands that carry a value, such as a count, a file or a port. A raw
  phrase already generalises well for fixed commands.
- Teaching costs about a second of translator time and up to nine System 1 calls. Teaching another
  value of a known template costs nothing.
- **Known gap:** auto mode cannot learn from success alone. `ls` succeeds when the user wanted
  `ls -ltr`.
- **Known gap:** the benchmark's paraphrases were written by the benchmark's author, and it has
  been run once per implementation plus once after its file cases moved.
