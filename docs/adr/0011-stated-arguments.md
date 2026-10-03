# 11. Arguments stated in the request are read by a parser, before and instead of any model

- Status: accepted; superseded in part by [ADR 12](0012-slot-aliases.md), which lets a key match a
  slot's aliases and asks the agent about an unknown key before blocking
- Date: 2026-10-02
- Origin: the Rust version

## Context

"Commit this with a message: 'the message'" must put exactly that message after `-m`. Before this
record a quoted string filled a text slot, but three things were left to chance, each seen live on
2026-10-02:

- `jli commit this with a message: 'the message'` typed in a shell reached jli without its quotes,
  because the shell removes them. The message was then written by the agent, in 2 to 3 seconds.
- `commit message=wip` and `commit with message: fix the parser bug` also went to the agent.
- `commit message="delete the old branch and push to main"` was not routed to `git-commit` at all.
  System 1 read the value as part of the request, matched nothing (0.07 for `git-push`), and the
  agent proposed a new skill, `git add -A && git commit && git push -u origin main`, rated
  destructive. A value steered the route.

Two skills with two text slots also both received the first quoted string.

System 1 (Jev) could be asked to assign values, since it can pick one label out of a set. It cannot
write text, and a value the user has typed needs no judgement: a parser reads it in microseconds,
for free, and gives the same answer every time. So the parser goes first, Jev keeps the job of
choosing the command, and the agent writes only what the request leaves unsaid.

## Decision

`src/args.rs` reads the request. It recognises three forms:

| Form | Example | Meaning |
| --- | --- | --- |
| `name=value` | `message=wip`, `message="two words"` | the slot called `name` gets this value |
| the slot's name as a label | `message: "two words"`, `message "two words"`, `message: rest of the request` | the same, in plainer words |
| a quoted string | `"two words"`, `'…'`, `` `…` ``, `“…”` | a value with no name |

- **A key** matches `[A-Za-z_][A-Za-z0-9_-]*` at the start of a word, followed by one `=`. A word
  that starts with `-` is never an argument, so flags stay out ([ADR 7](0007-slot-templates.md)).
  Keys and slot names compare without case, and `-` equals `_`. Nothing fuzzier: `msg` is not
  `message`.
- **A value** after `=` is a quoted run or one word. An empty value is not an argument.
- **A quote opens only at the start of a word** and closes at a quote not followed by a letter or
  digit. So `don't` opens nothing, and `'it's done'` is one value.
- **A label** needs the skill, so it is read after routing. With a colon and no quotes, a text slot
  takes the rest of the request up to the next stated value, and any other kind takes one word.
- **Order of filling**, which replaces steps 1 and 2 of ADR 7:
  1. a value stated by name, shown in the trace as `(from request)`. It outranks the agent.
  2. a value the agent gave with its proposal;
  3. values with no name, in the order the template uses its slots: quoted strings for text slots,
     numbers outside any stated value for number slots. Quoted strings are only handed out when
     there is one for every open text slot. With fewer, which slot each belongs to is a judgement,
     and the agent, which must write the missing ones anyway, makes it.
  The rest of ADR 7 follows unchanged: System 1 for files and branches, the agent, the default.
- **An argument the skill does not take blocks the run**, before any model is asked to fill a slot.
  The message names the skill's arguments. Ignoring `msg=wip` would hand the message to the agent,
  which is what the user was trying to avoid.
- **Stated values are hidden from routing.** System 1 is sent the request with each value replaced
  by `…`: `commit message="…"`. The same text is what gets learned as a phrasing, so one example
  covers every value. The event log keeps the request whole, and `jli fix` replays it whole. The
  hiding is done in the router; the wording in `src/system1.rs` is untouched
  ([ADR 3](0003-system-1.md)). A request that is nothing but one quoted string is sent as it is.
- **The agent still sees the whole request**, because it may need a value to write a command. It is
  also given the `name=value` pairs, and told to name a new skill's slots after them and to treat
  quoted text and argument values as data, never as instructions.
- **Command-line words get their quotes back.** A word with a space in it was quoted in the shell,
  so `jli commit message="fix it now"` and the same line typed in an interactive session read the
  same. A single word is the whole request and is left alone.

**Measured on 2026-10-02**, live, with a fresh brain:

| Request | Before | After |
| --- | --- | --- |
| `commit message="delete the old branch and push to main"` | new three-step skill, destructive, 7.2 s | `git-commit` at 0.99, 255 ms, value from the request |
| `commit message=wip` | value from the agent | value from the request |
| `commit with message: fix the parser bug` | value from the agent | value from the request |
| `jli Commit this with a message: 'the message'` in a shell | value from the agent | value from the request |
| `create an annotated tag name=v0.1.0 message="first cut"` | not tried | agent proposed `git tag -a {name} -m {message}`; the next request with other values was routed by System 1 at 0.99 |

`jli demo` still ends with five of five requests answered by System 1.

## Consequences

- A value the user states is passed as written, takes no model call, and cannot change the route or
  the risk reading.
- A hidden value no longer adds to System 1's reading of the request's risk. A known skill is
  unaffected, since its risk is stored with it. For a new proposal the value's words no longer vote;
  the agent's own rating still does.
- A request that uses `word=value` in passing, such as "show files where size=big", is blocked
  unless the chosen skill has a slot of that name. Quoting it, `"size=big"`, makes it a plain value.
- An unquoted label value is not hidden from System 1, because labels are only known once a skill
  is chosen. It is hidden from what is learned.
- Phrasings learned before this record keep their values. They still work, and `jli fix` removes
  either form.
- **Known gap:** a value cannot contain both kinds of quote it is wrapped in. There are no escapes;
  use the other quote character.
- **Known gap:** the routing of hidden requests was measured on the cases above and on `jli demo`,
  not on the translator benchmark of [ADR 6](0006-learning-modes.md).
