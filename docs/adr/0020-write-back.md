# 20. An accepted route links its phrasing, a rejected command is refused, and a path is hidden as it was typed

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version; issues #20, #15 and #18
- Supersedes in part: [ADR 13](0013-learning-fixes.md), on what happens when the agent proposes a
  rejected skill's command, and on hiding being verbatim; [ADR 18](0018-agent-contract.md), on the
  closed set of `blockedCode`, which gains `already_rejected`

## Context

Three defects in what a route writes back, each found while building ADR 13 and each now covered by
an offline test that fails without the fix:

1. **A phrasing could be an example and a counter-example of one skill** (#20). When System 2 routed
   a request to a known skill, the write-back called `add_example`. Teaching (`learn_phrase`,
   `learn_command`) calls `link_phrase`, which also drops the phrasing from that skill's
   counter-examples. So after `jli fix` had made `files by date` a counter-example of a skill, an
   approved agent route back to that skill listed the phrasing under both `+` and `−`. System 1 was
   then sent a criterion that says both.
2. **A rejected command could come back as a second row** (#15). ADR 13 left the skill just rejected
   out of the duplicate check on purpose, so that `jli fix` could not route back to it. But the agent
   can still propose its command: after `jli fix` rejected `count-lines-in-file`, it proposed
   `wc -l {file}` again and the router stored `count-lines-in-file-x`. The instructions tell the
   agent never to do this, and nothing enforced it. The agent can also answer kind `existing` with
   the rejected label; that label is not on offer, so the router found no skill and no steps and
   reported "not a command" with no answer.
3. **A path written with `~` stayed in the learned phrasing** (#18). ADR 13 hides every value the
   command ran with, found verbatim in the request. ADR 14 expands `~` and `$NAME` in file values
   before they run, so for `how many lines are in ~/notes/n.txt` the value is `/Users/x/notes/n.txt`,
   which the request does not contain, and the example kept the path of one instance. It happened
   whenever the value came from a System 1 pick or from the agent; a stated value was already
   hidden by ADR 11.

## Decision

### 1. An accepted route links its phrasing as teaching does

`Route::write_back` adds the phrasing with `Brain::link_phrase`, not `add_example`. `link_phrase`
now returns whether it changed anything (the example added, or the counter-example dropped), so the
trace and `learned` say `"files by date" means list-files-by-time` when only the counter-example
went. Its other callers ignore the value.

Every path that writes a phrasing back was checked:

| Path | Before | Now |
| --- | --- | --- |
| a route to a known skill, System 1 or System 2, run now | `add_example` | `link_phrase` |
| `jli run` of a plan ([ADR 18](0018-agent-contract.md)) | the same `write_back` | `link_phrase` |
| auto mode, a failure diagnosed as `environment` | the same `write_back` | `link_phrase` |
| a correction, at the prompt or `jli fix <eventId> -- <command>` | `learn_phrase`, which links | unchanged |
| a new skill | a new row, with no counter-examples to drop | unchanged |
| `jli teach` | `learn_phrase` or `learn_command`, which link | unchanged |

- **Only a route that teaches links.** At System 1 confidence 0.95 or above nothing is written
  (ADR 5), so a counter-example of the skill that ran is left as it is. A confident route that
  contradicts a counter-example is System 1's call; the write-back did not change it before and
  does not now.
- Inside a project, a phrasing for a global skill is a lesson ([ADR 17](0017-project-scope.md)), and
  linking it lifts the counter-example in the lesson, as teaching already did there.

### 2. A proposal of a rejected skill's command is refused

"Rejected" is what ADR 13 meant by it: the labels a route is told to leave out
(`RouteOptions::rejected`), set by bare `jli fix`, `jli fix <eventId>`, `jli fix <eventId>
--expected`, and a `?` or an answer after a "no". Counter-examples and corrected events are not
rejections of this kind: they say a phrasing does not mean a skill, not that its command must
not run.

After the agent answers and its proposal is checked against the skills on offer (ADR 13, which is
unchanged: a rejected skill is still not on offer), it is checked against the rejected skills too.
It is refused when it names a rejected skill's label as kind `existing`, or when a rejected skill
runs its command:

| Rejected skill | Agent's proposal | Before | Now |
| --- | --- | --- | --- |
| `count-lines-in-file`, `wc -l {file}` | new, `wc -l {path}`, `path = notes.txt` | `count-lines-in-file-x` stored (seen live) | refused |
| `list-files-by-time`, `ls -ltr` | new, `list-by-date`, `ls -ltr` | `list-by-date` stored | refused |
| `count-notes`, `grep -c TODO /Users/x/notes.txt` | new, `grep -c TODO {file}`, `file = ~/notes.txt` | `count-todos` would be stored | refused |
| `count-lines-in-file` | existing, `count-lines-in-file` | "not a command", with no answer (seen live) | refused |

"Runs its command" is `match_proposal` (the same template, placeholders renamed, or a literal the
template produces), or else the proposal rendered with the agent's values, file values expanded as
`fill_slots` would, matching the rejected template (`match_template`). The second test is the one
ADR 13 declined for routing, because routing a template to a literal skill loses the template. For
refusing it is right: the command that would run is the one the person turned down.

A refusal runs nothing, learns nothing, stores no plan, and logs the request as an event that was
not accepted, like a route nobody confirmed. It is not marked `corrected`: the person turned the
skill down once, and `jli eval` ([ADR 16](0016-measuring-the-matrix.md)) should count that once, not
the agent's repeat as a second verdict against the skill. `jli stats` counts it as a request that went
to the agent, which it was. The outcome has `blockedCode: already_rejected` (exit 3, like every other refusal),
`label` set to the rejected skill it ran into, and `commands` set to what would have run. The trace
says:

```
! Not run: the agent proposed wc -l notes.txt again, which count-lines-in-file runs and was just rejected for this request.
  Nothing was learned. To teach the right command: jli teach "how many lines are in notes.txt" -- <command>
```

**Refusal, not a retry.** Sending the proposal back to the agent once, with the collision as
feedback, was considered. It was not built:

- The agent already receives `rejectedSkills` with their steps, and the rule "never propose them
  again, under any label, nor a command that does the same thing". A repeat comes when it has
  nothing better: in each of the four live repeats below, the command it repeated was a correct
  answer to the request (`wc -l`, `wc -w`).
- A retry would cost a second agent call, about 1.3 s, for what would most often be the same
  answer or a worse spelling of it, and the rule above forbids the spelling.
- A retry needs a new input or instruction, which ADR 4 requires to be measured. Refusal changes no
  prompt, no schema and nothing sent to the agent.

The person is told how to say what was right instead. `jli fix` on the same event is refused once
it has been corrected, so the way out is `jli teach`.

### 3. A file value is hidden in the form the request wrote it

The phrase is still worked out from the request and the command that ran (`slots::shape`, used by
the router and by `jli fix` through `learned_phrase`). Besides each value read back from the
command, it now hides each word of the request that expands to one of those values the way ADR 14
expands a file value: a leading `~`, `$NAME` and `${NAME}`, from the environment, with nothing run.
The words are the path-like words of the request (`path_words`, the same ones offered to the file
pick), with a full stop that ends the sentence left off.

| Request | Value in the command | Learned as, before | Now |
| --- | --- | --- | --- |
| `how many lines are in ~/notes/n.txt`, the agent gave `/Users/x/notes/n.txt` | `/Users/x/notes/n.txt` | unchanged | `how many lines are in …` |
| the same, the agent gave `~/notes/n.txt` | `/Users/x/notes/n.txt` | unchanged | `how many lines are in …` |
| `how many lines are in $HOME/notes/n.txt` | `/Users/x/notes/n.txt` | unchanged | `how many lines are in …` |
| `how many lines are in ${HOME}/notes/n.txt, please` | `/Users/x/notes/n.txt` | unchanged | `how many lines are in …, please` |
| `how many lines are in ~/notes/n.txt.` | `/Users/x/notes/n.txt` | unchanged | `how many lines are in ….` |
| `count the words of ~/p/m.txt`, System 1 picked `~/p/m.txt` | `/Users/x/p/m.txt` | unchanged | `count the words of …` (seen live) |

`expand_path` checks first whether the value names a path that exists as written, and then does not
expand it. That check is left out here (`expand_words`): it depends on the working directory, which
`jli fix` may not be in. Leaving it out cannot hide a word wrongly: a value that was not expanded is
in the command as written and is hidden verbatim, and a word that expands to something not in the
command hides nothing.

`jli fix` reaches the same phrase because it calls the same function on the request and the logged
command. A test routes requests with each spelling, works the phrase out again from the request and
the command, and checks that it is the phrase the route learned; a second routes `is $HOME there`,
reads the event back, and removes the example `is … there` it worked out from it.

### Measured on 2026-10-03

Live, with scratch brains (`JLI_DB_URL=file:<scratch>/brain.db`) in scratch repositories, the release
binary of this branch. The first runs were made through the agent alone (`--agent`), before the
System 1 key was read from 1Password; the rest used System 1 as well.

| Run | Result |
| --- | --- |
| `--agent -y what files are here` (to `list-files`), `--mode=manual fix 1 -- ls -la`, `forget ls-la`, `--agent -y what files are here` | after the fix `list-files` held `− "what files are here"`; after the agent routed there again it held `+ "what files are here"` and no `−`. On the first attempt the agent chose `ls-la`, which was not yet forgotten; on the second its reply was unreadable (`null` for a string) |
| `--agent -y how many lines are in notes.txt` (new `count-lines`), then `-y fix` | the agent answered kind `existing`, `count-lines`; refused, exit 3; no second row |
| `-y how many lines are in notes.txt` (System 1 unsure, new `count-lines-in-file`), then `--json -y fix` | the agent proposed `wc -l notes.txt` again; `blockedCode: already_rejected`, `label: count-lines-in-file`, `commands: ["wc -l notes.txt"]`, exit 3 |
| `-y 'how many words are in ~/p/n.txt'` (new `count-words-in-file`), then `-n fix`; and on another brain the same, then `-y fix` | stored as `how many words are in …` both times; both times the agent proposed `wc -w {file}` again under the same label, and it was refused. The first refusal showed the command as `wc -w '~/p/n.txt'`, so it is now shown with file values expanded, as it would have run: `wc -w /Users/x/p/n.txt` |
| `--agent --json -n how many lines are in \$HOME/p/n.txt` | `phrase: "how many lines are in …"`, the value from the agent, expanded |
| `--json -n 'count the words of ~/p/m.txt'` routed by System 1 to `count-words-in-file`, then the same with `-y`, then `--mode=manual fix 2 -- wc -c '~/p/m.txt'` | `phrase: "count the words of …"`, the value picked by System 1; the fix worked out the same phrase from the event: `− "count the words of …"` on `count-words-in-file` |

The acceptance checks 1 to 12 of [ADR 9](0009-port-to-rust.md) were re-run and held; 9 to 11 at a
terminal driven by `expect`. Check 12, `jli demo`: exit 0, instantly 2 of 5 then 5 of 5, deciding
5.1 s then 1.3 s. Check 13 needs the TypeScript version, which is not in this repository, and was not
run.

Later on 2026-10-03, on the merged build of ADRs 19 to 23 (the integration branch), with fresh scratch
brains:

- `-y how many lines are in notes.txt` went to the agent (System 1 at `none-of-these` 1.00), which
  proposed the new `count-lines` (`wc -l {file}`), verified at 0.99. `--json -y fix` unlearned the
  phrasing, and the agent proposed `wc -l notes.txt` again: `blockedCode: already_rejected`, `label:
  count-lines`, `ran: false`, exit 3. `jli skills` showed `count-lines` with `− "how many lines are in
  …"` and no `count-lines-x`.
- With `HOME` set to a scratch directory, `-y 'how many lines are in ~/notes/n.txt'` went to the agent,
  which gave `~/notes/n.txt`; it ran expanded, and `jli skills` showed the new `count-lines` with
  `+ "how many lines are in …"`, the scratch home path nowhere in it. `-y 'count the lines of
  ~/notes/n.txt'` then went to `count-lines` at 1.00 and, being confident, wrote nothing.
- A rejected `ls {directory}` not refusing `ls -la` was checked offline only.

## Consequences

- A skill never lists one phrasing as both an example and a counter-example after a route; System 1
  is no longer sent a criterion that contradicts itself.
- After `jli fix`, the agent's second answer is either another command or nothing. A command the
  person rejected does not run again under that request, and does not come back as `label-x`.
- An agent caller sees `already_rejected` and knows to teach the right command or give up; it never
  receives a "not a command" that was really a repeat.
- Examples learned from requests that name a home path are shapes, like the others, and `jli fix`
  can find and remove them.
- **Known gap:** a refusal ends the correction. The person is not asked again, and the agent is not
  given a second chance; `jli teach` is the way to say what was right.
- **Known gap:** a command that does the same thing in other words (`awk 'END { print NR }' {file}`
  for `wc -l {file}`) is not recognised as the rejected command.
- A rejected `ls {directory}` does not refuse `ls -la`. When this record was written `match_template`
  let a slot capture a flag (issue #17), and it would have; [ADR 19](0019-one-row-per-command.md)
  made it refuse a flag, and `a_rejected_template_does_not_refuse_its_command_with_a_flag` checks that
  after `jli fix` rejects `ls {directory}` the agent's `ls -la` runs and is learned as its own skill.
- **Known gap:** a counter-example on the skill a confident System 1 route ran (0.95 or above) stays,
  since such a route writes nothing.
- **Known gap:** a written form is found with the environment as it is when the phrase is worked out.
  If `HOME` or the variable a request named changed between the route and `jli fix`, the fix works out
  a different phrase and can miss the example. A bare `~` is not hidden (it has no letter or digit,
  ADR 13's rule for short values), and neither is `~user`, which ADR 14 does not expand.
