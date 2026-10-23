# Examples

## Durable corrections

From the repository root:

```sh
cargo run --quiet --example durable-corrections
```

[Source](durable-corrections.rs). Rust/Cargo and the project dependencies are required; the demo
itself is offline and needs no API keys or sign-in. It creates and removes its own temporary brain,
uses fake model transports, and runs only `printf corrected` through the real correction flow.
The other routes are dry runs. It does not open your personal brain or model sessions.

The scripted person rejects a confident wrong command and supplies the right one. Ordinary later
lessons evict the original example from the bounded list, but the explicit binding keeps winning.
The demo also inspects the binding, reopens the database, and revokes it. Every result below is
checked; a mismatch exits with an error.

Expected stdout:

```text
Offline fixture: System 1 always chooses the wrong greeting at confidence 1.0.
Before correction: printf wrong (system1, dry run)
Reject wrong command; type printf corrected. Output: corrected
Stored binding: right-greeting; rejected: wrong-greeting; source: prompt
After 1 later lessons: printf corrected (binding, no model call)
After 10 later lessons: printf corrected (binding, no model call)
After 50 later lessons: printf corrected (binding, no model call)
Original bounded example has expired; the binding remains.
After database reopen: printf corrected (binding, no model call)
After revocation: printf wrong (system1, dry run)
All checks passed. This demonstrates persistence, not live model quality.
```

This demonstrates the exact-phrase correction contract, not paraphrase accuracy or live model
quality. See [ADR 32](../docs/adr/0032-durable-corrections.md) for scope and limitations. With the
CLI, `jli bindings` inspects the same records and `jli bindings remove <id>` revokes one.

## Fixed-word targets

From the repository root, with dependencies already installed:

```sh
cargo run --offline --quiet --example fixed-targets
cargo run --offline --quiet --example fixed-targets -- --strict
cargo run --offline --quiet --example fixed-targets -- --oracle --strict
```

[Source](fixed-targets.rs), [held-out cases](fixtures/fixed-target-cases.json), and
[injected choices](fixtures/fixed-target-choices.json). This is an executable characterisation of
the [ADR 30 fixed-word gap](../docs/adr/0030-taught-as-given.md#consequences), not a fix or a live
benchmark. It needs no credentials, uses a temporary brain with fake transports, and **executes no
commands**. Saved plans are inspected, never run. No personal brain or sessions are opened.

Default stdout:

```text
Offline synthetic adversarial choices; no commands executed, no live model measured.
OK: origin | wanted git pull origin | planned git pull origin
WRONG: upstream | wanted git pull upstream | planned git pull origin
OK: incidental-upstream | wanted git pull origin | planned git pull origin
WRONG: negated-origin | wanted git pull upstream | planned git pull origin
OK: implicit-origin | wanted git pull origin | planned git pull origin
WRONG: arbitrary-remote | wanted git pull orbital | planned git pull origin
OK: main | wanted git switch main | planned git switch main
WRONG: release | wanted git switch release | planned git switch main
OK: source-branch | wanted git switch main | planned git switch main
OK: private-preview | wanted mise run deploy-preview | planned mise run deploy-preview
WRONG: shared-preview | wanted mise run deploy-preview-shared | planned mise run deploy-preview
OK: negated-team | wanted mise run deploy-preview | planned mise run deploy-preview
CONTROL: literal skill refuses remote=upstream (unknown_argument)
CONTROL: explicit teaching keeps git pull origin (binding, no model call)
CONTROL: stated template value keeps git pull upstream (binding, no model call)
Held-out commands: 7 correct, 5 wrong; 3 boundary controls passed.
Characterisation completed; this is not a live routing-quality score.
```

The default exits 0 when the harness completes, **even with wrong commands**. `--strict` exposes
the unresolved gap as exit 1 with `fixed-target acceptance failed: 5 wrong commands`. `--oracle`
replaces the injected choices with the author-labelled correct skill, a perfect-choice control:
all twelve lines say `OK`, the summary is `12 correct, 0 wrong`, and `--oracle --strict` exits 0.
These are synthetic counts by construction, not model accuracy estimates. The two characterisation
tests run in `cargo test --workspace`; a future behavior change should update their baseline
intentionally, not preserve the wrong-command count.

The real `eval::measure` judges held-out labels, then the real router renders and saves plans whose
commands are compared separately with expected commands. Cases are absent from all stored examples
and counter-examples. They are author-written rather than independently sampled; the fixture fixes
the meaning of `upstream` to the remote literally named upstream, and makes origin the default for
the implicit request. It supplies no actual Git configuration. Explicit-binding and named-argument
controls run separately and are not counted as held-out paraphrase improvements.

Why stop at a harness? Skills have literal commands and prose scenarios, but no structured mapping
between natural-language target roles and fixed arguments. Requiring the command's target word
would reject the implicit request; rejecting any other target word would reject `pull upstream
changes from origin` and `switch from release to main`. Preview aliases need meaning, not token
overlap. A personal explicit teaching must also win even when it contradicts this corpus's default
interpretation. No general routing guard or prompt change is justified by these forced choices.

A future guard must avoid wrong-target plans while preserving the matching, implicit, negated,
incidental-mention, and explicit-teaching cases. This exact-command harness currently treats
escalation as an unexpected model call; a guard proposing safe refusal or a second-model check must
add separate refusal/escalation assertions and account for false refusals. Live quality, prevalence,
latency and paraphrase tradeoffs require a separately authorised scratch evaluation with held-out
requests and recorded real decisions. No live API was called for this iteration.

## Live benchmarks

`cargo run --release --example bench-translator` compares learning modes on held-out phrases.
`cargo run --release --example bench-context` measures project-specific phrasing. These call live
models and can cost money; they are separate from the offline correction demo. See the source
headers and the repository README for setup and interpretation.
