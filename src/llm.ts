/**
 * The only way this app talks to a language model: one POST to an
 * OpenAI-compatible `/chat/completions` endpoint with a JSON Schema for the
 * reply. No SDK, so a port to another language is one HTTP call.
 *
 *   OPENROUTER_API_KEY (+ OPENROUTER_BASE_URL)   OpenRouter, the default
 *   AI_GATEWAY_API_KEY                           Vercel AI Gateway, the fallback
 *   JLI_LLM_API_KEY + JLI_LLM_BASE_URL           any other compatible endpoint
 */
export interface Endpoint {
  baseUrl: string;
  apiKey: string;
}

type Env = Record<string, string | undefined>;
const clean = (value: string | undefined): string | undefined => value?.trim() || undefined;

export const endpoint = (env: Env = process.env): Endpoint => {
  const explicit = clean(env.JLI_LLM_API_KEY);
  const explicitUrl = clean(env.JLI_LLM_BASE_URL);
  if (explicit && explicitUrl) return { apiKey: explicit, baseUrl: explicitUrl };
  const openrouter = clean(env.OPENROUTER_API_KEY);
  if (openrouter) return { apiKey: openrouter, baseUrl: clean(env.OPENROUTER_BASE_URL) ?? "https://openrouter.ai/api/v1" };
  const gateway = clean(env.AI_GATEWAY_API_KEY);
  if (gateway) return { apiKey: gateway, baseUrl: "https://ai-gateway.vercel.sh/v1" };
  throw new Error("No language model key found. Set OPENROUTER_API_KEY or AI_GATEWAY_API_KEY.");
};

/** Fast model: rewrites what was taught and suggests alternatives. */
export const TRANSLATOR_MODEL = clean(process.env.JLI_TRANSLATOR_MODEL) ?? "inception/mercury-2.5";
/** Reasoning agent: proposes commands for requests nothing in the matrix fits. */
export const AGENT_MODEL = clean(process.env.JLI_AGENT_MODEL) ?? "openai/gpt-5-mini";

/** A plain JSON Schema object. Portable as-is. */
export type JsonSchema = { readonly [key: string]: unknown };

export interface JsonRequest {
  model: string;
  system: string;
  /** Serialised as the user message. */
  input: unknown;
  /** Name for the response schema. */
  name: string;
  schema: JsonSchema;
  timeoutMs?: number;
  /** "off" is fastest and steadiest; "low" lets the model think briefly. Default "low". */
  reasoning?: "off" | "low";
}

const RETRY_STATUSES = new Set([408, 429, 500, 502, 503, 504]);

/** Ask for a reply that conforms to `schema`. The caller owns the type `T`. */
export const completeJson = async <T>(request: JsonRequest): Promise<{ data: T; ms: number }> => {
  const { baseUrl, apiKey } = endpoint();
  const started = performance.now();
  const body = JSON.stringify({
    model: request.model,
    messages: [
      { role: "system", content: request.system },
      { role: "user", content: typeof request.input === "string" ? request.input : JSON.stringify(request.input, null, 1) },
    ],
    reasoning: request.reasoning === "off" ? { enabled: false } : { effort: "low" },
    response_format: { type: "json_schema", json_schema: { name: request.name, strict: true, schema: request.schema } },
  });

  let lastError = "no attempt made";
  for (let attempt = 0; attempt < 2; attempt++) {
    let response: Response;
    let text: string;
    try {
      response = await fetch(`${baseUrl.replace(/\/+$/, "")}/chat/completions`, {
        method: "POST",
        headers: { authorization: `Bearer ${apiKey}`, "content-type": "application/json", "x-title": "jli" },
        body,
        signal: AbortSignal.timeout(request.timeoutMs ?? 30_000),
      });
      text = await response.text();
    } catch (error) {
      // Timeouts and dropped connections get the second attempt too.
      lastError = `${request.model} did not answer: ${error instanceof Error ? error.message : error}`;
      continue;
    }
    if (!response.ok) {
      lastError = `${request.model} returned HTTP ${response.status}: ${text.slice(0, 300)}`;
      if (RETRY_STATUSES.has(response.status)) continue;
      break;
    }
    try {
      const content: unknown = JSON.parse(text)?.choices?.[0]?.message?.content;
      if (typeof content !== "string") throw new Error("no message content");
      const json = content.trim().replace(/^```(?:json)?\s*|\s*```$/g, "");
      return { data: JSON.parse(json) as T, ms: Math.round(performance.now() - started) };
    } catch (error) {
      lastError = `${request.model} returned an unreadable reply: ${error instanceof Error ? error.message : error}`;
    }
  }
  throw new Error(lastError);
};

// ── Schema fragments shared by every skill-shaped reply ─────────────────────

const nullable = (type: string, description: string) => ({ type: [type, "null"], description });

export const object = (properties: Record<string, unknown>): JsonSchema => ({
  type: "object",
  properties,
  required: Object.keys(properties),
  additionalProperties: false,
});

export const RISK_SCHEMA = {
  type: "string",
  enum: ["safe", "modifies", "destructive"],
  description:
    "safe only reads. modifies changes local state and can be undone. destructive deletes data, rewrites history or changes remote systems.",
} as const;

export const SLOTS_SCHEMA = {
  type: "array",
  items: object({
    name: { type: "string", description: "placeholder name, used as {name} in steps" },
    kind: {
      type: "string",
      enum: ["file", "branch", "number", "text"],
      description: "file: a path in the working directory. branch: a git branch. number: an integer. text: free text.",
    },
    description: { type: "string", description: "what the value is, e.g. 'the file to count lines in'" },
    default: nullable("string", "value to use when the request does not give one"),
  }),
} as const;

export const SLOT_VALUES_SCHEMA = {
  type: "array",
  description: "the value of every slot for THIS request, unquoted",
  items: object({ name: { type: "string" }, value: { type: "string" } }),
} as const;

export const nullableString = (description: string) => nullable("string", description);

export interface SlotDraft {
  name: string;
  kind: "file" | "branch" | "number" | "text";
  description: string;
  default: string | null;
}
export interface SlotValue {
  name: string;
  value: string;
}
