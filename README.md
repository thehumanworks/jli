# jli

Say what you want in plain language. `jli` runs the right command, and learns how you say it.

```
$ jli show me the newest files
⚡ system 1 · jev-1.13.0 · 289 ms · 9 skills in the matrix
   list-files-by-time  ████████████████████  0.98
→ ls -ltr  risk: safe
```

## How it works

An explicit phrase choice is checked first. Otherwise, two models handle fast and slow thinking.

- **System 1** is TypeSafe's `jev` decision model. One call, about 250 ms, answers three typed
  questions against the request: a `choice` over every known skill, a `noul` on whether a command is
  wanted at all, and a `score` on how risky the intent is.
- **System 2** is a reasoning agent: GPT-6-Luna on your ChatGPT plan after `jli login` (about 3 s),
  or Mercury 2.5 with an API key (about 2 s). It is only called when System 1 has no confident
  answer. It proposes a reusable skill, not a one-off command.
- **The translator** is a fast model, the same one (2 to 5 s on the plan, under 1 s for Mercury). It
  never sits on the request path. It writes the scenario of what you teach, or in semi mode rewrites
  it into a general matrix row, and in auto mode it diagnoses failures and suggests alternatives.
- **The brain** is a SQLite database. Each row is a skill. The rows are rendered into the `choice`
  criteria on every request, so the decision matrix *is* the database.

Learning is writing to that database:

| What happened | What is stored | Effect |
| --- | --- | --- |
| System 2 proposed a command and it ran, with exit 0 or a failure that was not the command's fault ([ADR 13](docs/adr/0013-learning-fixes.md)) | a new skill with the request as its first example, its values hidden, once System 1 has checked it; a command a skill already runs adds the phrasing to that skill instead, and drops it from that skill's counter-examples ([ADR 20](docs/adr/0020-write-back.md)) | System 1 answers next time |
| You said yes to "Run it?" | the phrasing, as an example of that skill and no longer a counter-example of it | that phrasing becomes more confident |
| You said no and typed the right command | a durable scoped binding to the right skill, a durable rejection of the wrong one, and examples for paraphrases | the exact phrase bypasses System 1 next time |
| You ran `jli teach` | the phrase, and the command exactly as you wrote it, under the name you gave it with `--name`; a fast model writes only when to run it, from your phrase and your `--description`, and it is kept once System 1 has checked it ([ADR 30](docs/adr/0030-taught-as-given.md)) | the exact phrase bypasses System 1 next time, and runs what you taught |
| You ran `jli fix` | a durable rejection and, when a replacement is chosen, a binding; a repeated rejected proposal is refused (`already_rejected`) | as for a "no" and the command you typed |
| You wrote `dir=src` for a slot called `directory` | `dir` as another name for that slot | `dir=` is read with no model next time |
| You ran `jli alias` or `jli forget-phrase` | a slot name added or removed; a phrasing moved to the counter-examples | the same, by hand, for one name or one phrasing |
| You ran `jli slot` | a slot's default, whether it may be left out with its flag, and its choices: names for values, or the only values it takes when one writes a flag | an unsaid value runs as you said it should, with no model ([ADR 24](docs/adr/0024-optional-slots.md), [ADR 31](docs/adr/0031-choices-name-values.md)) |

One command can hold many phrasings. Teaching a phrase for a command that a skill already runs adds
the phrase to that skill. If that skill runs only the one command, as one taught by default does,
and you teach it again with `--mode=semi` and the translator finds a value in it, the skill is made
general in place and keeps its phrasings: teaching `git log --oneline -n 3` again turns it into
`git log --oneline -n {count}`, with 3 as the count's default. Two jli processes teaching one command
at once also end up with one skill. See [ADR 19](docs/adr/0019-one-row-per-command.md).

Explicit choices survive later learning, even after examples expire. `jli bindings` shows their
IDs, scopes and source; `jli bindings --json` includes their commands and timestamps. Revoke one
with `jli bindings remove <id>` (`--global` for an inherited global rule). Removing a project rule
reveals a global rule for the same phrase. `forget-phrase` also rejects a matching bound phrase.
Bindings preserve the existing value parser, confirmation and saved-plan behavior. A correction of
an argument value keeps the skill eligible. See [ADR 32](docs/adr/0032-durable-corrections.md).

## Teaching

`jli teach` keeps what you teach. The command is stored exactly as you wrote it, under the name you
give it, and the phrase is its first example:

```
$ jli teach --name=pull --risk=modifies --description "bring the current branch up to date with origin" pull -- git pull origin
  gpt-6-luna · using ChatGPT plan described it in 1.9 s: the user wants to integrate changes from the current branch's configured upstream on the remote named origin into the current branch, bringing it up to date with origin
  Verified with System 1: the phrase routes to it at 0.57, and 1 phrasings of the closest skills still route where they did.
✚ learned "pull" means pull (git pull origin)
  new skill
$ jli -n pull
  Explicit phrase binding: pull.
→ git pull origin  risk: modifies
```

A fast model writes one thing: the scenario, the sentence System 1 reads to know when to run the
skill, so that other ways of asking reach it. It is given your phrase, your `--description` of when
and why to run it, and the skills it must be told apart from, and it cannot change the command. A
scenario that leaves out a value the command runs with, a file, a number or a port, is sent back,
since it would take a request for another value and run this one. Then System 1 checks it as it
checks every new row. If it fails twice, or no model can be reached, the scenario is your own words:
`the user wants: pull. bring the current branch up to date with origin`.

- `--name=<name>` is refused, before any model is asked, if a skill of that name runs another
  command, or another skill already runs this one: one command has one skill, and a skill is never
  renamed. `jli forget` the other skill first, or teach without `--name` to add the phrase to it.
- `--description` matters most between neighbours. Taught `mise run deploy-preview` and
  `mise run deploy-preview-shared` with no description, "put up the preview the team reviews" went
  to the first; described as "my own preview, which only I look at" and "the preview the whole team
  reviews before a release", five requests in other words each reached the right one at 0.98 or more.
- A command with a value stays that value. `teach "show the last 3 commits" -- git log --oneline -n 3`
  runs `-n 3`, and "the last 10 commits" goes to the agent. `--mode=semi` (or `auto`) on the command
  line lets the translator turn values into slots as well, `git log --oneline -n {count}`, and
  `--mode=manual` asks no model at all. `jli config mode` and `JLI_MODE` do not reach `jli teach`.

A skill whose command starts with a slot, such as `{editor} {file}`, never takes a command it was
not routed to: `git pull` is not that skill with `git` for the editor. See
[ADR 30](docs/adr/0030-taught-as-given.md).

## Learning modes

Set with `jli config mode <manual|semi|auto>`, `JLI_MODE`, or `--mode=`. The default is `semi`. The
mode decides how jli learns from what happens: a "no" and the command you type, a failed command,
and `jli fix`. `jli teach` keeps what you teach, and only `--mode=` on its own command line changes
that (see [Teaching](#teaching)).

| Mode | Who writes the matrix row | What a "no" does | What a failed command does |
| --- | --- | --- | --- |
| `manual` | you: phrase and command are stored as given | asks you to type the right command | nothing |
| `semi` | the translator generalises what you teach | asks you to type the right command | nothing |
| `auto` | the translator, from what happens | offers numbered alternatives; pick one or say what you expected | diagnoses it, and offers alternatives only if the command was wrong |

**Translated rows are checked, not trusted.** After the translator writes a row, and before it is
stored, System 1 is asked to route your original phrase with the row in the matrix, then up to eight
phrasings of the skills closest to it: those System 1 ranked just behind it, then those that share
the most words. The row is kept only if your phrase lands on it, none of the others do, and none
that routed home without it stops doing so. A skill the agent proposes gets the same check while
its command runs. It gets one retry with that feedback, and
then falls back to your own words: the phrase, and your `--description` if you gave one. Slots are
kept only if filling them back in reproduces the exact command you taught.

**A failure is not a wrong route.** `git status` outside a repository fails, and it was still the
right command. In auto mode the translator reads stderr and gives one of three verdicts: wrong
command, wrong value, or an environment problem. Only the first two lead to a correction.

`cargo run --release --example bench-translator`, run from the repository root, compares manual,
`jli teach`'s default and semi on held-out paraphrases. One run on 2026-10-04 with `gpt-6-luna`, ten
commands, thirty requests written by the author of the benchmark. "Wrong" is a request that reached
its skill and still rendered another command:

| | manual: routed, right, wrong | `jli teach`: routed, right, wrong | semi: routed, right, wrong |
| --- | --- | --- | --- |
| fixed commands (21 requests) | 100%, 100%, 0% | 100%, 100%, 0% | 100%, 100%, 0% |
| commands with a value (9 requests) | 0%, 0%, 0% | 0%, 0%, 0% | 100%, 100%, 0% |
| all (30 requests) | 70%, 70%, 0% | 70%, 70%, 0% | 100%, 100%, 0% |

The gain of semi mode is entirely in commands that carry a value, such as a count, a file or a port:
it turns them into slots. A raw phrase already generalises well for fixed commands. `jli teach`
keeps the command, so a request for another value goes to the agent rather than running the taught
one; its scenario pays off where two skills are close, which this benchmark does not test (see
[Teaching](#teaching)). A run costs about 300 System 1 requests (about $0.02) and two dozen
translator calls. Earlier runs, including the TypeScript prototype's, are in
[ADR 6](docs/adr/0006-learning-modes.md) and [ADR 30](docs/adr/0030-taught-as-given.md).

## Measuring the matrix

The learning curve in `jli stats` counts a confident wrong route as a win. `jli eval` measures
whether routes are right, from the brain's own evidence and with the real System 1. The matrix
measurement remains unassisted by bindings. The report also counts repeated corrections per 100
judged requests in its latest 2,000 scoped events, with stated values hidden. The matrix probes are:

- every stored example is routed with itself held out of the matrix, and should come home;
- every accepted request that was not declined or corrected later is routed again, and should
  reach the skill that was accepted.

Each phrasing comes **home**, is **stolen** by another skill, or **escalates** below the threshold.
The report lists, per skill, the least confident phrasings, even those that came home, and who
stole what. The number of System 1 calls and their price are printed before anything is spent,
about $0.00006 a call. `jli stats` shows the trend, and how often a request was right the first
time, with declined and corrected attempts counted.

`jli eval --optimise` ranks skills by expected gain: how much each is used and how often the
person accepted its routes, times how far its phrasings fall short of coming home. System 2 then
rewrites the top rows. A rewrite is kept only if the measured score of the skill and its
competitors rises and no other skill loses a phrasing. Nobody is asked: the measurement is the
approval. Each kept change is recorded, and `jli eval undo` puts it back. With `-n` nothing at all
is written. Inside a project the rewrite of a global skill's scenario is kept for that project only
([ADR 22](docs/adr/0022-project-scenarios.md)). On a scratch brain of 16 skills on 2026-10-03, a measurement took 22 System 1 calls
($0.0014). An optimisation run took 80 System 1 and 6 System 2 calls, kept one rewrite of six, and
cut the steal rate from 9% to 5%: the phrasing it freed
now escalates to the agent instead, so the share that came home stayed at 91%. See [ADR 16](docs/adr/0016-measuring-the-matrix.md).

## Confirmation

`jli` shows the command and asks "Run it?" every time, unless you approve up front. There are two levels, each a flag or an environment variable set to `1` or `true`:

- `-y`, `--yes` or `JLI_YES`: run without asking, except destructive commands, which still ask.
- `-Y`, `--yolo` or `JLI_YOLO`: run without asking, destructive commands included.

Answer no and it asks what it should have run: type the command, type `?` to ask the agent, or
press enter to skip.

`-q`, `--quiet` or `JLI_QUIET=1` hides the decision trace, System 1's table with it, but a question
still shows what it asks about: the command and its risk before "Run it?", the numbered options
before "Pick a number". Warnings stay, so a quiet run that does nothing says why. `--json` hides the
warnings too, since its outcome says the same. See [ADR 27](docs/adr/0027-quiet.md).

```
$ JLI_QUIET=1 jli list the files here
→ ls  risk: safe
? Run it? [Y/n]
```

System 1's pick is used when its confidence reaches the threshold, 0.5 by default. Below that the
agent is asked. Set the threshold with `jli config confidence 0.7` (stored in the brain, so shared
across hosts), `JLI_THRESHOLD=0.7` (`JLI_CONFIDENCE` also works), or `--confidence=0.7` for one run.

## Try it

```sh
export TYPESAFE_API_KEY=…      # System 1

cargo build --release          # the binary is target/release/jli
cargo install --path .         # optional: puts `jli` on your PATH
jli login                      # System 2 and the translator on your ChatGPT plan, or:
export OPENROUTER_API_KEY=…    # on OpenRouter instead (here it is injected by fnox)
jli demo                       # watch a fresh brain learn in a throwaway git repo
jli which branch am I on
jli                            # interactive session
jli help
cargo test --workspace         # offline: System 1, the language models and ChatGPT are faked
```

The demo asks five things, then asks them again in different words. In a real run, three requests
that needed the agent the first time (4 to 6 s each) were answered by System 1 in about 250 ms the
second time, and the time spent deciding fell from 12.5 s to 1.4 s.

## Commands

| Command | What it does |
| --- | --- |
| `jli <request>` | route, run, learn |
| `jli <command> --help` | that command's usage and the options that matter to it, with no brain opened and no model asked (also `-h`, `jli help <command>`); `jli --help` lists everything. Only before `--`: after it, `--help` is the command's |
| `jli -n <request>` | dry run: show the command, execute and learn nothing, and keep it as a plan for `jli run` |
| `jli -y <request>` | run without asking; never approves destructive commands (also `--yes`, `JLI_YES=1`) |
| `jli -Y <request>` | run without asking, destructive commands included (also `--yolo`, `JLI_YOLO=1`) |
| `jli -q <request>` | hide the decision trace; a question still shows what it asks about (also `--quiet`, `JLI_QUIET=1`) |
| `jli config confidence <0..1>` | set the confidence threshold |
| `jli config mode <manual\|semi\|auto>` | set the learning mode |
| `jli config model <slug>` | the model the agent and the translator run on your ChatGPT plan, from `jli models`; kept on this host |
| `jli login` | continue with ChatGPT in the browser: the agent and the translator run on your ChatGPT plan |
| `jli login --new` | the same with another ChatGPT account than the one saved here |
| `jli login --device-auth` | sign in with a code entered on another device: over SSH, or wherever no browser opens |
| `jli login --new --device-auth` | the same with another ChatGPT account than the one signed in that way |
| `jli login --codex` | use the ChatGPT account the Codex CLI is signed in with |
| `jli logout` | stop using your ChatGPT plan; jli's session with it is revoked |
| `jli models` | the models your ChatGPT plan has, the one in use marked |
| `jli --json <request>` | machine-readable outcome with captured output, for agents; an error is one JSON object too |
| `jli run <planId> [<planId> …] [name=value …]` | run stored plans exactly as shown, in order, with no model call; stop at the first that fails |
| `jli bindings [--global] [remove <id>]` | inspect durable phrase choices and rejections, or revoke a rule in the selected scope; supports `--json` and `-n` |
| `jli skills` | every skill, with learned phrasings and counter-examples |
| `jli skills --project` | only the project's own skills, not the global ones |
| `jli stats` | routing mix, latency, the learning curve, how often routes were right first time, and the `jli eval` trend |
| `jli eval [<label>] [--limit N]` | route every stored example with itself held out, and replay accepted requests; priced before it spends |
| `jli eval --optimise` | the agent rewrites the weakest rows; a rewrite is kept only if the measurement improves, and inside a project only for that project |
| `jli eval -n [--optimise]` | measure, and show the rewrites and their effect, but write nothing |
| `jli eval undo [<label>]` | put back what the last optimisation changed (inside a project, the last one made there) |
| `jli history` | recent requests and how each was routed |
| `jli teach "<phrase>" -- <command>` | link a phrase to the command as you wrote it; a fast model writes only when to run it; repeat to add more phrasings |
| `jli teach --name=<name> "<phrase>" -- <command>` | the same, and call the new skill `<name>`; refused if a skill of that name runs another command, or another skill already runs this one |
| `jli teach --description="<when and why>" "<phrase>" -- <command>` | the same, with your own words about when and why to run it, which the model keeps to |
| `jli teach --mode=semi "<phrase>" -- <command>` | let the translator turn the command's values into slots too (`--mode=manual`: no model at all) |
| `jli teach --global "<phrase>" -- <command>` | the same, from inside a project, for every project |
| `jli fix` | the last command run here was wrong: unlearn it and ask the agent |
| `jli fix <eventId>` | the same for one of your events, from any directory (an event made in a project, from inside that project or with `--project=<id>`) |
| `jli fix <eventId> -- <command>` | that was wrong, this is right: learn it as a "no" and the command at the prompt would, and return a plan of it (`--run` runs it) |
| `jli fix <eventId> --expected "<words>"` | unlearn it and ask the agent with what you expected; returns a plan (`--run` runs it) |
| `jli forget <label>` | delete a skill; inside a project only the project's own, `--global` for a global one |
| `jli forget-phrase <label> "<phrase>"` | take one phrasing back from a skill and keep it as a counter-example, so it is not learned straight back; inside a project, for a global skill, in that project only (`--global` for every project) |
| `jli alias <label> <slot> <name>` | give a slot another name for `name=value`; `--remove` takes one away. Inside a project an alias of a global skill holds in every project; `--global` reaches a global skill that a project skill of the same label hides |
| `jli slot <label> <slot> --default=<value>` | the value a slot takes when the request names none; also `--optional` or `--required`, `--flag=<flag>`, `--choice=` and `--kind=`. Written where `jli alias` writes |
| `jli slot <label> <slot> --choice=release --choice=debug` | names for a slot's values, each put in the command as its own name; `--choice=release=--release` puts other text, `--choice=debug=` nothing, and `--no-choices` takes them away. A slot whose choices write a flag takes only them; any other also takes a value you state |
| `jli project` | the project this directory is in, from the nearest `.jlirc` (or the one `--project` names), and how many skills are its own |
| `jli project init [<id>]` | make this directory a project: what jli learns here stays with it |
| `jli --project=<id> …` | any command, in project `<id>`, as if run in one of its directories (also `--project <id>`); a bare `--project` is the project of this directory's `.jlirc` |
| `jli generalize [<label>]` | the agent proposes how to make project skills global; each is written only once you approve (`--all-projects`, `--approve-all`, `-n`) |
| `jli shell-init [zsh\|bash\|fish]` | print the shell function that lets `cd`, `export` and `source` change your shell |
| `jli --user-id=<id> …` | any command, as another user with their own skills, history and settings (also `JLI_USER_ID`) |
| `jli share <label> --to <user>` | give another user a copy of one of your skills |

The decision trace goes to stderr and the command's output to stdout, so `jli` composes in pipes.

## For agents

An agent drives `jli` with `--json` and no terminal. It can review a route, then run exactly that:

```sh
jli --json -n how many words are in notes.txt   # {"commands": ["wc -w notes.txt"], "planId": 1, "blockedCode": "dry_run", …}
jli --json run 1                                 # [{"ran": true, "exitCode": 0, "stdout": "6 notes.txt\n", "eventId": 1, …}]
jli --json run 2 3 4                             # in order; after the first failure the rest say "not_run"
jli --json run 5 message="fix the parser"        # a value the plan was missing (see "missing" in its outcome)
jli --json fix 6 -- ls -la                       # event 6 was wrong, ls -la was right: learned, and a plan of it returned
```

A dry run, and a run that stopped for lack of a yes or a value, stores a plan for an hour. `jli run`
asks no model and routes nothing; running a plan by id approves it as `--yes` does, so a destructive
plan still needs `--yolo`, and a plan runs once. Every outcome says why nothing ran as a code
(`blockedCode`), gives the event and plan ids, the phrase that is learned, each slot's value and
source, System 1's runners-up, stdout and stderr apart, and auto mode's diagnosis. An error is
`{"error": "<kind>", "errorMessage": …}`, and a failure after the command ran is reported beside
`ran: true`, never instead of it. See [ADR 18](docs/adr/0018-agent-contract.md).

## Commands that change your shell

jli runs commands in a shell of its own, so on its own a `cd` or an `export` would change nothing
you can see. Install the shell function once:

```sh
eval "$(jli shell-init zsh)"        # in ~/.zshrc; bash: the same with bash, in ~/.bashrc
jli shell-init fish | source        # in ~/.config/fish/config.fish
```

Then a step such as `cd`, `pushd`, `export`, `unset`, `source` or `alias`, and every step after it,
is handed to your shell, which runs it once jli exits: `jli go to src` leaves you in `src`. jli
passes it through a temp file, not stdout, so output still goes down a pipe, and the function
returns jli's exit code, or the status of what your shell ran. A plain `cd` to a directory that does
not exist fails in jli, with the shell's own message, before anything is handed over. A ctrl-c while
jli runs leaves no temp file behind.
Without the function such a step is not run and nothing is learned from it: jli says how to install
the function in the shell it was started from (a bash started from zsh gets `~/.bashrc`) and exits 3,
and `--json` lists the steps in `callerShell` for an agent to run itself.
See [ADR 14](docs/adr/0014-shell-integration.md) and [ADR 23](docs/adr/0023-shell-fixes-and-number-words.md).

## Slots

Skills are templates such as `git log -n {count}` or `git commit -m {message}`. A flag is never read
as a value, in a command you teach or one the agent proposes: `cd -` is a skill of its own, not
`cd {directory}` with `-` ([ADR 19](docs/adr/0019-one-row-per-command.md)). Nor is a program:
`git pull` is not `{editor} {file}` with `git` for the editor, and a program the agent gives such a
skill is written into the command unless the request names it
([ADR 30](docs/adr/0030-taught-as-given.md)). A value you state is passed as written, by a parser
and no model:

```sh
jli commit message="fix the parser"            # name=value, for the slot called message
jli commit this with a message: 'fix the parser'   # the slot's name as a label
jli commit with message: fix the parser        # unquoted, a label takes the rest of the request
jli commit "fix the parser"                    # a quoted string goes to the next open text slot
jli tag this name=v1.2.0 message="first cut"   # several arguments, each to its own slot
```

The trace shows `{message} = fix the parser (from request)`. A stated value is hidden from routing,
so the words of a commit message cannot change which command is picked.

You do not have to remember what a slot is called. Each slot answers to other names as well, such
as `dir` and `path` for `{directory}`. The model that writes a skill suggests some. When you use a
name the skill does not know, `msg=wip` for `{message}`, the agent is asked once which slot it
means; the value stays yours, and after the run the name is kept, so next time the parser reads it.
A name that fits no slot stops the run and lists the command's arguments. `jli skills` shows each
slot as `{name}` in the command, with its other names below it. See
[ADR 12](docs/adr/0012-slot-aliases.md).

A name can also be given or taken away by hand, for example when the agent read one wrong:

```sh
jli alias count-files directory folder            # folder=docs now fills {directory}
jli alias --remove count-files directory d        # and d= no longer does
jli forget-phrase du-sh "size of each file here"  # a phrasing taught to the wrong skill
```

Both take `--json` and `-n`, and refuse an unknown skill, slot or phrasing with exit 1. A forgotten
phrasing is kept as a counter-example, so the next request in those words is not learned back; `jli
teach` with the right command undoes it. See [ADR 21](docs/adr/0021-hand-edits.md).

Anything not stated is filled by the cheapest method that works: a number in the request, a System 1
`choice` among the real files or branches, the agent, and last the slot's default. See
[ADR 11](docs/adr/0011-stated-arguments.md).

### Optional slots, defaults and choices

A value given with a flag is often not part of the job: `git log` lists commits without `-n 3`. Such a
slot is optional. When the request names no value, the placeholder and its flag are left out, with no
model call:

```
$ jli show the recent commits
→ git log --oneline  risk: safe
  {count} is left out with -n: it is optional, and nothing gave it a value.
$ jli show the last 5 commits
→ git log --oneline -n 5  risk: safe
```

The translator and the agent mark such a slot optional, with its flag, when the command does the same
job without it. A model never leaves out a value that has no flag before it, never gives an optional
slot a default, and never writes choices: those are yours.

`jli slot` sets them. A default is what an unsaid value means, and it wins over leaving the slot out.
Choices are names for a slot's values, and a default among them is used with no model. A choice puts
its own name in the command, so for a skill that runs `cargo install --path . --profile {profile}`:

```sh
jli slot install-app profile --choice=release --choice=dev --default=release
```

A value you state that names no choice is taken as written, quoted as any value is: `profile=bench`
runs `--profile bench`.

When the command needs other text than the word you say, write that text after a second `=`. It is
put in the command as you wrote it and never quoted, and an empty text puts nothing there. For a
skill that runs `xai tts --voice {voice} --normalize -o {output} {text}`, where the text is left out
so that it can be piped in:

```sh
jli slot tts output --choice=output=output.mp3 --default=output
jli slot tts text --choice=empty= --default=empty
jli tts                               # xai tts … -o output.mp3
jli tts output=hello.mp3 text='Hi'    # xai tts … -o hello.mp3 Hi
```

A slot whose choices write a flag takes only its choices, since any other value in that place would
mean nothing to the command. For `cargo build`, `release` is `--release` and `debug` is nothing, so
teach the command with a placeholder and give it choices:

```sh
jli --mode=manual teach "build the app" -- 'cargo build {profile}'
jli slot cargo-build-profile profile --choice=release=--release --choice=debug= --default=release
jli build the app                    # cargo build --release
jli build the app in debug mode      # cargo build
jli build the app profile=fast       # stops: {profile} takes release or debug, not fast
jli slot git-log count --optional --flag=-n   # leave the count out, and -n with it
jli slot list-largest-files count --default=5
```

A choice is found by name (`profile=debug`), as a label, or as a word of its own in the request ("a
debug build"). Each setting is written with `=`, so `--flag=-n` stays one word. `jli skills` shows
what each slot has: `{profile}: release → --release, debug → nothing; default release`, and a choice
that puts its own name in the command as that name alone, with a slot that takes other values saying
so: `{profile}: release, dev, or another value; default release`. An older jli refuses a value that
names no choice, and one older still reads a slot with choices as a plain one, and writes
`cargo build release`. See [ADR 24](docs/adr/0024-optional-slots.md) and
[ADR 31](docs/adr/0031-choices-name-values.md).

A file value has a leading `~` and any `$NAME` or `${NAME}` expanded by jli before it is quoted, so
`jli list dir=~/notes` runs `ls /Users/you/notes`, and the trace says
`(from request, ~/notes expanded)`. Nothing is ever run to expand a value, an unset variable is left
as written, and a text value such as a commit message is never expanded. A path named anywhere in
the request, not only in the working directory, can be picked for a file slot. See
[ADR 14](docs/adr/0014-shell-integration.md).

What is learned is the shape of a request, not the instance: every value the command ran with is
hidden in the stored phrasing wherever it appears as a whole word, so `how many lines are in
notes.txt` is kept as `how many lines are in …`. A file value is hidden as the request wrote it, so
`~/notes/n.txt` goes too, though the command ran with `/Users/you/notes/n.txt`. See
[ADR 13](docs/adr/0013-learning-fixes.md) and [ADR 20](docs/adr/0020-write-back.md).

## One brain for every host

Storage goes through libSQL, so the same code runs on a local file or a cloud database.

```sh
# default: local file at ~/.jli/brain.db
export JLI_DB_URL=libsql://your-db.turso.io   # every host reads and writes one cloud brain
export JLI_DB_TOKEN=…

# or keep a local replica and sync it, for local-speed reads
export JLI_SYNC_URL=libsql://your-db.turso.io
```

What one host learns, every host knows. Each skill and event records the host it came from.
The cloud modes are wired but have not been run against a live Turso database yet.

## Users

One brain can hold several users, so an agent can learn its own way of using jli without changing
yours. This is isolation, not authentication.

```sh
jli --user-id=agent teach "count the todo items" -- grep -c '^- ' todo.md   # or JLI_USER_ID=agent
jli share count-lines-matching-pattern --to default   # as agent: give the default user a copy
```

Without `--user-id` or `JLI_USER_ID` the user is `default`, which is everything a brain held before
users existed. A user sees, routes against, teaches, fixes and forgets only their own skills; their
history, stats and settings are their own; a new user starts from the seed skills with the built-in
settings. `jli share` copies one skill as it is now, with its phrasings and slot names but not its
counter-examples, and refuses if the other user already has that label or that command. Several
jli processes can write one brain at once without losing a lesson, and two that teach the same
command at once get one skill ([ADR 19](docs/adr/0019-one-row-per-command.md)). An older jli on the same brain
sees only the default user. See [ADR 15](docs/adr/0015-user-scope.md).

## Projects

"Run the tests" is `cargo test` in one repository and `npm test` in the next. Make each one a
project, and what jli learns in one stays there:

```sh
jli project init                     # writes .jlirc: project = shop-api-4k2x9q (commit it)
jli teach "run the tests" -- cargo test     # kept for this project
jli generalize                       # the agent proposes which project skills could be global; you approve each
```

A project is named by the id in the nearest `.jlirc` at or above the working directory (up to
`$HOME`), or by `--project`, never by a path, so every checkout is the same project. Inside one,
System 1 sees the project's skills and your global ones together, and a project skill wins a label
both hold. A new skill is the project's; a phrasing or a counter-example for a global skill is kept as the project's
own lesson about it, so correcting `cargo test` to `npm test` in one project never reaches another.
A scenario `jli eval --optimise` rewrites there for a global skill is kept as the project's own too,
and `jli skills` tags it `scenario rewritten here`.
`teach --global` and `forget --global` reach the global skills from inside a project. Settings stay
yours; history, stats, and the event and plan ids that `jli fix` and `jli run` take are the
project's. `jli generalize` checks each proposal with System 1 in
the project, among the global skills and against your other projects, and writes it only when you
say yes. Without a `.jlirc` or `--project` nothing changes. See [ADR 17](docs/adr/0017-project-scope.md) and
[ADR 22](docs/adr/0022-project-scenarios.md).

`--project=<id>`, or `--project <id>`, runs any command as if from a directory of that project,
wherever you are, so a project can be reached without a checkout of it:

```sh
jli skills --project=shop-api              # only that project's own skills, no global ones
jli teach --project=shop-api "run the tests" -- cargo test   # kept for shop-api
jli --project=shop-api -n start the app    # routed against shop-api's skills and your global ones
jli skills --project                       # on its own: this directory's project, from its .jlirc
```

`--project` on its own outside a project is an error, and so is an id that breaks the `.jlirc` rule.
In the space form the id is the next word unless it starts with `-`, so `jli --project deploy the
app` means a project called `deploy`: put `--project` last, or write `--project=<id>`. A project jli
has nothing for yet gets one line on stderr, `jli has nothing for project shop-ap yet.`, with the
projects it does know, in case the id was mistyped; the command still runs. `jli project init` takes
no `--project`. See [ADR 26](docs/adr/0026-project-flag.md).

## Layout

| File | Role |
| --- | --- |
| `src/system1.rs` | builds the matrix from the database and asks Jev; the only module that uses `typesafe-jev` |
| `src/system2.rs` | the agent: structured proposals, free-text slot filling, naming an unknown argument's slot, rewriting a weak row, and proposing how a project skill could be global |
| `src/translator.rs` | writes the scenario of a command kept as taught, or turns a taught phrase and command into a general matrix row |
| `src/learn.rs` | keeps a taught command and its name as given, writes a row once System 1 has verified it, or makes a literal skill general in place, and settles a skill the agent proposed |
| `src/eval.rs` | measures the matrix against its own examples and history, optimises it, and checks a new row against its closest skills |
| `src/suggest.rs` | auto mode: diagnoses a failure and offers alternatives |
| `src/llm.rs` | where every language model request goes: one HTTP call to an API-key endpoint, or a Responses request on a ChatGPT plan |
| `src/account.rs` | `jli login`, `jli logout`, `jli models` and the model on the plan, kept on this host in `~/.jli/chatgpt` |
| `crates/chatgpt-sdk/` | a library of its own: Sign in with ChatGPT, saved credentials, Codex's sign-in, sign-in with a device code, and streamed Responses requests, with typed errors |
| `src/router.rs` | the loop: decide, escalate, confirm, run, write back; and a stored plan through the same confirmation and write-back |
| `src/plan.rs` | plans: what a route pins, and `jli run` |
| `src/store.rs` | the brain: skills, events, settings, plans, corrections and durable phrase rules per user and project in libSQL, behind a runtime that never leaves the module |
| `src/project.rs` | finds, reads and writes the `.jlirc` that names a project |
| `src/generalize.rs` | `jli generalize`: checks the agent's proposal for a project skill and writes it once approved |
| `src/hand.rs` | `jli alias`, `jli forget-phrase` and `jli slot`: one slot name, one phrasing or one slot's settings changed by hand, where the router would write it |
| `src/args.rs` | reads the arguments a request states: `name=value`, labels and quoted text |
| `src/slots.rs` | fills, expands and shell-quotes `{slot}` values, leaves out an optional slot with its flag, writes a choice's text, holds a model's slots to what it may say, and matches a command back to its template, never with a flag or a program for a value |
| `src/shell.rs` | the shell function, which steps it must run in your shell, and which shell jli is running under |
| `src/seed.rs` | the original hard-coded matrix, now only the seed for an empty brain |
| `src/exec.rs` | runs the steps with `bash -c`, watching stderr in auto mode and for a new proposal, tries builtins before they are handed over, and reads the process table to find the shell jli was started from; for `jli login`, opens the browser |
| `src/cli.rs` | argument parsing, subcommands, the interactive session, and errors as JSON |
| `src/help.rs` | the help, written once as a table of commands, options and variables, and which help a command line asks for |
| `src/ui.rs` | the trace and its warnings on stderr, and the one owner of stdin, which shows each question after what it asks about |
| `src/demo.rs` | the two-pass learning demo |
| `src/tests.rs` | the router and learning loop, offline, against fake models and a scripted prompt |
| `src/tests/bindings.rs` and `tests/bindings.rs` | durable correction routing, isolation, revocation and concurrency, including separate offline CLI processes |
| `src/tests/eval_tests.rs` | `jli eval`, the optimiser, undo, and the check a new row gets, offline |
| `examples/bench-translator.rs` | manual, `jli teach`'s default and semi on held-out paraphrases, and with `--forgetting` the forgetting rate |
| `examples/bench-context.rs` | one phrase taught to two commands in two projects: whether a correction in one undoes the other |
| `docs/adr/` | the design decisions, and the lessons and known gaps behind them |

## How it talks to the world

The app makes two kinds of HTTP call and uses one database. It is synchronous throughout.

- **Language models:** `POST {base}/chat/completions` in the OpenAI-compatible shape, with a
  `response_format` of type `json_schema`, sent with `ureq`. Schemas are plain JSON Schema objects
  in the source. `JLI_LLM_BASE_URL` with `JLI_LLM_API_KEY` is used when both are set, then your
  ChatGPT plan once you have run `jli login`, then OpenRouter when `OPENROUTER_API_KEY` is set, then
  the Vercel AI Gateway. `JLI_PROVIDER=chatgpt|openrouter|gateway|custom` picks one instead.
- **Your ChatGPT plan:** the same prompts and schemas as a streamed, unstored Responses request,
  through the `chatgpt-sdk` crate in `crates/`. After `jli login` it goes to
  `https://api.openai.com/v1/responses`; after `jli login --device-auth`, or `jli login --codex` with
  the Codex CLI's sign-in, to the Codex backend. The trace says `using ChatGPT plan`, and a usage
  limit stops the run with a link to <https://chatgpt.com/settings/usage>. See
  [ADR 28](docs/adr/0028-chatgpt-plan.md) and [ADR 29](docs/adr/0029-device-code.md).
- **System 1:** `POST https://api.typesafe.ai/v1/systemone` with `{ model, state, questions }`,
  through the `typesafe-jev` crate, with a 10 second timeout and two retries.
- **Storage:** libSQL through its first-party crate. It is async, so `store.rs` owns a
  current-thread Tokio runtime and blocks on it; nothing outside that module is async.

Models are set with `JLI_AGENT_MODEL` and `JLI_TRANSLATOR_MODEL`. Without them, both are
`gpt-6-luna` on a ChatGPT plan (or what `jli config model` picked) and `inception/mercury-2.5`
with an API key.

## Your ChatGPT plan

```
$ jli login
  Continue with ChatGPT in the browser that just opened.
Signed in with ChatGPT as you@example.com.
The agent and the translator now run on gpt-6-luna using your ChatGPT plan. jli models lists the others.
$ jli models
  gpt-6.1-sol  GPT-6.1-Sol
  gpt-6-luna   GPT-6-Luna  ← in use
  …
$ jli config model gpt-6.1-sol
```

`jli login` registers jli with your ChatGPT account in the browser, the first time, and signs in to
it again after that; it asks to use your plan for jli's requests, and nothing changes if you say no.
The sign-in is kept on this host only, in `~/.jli/chatgpt` (`JLI_CHATGPT_DIR`), never in the brain.
If the Codex CLI is signed in, `jli login --codex` uses its account instead, and leaves Codex's file
as it is. On a host where no browser opens, such as over SSH, `jli login --device-auth` prints a link
and a one-time code to enter on any other device, and waits up to 15 minutes. OpenAI gives such a
code only to the Codex CLI's client, so jli asks for it the way Codex does, as other open-source
agents do: the page names Codex, device code sign-in has to be on in ChatGPT's security settings,
and requests go to the Codex backend. The sign-in is jli's own, in `~/.jli/chatgpt/device.json`;
Codex's file is never touched. `jli logout` stops, and `jli login --new` adds another account. Requests count toward your
plan's usage; review and limit them at <https://chatgpt.com/settings/usage>. System 1 still needs
`TYPESAFE_API_KEY`.

## Contributing

`AGENTS.md` has the rules for changing this code, for people and for coding agents. `mise install`
then `hk install` sets up the git hooks: format, strict clippy, the `ast-grep` architecture rules in
`rules/`, and the offline tests before a push. `mise run check` runs all of it.

## Design decisions

The reasons behind all of this are in [docs/adr](docs/adr/README.md): why there are two systems,
why the prompts are frozen, why rows are never replaced, how slots are quoted, and what is known
not to work yet. `jli` started as a TypeScript prototype; [ADR 9](docs/adr/0009-port-to-rust.md)
records the port, and the acceptance checks to re-run after changing routing, learning or storage.

