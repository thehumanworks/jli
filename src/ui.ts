import type { Decision } from "./system1.ts";
import { NONE } from "./system1.ts";
import type { Proposal } from "./system2.ts";
import type { Risk } from "./types.ts";

const useColor = !process.env.NO_COLOR && (process.stderr.isTTY || Boolean(process.env.FORCE_COLOR));
const paint = (code: string) => (text: string | number) => (useColor ? `\x1b[${code}m${text}\x1b[0m` : String(text));

export const c = {
  dim: paint("2"),
  bold: paint("1"),
  green: paint("32"),
  yellow: paint("33"),
  red: paint("31"),
  cyan: paint("36"),
  magenta: paint("35"),
};

export const bar = (fraction: number, width = 20): string => {
  const filled = Math.round(Math.max(0, Math.min(1, fraction)) * width);
  return "█".repeat(filled) + c.dim("░".repeat(width - filled));
};

const SPARKS = "▁▂▃▄▅▆▇█";
export const sparkline = (fractions: readonly number[]): string =>
  fractions.map((f) => SPARKS[Math.min(SPARKS.length - 1, Math.floor(f * SPARKS.length))] ?? " ").join("");

export const duration = (ms: number): string => (ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(1)} s`);

const riskPaint: Record<Risk, (text: string) => string> = { safe: c.green, modifies: c.yellow, destructive: c.red };
export const riskTag = (risk: Risk): string => riskPaint[risk](risk);

/** The decision trace. Everything goes to stderr so stdout stays the command's own output. */
export class Ui {
  constructor(readonly enabled: boolean) {}

  line(text = ""): void {
    if (this.enabled) process.stderr.write(`${text}\n`);
  }

  decision(decision: Decision): void {
    this.line(
      `${c.cyan("⚡ system 1")} ${c.dim(`· ${decision.model} · ${duration(decision.ms)} · ${decision.matrixSize} skills in the matrix`)}`,
    );
    const shown = decision.candidates.filter((candidate) => candidate.probability > 0).slice(0, 3);
    const width = Math.max(...shown.map((candidate) => candidate.label.length), 0);
    for (const candidate of shown) {
      const label = candidate.label === NONE ? c.dim(candidate.label.padEnd(width)) : candidate.label.padEnd(width);
      this.line(`   ${label}  ${bar(candidate.probability)}  ${candidate.probability.toFixed(2)}`);
    }
  }

  escalate(reason: string, model: string): void {
    this.line(`${c.magenta("◆ system 2")} ${c.dim(`· ${model} · ${reason}`)}`);
  }

  proposal(proposal: Proposal, ms: number): void {
    const what =
      proposal.kind === "existing"
        ? `matched known skill ${c.bold(proposal.existingLabel ?? "?")}`
        : proposal.kind === "new"
          ? `proposes new skill ${c.bold(proposal.label ?? "?")}`
          : "says this is not a command";
    this.line(`   ${what} ${c.dim(`· ${duration(ms)}`)}`);
    if (proposal.explanation) this.line(`   ${c.dim(proposal.explanation)}`);
  }

  command(commands: readonly string[], risk: Risk): void {
    commands.forEach((command, index) => {
      const tag = index === 0 ? `  ${c.dim("risk:")} ${riskTag(risk)}` : "";
      this.line(`${c.bold("→")} ${c.bold(command)}${tag}`);
    });
  }

  learned(text: string): void {
    this.line(`${c.green("✚ learned")} ${text}`);
  }

  note(text: string): void {
    this.line(c.dim(`  ${text}`));
  }

  warn(text: string): void {
    this.line(`${c.yellow("!")} ${text}`);
  }
}

let lines: AsyncIterator<string> | undefined;

/**
 * Read one line from stdin, or undefined at end of input. The session prompt and
 * the confirmation prompt share this single reader; two readers would fight over stdin.
 */
export const readLine = async (): Promise<string | undefined> => {
  lines ??= console[Symbol.asyncIterator]();
  const next = await lines.next();
  return next.done ? undefined : next.value;
};

/** Ask a yes/no question. Returns undefined when nobody is there to answer. */
export const confirm = async (question: string): Promise<boolean | undefined> => {
  if (!process.stdin.isTTY) return undefined;
  process.stderr.write(`${c.yellow("?")} ${question} ${c.dim("[Y/n]")} `);
  const line = await readLine();
  if (line === undefined) return undefined;
  const answer = line.trim().toLowerCase();
  return answer === "" || answer === "y" || answer === "yes";
};

/** Ask a free-text question. Returns undefined when nobody is there to answer. */
export const ask = async (question: string): Promise<string | undefined> => {
  if (!process.stdin.isTTY) return undefined;
  process.stderr.write(`${c.yellow("?")} ${question}\n${c.yellow("›")} `);
  return readLine();
};
