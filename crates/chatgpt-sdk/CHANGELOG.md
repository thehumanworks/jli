# Changelog

## 0.1.0 (unreleased)

- Sign in with ChatGPT for open-source apps: dynamic registration and reauthorization, PKCE, the
  loopback callback, ID token verification against the published key set, and the plan usage
  scope check.
- `CredentialStore`: documented credential records, written atomically with mode 0600, a stable
  host id, the active account, refresh serialised across processes, and sign-out with revocation.
- `CodexAuth`: the Codex CLI's sign-in, for the Codex backend.
- `Client`: model lists and streamed Responses requests, with typed requests, events and errors,
  and the documented recovery for each error.
