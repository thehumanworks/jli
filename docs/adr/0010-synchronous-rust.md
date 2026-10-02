# 10. The Rust version is synchronous, with async confined to the store

- Status: accepted
- Date: 2026-10-02
- Origin: the Rust port ([ADR 9](0009-port-to-rust.md))

## Context

The `typesafe-jev` crate blocks. It uses `ureq`, has no async runtime, and its client is
`Send + Sync` with a shared connection pool. The CLI is a prompt-and-wait loop. The only place
with concurrency is verification, which asks System 1 up to nine questions at once
([ADR 6](0006-learning-modes.md)). The `libsql` crate, on the other hand, is async and needs Tokio.

## Decision

- **Write the app synchronously.** Verification uses `std::thread::scope`, one thread per System 1
  call.
- **Keep libSQL behind a current-thread Tokio runtime owned by `Brain`.** Every method blocks on
  it, and nothing async leaves `src/store.rs`. The alternative, `rusqlite`, was rejected because it
  drops the remote and replica modes ([ADR 2](0002-brain-in-libsql.md)).
- **Use `ureq` 3 with rustls** for the language model call. `typesafe-jev` already uses it, so
  there is one HTTP stack. Each call gets an agent with that call's timeout.
- **Use `serde` and `serde_json`, with `preserve_order`.** Schemas are `json!` literals, and replies
  are plain structs with `Option` for nullable fields.
- **Run commands with `std::process::Command` and `bash -c`.** When output is captured, stdin is
  closed and both streams are read concurrently. In auto mode, stderr is piped, forwarded as it
  arrives, and its last 4000 characters are kept. A command killed by a signal reports 128 plus
  the signal number, as a shell would.
- **Parse arguments by hand.** The grammar is small, and a free-form request does not suit a
  declarative parser.
- **Make stdin have one owner.** A `Prompt` trait (`read_line`, `confirm`, `ask`) is implemented by
  `Terminal`, the only reader of stdin. The session prompt and the confirmation prompt both go
  through it.
- **Build the models on first use,** so commands that make no model call need no keys.
- **Leave seams for tests.** `System1::fake` (over `Client::with_transport`),
  `Llm::with_transport` and a scripted `Prompt` let `cargo test` run the router and the learning
  loop with no network and no keys.
- **Make the crate a library with a thin binary,** so `examples/bench-translator.rs` can use the
  modules.

**Lesson paid for:** use one stdin reader. In the prototype, the session prompt and the
confirmation prompt each opened their own, and they collided.

## Consequences

- There is no async runtime outside the store, and blocking is fine in a CLI.
- A stalled call holds the prompt until its timeout: up to three attempts of 10 seconds for
  System 1, and two of 30 seconds for the agent.
- libSQL's remote and replica support brings a large dependency tree and a bundled SQLite, so a
  clean build takes minutes.
