# 29. Where no browser opens, `jli login --codex --device-auth` signs the Codex CLI in with a code

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version; feedback from the owner
- Supersedes in part: [ADR 28](0028-chatgpt-plan.md), on the account commands, which gain
  `jli login --codex --device-auth`

## Context

The feedback: add `--device-auth` to `jli login` (and `--new`) "so chatgpt login can be done with a
device code too for non-interactive logins when running over ssh or on a remote host".

`jli login` ([ADR 28](0028-chatgpt-plan.md)) ends with the browser returning to a listener on
`127.0.0.1`. Over SSH that address is the remote host's, while the browser runs on the person's own
computer. The URL can be printed and opened elsewhere, but the callback then reaches the wrong
machine. The Codex CLI has `codex login --device-auth` for this case. Codex prints a link and a
one-time code, the person enters the code on any device, and Codex polls until it is entered.

Probed on 2026-10-03:

| Request | Reply |
| --- | --- |
| `POST https://auth.openai.com/api/accounts/deviceauth/usercode`, `{"client_id": "dynamic_agent_client"}` | 400 `invalid_client`, "This app is unavailable. Contact the app owner and try again later." |
| the same, adding `agent_name_hint`, `ext_agent_host_id`, `scope` and `resource` | the same 400 |
| the same, with an unknown `oaiapp_` client id | the same 400 |
| the same, with Codex's client id | 200: `device_auth_id`, `user_code`, `interval` as the string `"5"`, and an `expires_at` 15 minutes later |
| `GET https://auth.openai.com/.well-known/openid-configuration` | grant types `authorization_code` and `refresh_token` only; no device authorization endpoint |

[Sign in with ChatGPT](https://developers.openai.com/siwc/token-sharing-open-source/sign-in) documents
only the browser and a loopback callback. For a remote VM, its
[self-hosted VMs](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms) page
says to sign in on a computer with a browser and copy the credential file over SSH, keeping the VM's
own host id.

Even if OpenAI accepted jli's client, a new registration (`--new`, or a first `jli login`) could not
finish with a device code. The reply once the code is entered is the Codex binary's
`CodeSuccessResp`, a struct of three fields (`authorization_code`, `code_challenge` and
`code_verifier`, in its strings) with no client id. A new registration's
client id comes back only in the browser's callback. A saved registration could use such a flow,
but this host has none to try it with (ADR 28's `jli login` has not completed live).

## Decision

`jli login --codex --device-auth` (the two flags in either order) runs the installed Codex CLI's
`codex login --device-auth`, then does what `jli login --codex` does. It is Codex's own sign-in,
under Codex's own client and name. jli never asks for a code under Codex's client id itself: using
the file Codex saved is one thing, signing in as Codex is another.

- `exec::codex_device_login` starts it. stdin is inherited. Codex's stdout goes to jli's stderr, so
  stdout keeps only jli's answer, and the link and code reach the person even with `--quiet` or
  `--json`, as the sign-in URL does.
- When Codex exits 0, jli reads `$CODEX_HOME/auth.json` as `--codex` does. Codex and jli read the same
  `CODEX_HOME`, because the child inherits jli's environment.
- When Codex exits otherwise (a code never entered, ctrl-c, a refusal), the run stops with
  "`codex login --device-auth` did not sign in (exit status: 1), so jli changed nothing." (exit 1,
  `configuration`). `jli.json` is not written.
- Without Codex on `PATH`: "Cannot find the Codex CLI. Install it to sign in with a code, or run
  `jli login` where a browser can open." (exit 1, `configuration`).
- `jli login --device-auth` and `jli login --new --device-auth` are refused before any account is
  read: "OpenAI gives a sign-in code to the Codex CLI but not to jli: run jli login --codex
  --device-auth to sign Codex in with a code and use its account, or jli login where a browser can
  open." (exit 1, `usage`).
- Under `-n`, `--codex --device-auth` is refused like the other sign-ins, and `--device-auth` without
  `--codex` gets the refusal above, since it signs in to nothing. Any other set of words, such as
  `login --device-auth now` or `login --new --codex`, is still a request.

| Command | What it does |
| --- | --- |
| `jli login --codex --device-auth` | sign the Codex CLI in with a code entered on another device, then use its account |

**Measured on 2026-10-03.** With a fake `codex` on `PATH` and a scratch `JLI_CHATGPT_DIR` and
`CODEX_HOME`: `--json login --codex --device-auth` printed only the JSON outcome on stdout, with what
the fake printed on stdout and stderr both on stderr. The flags in the other order signed in the same
way. A fake that exited 1 left `jli.json` unwritten. Without `codex` on `PATH`, and for
`--device-auth` without `--codex`, the run stopped with the messages above. Then with the real Codex
CLI 0.160.0 and a scratch `CODEX_HOME`: Codex's "Follow these steps to sign in with ChatGPT using
device code authorization", the link `https://auth.openai.com/codex/device` and a one-time code that
expires in 15 minutes all came out on stderr, and stdout stayed empty while Codex polled. Nobody
entered the code; the run was stopped after 20 s, and no Codex process was left running.

## Consequences

- jli can use a ChatGPT plan on a host where no browser opens, as long as the Codex CLI is installed
  there.
- The sign-in is Codex's: it replaces whatever `$CODEX_HOME/auth.json` held, Codex itself is then
  signed in to that account, and requests go to the Codex backend, as after `jli login --codex`.
- jli's own sign-in still needs a browser on the same machine.

## Known gaps

- No sign-in with a code has completed live. Only the steps up to the code being shown have run.
- It needs the Codex CLI. Codex's prompt is Codex's own text, colours included, whatever jli's
  settings.
- jli's own sign-in has no route for a remote host. The documented route, copying the credential file
  and keeping the host's own id, is not built. If OpenAI enables device codes for registered
  `oaiapp_` clients, a saved registration could use them, but a new one still could not.
- A person who uses Codex with another account on that host is moved to the one signed in here.
