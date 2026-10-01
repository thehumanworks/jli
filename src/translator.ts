import {
  completeJson,
  nullableString,
  object,
  RISK_SCHEMA,
  SLOT_VALUES_SCHEMA,
  SLOTS_SCHEMA,
  TRANSLATOR_MODEL,
  type SlotDraft,
  type SlotValue,
} from "./llm.ts";
import type { Risk, Skill } from "./types.ts";

/** What the translator makes of "this phrase means this command". */
export interface Draft {
  existingLabel: string | null;
  label: string;
  steps: string[];
  scenario: string;
  slots: SlotDraft[];
  slotValues: SlotValue[];
  risk: Risk;
}

const DRAFT_SCHEMA = object({
  existingLabel: nullableString(
    "label of a known skill whose steps produce exactly this command once slot values are filled in, else null",
  ),
  label: { type: "string", description: "short kebab-case name for the skill, e.g. git-diff-summary" },
  steps: {
    type: "array",
    items: { type: "string" },
    description: "the command, with instance-specific values replaced by {slot} placeholders",
  },
  scenario: { type: "string", description: "one sentence starting with 'the user wants'" },
  slots: SLOTS_SCHEMA,
  slotValues: SLOT_VALUES_SCHEMA,
  risk: RISK_SCHEMA,
});

const INSTRUCTIONS = `You write one row of a decision matrix.

A fast classifier reads a user's plain-language request and picks the row whose "scenario" fits it best.
A person has just told us that a phrase means a command. Turn that single instance into a row that
matches every way someone might ask for the SAME outcome, and nothing else.

scenario
- One sentence starting with "the user wants". Describe the outcome the command produces, not the
  wording of the phrase and not the name of the tool.
- Be exactly as broad as the command. Everything the scenario covers must be something this command
  actually does. If you are unsure whether a neighbouring intent is covered, leave it out.
- Name the distinguishing detail: the form of the output, the scope, the ordering, the target.
  "a per-file summary of uncommitted changes" beats "see changes".
- Read knownSkills. If one is close, your scenario must say what differs from it, so the classifier can
  tell them apart. Never write a scenario that would also be true of a known skill.
- Do not copy details that only belong to this instance (a specific file name, a specific number).

steps and slots
- Replace values that belong only to this instance (a file, a branch, a count, a message) with {slot}
  placeholders and put the instance's value in slotValues. Never quote placeholders.
- Substituting slotValues into steps must reproduce the original command exactly.
- Flags, subcommands and fixed arguments are part of the skill. Do not turn them into slots.
- If nothing is instance-specific, return the command unchanged with no slots.

existingLabel
- Set it only when a known skill's steps, with some slot values, are exactly this command.

risk
- "safe" only reads. "modifies" changes local state and can be undone. "destructive" deletes data,
  rewrites history or changes remote systems.`;

export interface TranslateInput {
  phrase: string;
  steps: readonly string[];
  skills: readonly Skill[];
  /** What went wrong with the previous draft, when retrying. */
  feedback?: string;
}

/** Turn one taught instance into a generalised row for the decision matrix. */
export const translate = async (input: TranslateInput): Promise<{ draft: Draft; ms: number }> => {
  const { data, ms } = await completeJson<Draft>({
    model: TRANSLATOR_MODEL,
    system: INSTRUCTIONS,
    name: "matrix_row",
    timeoutMs: 8_000,
    reasoning: "off",
    schema: DRAFT_SCHEMA,
    input: {
      phrase: input.phrase,
      command: input.steps,
      previousAttemptProblem: input.feedback ?? null,
      knownSkills: input.skills.map((skill) => ({ label: skill.label, steps: skill.steps, scenario: skill.scenario })),
    },
  });
  return { draft: data, ms };
};
