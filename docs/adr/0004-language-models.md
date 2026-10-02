# 4. Language models are reached with one OpenAI-compatible HTTP call and strict JSON Schemas

- Status: accepted
- Date: 2026-10-02
- Origin: the TypeScript prototype; ported to `ureq` unchanged

## Context

Three callers need a language model with structured output: the agent (System 2), the translator
and the suggester. A provider SDK would tie `jli` to one provider, and each SDK is a dependency to
port. Every provider worth using speaks the OpenAI chat-completions shape.

## Decision

One function, in `src/llm.rs`, sends every request:

```
POST {base}/chat/completions
Authorization: Bearer {key}
X-Title: jli

{ "model": "inception/mercury-2.5",
  "messages": [ { "role": "system", "content": "..." }, { "role": "user", "content": "<JSON input>" } ],
  "reasoning": { "enabled": false },
  "response_format": { "type": "json_schema", "json_schema": { "name": "...", "strict": true, "schema": {} } } }
```

- The user message is the input as JSON indented by one space. The reply is the JSON text in
  `choices[0].message.content`, with any surrounding code fence stripped.
- The endpoint is the first of these that is set:
  1. `JLI_LLM_API_KEY` with `JLI_LLM_BASE_URL`
  2. `OPENROUTER_API_KEY`, with `OPENROUTER_BASE_URL` or `https://openrouter.ai/api/v1`
  3. `AI_GATEWAY_API_KEY`, with `https://ai-gateway.vercel.sh/v1`
- Two attempts. Retry on HTTP 408, 429, 500, 502, 503 and 504, on a timeout or a dropped
  connection, and on a reply that does not parse into the expected shape. Fail at once on any
  other status.
- Schemas use strict mode. Every property is listed in `required`, an optional field is
  `"type": ["string", "null"]` (an `Option` in Rust), and `additionalProperties` is false.
- Schemas and request bodies keep their keys in the order they are written (`serde_json` with
  `preserve_order`), because a structured-output model generates fields in schema order.
- The system prompts and schemas are frozen, like System 1's wording ([ADR 3](0003-system-1.md)).

| Caller | Model | Reasoning | Timeout | Attempts |
| --- | --- | --- | --- | --- |
| Agent: `propose`, `extract` | `JLI_AGENT_MODEL`, default `openai/gpt-5-mini` | `{ "effort": "low" }` | 30 s | 2 |
| Translator | `JLI_TRANSLATOR_MODEL`, default `inception/mercury-2.5` | `{ "enabled": false }` | 8 s | 2 |
| Suggester | the translator's model | `{ "enabled": false }` | 10 s | 2 |

**Lessons paid for:**

- Mercury must run with reasoning disabled. With reasoning on, it stalled for 9 to 16 seconds.
- The Vercel gateway rejects Mercury on the author's account because of a zero-data-retention
  policy, so the translator needs OpenRouter.
- `gpt-5-mini` occasionally hangs on OpenRouter, hence the 30 second timeout and the retry. It
  happened again on 2026-10-02: one proposal took 33 s, because the first attempt timed out.

## Consequences

- Any compatible provider works by setting two variables.
- A hung agent can hold the prompt for up to a minute: two attempts of 30 seconds.
- When the translator cannot be reached, learning falls back to storing the phrase as given
  ([ADR 6](0006-learning-modes.md)). When the suggester cannot be reached, a "no" falls back to
  asking the user for the command ([ADR 8](0008-corrections.md)).
