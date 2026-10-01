import {
  choice,
  noul,
  score,
  TypeSafeClient,
  type ChoiceCriteria,
  type JsonValue,
} from "@typesafe-ai/sdk";
import type { Context } from "./context.ts";
import type { Risk, Skill } from "./types.ts";

/** Label for "nothing in the matrix fits". Gives low-fit requests somewhere to land. */
export const NONE = "none-of-these";
/** A choice question accepts at most 255 labels; stay well under and prefilter beyond it. */
export const MAX_MATRIX = 200;

let client: TypeSafeClient | undefined;
export const systemOne = (): TypeSafeClient => (client ??= new TypeSafeClient({ logLevel: "error" }));

/**
 * The static half of the matrix. These keys are literals, so the SDK infers
 * exact answer types: `answers.isCommandRequest.noul`, `answers.intentRisk.score`.
 */
const GATE = {
  isCommandRequest: noul(
    [
      "The user has asked explicitly to run a command",
      "The user's request can only be fulfilled if a command is ran",
      "The user has explicitly typed a command",
    ],
    {
      false: {
        intent: "No command execution was requested, explicitly or implicitly",
        examples: ["How much is 2 + 2", "Help me understand the text I will paste below:"],
      },
      true: {
        intent:
          "The user's intent can only be fulfilled by running one or more commands, whether by explicit request or implicitly inferred",
        examples: [
          "Edit the file file.txt",
          "What changes are unstaged in this repository?",
          "Run 'ls -la'",
          "Commit my work with a meaningful, yet short, message",
          "Where am I?",
          "Which files are in here?",
        ],
      },
    },
  ),
  intentRisk: score("How much could fulfilling this request change or destroy?", [
    "Read-only: it only inspects or displays information",
    "It creates or modifies local files or state, and can be undone",
    "It deletes data, rewrites history, or changes remote systems, and is hard to undo",
  ]),
} as const;

/**
 * The learned half: database rows rendered into choice criteria.
 * Learning is nothing more than these descriptions getting richer.
 */
export const buildMatrix = (skills: readonly Skill[]): ChoiceCriteria => {
  const criteria: ChoiceCriteria = {};
  for (const skill of skills) {
    const entry: { [key: string]: JsonValue } = { runs: skill.steps.join(" && "), scenario: skill.scenario };
    if (skill.examples.length > 0) entry.examples = skill.examples;
    if (skill.counterExamples.length > 0) entry.doesNotApplyTo = skill.counterExamples;
    criteria[skill.label] = entry;
  }
  criteria[NONE] = {
    scenario: "None of the listed commands does what the user asked",
  };
  return criteria;
};

const words = (text: string): Set<string> => new Set(text.toLowerCase().match(/[a-z0-9]+/g) ?? []);

/** Keep the matrix under the label cap: most lexically relevant rows first, then most used. */
export const shortlist = (skills: readonly Skill[], query: string, limit = MAX_MATRIX): Skill[] => {
  if (skills.length <= limit) return [...skills];
  const wanted = words(query);
  const relevance = (skill: Skill): number => {
    const have = words([skill.label, skill.scenario, ...skill.steps, ...skill.examples].join(" "));
    let hits = 0;
    for (const word of wanted) if (have.has(word)) hits++;
    return hits * 10 + Math.log1p(skill.uses);
  };
  return [...skills].sort((a, b) => relevance(b) - relevance(a)).slice(0, limit);
};

/**
 * Read the risk rubric pessimistically. The expected score would average a
 * "safe or destructive" split into "modifies"; the distribution does not.
 */
const readRisk = (p: { readonly 0: number; readonly 1: number; readonly 2: number }): Risk => {
  if (p[2] >= 0.3) return "destructive";
  if (p[1] + p[2] >= 0.5) return "modifies";
  return "safe";
};

export interface Candidate {
  label: string;
  probability: number;
}

export interface Decision {
  /** The matched skill, or undefined when the model chose "none of these". */
  skill: Skill | undefined;
  /** Reported confidence in the chosen label, 0 to 1. */
  confidence: number;
  /** Probability that the request needs a command at all. */
  isCommand: number;
  /** Risk the model reads from the request itself. */
  intentRisk: Risk;
  /** Labels by probability, highest first. */
  candidates: Candidate[];
  matrixSize: number;
  ms: number;
  model: string;
  tokens: number;
}

/** One System 1 call: gate, risk and routing answered together against the same state. */
export const decide = async (query: string, context: Context, skills: readonly Skill[]): Promise<Decision> => {
  const rows = shortlist(skills, query);
  const started = performance.now();
  const result = await systemOne().systemOne({
    state: {
      request: query,
      context: {
        directory: context.directory,
        insideGitRepository: context.insideGitRepository,
        platform: context.platform,
      },
    },
    questions: {
      ...GATE,
      skill: choice(
        "Which known command fulfils the user's request?",
        buildMatrix(rows),
      ),
    },
  });
  const ms = Math.round(performance.now() - started);

  const { isCommandRequest, intentRisk, skill } = result.answers;
  const candidates = Object.entries(skill.probabilities)
    .map(([label, probability]) => ({ label, probability }))
    .sort((a, b) => b.probability - a.probability);

  return {
    skill: rows.find((row) => row.label === skill.choice),
    confidence: skill.confidence,
    isCommand: isCommandRequest.noul,
    intentRisk: readRisk(intentRisk.probabilities),
    candidates,
    matrixSize: rows.length,
    ms,
    model: result.model,
    tokens: result.usage.input_tokens + result.usage.output_tokens,
  };
};

/** Fill a slot by letting System 1 pick among concrete candidates (files, branches). */
export const pick = async (
  query: string,
  slotDescription: string,
  options: readonly string[],
): Promise<{ value: string | undefined; confidence: number; ms: number }> => {
  const criteria: ChoiceCriteria = {};
  for (const option of options.slice(0, MAX_MATRIX)) criteria[option] = null;
  criteria[NONE] = "The request does not refer to any of the listed options";
  const started = performance.now();
  const { answers } = await systemOne().systemOne({
    state: { request: query },
    questions: { value: choice(`Which option is ${slotDescription}, as referred to in the request?`, criteria) },
  });
  const ms = Math.round(performance.now() - started);
  const value = answers.value.choice === NONE ? undefined : answers.value.choice;
  return { value, confidence: answers.value.confidence, ms };
};
