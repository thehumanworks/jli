# 16. The matrix is measured against the brain's own evidence, optimised only where the measurement improves, and every new row is checked against its closest skills

- Status: accepted; superseded in part by [ADR 22](0022-project-scenarios.md), on how a kept rewrite
  is written and what `jli eval undo` puts back inside a project
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 5](0005-routing-loop.md), on the write-back of a proposed skill and on
  what `msSystem2` counts; [ADR 6](0006-learning-modes.md), on how a translated row is verified;
  [ADR 8](0008-corrections.md), on what a declined route leaves in the events log

## Context

`jli stats` measured speed only. Its learning curve is the share of requests System 1 answered,
so a confident wrong route counted as a win. Nothing measured whether routes were right, nothing
re-checked the matrix when a row was added, and the only benchmark used paraphrases written by
its author (the known gap of [ADR 6](0006-learning-modes.md)).

The evidence needed was partly missing too:

- In `Route::correct` a typed correction overwrote the outcome. The one row it logged paired the
  new label with System 1's confidence in the label the person had just rejected. A `?` after a
  "no" returned a fresh `route()`, so the declined attempt left no row at all.
- `verify` in `src/learn.rs` replayed the two newest examples of skills in `uses DESC` order, cut
  at eight. It always checked the most-used skills, whatever was taught, so a broad row that stole
  a little-used neighbour's phrasing passed. It also stored the row before checking it, and a
  transport error returned through `?` before the row was removed.
- A skill the agent proposed was stored after a clean run without any check.

The owner asked for a correctness measurement drawn from the brain itself, and on top of it a
loop in which System 2 improves the decision matrix, weighted by how often each command runs and
is deemed correct by the user.

## Decision

### What is recorded

- **A declined route is its own event.** When the person says no and then types a command, picks
  an option, or asks the agent, the route they turned down is logged first: not accepted, with
  System 1's confidence in that label, and `learned = 'corrected'`, the marker `jli fix` already
  writes on a route it undoes. Every existing reader skips that marker, including `jli stats` on
  hosts that run an older version, so no version counts it as a success. A "no" followed by enter
  is still one row, now also marked `corrected`. No column was added.
- **The corrected row no longer carries the old confidence.** A `taught` row has no confidence:
  the person chose the label, not System 1.
- **The suggester's time is counted in `msSystem2`**, both on the event and in `--json`. It is a
  language model call like the agent's, and before this record it was not logged at all.
- **A request** is its first event plus any events right after it, from the same host and with
  the same words, that follow a `corrected` row. `src/eval.rs` groups the log this way to count
  first tries.

### `jli stats` reports correctness

Over the routed requests in the last 500 events:

| Line | Counts a request whose first attempt… |
| --- | --- |
| right the first time | was accepted, exited 0, and was not later declined or corrected |
| declined or corrected | was turned down, undone by `jli fix`, or is a `taught` row with no attempt logged before it (a brain older than this record) |
| ran and failed | was accepted and exited non-zero |
| never judged | never ran: nobody to confirm, a destructive command under `--yes`, or a missing value |

Declined and corrected attempts stay in the denominator. Under it: how many of System 1's routes
still called a language model (`msSystem2 > 0`: slot filling, naming an argument, or the
suggester), and the trend of `jli eval`.

### `jli eval` measures the matrix with the real System 1

- **Leave-one-out.** Every stored example of every skill is routed with itself held out of the
  matrix. The phrase is removed from the examples **and the counter-examples** of every row, not
  only from its own skill. Otherwise a correction, which stores the phrase as an example of the
  right skill and a counter-example of the wrong one, would leave the answer in the matrix. As a
  result a corrected phrasing often fails leave-one-out. That is a finding, not an error: the
  matrix holds that phrasing only by remembering it.
- **Replay.** Each distinct wording among the accepted, uncorrected requests that exited 0 is
  routed again against the matrix as it is, and compared with the label that was accepted. A
  request whose skill was forgotten is counted and left out.
- **Context.** The state System 1 sees is `{ directory, insideGitRepository, platform }`. A replay
  reads it again from the event's directory. When that directory is gone, `directory` still comes
  from the stored path. `insideGitRepository` is true if the accepted command was a `git` command
  that exited 0, or if a directory above the path is still a repository. `platform` is this
  host's, because events do not store one. What is lost: whether a repository that is now gone
  was one, unless a git command proves it; and the platform of a request made on another host.
  The report counts the replays that ran in an inferred context. A stored example has no context
  of its own. It is routed in the context of the newest request that had its words, else in the
  directory `jli eval` runs in.
- **Verdicts.** A phrasing is **home** when System 1 picks its own skill at or above the
  threshold, **stolen** when it picks another skill at or above the threshold, and **escalated**
  otherwise (below the threshold, or no skill). Its **home confidence** is System 1's confidence
  when it picked the owner, else System 1's probability for the owner.
- **The report.** Per skill: its weight (below), the three phrasings with the lowest home
  confidence even when they route home, and each steal with who took it. Overall, for
  leave-one-out, replay and both: accuracy, steal rate, escalation rate, and mean and worst home
  confidence. Then the System 1 calls and their cost. The number of calls and an estimate at
  $0.00006 a call (the README's $0.012 for 200) are printed to stderr **before** anything is
  spent. `--limit N` takes at most N phrasings, in turn from each skill. `jli eval <label>`
  measures one skill. `--json` prints the whole report, including every phrasing, on stdout. The
  price and the progress still go to stderr.
- **The trend.** A summary is stored in `meta` under `eval:<timestamp>`. `jli stats` shows the
  accuracy of the full evaluations, leaving out those cut by `--limit` or narrowed to one skill.
- **`-n` / `--dry-run` writes nothing at all.** The measurement runs and no summary is stored.
  With `--optimise`, the rewrites are proposed and measured and none is applied. The events log is
  never written by `jli eval`.

### `jli eval --optimise` lets System 2 rewrite the weakest rows

**Ranking.** Each measured skill gets

```
weight     = importance × deficit
importance = (1 + ln(1 + uses)) × (correct + 1) / (judged + 2)
deficit    = mean(1 − home confidence) + share of its phrasings stolen or escalated
```

`uses` is the skill's own counter. `judged` counts its events that were deemed correct (accepted,
exit 0, not corrected) or marked `corrected`, and `correct` the first kind. Why this shape:

- The logarithm makes use count without letting one skill used a thousand times drown the rest.
  The 1 keeps a never-used skill in the ranking.
- The second factor is Laplace's rule of succession: the share of the person's verdicts that said
  the route was right, with a skill nobody has judged counted as a coin toss. It multiplies rather
  than adds. More confidence is only worth having for a skill whose routes the person accepts.
  Raising the confidence of a skill that is often turned down makes more wrong routes instant.
- The deficit adds the misses to the shortfall, so a phrasing that is stolen or escalates counts
  more than one that comes home at a modest confidence.

The owner's example holds. A skill used 40 times, with 19 of 20 judged routes right and routed
home at 0.55, weighs 4.25 × 0.91 × 0.45 = 1.74. A skill never used or judged, escalating at 0.30,
weighs 1 × 0.5 × (0.70 + 1) = 0.85. The ranking is printed with each factor.

**The rewrite.** For each of the top skills (`--max-skills`, 3 by default), System 2 is called
with a new schema, `matrix_rewrite` (`rewrite_row` in `src/system2.rs`, through `src/llm.rs`,
reasoning off). It is given the skill, its six least confident phrasings with where they went,
and the rows of the skills it competes with. It returns a new `scenario`, up to three of the
skill's own examples to drop (`dropExamples`: phrasings taught by mistake), up to three
counter-examples for competing skills that take its phrasings (`counterExamples`), and an
explanation. Only that data changes. The questions and criteria System 1 is asked stay as they
are ([ADR 3](0003-system-1.md)).

**The competitors** of a skill are the skills that took its phrasings or whose phrasings it took,
the skills System 1 ranked at 0.05 or more behind either, and the three skills that share the
most distinctive words with it.

**The gate.** The rewrite is applied in memory, and every phrasing of the skill and of its
competitors is routed again. The probe set stays fixed: a dropped example is still probed, so
dropping a hard phrasing cannot raise the score. The rewrite is kept only if both of these hold:

- the **weighted score** rises by at least 0.02, where the score is the mean home confidence of
  the phrasings in scope, each weighted by its owner's importance, with a stolen phrasing scoring
  0;
- **no phrasing of another skill** that routed home stops doing so, by falling below the
  threshold or by moving to a third skill.

The 0.02 floor is measured. On 2026-10-03 the same 22 phrasings were measured twice, unchanged.
The largest change in any one phrasing's home confidence was 0.02 (0.04 in raw confidence), and
no verdict changed. The score is a weighted mean of those values, so noise moves it by no more
than 0.02.

A rewrite that fails gets one retry, with the measured problem as feedback, the pattern
`learn_phrase` uses. **There is no human confirmation: the measurement is the approval.**

**Bounds.** At most `--max-skills` skills (3), and at most `--max-calls` model calls (400),
System 1 and System 2 together, after the measurement. Before each skill the loop prints the
phrasings in scope and the calls it may spend. It stops, and says so, before it would cross the
bound.

**Record and undo.** A kept change is written through the paths that already exist: the scenario
through `add_skill`'s upsert, dropped examples through `remove_example`, counter-examples through
`add_counter_example`. No row is deleted. Before the write, the change is stored in `meta` under
`optimised:<timestamp>:<label>`. The record holds the scenario before and after, what was dropped
and added, and the scores and home counts before and after. `jli eval undo [<label>]` puts back the
newest change still in force: the scenario, the dropped examples, and the counter-examples it
added, removed. It refuses when the scenario has changed since. Undone changes are marked in their
record, which is never deleted. `jli eval undo -n` shows what it would put back.

### A row is checked against its closest skills before it is written

`verify_row` in `src/eval.rs` replaces `verify`. It serves `learn_phrase` and the agent's
proposals alike:

1. The row is added to the matrix **in memory only**, at the place the brain would list it. The
   taught phrase must route to it at or above the threshold.
2. Its **neighbours** are the skills System 1 itself ranked behind it for that phrase
   (`Decision.candidates`), then the skills sharing the most distinctive words with the phrase,
   the scenario and the command. The two newest examples of each are taken, up to eight phrasings.
3. Those phrasings are routed with the row in the matrix. A phrasing that lands on the row
   rejects it, as before. A phrasing that now falls below the threshold or moves to a third skill
   is routed once more without the row. If it routed home without the row, the row is rejected:
   "with it in the matrix, "…", which belongs to …, falls to 0.30 confidence". If it was already
   weak, the row is not blamed.
4. Only a row that passes is written. If System 1 cannot be reached during the check, nothing
   unchecked is stored: the person's phrase becomes the scenario, as when both attempts fail.

**A skill the agent proposed** gets the same check. The check starts in a thread when the command
starts, and the write-back waits for it, so it never holds up the command's output. If the check
fails, the agent gets one rewrite of the scenario through `rewrite_row`, with the problem as
feedback, and the rewrite is checked again. If that fails too, the skill is stored with the
request (values hidden) as its scenario, as `learn_phrase` falls back. If System 1 cannot be
reached, the skill is kept as proposed with a warning, because the command ran and the person
accepted it. The outcome's `learned` records anything but a clean pass beside "new skill …": "the
scenario of … was rewritten after the System 1 check", or "… keeps the request as its scenario".

### The forgetting rate

`cargo run --release --example bench-translator -- --forgetting` teaches the ten benchmark cases
one at a time. After each lesson it routes the held-out requests of every case taught so far. A
request that routed home after one lesson and does not after the next was forgotten. The rate is
forgotten requests over the chances to forget. It costs about 330 System 1 calls more.

## Measured on 2026-10-03

All runs were live, on scratch brains (`JLI_DB_URL=file:$TMPDIR/…`), in a throwaway git
repository.

**`jli demo`, run twice:** 2 of 5 requests answered instantly, then 4 of 5 (5 of 5 in the second
run). Time spent deciding went from 6.5 s to 2.6 s, then from 4.8 s to 1.4 s. In the second run
all three skills the agent proposed passed the check before they were stored, for example "the
phrase routes to it at 1.00, and 3 phrasings of the closest skills still route where they did".
The second run exited 0.

**A brain with overlapping skills.** Seven requests were run with `-y`; the agent proposed three
skills and each passed the check. Then eleven `teach` commands added `git diff`, `git diff
--stat`, `git diff --name-only` and `du -sh .` with two or three phrasings each. One of them,
`teach "size of each file here" -- du -sh '*'`, matched the template `du -sh {directory}` and so
joined that skill: an example taught by mistake. That made 16 skills, 15 stored examples and 7
distinct accepted requests.

| Run | System 1 calls (cost) | System 2 calls | Result |
| --- | --- | --- | --- |
| `jli eval`, priced first at 22 calls, about $0.0013 | 22 ($0.0014) | 0 | leave-one-out 15: home 87%, stolen 13%, escalated 0%, mean 0.83, worst 0.00 · replay 7: home 100%, mean 0.96, worst 0.71 · overall 91% home, 9% stolen |
| `jli eval -n --json`, twice | 44 | 0 | the same verdicts both times; largest change in one phrasing's home confidence 0.02 |
| `jli eval -n --optimise` | 70 ($0.0046) | 5 | ranked git-status 0.37, disk-usage-summary 0.33, git-diff-line-by-line 0.31. git-status: two rewrites discarded, because each made "which files differ from the last commit" (git-diff-name-only) fall to 0.43 and 0.41. disk-usage-summary: two discarded, score 0.926 → 0.926. git-diff-line-by-line: kept, 0.848 → 0.875. Nothing applied: the brain file was byte-identical afterwards (`cmp`) |
| `jli eval --optimise --json` | 80 ($0.0053) | 6 | git-status rewrite 1 discarded (0.869 → 0.864); rewrite 2 **kept**, 0.869 → 0.900, adding a counter-example each to git-diff-line-by-line and git-diff-name-only. The other four rewrites were discarded |
| `jli eval` after | 22 | 0 | overall 91% home, stolen 9% → 5%, escalated 0% → 5%, mean 0.87 → 0.88. "what did I edit" (git-diff-line-by-line) moved from stolen by git-diff-name-only at 0.24 to escalated at 0.37 |
| `jli eval undo` | 0 | 0 | scenario of git-status restored and both counter-examples removed. A second `undo` answered "No optimisation to undo." with exit 1 |

The scenarios, before and after:

- git-status: "the user wants to know the staged and unstaged changes in a repo" became "the user
  wants a summary of which changes are staged and which are unstaged in the repository".
- disk-usage-summary: the agent's first rewrite in the dry run proposed dropping the mistaught
  "size of each file here" and adding a counter-example to `ls -lh`. The gate scored it flat, and
  discarded it (see the known gaps).

**The new `matrix_rewrite` schema** ([ADR 4](0004-language-models.md)) was called 11 times with
Mercury 2.5. Every reply parsed against the schema, in 1.1 s to 3.1 s. Every reply had a
scenario. Three proposed dropping an example (each time the mistaught one), and nine proposed one
to three counter-examples.

**Write-time checks.** `teach "tell me about my changes" -- git diff --stat --summary`, a broad
phrasing next to three `git diff` skills, passed: "the phrase routes to it at 0.96, and 8
phrasings of the closest skills still route where they did". A declined route, driven with
`expect`, left two rows: `system1 list-files 1.0 accepted=0 corrected`, then `taught
list-files-with-details` with no confidence. In auto mode the suggester's 1.4 s was logged in
`ms_system2`. `jli stats` then showed 1 of 5 routed requests right the first time, 2 declined or
corrected, and 2 never judged.

**The benchmark** ([ADR 6](0006-learning-modes.md)) with the new check: manual 73% routed and 70%
right; semi 100% and 100%; semi kept all ten translated rows. That is the recorded shape. With
`--forgetting`, manual forgot 1 of 95 (1%): "what were the latest 5 commits" stopped routing home
after "who changed this repo the most" was taught. Semi forgot 0 of 135. The whole run made 556
System 1 requests (about $0.035) and about 20 translator calls.

The acceptance checks 1 to 8, 10 and 12 of [ADR 9](0009-port-to-rust.md) were re-run and held.
Check 10 was driven with `expect`. Checks 9 and 11 were not re-run.

## Consequences

- The brain now has a measure of whether it is right as well as fast, from its own evidence and
  at about $0.00006 a phrasing. An optimisation run on a brain of 16 skills cost half a cent in System 1 calls; its six System 2
  calls are not priced.
- `--optimise` changes the brain without asking anyone, and that is deliberate. What limits the
  harm: only the scenario, at most three of the skill's own examples, and at most three
  counter-examples on its competitors can change; every change must win on the measurement and
  cost no other skill a phrasing in scope; every change is recorded and can be undone.
- A row taught or proposed is checked against the skills it is closest to, not the busiest ones.
  A check costs 1 to 17 System 1 calls, against 9 before. Most checks are 2 rounds of about
  300 ms each. The extra calls are made only when a neighbour phrasing has stopped routing home.
- The `--json` field `msSystem2` now includes the suggester's time in auto mode.
- **Known gap:** a stored example is taken as ground truth. An example taught by mistake can never
  be dropped through the gate: the probe set is fixed so that dropping hard phrasings cannot game
  the score, and so dropping a wrong one cannot raise it either. On 2026-10-03, "size of each file
  here" joined `du -sh {directory}` because `du -sh '*'` matched its template. The agent proposed
  dropping it, and the gate discarded the change as flat.
- **Known gap:** the gate checks the skill and its competitors, not the whole matrix. A rewrite
  could take a phrasing of a skill that is in none of the three lists of competitors.
- **Known gap:** rewrites are not repeatable. The dry run and the real run, a minute apart, kept
  rewrites of different skills.
- **Known gap:** a skill taught with `jli teach` has `uses` 0 until it is routed, so its
  importance counts only the person's verdicts.
- **Known gap:** a stored example carries no context, and a replay whose directory is gone runs in
  an inferred one (above). Neither was measured for how much it moves a route.
- **Known gap:** a "no" followed by the same command with another value is marked `corrected`, so
  it counts against the skill's share of right routes, though the skill was right.
- **Known gap:** a request is grouped by host and words. A `jli fix` made after other requests is
  counted as a request of its own.
- **Known gap:** for a command that finishes faster than the check, an agent-proposed skill delays
  the shell prompt by about half a second, and `--json` waits for it. The command's output is not
  held up.
