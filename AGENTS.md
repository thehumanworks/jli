# Working on jli

`jli` routes a plain-language request to a shell command and learns from what happens. Read
`README.md` first, then `docs/adr/README.md`. The ADRs are the specification: there is no reference
implementation, and most rules below are a decision recorded there.

## Before you commit

```sh
mise install          # hk, pkl and ast-grep at the pinned versions (run `mise trust` in a new worktree)
hk install            # once per clone: installs the git hooks
mise run check        # format, clippy, ast-grep rules and the offline tests
mise run fix          # apply what the linters can fix
```

The hooks run the same checks: `pre-commit` runs the linters and fixes what it can, `pre-push` adds
`cargo test`. Do not skip them with `--no-verify`. A commit that fails them is not done.

| Check | Command | What it holds |
| --- | --- | --- |
| Format | `cargo fmt --check` | `rustfmt.toml`: 125 columns |
| Lints | `cargo clippy --all-targets -- -D warnings` | `[lints]` in `Cargo.toml`: no `unsafe`, no `unwrap()`, `panic!`, `todo!` or `dbg!` outside tests |
| Architecture | `ast-grep scan --error` | `rules/*.yml`: one rule per boundary an ADR draws |
| Behaviour | `cargo test` | the router and the learning loop, offline, against fake models |

## Rules that are not negotiable

- **Tests are offline.** `src/tests.rs` fakes System 1, the language models and the person at the
  keyboard. A test never calls a network API, and an unexpected language model call panics. Every
  change to routing, learning, slots or storage comes with a test there or in the module's own
  `tests`.
- **Live runs use a scratch brain.** Set `JLI_DB_URL=file:<scratch dir>/brain.db` and run inside a
  scratch directory. Never touch `~/.jli/brain.db`. Unset `JLI_YES` and `JLI_YOLO` first: with
  `JLI_YOLO=1` in the environment a destructive command runs without asking.
- **Storage is additive** ([ADR 2](docs/adr/0002-brain-in-libsql.md)). Never `INSERT OR REPLACE` a
  skill, never drop or rename a column, and write new fields so that a row without them reads as
  before (`#[serde(default, skip_serializing_if = …)]`). Several hosts and versions share one brain.
- **The System 1 wording is frozen** ([ADR 3](docs/adr/0003-system-1.md)). Changing a question, a
  criterion or the state sent to Jev is a behaviour change that needs the benchmark and the
  acceptance checks re-run.
- **Prompts and schemas are measured** ([ADR 4](docs/adr/0004-language-models.md)). When you change
  one, say in the ADR what you ran and what it returned.
- **The trace goes to stderr, the command's output to stdout**
  ([ADR 5](docs/adr/0005-routing-loop.md)). The `--json` outcome keeps its field names and order;
  new fields are appended.
- **Slot values are always shell-quoted** ([ADR 7](docs/adr/0007-slot-templates.md)), and a value the
  user states is passed as written, by no model ([ADR 11](docs/adr/0011-stated-arguments.md)).
- **Everything outside `src/store.rs` is synchronous** ([ADR 10](docs/adr/0010-synchronous-rust.md)).

## Recording a decision

A change in behaviour gets a record in `docs/adr/`, numbered next in sequence, in the shape of the
existing ones: Context, Decision, Consequences, with known gaps stated and anything measured live
given with its date. When a decision changes, write a new record and mark the old one superseded in
its Status line; do not rewrite it. Add the record to the table in `docs/adr/README.md`, and its
known gaps to the list below the table. Keep `README.md` true: the tables under "How it works",
"Commands" and "Layout" describe the code as it is.

## Style

- Match the surrounding code: short doc comments that say why, no commented-out code, names in
  plain words.
- Errors are `anyhow::Result`. Outside tests there is no `unwrap()`; `expect("why this holds")` is
  for invariants only.
- Messages to the user are sentences, in the voice of the existing ones.
- Prose in ADRs and the README is plain: say what happens, give the example, state the gap.

## After changing routing, learning or storage

Re-run the acceptance checks in [ADR 9](docs/adr/0009-port-to-rust.md) against the live APIs with a
scratch brain. They cost a few cents. Checks 9 to 11 need a terminal (`expect`).

## Branches

Work lands on `dev` through pull requests from feature branches; `dev` is merged to `main` by pull
request.
