import { readContext } from "./context.ts";
import { TRANSLATOR_MODEL } from "./llm.ts";
import { matchTemplate, render, undeclared } from "./slots.ts";
import type { Brain } from "./store.ts";
import { decide } from "./system1.ts";
import { translate } from "./translator.ts";
import type { Mode, Risk, Slot } from "./types.ts";
import { c, duration, type Ui } from "./ui.ts";

export interface LearnInput {
  phrase: string;
  /** The concrete command the phrase should run. */
  steps: string[];
  /** Overrides the translator's reading of the risk. */
  risk?: Risk;
  mode: Mode;
  threshold: number;
  cwd: string;
}

export interface Learned {
  label: string;
  created: boolean;
  /** True when the stored row was written by the translator and passed verification. */
  translated: boolean;
}

/** How many phrasings of other skills are replayed to check a new row does not steal them. */
const NEIGHBOUR_CHECKS = 8;

const comparable = (command: string): string => command.replace(/['"]/g, "").replace(/\s+/g, " ").trim();
const slug = (text: string): string =>
  text.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 48) || "skill";

/**
 * Check a freshly written row against the real classifier.
 * Returns what is wrong with it, or undefined when it holds.
 */
const verify = async (brain: Brain, label: string, input: LearnInput): Promise<{ problem?: string; summary: string }> => {
  const skills = await brain.skills();
  const context = readContext(input.cwd);
  const neighbours = skills
    .filter((skill) => skill.label !== label)
    .flatMap((skill) => skill.examples.slice(-2).map((example) => ({ example, owner: skill.label })))
    .filter(({ example }) => example.trim().toLowerCase() !== input.phrase.trim().toLowerCase())
    .slice(0, NEIGHBOUR_CHECKS);

  const [own, ...others] = await Promise.all([
    decide(input.phrase, context, skills),
    ...neighbours.map(({ example }) => decide(example, context, skills)),
  ]);
  if (own!.skill?.label !== label || own!.confidence < input.threshold) {
    const got = own!.skill?.label ?? "no skill";
    return {
      problem: `the phrase itself routed to ${got} at ${own!.confidence.toFixed(2)}, so the scenario does not describe it well enough`,
      summary: "",
    };
  }
  for (const [index, decision] of others.entries()) {
    const neighbour = neighbours[index]!;
    if (decision.skill?.label === label) {
      return {
        problem: `the scenario is too broad: it captured "${neighbour.example}", which belongs to ${neighbour.owner}`,
        summary: "",
      };
    }
  }
  return {
    summary: `the phrase routes to it at ${own!.confidence.toFixed(2)}` +
      (neighbours.length ? `, and ${neighbours.length} phrasings of other skills still route where they did` : ""),
  };
};

/**
 * Link a phrase to a command. In manual mode the pair is stored as given. Otherwise a fast model
 * rewrites it into a general row, and the row is kept only if the classifier confirms that it
 * matches the phrase and does not capture phrasings that belong to other skills.
 */
export const learnPhrase = async (brain: Brain, input: LearnInput, ui: Ui): Promise<Learned> => {
  const command = input.steps.join(" && ");
  const skills = await brain.skills();
  // A command some skill already runs, exactly or with other slot values, needs no model at all.
  const known = skills.find((skill) => matchTemplate(skill.steps, command));
  if (known) {
    await brain.linkPhrase(known.label, input.phrase);
    return { label: known.label, created: false, translated: false };
  }
  if (input.mode === "manual") {
    const { label, created } = await brain.learnCommand(input.phrase, input.steps, input.risk ?? "modifies");
    return { label, created, translated: false };
  }

  let feedback: string | undefined;
  let kept: { label: string; steps: string[]; slots: Slot[]; risk: Risk } | undefined;
  for (let attempt = 0; attempt < 2; attempt++) {
    let draft;
    try {
      const translated = await translate({ phrase: input.phrase, steps: input.steps, skills, feedback });
      draft = translated.draft;
      ui.note(`${TRANSLATOR_MODEL} generalised it in ${duration(translated.ms)}: ${c.dim(draft.scenario)}`);
    } catch (error) {
      ui.warn(`Translator unavailable, storing the phrase as given. ${error instanceof Error ? error.message : error}`);
      break;
    }
    const values = Object.fromEntries(draft.slotValues.map(({ name, value }) => [name, value]));

    // "This is a known skill with other values": trust it only if the arithmetic checks out.
    const existing = draft.existingLabel ? skills.find((skill) => skill.label === draft.existingLabel) : undefined;
    if (existing && comparable(render(existing.steps, values).join(" && ")) === comparable(command)) {
      await brain.linkPhrase(existing.label, input.phrase);
      return { label: existing.label, created: false, translated: true };
    }

    // Slots are kept only if filling them back in reproduces the command that was taught.
    const slots: Slot[] = draft.slots.map((slot) => ({
      name: slot.name,
      kind: slot.kind,
      description: slot.description,
      ...(slot.default != null ? { default: slot.default } : {}),
    }));
    // A flag is part of what the skill does, never a value. Models sometimes slot one anyway.
    const slotsAFlag = Object.values(values).some((value) => value.trim().startsWith("-"));
    const faithful =
      !slotsAFlag &&
      undeclared(draft.steps, slots).length === 0 &&
      comparable(render(draft.steps, values).join(" && ")) === comparable(command);
    if (!faithful && draft.slots.length > 0) ui.note("Its slots did not reproduce your command, so the command is stored literally.");

    let label = slug(draft.label);
    while (skills.some((skill) => skill.label === label)) label = `${label}-x`;
    kept = {
      label,
      steps: faithful ? draft.steps : input.steps,
      slots: faithful ? slots : [],
      risk: input.risk ?? draft.risk,
    };
    await brain.addSkill({ ...kept, scenario: draft.scenario, origin: "taught", examples: [input.phrase] });

    const check = await verify(brain, label, input);
    if (!check.problem) {
      ui.note(`Verified with System 1: ${check.summary}.`);
      return { label, created: true, translated: true };
    }
    ui.note(`Rejected by System 1: ${check.problem}.${attempt === 0 ? " Trying once more." : ""}`);
    await brain.forget(label);
    feedback = check.problem;
  }

  // The model's row did not hold up. Keep what the human said, exactly as they said it.
  if (kept) {
    await brain.addSkill({ ...kept, scenario: `the user wants: ${input.phrase}`, origin: "taught", examples: [input.phrase] });
    ui.note("Stored with your phrase as the scenario instead.");
    return { label: kept.label, created: true, translated: false };
  }
  const { label, created } = await brain.learnCommand(input.phrase, input.steps, input.risk ?? "modifies");
  return { label, created, translated: false };
};
