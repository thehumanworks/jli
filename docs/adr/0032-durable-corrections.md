# 32. An explicit phrase correction outranks the decision model

- Status: accepted
- Date: 2026-10-22
- Origin: issue #4
- Supersedes in part: [ADR 1](0001-two-systems.md), on System 1 choosing every request;
  [ADR 2](0002-brain-in-libsql.md), on the tables; [ADR 8](0008-corrections.md), on corrections
  being only examples; [ADR 18](0018-agent-contract.md), with the additive `binding` path and
  `binding_unavailable` blocked code; [ADR 21](0021-hand-edits.md), on forgetting a bound phrase.

## Context

Examples and counter-examples are bounded at sixteen and are evidence for System 1. A confident
wrong model can ignore an explicit correction, even before a later lesson evicts that evidence.
The user's choice needs to survive independently of the model's ability to generalise it.

## Decision

`phrase_bindings` is an additive table keyed by user, project and phrase. Its JSON body holds the
chosen skill, rejected skill identities, source (`teach`, `fix`, `prompt` or `forget-phrase`),
whether the choice came through an interactive prompt, and time. Ordinary accepted routes and
examples do not create bindings. Existing brains are not backfilled from ambiguous old examples.

- `jli teach`, a typed or numbered correction, and `jli fix -- <command>` bind the phrase after
  the existing learning path has stored its skill. They still teach examples for paraphrases.
- An unresolved correction rejects the old skill durably. Both System 1's matrix and the agent's
  proposal checks exclude it. A wrong-value correction does not reject the skill.
- A correction that asks the agent binds its replacement only when accepted and learned. A saved
  correction plan carries the decision it answered; running it still executes its pinned command
  once, with no model call, but cannot replace a newer correction or revive a revoked one.
- Routing looks up the phrase before System 1. A binding uses the existing slot, risk,
  confirmation, execution and plan machinery, and reports `path: "binding"`. `--agent` explicitly
  skips positive bindings as it skips System 1; durable rejections still apply.

The key is exactly `args::phrase(query, &[])`, the existing pre-routing parser with stated values
hidden. It introduces no fuzzy matching or new System 1 wording. Quoting, spelling and whitespace
remain significant. A fixed command with no slots also requires the original query, so a changed
quoted value cannot silently run the old literal. Once that command is made general in place, its
slots can take stated values normally. This does not address ADR 30's fixed-word target confusion.

A rule belongs to its user and project. Global rules are visible in projects; a project's rule for
the same key takes precedence, while inherited rejections remain excluded unless explicitly bound
again. A target pins its label, original scope, creation time and command. It cannot become a
project shadow or a recreated label. A compatible literal-to-template upgrade keeps the binding;
a different command, missing skill or incompatible literal value blocks with `binding_unavailable`.
Rejections follow skill identity through generalisation.

`jli bindings` lists visible rules and their IDs, scopes and source. `--json` includes all stored
metadata, including target commands and timestamps. `jli bindings remove <id>` removes the whole
rule in the selected scope; `--global` is required to remove an inherited global rule. Removing a
project rule exposes any global rule again. `-n` makes no change. Examples remain after revocation.
`forget-phrase` turns a matching binding into a rejection too, even if its example has expired.

Writes use the store's existing compare-and-swap pattern. Concurrent rejections accumulate, and
concurrent explicit positive choices resolve to the last committed complete choice. A plan's
conditional write compares both rule ID and body, so delete-and-recreate does not revive it.
No skill or old table is replaced. Inspection, revocation and bound literal routes need no model
credentials. The existing saved-plan unknown-argument contract is unchanged.

`jli eval` continues to measure the unassisted matrix, including leave-one-out and optimisation.
Its report adds repeated corrections per 100 judged requests in the latest 2,000 scoped events:
judged requests are the existing attempt groups with an accepted or corrected event; each correction
after the first for a hidden phrase is a repeat. This is observed history, not a model quality
estimate. Direct teaching has no judged event. `jli stats` counts bindings separately and includes
them in the share answered without an agent.

## Consequences and verification

Offline fixtures cover correction retention after 1, 10 and 50 lessons, database reopen and fresh
CLI processes; an adversarial confident System 1 and repeated rejected agent proposal; wrong values
and shell quoting; users, projects, shadows, missing/recreated skills, compatible generalisation,
revocation, evicted examples, older correction plans, concurrent database connections and concurrent
CLI writers. The existing evaluation and pinned-plan suites remain in the full check.

The process architecture rule allows only the new CLI integration-test file to launch fixture
processes. Production boundaries and model wording are unchanged.

- Live API acceptance checks from ADR 9 were not run: this task requires fixtures and excludes
  runtime credentials. No live latency or model-quality improvement is claimed.
- Remote/replica concurrency remains unverified; these tests use disposable local databases.
- Older jli versions ignore the new table and do not enforce bindings. They cannot parse a plan's
  new `binding` path. Hosts need this version for the new guarantee.
- Exact keys do not bind paraphrases, alternate quoting, or inferred argument values. Those still
  rely on ordinary examples. A bound template may still ask models for unstated slot values.
- Skill creation timestamps are the existing identity mechanism; the rule also checks command
  compatibility. This is scoped preference storage, not authentication or a security boundary.
- Skill learning and rule creation are separate writes. A crash between them can leave a learned
  example without the binding; retrying the explicit lesson establishes it. The CLI only reports
  success after both complete.
