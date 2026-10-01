import { listBranches, listEntries, type Context } from "./context.ts";
import { pick } from "./system1.ts";
import { extract } from "./system2.ts";
import type { Skill, Slot } from "./types.ts";

const NUMBER_WORDS: Record<string, string> = {
  one: "1", two: "2", three: "3", four: "4", five: "5",
  six: "6", seven: "7", eight: "8", nine: "9", ten: "10",
};

const quoted = (query: string): string | undefined => query.match(/"([^"]+)"|'([^']+)'|`([^`]+)`/)?.slice(1).find(Boolean);

const numberIn = (query: string): string | undefined => {
  const digits = query.match(/\b\d+\b/)?.[0];
  if (digits) return digits;
  for (const word of query.toLowerCase().match(/[a-z]+/g) ?? []) {
    const value = NUMBER_WORDS[word];
    if (value) return value;
  }
  return undefined;
};

export interface Filled {
  values: Record<string, string>;
  /** Slots that could not be given a value. */
  missing: string[];
  /** How each value was found, for the trace. */
  sources: Record<string, "agent" | "system1" | "pattern" | "default">;
  msSystem1: number;
  msSystem2: number;
}

const PICK_CONFIDENCE = 0.6;

/**
 * Give every `{slot}` a value, cheapest method first:
 * a pattern in the request, a System 1 pick among real candidates,
 * the slot's default, and only then the agent.
 */
export const fillSlots = async (
  query: string,
  skill: Pick<Skill, "steps" | "scenario" | "slots">,
  context: Context,
  provided: Record<string, string> = {},
): Promise<Filled> => {
  const filled: Filled = { values: {}, missing: [], sources: {}, msSystem1: 0, msSystem2: 0 };
  const pending: Slot[] = [];

  for (const slot of skill.slots) {
    const given = provided[slot.name];
    if (given) {
      filled.values[slot.name] = given;
      filled.sources[slot.name] = "agent";
      continue;
    }
    if (slot.kind === "number") {
      const value = numberIn(query);
      if (value) {
        filled.values[slot.name] = value;
        filled.sources[slot.name] = "pattern";
        continue;
      }
    }
    if (slot.kind === "text") {
      const value = quoted(query);
      if (value) {
        filled.values[slot.name] = value;
        filled.sources[slot.name] = "pattern";
        continue;
      }
    }
    if (slot.kind === "file" || slot.kind === "branch") {
      const options = slot.kind === "file" ? listEntries(context.cwd) : await listBranches(context.cwd);
      if (options.length > 0) {
        const picked = await pick(query, slot.description, options);
        filled.msSystem1 += picked.ms;
        if (picked.value && picked.confidence >= PICK_CONFIDENCE) {
          filled.values[slot.name] = picked.value;
          filled.sources[slot.name] = "system1";
          continue;
        }
      }
    }
    pending.push(slot);
  }

  // Text always goes to the agent when the request did not quote it; a default is only a last resort.
  const forAgent = pending.filter((slot) => slot.kind === "text" || slot.default == null);
  if (forAgent.length > 0) {
    const extracted = await extract(query, skill, forAgent);
    filled.msSystem2 += extracted.ms;
    for (const slot of forAgent) {
      const value = extracted.values[slot.name];
      if (value) {
        filled.values[slot.name] = value;
        filled.sources[slot.name] = "agent";
      }
    }
  }
  for (const slot of pending) {
    if (filled.values[slot.name]) continue;
    if (slot.default != null) {
      filled.values[slot.name] = slot.default;
      filled.sources[slot.name] = "default";
    } else {
      filled.missing.push(slot.name);
    }
  }
  return filled;
};

/** `{name}`, but not the shell's own `${name}`. */
const PLACEHOLDER = /(?<!\$)\{([A-Za-z_][A-Za-z0-9_]*)\}/g;

const shellQuote = (value: string): string =>
  /^[A-Za-z0-9_\-.,/:=@%+]+$/.test(value) ? value : `'${value.replaceAll("'", `'\\''`)}'`;

/** Substitute shell-quoted slot values into the step templates. */
export const render = (steps: readonly string[], values: Record<string, string>): string[] =>
  steps.map((step) => step.replace(PLACEHOLDER, (whole, name: string) => {
    const value = values[name];
    return value == null ? whole : shellQuote(value);
  }));

/**
 * If `command` is what `steps` would produce for some slot values, return those values.
 * Lets a taught command be recognised as a known skill with different values, with no model call.
 */
export const matchTemplate = (steps: readonly string[], command: string): Record<string, string> | undefined => {
  const names: string[] = [];
  const pattern = steps
    .join(" && ")
    .split(PLACEHOLDER)
    .map((part, index) => {
      if (index % 2 === 1) {
        names.push(part);
        return "(\\S+|'[^']*'|\"[^\"]*\")";
      }
      return part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&").replace(/\s+/g, "\\s+");
    })
    .join("");
  const match = new RegExp(`^${pattern}$`).exec(command.trim());
  if (!match) return undefined;
  const values: Record<string, string> = {};
  names.forEach((name, index) => {
    values[name] = (match[index + 1] ?? "").replace(/^(['"])(.*)\1$/, "$2");
  });
  return values;
};

/** Placeholders used in steps that no slot declares. */
export const undeclared = (steps: readonly string[], slots: readonly Slot[]): string[] => {
  const declared = new Set(slots.map((slot) => slot.name));
  const used = steps.flatMap((step) => [...step.matchAll(PLACEHOLDER)].map((m) => m[1]!));
  return [...new Set(used)].filter((name) => !declared.has(name));
};
