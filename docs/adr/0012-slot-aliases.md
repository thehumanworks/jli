# 12. A slot answers to several names, and a new name is worked out once and kept

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 11](0011-stated-arguments.md), on how a key is matched to a slot and on
  what an unknown key does

## Context

[ADR 11](0011-stated-arguments.md) matched an argument's key to a slot by its exact name: "nothing
fuzzier: `msg` is not `message`". A key that named no slot blocked the run before any model was
asked.

The slot's name is chosen by whichever model wrote the skill. In a brain in daily use on
2026-10-03 the agent had written `cd {directory}`, and `dir=src` was blocked. So the user had to
remember a name they never chose, which is the kind of remembering jli exists to remove. The block
also taught nothing: the same `dir=` was blocked again the next day.

The first idea was to store the names in the placeholder itself: `{dir|directory|path|d}`. That
was rejected. A placeholder's name is read in four places (`parts`, `match_template`,
`in_template_order` and `undeclared` in `src/slots.rs`), `|` is shell syntax, and every skill in
every brain would have had to be rewritten on first contact. The template is what runs; the names
a person may use for a value are a fact about the slot.

## Decision

- **A slot has `aliases`**, a list of other names, stored with it in the `slots` column. The
  placeholder keeps one canonical name. A row without the field reads as having none, and a slot
  with none is written without the field, so rows written before this record are byte-identical.
- **An alias is a key**: it matches `[A-Za-z_][A-Za-z0-9_-]*`, is stored in lower case with `-` as
  `_`, and compares the way names do. Within one skill a name or alias means one slot: an alias
  that already means a slot is refused, the first slot to claim it keeps it. A slot holds at most
  eight.
- **`name=value` matches a slot by its name or any alias.** So does the label form, `dir: src` or
  `dir "my files"`, for aliases of three letters or more. A shorter alias works only as
  `key=value`: `d:` and `n "…"` are too easily ordinary prose.
- **Aliases come from two places:**
  1. *When a skill is written.* The slot schema that the agent and the translator both fill has an
     `aliases` field. They are told to name a slot with the plain, full word (`directory`, not
     `d`) and to list up to four other names. The agent used to be told to name a new skill's
     slot exactly as the user's argument; it now puts the user's name among the aliases.
  2. *When a request uses a name the skill does not know.* The agent is asked one question, schema
     `argument_names`: for each unknown argument, which of the skill's slots it is another name
     for, or none. Only slots the request has not already named are on offer. The agent picks a
     slot and writes nothing: the value is the user's own, passed as written, and the trace shows
     `{directory} = src (from request, reading dir= as directory=)`.
- **An answer is checked, not trusted.** A slot that is not on offer, a slot that already has a
  value, and a name the user did not type are ignored. An argument left without a slot blocks the
  run as in ADR 11, and the message now lists each slot's aliases. If the agent cannot be reached
  the argument is unknown and the run is blocked. A skill with no slots, or with every slot already
  named, asks nobody.
- **The new alias is kept on the terms a phrasing is kept**
  ([ADR 5](0005-routing-loop.md)): after a human yes, or after a clean exit under `--yes` or
  `--yolo`. A dry run, a blocked run and a declined run keep nothing. For a known skill it is
  written to the slot and reported as `folder= means {directory} in <skill>`; for a skill proposed
  in the same request it is stored with the skill.
- **Aliases are learning, so re-teaching a label keeps them** ([ADR 2](0002-brain-in-libsql.md)):
  when a row is written again under its label, a slot that keeps its name keeps its aliases.
- **Aliases are not shown to System 1.** The matrix and its wording are unchanged
  ([ADR 3](0003-system-1.md)). The value of an unknown argument is hidden from routing like any
  other, so `dir=…` is routed and learned the same before and after the alias exists.

**Measured on 2026-10-03**, live, with a fresh brain:

| Request | Result |
| --- | --- |
| `--agent count the files inside d=src` | new skill `ls -p {directory} \| grep -v '/$' \| wc -l`; the agent named the slot `directory` with aliases `dir`, `d`, `path` |
| `count the files inside folder=docs` | System 1 at 0.99; `folder` read as `directory` by the agent; ran; `folder= means {directory}` kept |
| `count the files inside folder=src` | System 1 at 0.99, value from the request, no language model call |
| `how many files are in dir: docs` | the same, by label |
| `-n commit msg=wip` | `git commit -m wip`; the naming call took 1.1 s after 300 ms of System 1 |
| `-n commit branch=main` | blocked: `git-commit does not take branch=. Its arguments are: message.` |

The acceptance checks 1 to 8 and 12 of [ADR 9](0009-port-to-rust.md) were re-run and held.

## Consequences

- A name costs one agent call, about a second, the first time it is used with a skill, and nothing
  after that. The same name is learned separately by each skill that meets it.
- A user who types a name the agent rejects is blocked as before, about a second later than before.
- The agent can place a value in the wrong slot. What limits the harm: it can only pick an open
  slot of a skill already chosen, the command is shown with the reading before the question "Run
  it?", and the skill's stored risk still decides whether `--yes` is enough. Under `--yes` a wrong
  reading that exits cleanly is kept; `jli forget` is the only way to drop it.
- An alias of three letters or more is also a label, and with a colon a text slot takes the rest of
  the request. A common word as an alias, such as `path` or `name`, can therefore take prose as a
  value: `show the path: …`. The canonical name already had this property (ADR 11); aliases add
  more words that have it. The value is shown before "Run it?".
- **Known gap:** there is no command to remove or add one alias by hand.
- **Known gap:** an unknown name used as a label, `folder: src`, is not recognised. Any word can
  stand before a colon, so only `folder=src` raises the question.
- **Known gap:** a skill with no slots still blocks on any `key=value`. On 2026-10-03
  `list what is in dir=src` routed to `list-files` (`ls`) at 1.00 and was blocked, where asking the
  agent for a skill that takes a directory would have served the request.
- **Known gap:** the translator returned no aliases for `count` in the one live teach that was
  run. How often each model fills the field has not been measured.
