# 9. Port jli from TypeScript to Rust, keeping its behaviour and its brain

- Status: accepted; done on 2026-10-02; acceptance check 7 superseded in part by
  [ADR 30](0030-taught-as-given.md), under which both teaches take `--mode=semi`, since `jli teach`
  now keeps the command as given
- Date: 2026-10-02

## Context

`jli` was proven as a TypeScript program running on Bun (commit `f0231d9`). A handover,
`HANDOVER-RUST.md` (commit `68a4133`), asked for a Rust binary named `jli` with the same behaviour,
and made the TypeScript the specification. The handover does not record why Rust was chosen.

## Decision

- Port module by module, keeping the file boundaries: `types`, `store`, `seed`, `context`,
  `system1`, `llm`, `system2`, `translator`, `learn`, `suggest`, `slots`, `exec`, `router`, `ui`,
  `demo`, and `index` as `cli`.
- Copy the system prompts, the System 1 question wording and the JSON Schemas character for
  character ([ADR 3](0003-system-1.md), [ADR 4](0004-language-models.md)).
- Keep the brain's schema unchanged, so each version opens the other's brains
  ([ADR 2](0002-brain-in-libsql.md)).
- Use the `typesafe-jev` crate for System 1, and write the app synchronously
  ([ADR 10](0010-synchronous-rust.md)).
- The port was done when:
  1. every command and flag worked the same way;
  2. brains were interchangeable in both directions;
  3. `jli demo` showed requests that needed System 2 on the first pass answered by System 1 on the
     second, in different words;
  4. the benchmark was ported and reproduced the recorded shape: no difference on fixed commands,
     a large difference on commands with a value;
  5. the acceptance checks below passed.
- Keep the TypeScript until all of that held, then remove it. It was removed on 2026-10-02, and it
  remains in git history.

## Verification

Run on 2026-10-02 against the live APIs, with a fresh brain (`JLI_DB_URL=file:…`) in a scratch git
repository. Checks 9 to 11 need a terminal and were driven with `expect`.

| # | Check | Result |
| --- | --- | --- |
| 1 | `jli -n show me files sorted by most recent change` | printed `ls -ltr`, ran nothing, exit 0 |
| 2 | `jli -n 'commit with message "first draft"'` | printed `git commit -m 'first draft'` |
| 3 | `jli how much is 2 + 2` | reported it is not a command, exit 0 |
| 4 | `jli stage everything`, with no terminal and no `--yes` | ran nothing, exit 3 |
| 5 | `jli -y delete all the log files` | proposed a destructive command and refused to run it; the files were untouched |
| 6 | `jli --json -y where am I` | valid JSON, `path` is `system1`, the directory is in `output` |
| 7 | `jli teach "show the last 3 commits" -- git log --oneline -n 3`, then `jli teach "last 10 commits please" -- git log --oneline -n 10` | the first created `git log --oneline -n {count}`; the second joined it with every API key unset, so no model was called |
| 8 | `jli -n give me the 7 most recent commits` | rendered `-n 7` |
| 9 | auto mode outside a git repository, a request routed to `git status` and approved | an `environment` diagnosis and no counter-example |
| 10 | auto mode, a command declined | three numbered options; picking one learned it and ran it |
| 11 | an interactive session | took two requests with confirmations, then left on ctrl-d |
| 12 | `jli demo` | exit 0; answered instantly 2 of 5, then 5 of 5; time spent deciding 12.5 s, then 1.4 s |
| 13 | a brain written by the TypeScript version, read by Rust, and the reverse | `skills`, `history`, `stats` and `config` byte-identical both ways, and on a brain written by both |

Also checked: `jli fix`, both as a dry run and for real; every path that makes no model call;
`--json` output byte-identical to the TypeScript's apart from timings; and `cargo test`, which runs
the router and the learning loop offline against fake models and a scripted prompt.

## Deliberate differences from the prototype

- Directory entries offered to the models are sorted by name. The prototype used whatever order
  Bun's `readdir` returned.
- A model reply that lacks a required field counts as unreadable and gets the retry. The prototype
  cast it and failed later.
- Unknown values in stored rows read leniently ([ADR 2](0002-brain-in-libsql.md)) instead of failing
  when displayed.
- The benchmark prints System 1's request count and estimated cost. `jli stats` does not, because
  that count only lives as long as one process.
- The benchmark's file cases name `Cargo.toml`, `Cargo.lock`, `.gitignore` and `README.md`, because
  `package.json`, `tsconfig.json` and `bun.lock` went with the TypeScript.

## Consequences

- No reference implementation remains. These records, the acceptance checks above and the test
  suite are the specification.
- Brains written by the prototype keep working on every host.
- After any change to routing, learning or storage, re-run the acceptance checks. They cost a few
  cents; `jli demo` makes about 15 calls.
