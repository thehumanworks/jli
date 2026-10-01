import type { Context } from "./context.ts";
import { completeJson, object, TRANSLATOR_MODEL } from "./llm.ts";
import type { Skill } from "./types.ts";

export type Verdict = "wrong_command" | "environment" | "wrong_value" | "not_applicable";

export interface Suggestion {
  verdict: Verdict;
  explanation: string;
  options: { command: string; description: string }[];
}

const SCHEMA = object({
  verdict: {
    type: "string",
    enum: ["wrong_command", "environment", "wrong_value", "not_applicable"],
    description:
      "wrong_command: the command does not do what the request asked. " +
      "environment: the command fits the request, but this machine or directory is not in a state where it can work. " +
      "wrong_value: the right command with a wrong argument value. " +
      "not_applicable: there was no failure to judge.",
  },
  explanation: { type: "string", description: "one short sentence for the user, in plain words" },
  options: {
    type: "array",
    description: "up to three alternative commands, most likely first",
    items: object({
      command: { type: "string", description: "a complete, non-interactive shell command" },
      description: { type: "string", description: "what the user would get, in at most twelve plain words" },
    }),
  },
});

const INSTRUCTIONS = `A command-line assistant mapped a user's plain-language request to a command.
The user declined it, or it ran and failed. Work out what the user actually expected.

verdict
- Judge it only when "failure" is given; otherwise return "not_applicable".
- "environment" when the command is a sensible answer to the request and failed because of where it ran:
  not a git repository, a missing file or tool, no network, no permission. This is the most common case.
- "wrong_value" when the command is right but an argument (a file, branch, number) is wrong.
- "wrong_command" only when a different command is what the request called for.

options
- When there is no failure, the user looked at the command and said no. They wanted something else,
  so always return three options: nearby intents, other flags, another scope or level of detail.
- Options are commands the user more likely wanted, most likely first, each meaningfully different
  from the others and from the commands already tried.
- Complete and non-interactive, for the user's platform. No placeholders: use real values from the
  request and the directory listing.
- Describe each by its outcome, the way the user would say what they expected.
- For "environment", return no options.
- Prefer commands that known skills already run when one fits.`;

export interface SuggestInput {
  query: string;
  context: Context;
  entries: readonly string[];
  skills: readonly Skill[];
  /** Commands already offered or run for this request. */
  tried: readonly string[];
  failure?: { exitCode: number; stderr: string };
}

/** Diagnose a declined or failed command and offer what the user more likely expected. */
export const suggest = async (input: SuggestInput): Promise<{ suggestion: Suggestion; ms: number }> => {
  const { data, ms } = await completeJson<Suggestion>({
    model: TRANSLATOR_MODEL,
    system: INSTRUCTIONS,
    name: "suggestion",
    schema: SCHEMA,
    timeoutMs: 10_000,
    reasoning: "off",
    input: {
      request: input.query,
      platform: input.context.platform,
      insideGitRepository: input.context.insideGitRepository,
      directoryEntries: input.entries.slice(0, 60),
      commandsTried: input.tried,
      failure: input.failure ?? null,
      knownSkills: input.skills.map((skill) => ({ steps: skill.steps, scenario: skill.scenario })),
    },
  });
  if (data.verdict === "environment") data.options = [];
  data.options = data.options.filter((option) => option.command.trim() && !input.tried.includes(option.command)).slice(0, 3);
  return { suggestion: data, ms };
};
