# 29. Where no browser opens, `jli login --device-auth` signs in with a code, through Codex's client

- Status: accepted
- Date: 2026-10-03
- Origin: the Rust version; feedback from the owner
- Supersedes in part: [ADR 28](0028-chatgpt-plan.md), on the account commands, which gain
  `jli login --device-auth` and `jli login --new --device-auth`, and on where a plan's requests go

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
| the same, with Codex's client id (`app_EMoamEEZ73f0CkXaXp7hrann`) | 200: `device_auth_id`, `user_code`, `interval` as the string `"5"`, and an `expires_at` 15 minutes later |
| `POST …/deviceauth/token` with a made-up `device_auth_id` | 404 `deviceauth_not_found` |
| `POST https://auth.openai.com/oauth/revoke`, a made-up refresh token, as a form | 200, an empty body |
| the same as JSON | 401 `invalid_refresh_token`, "Could not validate your refresh token." |
| `GET https://auth.openai.com/.well-known/openid-configuration` | grant types `authorization_code` and `refresh_token` only; no device authorization endpoint |

[Sign in with ChatGPT](https://developers.openai.com/siwc/token-sharing-open-source/sign-in) documents
only the browser and a loopback callback. For a remote VM, its
[self-hosted VMs](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms) page
says to sign in on a computer with a browser and copy the credential file over SSH. Codex's
[authentication page](https://learn.chatgpt.com/docs/auth) describes device code sign-in as a beta
for Codex, turned on in the person's ChatGPT security settings (or by a workspace admin).

So jli's own registration cannot get a code. It could not use one for a new registration even if
OpenAI allowed it: the reply once the code is entered is the Codex binary's `CodeSuccessResp`, three
fields (`authorization_code`, `code_challenge` and `code_verifier`, in its strings) with no client
id, and a new registration's client id comes back only in the browser's callback.

Other open-source agents sign in with a code anyway, by asking as Codex does with Codex's public
client id: opencode (`packages/opencode/src/plugin/openai/codex.ts`, "ChatGPT Pro/Plus (headless)")
and pi (`packages/ai/src/auth/oauth/openai-codex.ts`) hard-code it, poll `deviceauth/token`, read 403
and 404 as "not entered yet" (pi also `slow_down` and `deviceauth_authorization_pending`), exchange
the code at `https://auth.openai.com/oauth/token` with `redirect_uri` set to
`https://auth.openai.com/deviceauth/callback`, and keep the tokens in their own store.

A first version of this record ran the installed Codex CLI's `codex login --device-auth` from
`jli login --codex --device-auth`. The owner chose to drop it for the approach above: it needed Codex
installed, and it signed Codex itself in again.

## Decision

`jli login --device-auth` signs in with a one-time code, as pi and opencode do: it asks for the code
with Codex's client id, polls until the person enters it, and exchanges it as Codex does. The tokens
are jli's own, kept in `~/.jli/chatgpt/device.json` (`JLI_CHATGPT_DIR`), mode 0600, in Codex's
`auth.json` format; Codex's own file is never read or written for it. They are tokens of Codex's
kind, so requests go to the Codex backend, as after `jli login --codex`, and `jli models` lists that
backend's models.

`chatgpt-sdk` gains the flow, as a published crate would have it:

| Part | What it does |
| --- | --- |
| `DeviceCode::request` | `POST {issuer}/api/accounts/deviceauth/usercode` with `{"client_id": CODEX_CLIENT_ID}`; reads `interval` as a string or a number (5 s when missing, 1 s at least) and `expires_at` (15 minutes when missing); the page is `{issuer}/codex/device` |
| `DeviceCode::wait` | polls `{issuer}/api/accounts/deviceauth/token`: 403, 404 and `authorization_pending` are "not entered yet", `slow_down` adds 5 s to the pause (RFC 8628), any other error stops at once; ends with `CodeExpired` at `expires_at`, counted by the clock and by the pauses taken, since an unknown code also gets 404; then exchanges the code, form-encoded, with no `resource` |
| `CodexAuth::create` | writes the tokens in Codex's format, the account id from the ID token's `https://api.openai.com/auth` claim |
| `CodexAuth::revoke` | a JSON body to `https://auth.openai.com/oauth/revoke` (`CODEX_REVOKE_TOKEN_URL_OVERRIDE`), as `codex logout` does; a token the server no longer accepts counts as revoked |
| `CodexAuth::sign_in_command` | the command an ended sign-in tells the person to run; `codex login` unless set |
| `FakeChatGpt` | the two device endpoints, refusing any client but Codex's, a person who enters the code after the polls scripted with `script_device_poll`, `/oauth/token` and `/oauth/revoke` |

The ID token is read without verifying its signature, as `CodexAuth` reads Codex's: it comes straight
from the token endpoint over TLS, and only names the account.

In jli:

- The code reaches stderr even with `--quiet` or `--json`, as the sign-in URL does, with the page,
  the minutes left, and three warnings: the page names Codex, device code sign-in may have to be
  turned on in ChatGPT's security settings, and the code is never to be given to anyone.
- A code not entered before it expires, or refused, stops the run with "Sign-in did not complete:
  …" (exit 1, `configuration`), and nothing is written.
- Without `--new`, a sign-in as another ChatGPT account than the one in `device.json` (by account
  id, else by email) is refused: "you signed in as two@example.com, a different ChatGPT account than
  the one saved here (one@example.com). Run `jli login --new --device-auth` to use it instead." With
  `--new`, the new sign-in replaces it, and the replaced session is revoked.
- `jli.json` records `"source": "device"`. An ended sign-in stops the run with "Your ChatGPT sign-in
  has ended. Run `jli login --device-auth` to sign in again, or `jli logout` to stop using it."
- `jli logout` revokes the session and removes `device.json`. If the revocation fails, it says
  to log out of all devices in ChatGPT's security settings.
- Under `-n`, the new shapes are refused like the other sign-ins. Any other set of words, such as
  `login --device-auth now` or `login --codex --device-auth`, is a request.

| Command | What it does |
| --- | --- |
| `jli login --device-auth` | sign in with a code entered on another device: over SSH, or wherever no browser opens |
| `jli login --new --device-auth` | the same with another ChatGPT account than the one signed in that way |

**Measured on 2026-10-03.** The probes above, and `cargo test -p chatgpt-sdk --test live a_device_code
-- --ignored`: `DeviceCode::request` got Codex's client a code, polled every 5 s, valid for 15
minutes.

## Consequences

- A person on a remote host signs jli in to their ChatGPT plan with any phone or laptop, without
  Codex installed and without touching Codex's sign-in.
- jli presents itself to OpenAI as Codex for this sign-in. OpenAI could refuse it at any time, and the
  page the person sees names Codex, not jli.
- These requests go to the Codex backend, not the documented `api.openai.com/v1` route.
- Requests carry `originator: jli`, as with the other sign-ins.

## Known gaps

- No sign-in with a code has completed live: the code was issued, and no one entered it. The
  exchange and the revocation follow the Codex binary, pi and opencode, and are tested offline;
  the revocation's JSON body was chosen because the endpoint read it (`invalid_refresh_token`), and
  has not ended a real session.
- A refusal on the other device cannot be told from a code not entered yet unless the server says
  so; otherwise the wait runs to the code's expiry.
- A sign-in refused because it is another account, and one replaced by a sign-in as the same
  account, are dropped without being revoked.
- An older jli reading `jli.json` with `"source": "device"` stops with "cannot be read", and says to
  run `jli logout` or `jli login`. The file is this host's, not the brain.
- There is one device sign-in per host; `--new` replaces it rather than keeping both, unlike
  `jli login --new`, which keeps the first registration.
