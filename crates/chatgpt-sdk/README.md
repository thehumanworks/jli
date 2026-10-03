# chatgpt-sdk

Sign in with ChatGPT from Rust, and run Responses API requests on the user's ChatGPT plan.

An open-source or locally run app can let people pay for model requests with the ChatGPT Plus or
Pro plan they already have, instead of an API key. This crate implements the documented flow,
[ChatGPT plan usage for open-source apps](https://developers.openai.com/siwc/token-sharing-open-source):
dynamic client registration in the browser, a loopback callback, PKCE, ID token verification,
credentials saved the way the documentation describes, refresh and revocation, and streamed
Responses requests with typed events and errors.

It also reads the sign-in the Codex CLI already keeps, for apps whose users have run `codex login`.

Everything is blocking. HTTP goes through a small `HttpClient` trait; the default uses `ureq` with
rustls.

## Sign in, then ask

```rust,no_run
use std::time::Duration;
use chatgpt_sdk::auth::{AuthorizationRequest, LoopbackListener, OAuthConfig, Registration};
use chatgpt_sdk::{Client, CredentialStore, InputItem, ReasoningEffort, ResponseRequest, UreqClient};

fn main() -> chatgpt_sdk::Result<()> {
    let store = CredentialStore::open("/home/me/.config/my-app/chatgpt")?;

    // 1. Sign in: a new registration the first time, the saved one after that.
    let listener = LoopbackListener::bind()?.app_name("My App");
    let registration = match store.active()? {
        Some(saved) => Registration::returning(&saved),
        None => Registration::new("My App"),
    };
    let request = AuthorizationRequest::new(OAuthConfig::default(), registration, store.host_id()?, listener.redirect_uri())?;
    println!("Continue with ChatGPT: {}", request.url());
    let callback = listener.wait(request.state(), Duration::from_secs(300))?;
    let credentials = request.complete(&UreqClient::new(), &callback)?;
    store.save(&credentials)?;
    store.set_active(Some(&credentials.client_id))?;

    // 2. Ask, on the user's plan. The session refreshes the token and saves it.
    let client = Client::new(store.session(credentials.client_id, OAuthConfig::default()));
    let reply = client.create(
        &ResponseRequest::new("gpt-6-luna")
            .input(InputItem::user("Why is the sky blue?"))
            .reasoning_effort(ReasoningEffort::None),
    )?;
    println!("{}", reply.text);
    Ok(())
}
```

`cargo run --example sign_in -- <dir>` runs the same flow, and `cargo run --example codex` streams
an answer with the Codex CLI's sign-in.

## What it covers

| Part | What it does |
| --- | --- |
| `auth::AuthorizationRequest` | the authorization URL for a new registration (`dynamic_agent_client`, `agent_name_hint`) or a returning one (`id_token_hint`, `login_hint`), with a fresh state, nonce and S256 challenge; `complete` checks the callback, exchanges the code, verifies the ID token and returns the credentials |
| `auth::LoopbackListener` | the `http://127.0.0.1:<port>/auth/callback` listener, on port 1455 or any free one, which answers the browser with a page and turns away a callback that belongs to another attempt |
| `auth::verify_id_token` | RS256 against the published key set, then issuer, audience, expiry and nonce |
| `CredentialStore` | one record per registration in the documented format, written atomically with mode 0600; the host id (`urn:uuid:…`) made once; the active account; sign-out that revokes the refresh token and keeps the client id |
| `Session` | the saved sign-in as a credential: refreshed five minutes before expiry and after a 401, serialised across processes with a lock file, signed out when the refresh token is unusable |
| `CodexAuth` | the Codex CLI's `auth.json` as a credential for the Codex backend |
| `Client` | `models()`, and `stream()` / `create()` for Responses requests, always with `store: false` and `stream: true` |
| `StreamEvent`, `Accumulator`, `Completion` | typed stream events; the text assembled from the stream, since the final event may carry no output |
| `Error::recovery` | the documented action for each failure: retry later, sign in again, manage usage, not eligible, fix the request |

## Two backends

| Credentials | Backend | Base URL |
| --- | --- | --- |
| `Session`, from Sign in with ChatGPT | `Backend::Api` | `https://api.openai.com/v1` |
| `CodexAuth`, from `codex login` | `Backend::Codex` | `https://chatgpt.com/backend-api/codex` |

A token works only on its own backend. On 2026-10-03, a Codex token sent to the public API was
refused with "Missing scopes: api.responses.write", and the same token on the Codex backend
completed a structured request with `gpt-6-luna` in 1.4 s. The Codex backend takes the account id
as a `chatgpt-account-id` header, answers some errors as `{"detail": "…"}`, and lists models only
for a recent `client_version` (`ClientBuilder::client_version`).

`CodexAuth` reads the file on every request, so a refresh Codex makes is picked up. It refreshes
only a token that has expired, the way Codex does, and writes the new tokens back with every other
field kept.

## The plan routes' rules

- Requests are streamed and unstored. The client sends `store: false` and `stream: true`.
- No `system` messages: use `instructions` or a developer message. `temperature`,
  `max_output_tokens`, `previous_response_id`, `metadata` and the other fields the plan route
  rejects are not in the typed request.
- A request succeeds only on `response.completed`. A usage limit can arrive as
  `response.failed` after text has streamed; `Recovery::ManageUsage` says to pause and send the user
  to `MANAGE_USAGE_URL`.
- Show **Using ChatGPT plan** where requests use it, and link to <https://chatgpt.com/settings/usage>.

## Testing

`cargo test -p chatgpt-sdk` runs offline: unit tests per module, and an end-to-end sign-in in
which a fake browser drives the real loopback listener and a fake authorization server checks PKCE
and signs ID tokens with a test key. `cargo test -p chatgpt-sdk --test live -- --ignored` calls the
real Codex backend with the Codex CLI's sign-in.

## Known gaps

- The Sign in with ChatGPT browser flow has been tested offline only: the authorization endpoint
  answers a scripted client with HTTP 403, so the live flow needs a person in a browser.
- The Codex refresh is made as the Codex binary makes it (a JSON body with `client_id`,
  `grant_type` and `refresh_token`, to `https://auth.openai.com/oauth/token`), read from the binary
  and tested offline. It has not run live, because it rotates Codex's refresh token.
- There is no async API, and no WebSocket transport.
- Tools are typed only as functions; others go through `Tool::Raw`.

## Licence

MIT or Apache-2.0, at your option.
