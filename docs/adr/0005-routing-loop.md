# 5. One routing loop, one confirmation rule, and a CLI that composes in pipes

- Status: accepted; superseded in part by [ADR 13](0013-learning-fixes.md), which also learns a
  new proposal that failed for a reason that is not the command's fault; and by
  [ADR 14](0014-shell-integration.md), which hands steps such as `cd` to the calling shell and
  appends `callerShell` and `handedOff` to the `--json` outcome;
  and by [ADR 16](0016-measuring-the-matrix.md), which checks a proposed skill with System 1 before it is stored, logs a
  declined route as its own event, and counts the suggester in `msSystem2`;
  and by [ADR 18](0018-agent-contract.md), under which a dry run stores a plan (and still writes no
  skill and logs no event), `jli run` and `jli fix <eventId>` join the CLI, the `--json` outcome gains
  fields, and an error under `--json` prints one JSON object;
  and by [ADR 25](0025-command-help.md), under which a line with `--help` or `-h` before `--` is
  jli's help and never a request, and each command has its own help;
  and by [ADR 27](0027-quiet.md), under which `JLI_QUIET=1` is `--quiet`, `--quiet` keeps the
  warnings, and a question shows what it asks about when the trace is hidden
- Date: 2026-10-02
- Origin: the TypeScript prototype; kept unchanged by the Rust port

## Decision

**The loop**, in `src/router.rs`, runs in this order:

1. **Decide.** One System 1 call ([ADR 3](0003-system-1.md)).
2. **Route.**
   - A skill was chosen with `confidence` at or above the threshold: use it.
   - No skill was chosen and `isCommandRequest` is below 0.1: say it is not a command, and stop.
   - Otherwise ask System 2, passing the known skills, any rejected skills and any clarification.
     It may pick a known skill, propose a new one, or say that nothing should run.
3. **Fill slots** ([ADR 7](0007-slot-templates.md)). A slot with no value blocks the run.
4. **Confirm.** Always ask "Run it?" unless `--yes` (or `JLI_YES=1`) is given. A destructive command asks even with
   `--yes`; only `--yolo` (or `-Y`, or `JLI_YOLO=1`) approves those too. With no terminal and neither
   flag, nothing runs and the exit code is 3.
5. **On no**, run the correction flow for the current mode ([ADR 8](0008-corrections.md)).
6. **Run** each step with `bash -c`, stopping at the first failure.
7. **In auto mode, on failure**, diagnose before learning anything ([ADR 8](0008-corrections.md)).
8. **Write back**, then log an event. A dry run writes nothing and logs nothing.

**Risk.** A known skill uses its stored risk. A new proposal uses the more severe of the agent's
risk and System 1's reading of the request's risk.

**Write-back.** A new proposal becomes a skill only if it exited 0. For a known skill the use is
recorded, and the phrasing is stored as an example only if the route taught something: it came
from System 2, or System 1 was less than 0.95 sure. The route must also have been vouched for. A
human yes vouches for it even if the command failed; `--yes` needs a clean exit.

| Constant | Value |
| --- | --- |
| Confidence threshold, by default | 0.5 |
| Not a command, with no skill chosen, below | 0.1 |
| Phrasing not stored at or above | 0.95 |
| Examples per skill | 16 |
| Labels in the matrix | 200 |
| Neighbour checks when verifying a row | 8 |
| Confidence to accept a picked file or branch | 0.6 |

The threshold and the mode are each resolved in this order: the flag (`--confidence=`, `--mode=`),
the environment (`JLI_THRESHOLD` or its older name `JLI_CONFIDENCE`, `JLI_MODE`), the setting stored in the brain
(`jli config …`), then the default.

**The CLI contract:**

```
jli <request>                         route, run, learn
jli                                   interactive session (needs a terminal)
jli skills | stats | history
jli teach "<phrase>" [--risk=…] -- <command>
jli fix                               unlearn the last executed route and ask the agent
jli forget <label>
jli config [confidence <0..1> | mode <manual|semi|auto>]
jli sync
jli demo
jli help

-n, --dry-run   -y, --yes (or JLI_YES=1)   -Y, --yolo (or JLI_YOLO=1)   -q, --quiet   --json   --agent
--confidence=N  --mode=M    --risk=R
```

- A subcommand is recognised only when it is the first word and its arguments fit its shape.
  Otherwise the whole line is a request: `jli forget the old branch` is a request.
- The trace goes to stderr and the command's own output to stdout, so `jli` composes in pipes.
- `--json` captures the command's output and prints one `Outcome` object, with these fields in this
  order: `query`, `path`, `label`, `commands`, `confidence`, `risk`, `ran`, `exitCode`, `blocked`,
  `learned`, `answer`, `output`, `msSystem1`, `msSystem2`.
- Exit codes: the command's own code when it ran; 0 for a dry run or a request that is not a
  command; 3 when nothing ran for any other reason (no one to confirm, destructive, declined, a
  missing slot value); 1 on an error.
- Colour is off when `NO_COLOR` is set to a non-empty value or stderr is not a terminal.
  `FORCE_COLOR` turns it on.

**Lesson paid for:** do not gate on `isCommandRequest` at 0.5. It scores requests phrased as
questions low: "where am I" scored 0.37 while the skill choice was 0.99. It is used only below 0.1,
and only when no skill was chosen.

## Consequences

- `jli` can be driven by scripts and agents through `--json` and the exit codes.
- Destructive commands cannot be automated. This is deliberate.
- The `blocked` values `"dry run"` and `"not a command request"` are part of the interface, because
  the exit code depends on them.
