# 7. Skills are templates with `{slot}` placeholders, filled cheapest first and always shell-quoted

- Status: accepted; superseded in part by [ADR 14](0014-shell-integration.md), which expands `~`
  and `$NAME` in a file value before it is quoted, and quotes a value that starts with `=`; and by
  [ADR 23](0023-shell-fixes-and-number-words.md), which does not read a number word joined by `-` to another word
- Date: 2026-10-02
- Origin: the TypeScript prototype; kept unchanged by the Rust port

## Context

A skill that runs `git log --oneline -n 3` only answers one request. Turning the count into a slot
lets one row answer "the last 3 commits" and "the 7 most recent commits". But a value has to be
found for each request, cheaply, and dropped into a shell command safely.

## Decision

- **A placeholder is `{name}`**, where the name matches `[A-Za-z_][A-Za-z0-9_]*` and the `{` is not
  preceded by `$`. So `${HOME}` stays shell syntax, and `{}` in `find -exec` is not a placeholder.
  The Rust version tokenizes this by hand, because the `regex` crate has no lookbehind.
- **A slot** has a name, a kind (`file`, `branch`, `number`, `text`), a description and an optional
  default. [ADR 12](0012-slot-aliases.md) adds a list of other names it answers to.
- **Values are found cheapest method first:**
  1. a value the agent already gave with its proposal;
  2. for a number slot, the first number in the request, in digits or as a word from one to ten;
     for a text slot, the first quoted string (`"…"`, `'…'` or `` `…` ``);
  3. for a file or branch slot, a System 1 `choice` among the real directory entries or git
     branches, accepted at confidence 0.6 or more;
  4. the agent, for any text slot not quoted in the request and any slot without a default;
  5. the slot's default.
  [ADR 11](0011-stated-arguments.md) puts a value the request states by name ahead of all of these,
  and hands quoted strings and numbers to slots in template order instead of giving each slot the
  first one.
- **Every value is shell-quoted** unless it matches `^[A-Za-z0-9_\-.,/:=@%+]+$`. Quoting uses single
  quotes, with `'` written as `'\''`. Placeholders are never quoted inside a template; quoting is
  added when the template is rendered.
- **Matching a command back to a template** is local and needs no model: the literal parts must
  match, with any run of whitespace matching any other, and each slot matches one word or one
  quoted string, with the quotes removed. This is how a taught command is recognised as a known
  skill with other values ([ADR 6](0006-learning-modes.md)).
- A placeholder that no slot declares blocks the run, as does a slot with no value.
- **A flag is never a slot.** A slot value from the translator that begins with `-` makes the
  translator's slots unfaithful, and the command is stored literally.
- A translator's template is kept only if rendering it with its own values reproduces the taught
  command, ignoring quotes and runs of whitespace.

**Lessons paid for:**

- Models turn flags into slots: `date -I` came back as `date {format}`.
- Placeholders must not match shell syntax.
- Every slot value must be shell-quoted.

## Consequences

- A slot value cannot inject shell syntax.
- **Known gap**, seen on 2026-10-02: a glob given as a slot value is quoted, so it does not expand.
  The agent proposed `rm -f -- {pattern}` with the value `*.log`, which rendered as
  `rm -f -- '*.log'`. The prototype quoted it the same way.
- Numbers go to number slots in template order. A request whose numbers come in another order can
  get them swapped; `name=value` ([ADR 11](0011-stated-arguments.md)) is the exact way.
