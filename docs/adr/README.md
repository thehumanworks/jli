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
| [11](0011-stated-arguments.md) | Arguments stated in the request are read by a parser, before and instead of any model |
| [12](0012-slot-aliases.md) | A slot answers to several names, and a new name is worked out once and kept |
| [13](0013-learning-fixes.md) | One row per command, the shape of a request, this host's own fix, and failures that are not the command's fault |
| [14](0014-shell-integration.md) | Steps such as `cd` are handed to the calling shell, and paths are expanded before they are quoted |
| [15](0015-user-scope.md) | What is learned belongs to a user, a skill is shared by copy, and parallel writers lose nothing |
| [16](0016-measuring-the-matrix.md) | The matrix is measured against the brain's own evidence, optimised only where the measurement improves, and every new row is checked against its closest skills |
| [18](0018-agent-contract.md) | A contract for agent callers: pinned plans, run in order, and corrections without a terminal |

## Known gaps, by record

- [2](0002-brain-in-libsql.md): the remote and replica modes have never run against a live remote
  database, and a replica over an existing local file is expected to fail.
- [6](0006-learning-modes.md): auto mode cannot learn from success alone, and the benchmark's
  paraphrases were written by its author.
- [7](0007-slot-templates.md): a glob given as a slot value is quoted and does not expand.
- [8](0008-corrections.md): a correction against a strongly matching seeded skill may not stick, and
  two correction flows have not been tested live.
- [11](0011-stated-arguments.md): a stated value has no escapes, and hidden requests were measured on a handful
  of live cases, not on the benchmark.
- [12](0012-slot-aliases.md): one alias cannot be removed by hand, an unknown name is only noticed as
  `name=value`, and a skill with no slots still blocks on any argument.
- [13](0013-learning-fixes.md): the duplicate check was verified offline only, a rejected skill can
  come back as a second row, hiding is verbatim and case-sensitive, and hidden examples were
  measured on the demo and six requests, not on a benchmark built for it.
- [14](0014-shell-integration.md): handed-over steps that are not tried first count as a clean
  exit, the trial runs in bash whatever the calling shell is, fish has only been tested offline,
  jli's own session hands its steps over only when it ends, and `~user` is not expanded.
- [15](0015-user-scope.md): an older jli ignores `--user-id` and `JLI_USER_ID` (and routes the flag
  as words of a request), two writers teaching the same new command at once get two skills, a share
  cannot be undone except by `forget`, and the safe writes have not run against a live remote.
- [16](0016-measuring-the-matrix.md): a stored example is ground truth, so one taught by mistake cannot
  be dropped through the gate; the gate checks a skill's competitors, not the whole matrix; rewrites are
  not repeatable; examples carry no context, and a replay in a directory that is gone runs in an
  inferred one; a `jli fix` made after other requests counts as a request of its own.
- [18](0018-agent-contract.md): a dry run needs a writable brain; plan rows are never deleted; a plan
  does not notice that its skill changed after it was made; a proposal learned through `jli run`
  skips the System 1 check; `jli run` refuses a plan with a `cd` even under the shell function; a plan
  blocked on an unknown argument runs without it once its slots are filled; `corrections` is shown
  nowhere; and the claim, like the rest of the cloud setup, has run only against local files.
