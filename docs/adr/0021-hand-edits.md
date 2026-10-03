# 21. One slot name and one phrasing can be changed by hand, written where the router would write the same lesson

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version, issues #23 and #24
- Supersedes in part: [ADR 12](0012-slot-aliases.md), on an alias being removable only with its
  skill; [ADR 18](0018-agent-contract.md), on the closed set of `error` kinds, to which seven are added

## Context

Two things jli learns could only be undone by forgetting the whole skill.

1. **A slot name.** An alias learned at run time from one agent answer is permanent ([ADR
   12](0012-slot-aliases.md)), and a wrong one fills a slot silently from then on. All four design
   reviews of ADR 12 raised it, and it was a known gap there (#24).
2. **A phrasing.** On 2026-10-03 "size of each file here" had been learned on `du -sh {directory}`.
   `jli eval --optimise` proposed dropping it, and the gate scored the change flat, so it was
   discarded: a stored example is ground truth, and a mis-taught one counts as evidence for its own
   skill ([ADR 16](0016-measuring-the-matrix.md)). The issue (#23) asked either for the measurement to
   recognise a mis-taught example or for `jli forget-phrase <label> "<phrase>"`. This record does the
   second; the first stays a gap of ADR 16.

Both edits had to respect who owns the row. [ADR 15](0015-user-scope.md) gives each user their own
skills, and a shared skill is a copy. [ADR 17](0017-project-scope.md) decides, for every lesson
learned inside a project, whether it is written to the project's row, to the global row, or to the
project's lesson about a global row. A hand edit is a lesson given directly, so it follows the same
table rather than a new one.

## Decision

### The commands

```sh
jli alias <label> <slot> <name>             # {slot} of <label> also answers to name=
jli alias --remove <label> <slot> <name>    # and no longer does
jli forget-phrase <label> "<phrase>"        # the phrasing no longer means <label>
```

- `--global` reaches the user's global row from inside a project, as for `teach` and `forget`.
- `-n` says what would change and writes nothing. ADR 18 found that agents add `-n` to preview
  anything; a write under `-n` would be a surprise, so both commands honour it.
- **Shape.** `alias` is the command only with exactly three words after it (besides its flags), and
  nothing after `--`. Anything else is a request, so `jli alias gs to git status` still routes. With
  `--remove` or `--global` a wrong shape is a usage error. `forget-phrase` is never a request: a
  label and at least one word, which are joined, so the phrase may be quoted or not.

### Names are stored and compared as ADR 12 stores and compares them

- A name is stored in lower case with `-` as `_`: `jli alias count-files directory Folder-Name`
  stores `folder_name`. It must be typable as a key (a letter or `_`, then letters, digits, `_` or
  `-`), or it is refused (`alias_invalid`).
- The slot may be named by its name or any of its aliases, in any spelling.
- A name another slot of the skill already answers to is refused (`alias_taken`): within one skill a
  name means one slot. A slot holds at most eight other names (`aliases_full`, with the command to
  remove one).
- A name the slot already answers to is not an error: nothing is written and the outcome says
  `changed: false`, exit 0.
- Removing a name the slot does not have, or the slot's own name, is refused (`alias_not_found`).
  For a name it does not have, the message lists the other names it has; for its own name, the
  message says that name is the one in the command and cannot be removed.
- A slot whose last alias is removed is written without the `aliases` field, so the row is byte for
  byte what it was before ADR 12 ([ADR 2](0002-brain-in-libsql.md)). A test checks this.

### A phrasing is matched as the brain matches phrasings

`forget-phrase` matches the phrase against the skill's examples as the store compares them, without
case or surrounding space, and acts on the stored text. A phrasing is stored with its values hidden
([ADR 13](0013-learning-fixes.md)), so it is typed as `jli skills` shows it (`how many lines are in
…`); a phrase that is not one of the skill's examples is refused (`phrase_not_found`) with the
skill's phrasings listed. A phrase that is already a counter-example of the skill is not an error:
`changed: false`, exit 0.

### Forgetting a phrasing keeps it as a counter-example

Two reasons, one from the router and one from projects:

- **Otherwise it is learned straight back.** The write-back keeps a phrasing after a yes whenever
  System 1 picked the skill below 0.95 ([ADR 5](0005-routing-loop.md)). A phrasing taught to the
  wrong skill was usually taught because the skill's scenario looks close to it, so System 1 is
  likely to pick the skill again and the next yes undoes the edit. As a counter-example the phrasing
  goes to System 1 under `doesNotApplyTo`, which is what it is. The counter-example makes that pick
  less likely; it does not forbid it. If System 1 still picks the skill below 0.95 and the person says
  yes, the write-back links the phrasing as teaching does ([ADR 20](0020-write-back.md)): it is an
  example again and leaves the counter-examples. A pick at 0.95 or above writes nothing and leaves the
  counter-example as it is.
- **Inside a project it is the only way to hide a global phrasing.** A project never writes a global
  row; it keeps a lesson, and a lesson hides a row's example only by holding it as a counter-example
  (`with_lesson`, ADR 17).

So the rule is the same in every scope, and it is what `jli fix` already does to the phrasing it
unlearns. It is undone the way any counter-example is: `jli teach "<phrase>" -- <command>` links
the phrase to the skill that runs the command and lifts it from that skill's counter-examples, and
so does a yes to a route of the phrase to the skill below 0.95.
Removing without a counter-example was rejected for the two reasons above; a person who only wants
a list tidied loses nothing by the counter-example except one place in the newest 16.

### Where each edit is written

Read straight from ADR 17's table: a name for a value is the skill's, and a phrasing is a route.

| Where the caller is, and whose skill it is | `alias` | `forget-phrase` |
| --- | --- | --- |
| outside every project | the user's row | the user's row: example out, counter-example in |
| inside a project, a project skill | the project's row | the project's row |
| inside a project, a global skill | **the global row**: an alias names a value and does not steer a route, as for one learned at run time; the message says it holds in every project | **the project's lesson about the global row**: the global row and every other project are untouched; the message says how to forget it everywhere |
| inside a project, with `--global` | the global row, even if a project row of that label hides it | the global row, for every project |
| another user's skill | not there: `No skill called …` | not there |
| a skill shared to the caller | the caller's own copy; the sharer's row is never touched, and `shared by` still shows | the caller's own copy |

The writes are the existing ones: `add_alias`, a new `remove_alias` built the same way, and
`remove_example` and `add_counter_example`, each a compare-and-swap ([ADR 15](0015-user-scope.md)),
so a hand edit and a router writing the same row at the same moment both keep their change.

### What a caller gets back

A person gets one sentence on stdout, in the voice of `jli forget` and `jli share`:

```text
folder_name is now another name for {target} in get-folder-size.
"size of each file here" no longer means get-folder-size, and is kept as a counter-example so that it is not learned straight back.
```

With `--json` it is one object on stdout, every field always present:

| Command | Fields, in order |
| --- | --- |
| `alias` | `label`, `slot` (the canonical name), `alias` (as stored), `action` (`add` or `remove`), `changed`, `dryRun`, `aliases` (the slot's, after), `writtenTo`, `project` |
| `forget-phrase` | `label`, `phrase` (as given), `changed`, `dryRun`, `examples` and `counterExamples` (after, as this scope sees them), `writtenTo`, `project` |

`writtenTo` is `global`, `project` or `project_lesson`; `project` is the project's id or `null`.

A refusal writes nothing and exits 1, as `jli share` does when it refuses. Under `--json` it is
ADR 18's error object, `{ "error": <kind>, "errorMessage": … }`, and these kinds are added to the
closed set: `skill_not_found`, `slot_not_found`, `alias_invalid`, `alias_taken`, `aliases_full`,
`alias_not_found`, `phrase_not_found`. A wrong shape is `usage`. `changed: false` exits 0, so an
agent can repeat an edit without checking first.

The logic is in `src/hand.rs`; `src/cli.rs` only parses the words and prints what comes back.

**Measured on 2026-10-03**, live, with a scratch brain in a scratch git repository:

| Step | Result |
| --- | --- |
| `teach "how big is this folder" -- du -sh .` | translated to `get-folder-size`, `du -sh {target}`, aliases `dir`, `path`, `folder`; verified at 1.00 |
| `teach "size of each file here" -- du -sh .` | added to `get-folder-size`: the issue's mis-taught phrasing |
| `--json -n size of each file here` | System 1 picked `list-files-with-sizes` (`ls -lh`) at 0.99, with `get-folder-size` among the runners-up |
| `-n forget-phrase get-folder-size size of each file here` | `Dry run: nothing was written. … would no longer mean …`; `skills` unchanged |
| `forget-phrase get-folder-size size of each file here` | exit 0; `skills` shows `− "size of each file here"`; a second time, `changed: false`, exit 0 |
| `--json -n size of each file here` again | `list-files-with-sizes` at 1.00; `get-folder-size` no longer among the runners-up |
| `forget-phrase get-folder-size disk usage`; `--json forget-phrase no-such-skill x` | exit 1, the phrasings listed; `skill_not_found`, exit 1 |
| `alias get-folder-size target Folder-Name`, then `folder_name` again | `folder_name is now another name for {target} …`; then `already …`, exit 0 |
| `-n folder_name=src how big is this folder` | System 1 at 1.00, `du -sh src`, `{target} = src (from request)`, no language model call |
| `alias get-folder-size nope x`; `alias … target "two words"`; `alias --remove` of a name it does not have | exit 1 each, with the slots, the rule for a name, and the names it has |
| in a project `shop-live`: `forget-phrase get-folder-size "how big is this folder"` | `… in project shop-live, and is kept there as a counter-example …`; `skills` there shows `global, +1 −1 learned here`; outside the project the global row still lists the phrasing |
| in the project: `alias get-folder-size target where_` | written to the global row (`writtenTo: global`, `project: shop-live`); outside the project `skills` shows it |

In the live run System 1 already preferred `ls -lh` for "size of each file here" before the edit,
so the edit could not change what ran; what it changed was the confidence (0.99 to 1.00) and that
`du` dropped out of the runners-up. A brain where the mis-taught phrasing wins the route was not
reproduced live; the offline tests check that System 1 is shown the phrasing under `doesNotApplyTo`.

The acceptance checks of [ADR 9](0009-port-to-rust.md) were re-run on fresh scratch brains with this
branch's release binary: 1 to 8 and 12 with no terminal, 9 to 11 under `expect`. All held: `ls -ltr`;
`git commit -m 'first draft'`; not a command, exit 0; `git add -A` not run with nobody to confirm,
exit 3; `rm '*.log'` refused as destructive, the files untouched; valid `--json` on the `system1` path
with the directory in `output`; the second teach joining `git log --oneline -n {count}` with every
API key unset; `-n 7`; an `environment` diagnosis for `git status` outside a repository and no
counter-example; three numbered options after a no, the first learned as a new skill and run; an
interactive session of two confirmed requests left with ctrl-d, exit 0; `jli demo` exit 0, answered
instantly 2 of 5 then 5 of 5, deciding 4.9 s then 1.3 s. Check 13 was not run: the TypeScript version
is not available.

## Consequences

- A wrong alias or a mis-taught phrasing costs one command, not the skill and everything else it
  learned.
- An agent can make both edits with no terminal and read back what changed; repeating one is safe.
- Inside a project an alias on a global skill changes every project, as a learned one already did,
  and the message says so. A phrasing forgotten there changes that project only.
- **Known gap:** `jli alias` followed by exactly three words is read as the command, so a request
  shaped like that, `jli alias gs git status`, is refused as `No skill called gs.` instead of routed.
- A forgotten phrasing that System 1 still routes to the skill is in one list or the other, never
  both. When this record was written the write-back added it back with `add_example` and left the
  counter-example; [ADR 20](0020-write-back.md) made it use `link_phrase`. Now, after a yes to a route
  below 0.95 (or one the agent made), the phrasing is an example again and is no longer a
  counter-example: the yes undoes the edit. A route at 0.95 or above writes nothing, so the phrasing
  stays a counter-example and nothing else. `a_forgotten_phrasing_routed_back_to_its_skill_is_one_list_or_the_other`
  checks both.
- **Known gap:** a yes to such a route below 0.95 undoes the edit, so a person who forgot a phrasing
  and then says yes to it out of habit has taught it back.
- **Known gap:** forgetting a phrasing does not touch the events log, so `jli eval` still replays an
  accepted request in those words as evidence for the skill ([ADR 16](0016-measuring-the-matrix.md)).
- **Known gap:** the measurement still cannot recognise a mis-taught example by itself
  ([ADR 16](0016-measuring-the-matrix.md)); a person or an agent has to notice it and run
  `jli forget-phrase`.
- **Known gap:** there is no alias for one project only: inside a project an alias of a global skill
  is written to the global row, and a project that disagrees can only hide the skill with its own.
- **Known gap:** like every compare-and-swap of ADR 15, the edits have run only against local files.
