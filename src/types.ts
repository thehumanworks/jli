/** How much damage a command can do. Drives the confirmation policy. */
export type Risk = "safe" | "modifies" | "destructive";
export const RISKS = ["safe", "modifies", "destructive"] as const satisfies readonly Risk[];

/** How a `{slot}` in a command template gets its value. */
export type SlotKind = "file" | "branch" | "number" | "text";

export interface Slot {
  name: string;
  kind: SlotKind;
  description: string;
  /** Used when nothing can be extracted from the request. */
  default?: string;
}

export type Origin = "seed" | "agent" | "taught";

/** One row of the decision matrix: something the brain knows how to do. */
export interface Skill {
  label: string;
  /** Shell steps run in order; `{name}` placeholders are filled from slots. */
  steps: string[];
  /** When this skill applies. Rendered into the System 1 criteria. */
  scenario: string;
  /** Phrasings that were accepted for this skill. This is the learning. */
  examples: string[];
  /** Phrasings that were routed here and rejected. */
  counterExamples: string[];
  slots: Slot[];
  risk: Risk;
  origin: Origin;
  uses: number;
  createdAt: string;
  lastUsedAt: string | null;
  createdHost: string;
}

export type Path =
  | "system1" // routed instantly by the decision model
  | "system1-confirmed" // legacy rows: the old "did you mean?" prompt
  | "system2" // escalated to the reasoning agent
  | "taught" // the user said no and typed the right command
  | "none"; // nothing ran

export interface BrainEvent {
  id: number;
  ts: string;
  host: string;
  cwd: string;
  query: string;
  path: Path;
  label: string | null;
  confidence: number | null;
  command: string | null;
  exitCode: number | null;
  accepted: boolean;
  learned: string | null;
  msSystem1: number | null;
  msSystem2: number | null;
}

/**
 * Who does the teaching.
 *   manual  you type phrase and command; they are stored as given
 *   semi    the same, but a fast model rewrites what you taught into a general, checked matrix row
 *   auto    as semi, and the system also learns from what happens: a "no" or a failed command
 *           is diagnosed, alternatives are offered, and your pick is what gets learned
 */
export type Mode = "manual" | "semi" | "auto";
export const MODES = ["manual", "semi", "auto"] as const satisfies readonly Mode[];
export const DEFAULT_MODE: Mode = "semi";
