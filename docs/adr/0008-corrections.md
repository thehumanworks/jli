# 8. A "no" or a failed command is a lesson, but only a diagnosis says what kind

- Status: accepted; superseded in part by [ADR 32](0032-durable-corrections.md), which makes explicit phrase choices durable;
  and by [ADR 13](0013-learning-fixes.md): bare `jli fix`
  unlearns this host's last route in this directory; and by
  [ADR 16](0016-measuring-the-matrix.md), which logs the declined route as its own event, marked `corrected`, before the correction;
  and by [ADR 18](0018-agent-contract.md), under which `jli fix <eventId> -- <command>` and
  `--expected` make the same corrections with no terminal, and every `jli fix` records whether one was there
- Date: 2026-10-02
- Origin: the TypeScript prototype; kept unchanged by the Rust port

## Context

When the user declines a command, they usually know what they wanted, and that is the most useful
thing `jli` can learn. A failed command is different. It might be the wrong command, but it might
also be the right command in the wrong place.

## Decision

**On "no" in manual and semi mode,** ask "What should it run instead?". The user can type a command
(split into steps on ` && `), type `?` to ask the agent, or press enter to skip.

**On "no" in auto mode,** the suggester ([ADR 4](0004-language-models.md)) offers up to three
alternatives, leaving out commands already tried. The user picks one by number, says what they
expected, types `?` to ask the agent, or presses enter to skip. If the suggester cannot be reached,
the user is asked for the command, as in semi mode.

What each answer teaches:

- **Choosing a different command** removes the phrase from the wrong skill's examples and adds it
  as a counter-example. The phrase is then learned for the chosen command
  ([ADR 6](0006-learning-modes.md)), and the command runs without another confirmation, because
  the user typed or picked it.
- **Choosing the same command with a different slot value** does neither. The skill was right.
- **A stated expectation, or `?`,** re-enters the loop with the agent only, passing the
  clarification and the rejected skill. The agent is told never to propose a rejected skill again.

**On a failed command in auto mode,** the suggester reads the end of stderr (the last 4000
characters) and gives one of three verdicts:

| Verdict | Meaning | What follows |
| --- | --- | --- |
| `environment` | the right command in the wrong place: not a repository, a missing file or tool, no network, no permission | explain it; the route is still recorded as correct |
| `wrong_value` | the right command with a wrong argument | the correction flow, but asking the agent does not reject the skill |
| `wrong_command` | a different command was needed | the correction flow; if the user skips, nothing is learned from the run |

**`jli fix`** unlearns the last executed route (a counter-example, and the event is marked
`corrected`) and asks the agent again with that skill rejected.

**Lesson paid for:** a failed command is not a wrong route. `git status` outside a repository exits
128, and it was the right command.

## Consequences

- Counter-examples reach System 1 as the `doesNotApplyTo` criterion of the skill they belong to.
- **Known gap:** correcting a phrase that strongly matches a seeded skill may not stick after one
  correction.
- **Known gap:** two flows have not been tested live since the prototype's last refactor: saying
  what you expected after a failed command, and correction in manual mode. The Rust port did not
  test them live either. The offline tests cover a typed command and `?` in semi mode, numbered
  options after a "no" in auto mode, and the `environment` verdict.
