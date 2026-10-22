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

## Live benchmarks

`cargo run --release --example bench-translator` compares learning modes on held-out phrases.
`cargo run --release --example bench-context` measures project-specific phrasing. These call live
models and can cost money; they are separate from the offline correction demo. See the source
headers and the repository README for setup and interpretation.
