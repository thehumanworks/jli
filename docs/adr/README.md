# Architecture decision records

Each record states one decision: the context it was made in, the decision, and what follows from
it. When a decision changes, write a new record that supersedes the old one, and mark the old one
superseded rather than rewriting it.

Records 1 to 10 were written on 2026-10-02 from `HANDOVER-RUST.md`, the handover that guided the
Rust port (in git history at commit `68a4133`). Records 1 to 8 were decided while building the
TypeScript prototype; 9 and 10 are about the port. Every lesson the handover recorded as "already
paid for", and every known gap, sits in the record of the decision it belongs to.

| # | Decision |
| --- | --- |
| [1](0001-two-systems.md) | A decision model on every request, an agent only when it is unsure |
| [2](0002-brain-in-libsql.md) | The brain is a libSQL database with a fixed schema, and rows are never replaced |
| [3](0003-system-1.md) | System 1 answers three questions in one call, with frozen wording |
| [4](0004-language-models.md) | Language models are reached with one OpenAI-compatible HTTP call and strict JSON Schemas |
| [5](0005-routing-loop.md) | One routing loop, one confirmation rule, and a CLI that composes in pipes |
| [6](0006-learning-modes.md) | Three learning modes, and translated rows are verified by System 1 before they are kept |
| [7](0007-slot-templates.md) | Skills are templates with `{slot}` placeholders, filled cheapest first and always shell-quoted |
| [8](0008-corrections.md) | A "no" or a failed command is a lesson, but only a diagnosis says what kind |
| [9](0009-port-to-rust.md) | Port jli from TypeScript to Rust, keeping its behaviour and its brain |
| [10](0010-synchronous-rust.md) | The Rust version is synchronous, with async confined to the store |

## Known gaps, by record

- [2](0002-brain-in-libsql.md): the remote and replica modes have never run against a live remote
  database, and a replica over an existing local file is expected to fail.
- [6](0006-learning-modes.md): auto mode cannot learn from success alone, and the benchmark's
  paraphrases were written by its author.
- [7](0007-slot-templates.md): a glob given as a slot value is quoted and does not expand.
- [8](0008-corrections.md): a correction against a strongly matching seeded skill may not stick, and
  two correction flows have not been tested live.
