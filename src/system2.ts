import type { Context } from "./context.ts";
import {
  AGENT_MODEL,
  completeJson,
  nullableString,
  object,
  RISK_SCHEMA,
  SLOT_VALUES_SCHEMA,
  SLOTS_SCHEMA,
  type SlotDraft,
  type SlotValue,
} from "./llm.ts";
import type { Risk, Skill } from "./types.ts";

export { AGENT_MODEL };

export interface Proposal {
  kind: "existing" | "new" | "not_a_command";
  existingLabel: string | null;
  label: string | null;
  steps: string[];
  scenario: string;
  slots: SlotDraft[];
  slotValues: SlotValue[];
  risk: Risk;
  explanation: string;
  answer: string | null;
}

const PROPOSAL_SCHEMA = object({
  kind: {
    type: "string",
    enum: ["existing", "new", "not_a_command"],
    description:
      "existing: a known skill already does this. new: a new skill is needed. not_a_command: nothing should run.",
  },
  existingLabel: nullableString("label of the known skill, when kind is existing"),
  label: nullableString("short kebab-case name for a new skill, e.g. git-recent-commits"),
  steps: {
    type: "array",
    items: { type: "string" },
    description: "shell commands for a new skill, run in order, with {slot} placeholders",
  },
  scenario: { type: "string", description: "for a new skill: when it applies, phrased like 'the user wants ...'" },
  slots: SLOTS_SCHEMA,
  slotValues: SLOT_VALUES_SCHEMA,
  risk: RISK_SCHEMA,
  explanation: { type: "string", description: "one short sentence on what will run and why" },
  answer: nullableString("for not_a_command: a brief direct answer to the user, if one is possible"),
});

const INSTRUCTIONS = `You are the slow, deliberate half of a command-line assistant.
A fast decision model normally maps the user's plain-language request to a known skill.
You are called only when it could not. What you return is stored and taught to the fast model,
so write skills that are reusable, not one-off answers.

Rules:
- If a known skill does what was asked, return kind "existing" with its label and the slot values.
- Otherwise return kind "new". Write the shortest correct command for the user's platform.
- Generalise: anything specific to this request (a file name, a count, a branch, a message) must be a
  {slot}, with its value for this request in slotValues. Never quote placeholders; quoting is added for you.
- Slot kinds: "file" is a path in the working directory, "branch" a git branch, "number" an integer,
  "text" free text such as a commit message.
- Use several steps only when the task truly is a chain of commands.
- Commands must be non-interactive: no editors, pagers or prompts (use flags such as --no-pager).
- risk: "safe" only reads; "modifies" changes local state and can be undone; "destructive" deletes data,
  rewrites history or changes remote systems.
- The scenario must describe the intent in general terms so similar phrasings match it later, and must
  say what sets it apart from similar known skills.
- rejectedSkills were already tried for this request and the user said they were wrong.
  Never propose them again, under any label, nor a command that does the same thing.
- clarification, when present, is the user explaining what they actually expected. It outranks your own reading.
- If the request is not something a command should fulfil, return kind "not_a_command".`;

export interface ProposeInput {
  query: string;
  context: Context;
  skills: readonly Skill[];
  entries: readonly string[];
  /** Skills the user already rejected for this request, with what they run. */
  rejected?: readonly { label: string; steps: string[] }[];
  /** The user's own words about what they expected instead. */
  clarification?: string;
}

/** Ask the reasoning agent what to do when the decision model has no confident answer. */
export const propose = async (input: ProposeInput): Promise<{ proposal: Proposal; ms: number }> => {
  const { data, ms } = await completeJson<Proposal>({
    model: AGENT_MODEL,
    system: INSTRUCTIONS,
    name: "proposal",
    schema: PROPOSAL_SCHEMA,
    input: {
      request: input.query,
      clarification: input.clarification ?? null,
      platform: input.context.platform,
      workingDirectory: input.context.directory,
      insideGitRepository: input.context.insideGitRepository,
      directoryEntries: input.entries.slice(0, 60),
      rejectedSkills: input.rejected ?? [],
      knownSkills: input.skills.map((skill) => ({
        label: skill.label,
        steps: skill.steps,
        scenario: skill.scenario,
        slots: skill.slots.map((slot) => slot.name),
      })),
    },
  });
  return { proposal: data, ms };
};

const VALUES_SCHEMA = object({
  values: {
    type: "array",
    items: object({ name: { type: "string" }, value: nullableString("null only when it cannot be determined") }),
  },
});

/** Free-text slot filling. Classification cannot write text, so this is agent work. */
export const extract = async (
  query: string,
  skill: Pick<Skill, "steps" | "scenario">,
  slots: readonly { name: string; description: string }[],
): Promise<{ values: Record<string, string>; ms: number }> => {
  const { data, ms } = await completeJson<{ values: { name: string; value: string | null }[] }>({
    model: AGENT_MODEL,
    system:
      "Fill in the placeholder values for a command from the user's request. Return each value unquoted. " +
      "Write a sensible value when the request implies one. Return null only when it cannot be determined.",
    name: "slot_values",
    schema: VALUES_SCHEMA,
    input: { request: query, command: skill.steps, purpose: skill.scenario, placeholders: slots },
  });
  const values: Record<string, string> = {};
  for (const { name, value } of data.values) if (value != null && value !== "") values[name] = value;
  return { values, ms };
};
