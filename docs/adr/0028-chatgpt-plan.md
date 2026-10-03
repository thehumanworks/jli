# 28. The agent and the translator can run on the user's ChatGPT plan, through a crate of jli's own

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version
- Supersedes in part: [ADR 4](0004-language-models.md), on every request being one OpenAI-compatible
  call, on the order endpoints are tried in, on the default models, and on a 429 always getting the
  second attempt

## Context

System 2 and the translator needed an API key: OpenRouter's, the Vercel AI Gateway's, or another
compatible endpoint's. Many people who would use jli already pay for ChatGPT Plus or Pro. OpenAI's
[Sign in with ChatGPT](https://developers.openai.com/siwc/token-sharing-open-source) lets an
open-source app use the person's plan for Responses API requests instead: the app registers itself in
the browser with `client_id=dynamic_agent_client`, gets an access token with the scope
`chatgpt.tokens.use.direct`, and sends streamed, unstored requests to `https://api.openai.com/v1/responses`.
No crate on crates.io implemented the flow on 2026-10-03 (searched for `chatgpt`, `siwc`, `codex auth`
and `openai-oauth`; the name `chatgpt-sdk` was free).

A developer's machine often holds a second kind of ChatGPT sign-in: the Codex CLI's, in
`~/.codex/auth.json`. It is issued to Codex's own client with the scopes `openid profile email
offline_access api.connectors.read api.connectors.invoke`. Probed on 2026-10-03 with that token:

| Request | Reply |
| --- | --- |
| `POST https://api.openai.com/v1/responses` | 401, "Missing scopes: api.responses.write" |
| `GET https://api.openai.com/v1/models` | 403, "Missing scopes: api.model.read" |
| `POST https://chatgpt.com/backend-api/codex/responses`, `gpt-6-luna`, a JSON Schema | a stream, 1.6 to 2.1 s in all, the JSON asked for |
| the same, with `stream: false` | 400, `{"detail": "Stream must be set to true"}` |
| the same, with `reasoning.effort: "minimal"` | 400, `unsupported_value`: gpt-6-luna takes none, low, medium, high, xhigh and max |
| the same, with model `gpt-nope` | 400, `{"detail": "The 'gpt-nope' model is not supported when using Codex with a ChatGPT account."}` |
| `GET …/codex/models?client_version=0.50.0`, then `0.160.0` | an empty list, then nine listed models |

On the Codex backend the `response.completed` event carries `output: []`, so the text has to be built
from the stream's deltas. The authorization endpoint answered a scripted request with HTTP 403, so the
browser sign-in cannot be run unattended.

## Decision

### A crate: `chatgpt-sdk`

`crates/chatgpt-sdk` is a library of its own, written to be published (MIT or Apache-2.0, documented,
`missing_docs` denied). It is kept local for now. The repository becomes a Cargo workspace, and the
hooks and `mise run check` lint and test the whole of it. It is blocking, like the rest of jli
([ADR 10](0010-synchronous-rust.md)), and sends HTTP through an `HttpClient` trait whose default uses
`ureq` with rustls, the stack jli already has. It needs Rust 1.89 for `File::lock`, so jli's
`rust-version` rises from 1.85 to 1.89.

| Part | What it does |
| --- | --- |
| `auth` | the authorization URL (a new registration, or a returning one with `id_token_hint`), a fresh state, nonce and S256 PKCE, the loopback listener on `127.0.0.1:1455` (any free port when that is taken), which turns away a callback with another state and keeps waiting, the code exchange, and the ID token checked against the published keys: RS256, issuer, audience, expiry, nonce |
| `CredentialStore` | one record per registration in the documented format, written atomically with mode 0600 in a 0700 directory; a `urn:uuid:` host id made once; the account in use; sign-out that revokes the refresh token, three tries on a temporary failure, and keeps the client id |
| `Session` | a saved sign-in as a credential: refreshed five minutes before expiry and after a 401, under a lock file and with the record read again once the lock is held, so a rotating refresh token is spent once; once the server has rotated it, the new tokens are kept even when the refreshed ID token cannot be checked, since that token is only the next sign-in's hint; an unusable refresh token clears the tokens |
| `CodexAuth` | Codex's sign-in as a credential for the Codex backend (below) |
| `Client` | `models()`, and `stream()` and `create()` for Responses requests, always `store: false` and `stream: true` |
| `Error::recovery` | the documented action for each failure: retry later, sign in again, manage usage, not eligible, enable plan usage, fix the request, check the configuration |
| `testing` (feature) | `FakeChatGpt`: an authorization server, a browser and both backends in one `HttpClient`, for offline tests |

Each token works on one backend, and the credential decides which: `Backend::Api`
(`https://api.openai.com/v1`) for a Sign in with ChatGPT session, `Backend::Codex`
(`https://chatgpt.com/backend-api/codex`, with the account id as `chatgpt-account-id`) for Codex's.

### jli uses it in two modules

`src/llm.rs` makes the requests; `src/account.rs` signs in and out and lists models. A new rule,
`rules/chatgpt-stays-in-llm-and-account.yml`, keeps every other module off the crate, and the rules
that confine `ureq` and printing to jli's own modules no longer look inside `crates/`. The browser is
opened by `exec::open_in_browser` (`open`, or `xdg-open`), the place processes start.

### Where requests go

The first of these that is set up:

1. `JLI_PROVIDER`, when set: `chatgpt`, `openrouter`, `gateway` or `custom`. It names exactly one; if
   that one is not set up, the run stops and says what is missing.
2. `JLI_LLM_API_KEY` with `JLI_LLM_BASE_URL`.
3. The ChatGPT plan, after `jli login` or `jli login --codex`.
4. `OPENROUTER_API_KEY`, then `AI_GATEWAY_API_KEY`, as before.

Signing in is the choice: with an OpenRouter key exported in every shell, `jli login` still moves the
agent and the translator to the plan, and `jli logout` moves them back. A sign-in that has ended (a
refresh token that no longer works, or Codex signed out) stops the run with "Your ChatGPT sign-in has
ended. Run `jli login`… or `jli logout`…". It never falls back to an API key on its own: the person
would pay another way without being told.

### The request on a plan

The same prompts, the same user message and the same strict schemas, in the Responses shape:

| Chat completions (ADR 4) | Responses on a plan |
| --- | --- |
| `messages[0]`, `role: system` | `instructions` (the plan route rejects system messages) |
| `messages[1]`, `role: user`, the input as JSON indented by one space | `input: [{type: message, role: user, content: [{type: input_text, text}]}]`, the same text |
| `reasoning: {enabled: false}` | `reasoning: {effort: "none"}` |
| `response_format: {type: json_schema, json_schema: {name, strict: true, schema}}` | `text: {format: {type: json_schema, name, schema, strict: true}}` |
| the reply in `choices[0].message.content` | the stream's text, once `response.completed` arrives |
| | `store: false`, `stream: true` |

Timeouts and attempts are ADR 4's: 30 s and two attempts for the agent, 8 s for the translator, 10 s
for the suggester. A failure the documentation calls final stops at once, with what to do: a usage
limit (a 429 with `subscription_sharing_usage_limit_exceeded`, or a `response.failed` with it), an
ineligible plan, a request the route refuses (such as an unknown model), a policy that refuses jli,
and an ended sign-in. Anything else gets the second attempt, as before: a transport failure, a stream
that ended early or incomplete, HTTP 408, 409, 429, 500, 502, 503 and 504, a `response.failed` with a
code nobody documented, and a reply that does not parse. What the person is told:

- "Usage limit reached on your ChatGPT plan (…). Manage usage at https://chatgpt.com/settings/usage"
  (`model`, as `--json`'s `error`);
- "Your ChatGPT sign-in no longer works (…). Run `jli login`." (`configuration`; `codex login` for
  Codex's sign-in);
- "gpt-x refused the request: … `jli models` lists the models your plan has." (`model`).

### Models

On a plan, the agent and the translator use `gpt-6-luna` until `jli config model <slug>` picks
another. The slug is checked against the plan's list first. The choice is kept on this host, in
`~/.jli/chatgpt/jli.json`, not in the brain: another host on the same brain may use OpenRouter, where
`gpt-6-luna` is not a model. `JLI_AGENT_MODEL` and `JLI_TRANSLATOR_MODEL` still win over it. With an
API key the default stays `inception/mercury-2.5`.

The models are now named by the `Llm` (`agent_model()`, `translator_model()`), not by process-wide
statics, so a fake transport never looks for a sign-in and the trace can say where requests go:
`◆ system 2 · gpt-6-luna · using ChatGPT plan · no known skill fits`.

### Commands

| Command | What it does |
| --- | --- |
| `jli login` | Continue with ChatGPT in the browser; the saved account again when there is one. Without a browser, the URL is printed to stderr, even with `--quiet` or `--json`. A person who declined plan usage before is asked for consent again. |
| `jli login --new` | the same with another ChatGPT account; the first registration is kept |
| `jli login --codex` | use the account the Codex CLI is signed in with |
| `jli logout` | stop using the plan; jli's own session is revoked, and its client id and host id kept |
| `jli models` | the plan's listed models, the one in use marked (`--json`: slug, name, current) |
| `jli config model <slug>` | the model the agent and the translator run on the plan |

None of them opens the brain. `jli login` and `jli logout` refuse `-n` (exit 1, `usage`): there is no
way to show a sign-in without making it. A damaged `jli.json` stops the commands that read it with
how to mend it, and `jli login` and `jli logout` write it again. After the first sign-in that allows
plan usage, and only then, jli says
"You're using your ChatGPT plan", that requests count toward its usage, and where to manage it.
`jli config` says where requests go: `model  gpt-6-luna  on ChatGPT plan (user@example.com)`.

### Codex's sign-in

It is used only after `jli login --codex`; finding the file is not consent. The file belongs to Codex.
It is read on every request, so a refresh Codex makes is picked up. It is written only when its access
token has expired (Codex's last for ten days): the refresh is made as the Codex binary makes it, a
JSON body with `client_id`, `grant_type` and `refresh_token` to `https://auth.openai.com/oauth/token`
(`CODEX_REFRESH_TOKEN_URL_OVERRIDE` overrides it, as for Codex), and the new tokens are written back
atomically, with every other field kept, to the file a symlink names rather than over the link. If
that write fails, the run stops with "run `codex login`": the old refresh token is already spent. Codex reloads the file before it refreshes and skips its own
refresh when the file changed ("Skipping token refresh because auth changed after guarded reload", in
its binary). A 401 for a token that has not expired is not refreshed: the file is read again, and if
it holds the same token the run stops with "run `codex login`". `jli logout` never touches the file.

### Where it is kept

`~/.jli/chatgpt`, or `JLI_CHATGPT_DIR`, mode 0700: `host-id`, `active`, `accounts/<client id>.json`
(0600) and jli's `jli.json` (which sign-in is used, the model, whether the welcome was shown). Nothing
is written to the brain, whose schema is unchanged ([ADR 2](0002-brain-in-libsql.md)): a credential
belongs to one host.

**Measured on 2026-10-03**, live, with Codex's sign-in on a Pro plan, on scratch brains
(`JLI_DB_URL`, `JLI_CHATGPT_DIR`), `JLI_YES` and `JLI_YOLO` unset.

The crate's live tests (`cargo test -p chatgpt-sdk --test live -- --ignored`): a strict-schema request
to `gpt-6-luna` with effort `none` returned `{"command":"ls -lt | head -n 10","risk":"safe"}` in 1.4 s
(71 tokens in, 24 out, none reasoning); "Say exactly: Hello, world!" streamed in 4 deltas; the model
list held nine listed models; `gpt-does-not-exist` was a 400 whose recovery is "fix the request".

The agent, as `--json -n --agent` on five requests: 3.4, 2.5, 3.2, 5.2 and 3.4 s (median 3.4 s). Then
twice more with a stable `prompt_cache_key` per schema: 3.3, 2.7, 9.1, 2.6, 3.1 and 3.3, 2.7, 2.7,
3.5, 4.3 s (median 3.2 s). The key made no difference that outlasted the spread, so it was not kept.
Mercury's first live proposal took 2.0 s ([ADR 4](0004-language-models.md)).

[ADR 9](0009-port-to-rust.md)'s acceptance checks, with the agent, the translator and the suggester on
`gpt-6-luna`:

| # | Result |
| --- | --- |
| 1 | `ls -ltr`, nothing ran, exit 0 |
| 2 | `git commit -m 'first draft'` |
| 3 | not a command, exit 0 |
| 4 | `git add -A` not run, exit 3 |
| 5 | the agent proposed `find . -type f -name '*.log' -delete` (destructive) in 2.8 s; refused under `-y`, the files untouched |
| 6 | valid JSON, `path` `system1`, the directory in `output` |
| 7 | the translator made `git log --oneline -n {count}`, `{count}` optional with `-n`, in 4.7 s; the second phrasing joined it with no API key and no sign-in |
| 8 | `-n 7` |
| 9 | outside a repository, `git status` approved: an `environment` diagnosis, and the phrasing learned with no counter-example |
| 10 | auto mode, `ls` declined: three numbered options; picking 1 learned `ls -lh` and ran it |
| 11 | an interactive session took two requests with confirmations, then left on ctrl-d |
| 12 | `jli demo`: answered instantly 2 of 5, then 5 of 5; time spent deciding 11.1 s, then 1.3 s |

Check 13 compared brains with the TypeScript version, which is gone.

`bench-translator`, the same day, the translator on each model:

| | semi routed | semi right command | fixed commands right | commands with a value right | rows kept |
| --- | --- | --- | --- | --- | --- |
| `gpt-6-luna` on the plan | 100% | 90% | 86% | 100% | 10 of 10 |
| `inception/mercury-2.5` on OpenRouter | 93% | 80% | 86% | 67% | 9 of 10 |

Both missed `du -sh .` the same way: the translator made `du -sh {directory}` with no default, and the
benchmark, which fills slots with System 1 alone, rendered `du -sh '?'`. Mercury also lost the three
port requests (`lsof -i :3000` kept as taught).

## Consequences

- A person with a ChatGPT plan needs no API key for the agent and the translator. System 1 still needs
  `TYPESAFE_API_KEY`.
- On the plan the agent takes about a second longer than Mercury did, with an occasional 5 to 9 s
  answer, and the translator wrote better rows on the benchmark.
- Requests count toward the person's plan, and stop when its limit, or jli's share of it, is reached.
- The workspace has a second crate to keep sound. It shares jli's HTTP stack, and of its dependencies
  only `thiserror` 2 was not already in jli's tree.

## Known gaps

- `jli login` has not completed live: the authorization endpoint refuses a scripted browser, and the
  consent is the person's to give. The flow is tested offline end to end, a fake browser driving the
  real loopback listener, and the public `api.openai.com/v1` route with such a token has not been
  called. Only Codex's sign-in on the Codex backend ran live.
- The Codex backend is not the documented route, and its model list depends on a `client_version`
  (`0.160.0` on 2026-10-03) that will age.
- Refreshing Codex's sign-in is copied from the Codex binary and tested offline; it has not run live,
  because it rotates Codex's refresh token.
- The prompts were measured on Mercury. On `gpt-6-luna` only the acceptance checks, the benchmark and
  about twenty agent requests have run; `jli eval --optimise` and `jli generalize` have not.
- The model is chosen per host and per ChatGPT plan, not per user or project, and `jli stats` does not
  say which model answered.
- A returning sign-in's URL carries the saved ID token as a hint: printed when no browser opens, and
  in the process list while `open` or `xdg-open` runs.
- A Codex refresh is serialised with other jli processes by a lock on the file, not with Codex, which
  takes no lock; Codex's guarded reload is what keeps the two from spending one token twice.
- An older jli ignores the sign-in and keeps using its API key.
