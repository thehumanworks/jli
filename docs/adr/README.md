# Architecture decision records

Each record states one decision: the context it was made in, the decision, and what follows from
it. When a decision changes, write a new record that supersedes the old one, and mark the old one
superseded rather than rewriting it.

Records 1 to 10 were written on 2026-10-02 from `HANDOVER-RUST.md`, the handover that guided the
Rust port (in git history at commit `68a4133`). Records 1 to 8 were decided while building the
TypeScript prototype; 9 and 10 are about the port. Every lesson the handover recorded as "already
paid for", and every known gap, sits in the record of the decision it belongs to.

| # | Decision |
| --- | --- |
| [1](0001-two-systems.md) | A decision model on every request, an agent only when it is unsure |
| [2](0002-brain-in-libsql.md) | The brain is a libSQL database with a fixed schema, and rows are never replaced |
| [3](0003-system-1.md) | System 1 answers three questions in one call, with frozen wording |
| [4](0004-language-models.md) | Language models are reached with one OpenAI-compatible HTTP call and strict JSON Schemas |
| [5](0005-routing-loop.md) | One routing loop, one confirmation rule, and a CLI that composes in pipes |
| [6](0006-learning-modes.md) | Three learning modes, and translated rows are verified by System 1 before they are kept |
| [7](0007-slot-templates.md) | Skills are templates with `{slot}` placeholders, filled cheapest first and always shell-quoted |
| [8](0008-corrections.md) | A "no" or a failed command is a lesson, but only a diagnosis says what kind |
| [9](0009-port-to-rust.md) | Port jli from TypeScript to Rust, keeping its behaviour and its brain |
| [10](0010-synchronous-rust.md) | The Rust version is synchronous, with async confined to the store |
| [11](0011-stated-arguments.md) | Arguments stated in the request are read by a parser, before and instead of any model |
| [12](0012-slot-aliases.md) | A slot answers to several names, and a new name is worked out once and kept |
| [13](0013-learning-fixes.md) | One row per command, the shape of a request, this host's own fix, and failures that are not the command's fault |
| [14](0014-shell-integration.md) | Steps such as `cd` are handed to the calling shell, and paths are expanded before they are quoted |
| [15](0015-user-scope.md) | What is learned belongs to a user, a skill is shared by copy, and parallel writers lose nothing |
| [16](0016-measuring-the-matrix.md) | The matrix is measured against the brain's own evidence, optimised only where the measurement improves, and every new row is checked against its closest skills |
| [17](0017-project-scope.md) | What is learned inside a project stays with it, global skills are still used there, and a project skill becomes global only with approval |
| [18](0018-agent-contract.md) | A contract for agent callers: pinned plans, run in order, and corrections without a terminal |
| [19](0019-one-row-per-command.md) | A flag is never a slot value, a literal skill is made general in place, and a label taken by the same command is joined |
| [20](0020-write-back.md) | An accepted route links its phrasing, a rejected command is refused, and a path is hidden as it was typed |
| [21](0021-hand-edits.md) | One slot name and one phrasing can be changed by hand, written where the router would write the same lesson |
| [22](0022-project-scenarios.md) | A scenario rewritten inside a project is kept for that project, and the global row is left as it was |
| [23](0023-shell-fixes-and-number-words.md) | The install line follows the running shell, the function cleans up after ctrl-c, and a hyphenated number word is not a count |
| [24](0024-optional-slots.md) | A slot can be left out with its flag, a person can give it a default, and choices say how each value is written |
| [25](0025-command-help.md) | `--help` before `--` is jli's own help, each command has its own from one table, and a choice is shown by its name |
| [26](0026-project-flag.md) | `--project` names the project a command acts on, from any directory |
| [27](0027-quiet.md) | A quiet run hides the trace, keeps its warnings, and still shows what a question asks about |
| [28](0028-chatgpt-plan.md) | The agent and the translator can run on the user's ChatGPT plan, through a crate of jli's own |
| [29](0029-device-code.md) | Where no browser opens, `jli login --device-auth` signs in with a code, through Codex's client |
| [30](0030-taught-as-given.md) | What `jli teach` is told is kept as given, with the person's name and description, and a program is never a slot value |
| [31](0031-choices-name-values.md) | A slot's choices name values unless one of them writes a flag, and a value stated for such a slot is taken as written |

## Known gaps, by record

- [2](0002-brain-in-libsql.md): the remote and replica modes have never run against a live remote
  database, and a replica over an existing local file is expected to fail.
- [6](0006-learning-modes.md): auto mode cannot learn from success alone, and the benchmark's
  paraphrases were written by its author.
- [7](0007-slot-templates.md): a glob given as a slot value is quoted and does not expand.
- [8](0008-corrections.md): a correction against a strongly matching seeded skill may not stick, and
  two correction flows have not been tested live.
- [11](0011-stated-arguments.md): a stated value has no escapes, and hidden requests were measured on a handful
  of live cases, not on the benchmark.
- [12](0012-slot-aliases.md): an unknown name is only noticed as `name=value`, and a skill with no
  slots still blocks on any argument.
- [13](0013-learning-fixes.md): the duplicate check was verified offline only, hiding is verbatim
  and case-sensitive, and hidden examples were measured on the demo and six requests, not on a
  benchmark built for it.
- [14](0014-shell-integration.md): handed-over steps that are not tried first count as a clean
  exit, the trial runs in bash whatever the calling shell is, fish has only been tested offline,
  jli's own session hands its steps over only when it ends, and `~user` is not expanded.
- [15](0015-user-scope.md): an older jli ignores `--user-id` and `JLI_USER_ID` (and routes the flag
  as words of a request), a share cannot be undone except by `forget`, and the safe writes have not
  run against a live remote.
- [16](0016-measuring-the-matrix.md): a stored example is ground truth, so one taught by mistake cannot
  be dropped through the gate (it can now be dropped by hand with `jli forget-phrase`, ADR 21); the
  gate checks a skill's competitors, not the whole matrix; rewrites are not repeatable; examples carry
  no context, and a replay in a directory that is gone runs in an inferred one; a `jli fix` made after
  other requests counts as a request of its own.
- [17](0017-project-scope.md): an older jli ignores `.jlirc` and teaches the global skills inside a
  project; a lesson about a global skill blocks only the phrasing it was learned from, and has no
  path to global but `teach --global`; a stop between `generalize`'s two writes leaves both rows; a
  project id cannot be renamed; there are no per-project settings; and the `generalization` prompt
  was measured on six live requests.
- [18](0018-agent-contract.md): a dry run needs a writable brain; plan rows are never deleted; a plan
  does not notice that its skill changed after it was made; a proposal learned through `jli run`
  skips the System 1 check; `jli run` refuses a plan with a `cd` even under the shell function; a plan
  blocked on an unknown argument runs without it once its slots are filled; `corrections` is shown
  nowhere; and the claim, like the rest of the cloud setup, has run only against local files.
- [19](0019-one-row-per-command.md): teaching a command a literal skill runs costs a translator call;
  a literal skill made general keeps its old label, and its old value as a default; only its own
  command makes it general, so a template taught with another value is a row beside it; inside a project a
  global literal skill is not made general, and the router still stores a more general proposal beside
  a literal skill; two agents' proposals for one command named differently still make two rows, and a
  writer that joins a row drops its own scenario and slot names; a value that begins with `-` is never
  read back out of a command; a seed that names a value is never made general; once a skill is
  chosen, a flag still reaches its slot as a value the agent works out, a System 1 pick of an entry
  named `-…`, or a default; the router writing an agent's flag into its command was verified offline
  only; and the compare-and-swap has run only against local files.
- [20](0020-write-back.md): a refused repeat ends the correction with no second chance for the agent;
  a command that does the same thing in other words is not recognised as the rejected one; a
  confident System 1 route leaves a counter-example of its skill in place; and a path's written form
  is found with the environment as it is when the phrase is worked out, never for a bare `~` or `~user`.
- [21](0021-hand-edits.md): `jli alias` and exactly three words is read as the command, never as a
  request; a yes to a route of a forgotten phrasing to its skill below 0.95 makes it an example again
  and lifts the counter-example, so the edit is undone (at 0.95 or above nothing is written); `jli
  eval` still replays an accepted request in those words as evidence for the skill; there is no alias
  for one project only; and the edits have run only against local files.
- [22](0022-project-scenarios.md): a project's scenario has no path to the global row; a project
  with one does not see later global rewrites, and only `eval undo` or `forget --global` drops it; a
  rewrite in a project cannot drop a global row's phrasing; a counter-example already listed as an
  example in the project's lesson is not measured as it is written; a jli with ADR 17 but not ADR 22
  still rewrites the global row from a project; and it was run live on one skill of one scratch brain.
- [23](0023-shell-fixes-and-number-words.md): whether fish removes the hand-off file after a ctrl-c
  is not measured; with no shell among jli's ancestors the install line still follows `$SHELL`, as
  `jli shell-init` with no argument does; and digits joined by `-` are still numbers.
- [24](0024-optional-slots.md): an older jli renders a slot with choices as its bare value
  (`cargo build release`); the models mark a flag's value optional unevenly (13 of 15 for the
  translator, 3 of 6 for the agent), and can still call a needed flag optional; a number word fills an
  optional count even when it counts something else; a choice is found only by the word that names
  it; nothing checks a value left out by hand or a choice's text; the translator's scenario can still
  be narrower than an optional value makes the skill; a plan's left-out slot cannot be given to `jli
  run`; and `jli slot` inside a project ran live once.
- [25](0025-command-help.md): a request with `-h` or `--help` as a word of its own before `--` is now
  help (quote the request, or put those words after `--` with a word before it); help is plain text
  under `--json`; the word after a bare `--project` is taken as its id unless it names a command,
  which ADR 26 does not do; the interactive session still routes `--help`; and a command's usage
  refusal is written apart from the table.
- [26](0026-project-flag.md): the space form takes the next word as the id, so `jli --project deploy
  the app` scopes into a project called `deploy`, and the note on stderr is the only guard; a mistyped
  id that names another real project gets no note; `jli skills --project=<id>` names but does not show
  the global skills the project learned about; the projects named are those with skills of their own;
  `jli project --project=<id>` cannot say where the checkouts are; and an older jli reads the flag as
  words of the request, so its `teach` puts it in a global phrasing.
- [27](0027-quiet.md): `-q -n` prints nothing; a quiet run nobody could confirm does not name the
  command or the plan it stored; the agent's answer to a request that is not a command, and System 1's
  "not a command", are hidden under `--quiet`; and `jli fix` prints its learned and dry-run lines
  under `--quiet` and `--json`.
- [28](0028-chatgpt-plan.md): `jli login` has not completed live, since the authorization endpoint
  refuses a scripted browser, so the public `api.openai.com/v1` route has not been called with such a
  token; only Codex's sign-in on the Codex backend ran live, a route that is not the documented one
  and whose model list depends on an aging `client_version`; refreshing Codex's sign-in has run
  offline only; the prompts were measured on Mercury, and on `gpt-6-luna` only the acceptance checks,
  the benchmark and about twenty agent requests ran; the model is chosen per host, not per user or
  project; a returning sign-in's URL carries the saved ID token as a hint, printed without a browser
  and in the process list while the opener runs; a Codex refresh is locked against other jli
  processes, not against Codex; and an older jli ignores the sign-in.
- [29](0029-device-code.md): the refresh (after 240 hours) and the revocation have not run live; jli
  asks as Codex, which OpenAI could stop, and
  the page names Codex; a refusal on the other device may look like a code not entered yet until
  it expires; a sign-in refused as another account, or replaced by the same account, is dropped
  without being revoked; an older jli cannot read `"source": "device"` in `jli.json`; and there is
  one device sign-in per host.
- [30](0030-taught-as-given.md): a request for another value still reaches a command kept as taught
  when the value is a plain word (`pull from upstream` ran into `git pull origin`), and a command with
  a value is not made general without `--mode=semi`; the value check misses a value spelled otherwise
  and a number above ten as a word; the description is not kept apart from the scenario, so `jli eval
  --optimise` can rewrite it away; a name taken between its check and its claim becomes `<name>-x`, and
  a global skill's name is refused, not shadowed, inside a project; a template whose program is a slot
  recognises no command, a program is named only as a whole word, and its place is only the start of a
  step or after `|`, `;`, `&`, `(` or a backtick; the risk is still the translator's guess; a one-word
  phrase verifies near the threshold; `jli fix -- <command>` and a typed correction follow the
  configured mode; and an older jli keeps the old matching, teaches in semi mode, and reads `--name`
  and `--description` as words of the phrase.
- [31](0031-choices-name-values.md): a slot whose choices write no flag cannot be closed, so a
  mistyped value runs; only a text that begins with `-` counts as a flag; a quoted string with no name
  is still not handed to a slot with choices (`jli "tts 'Hello'"` leaves the text out); a plain
  `--default` on a text slot is still the agent's last resort; a default of a slot with choices must
  be one of them; and an older jli still refuses a value that names no choice.
