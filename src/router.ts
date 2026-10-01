import { listEntries, readContext } from "./context.ts";
import { run } from "./exec.ts";
import { learnPhrase } from "./learn.ts";
import { fillSlots, matchTemplate, render, undeclared } from "./slots.ts";
import type { Brain } from "./store.ts";
import { decide, NONE, type Decision } from "./system1.ts";
import { suggest, type Suggestion } from "./suggest.ts";
import { AGENT_MODEL, propose } from "./system2.ts";
import type { Mode, Path, Risk, Skill } from "./types.ts";
import { RISKS } from "./types.ts";
import { ask as askText, c, confirm as askUser, type Ui } from "./ui.ts";

/** Default for the confidence System 1 needs before its pick is used. Configurable per user. */
export const DEFAULT_THRESHOLD = 0.5;
/** Below this, with no skill picked, the request is treated as not a command at all. */
export const NOT_A_COMMAND_BELOW = 0.1;
/** Routes this confident teach nothing new, so their phrasing is not stored. */
const ALREADY_KNOWN_AT = 0.95;

export interface RouteOptions {
  cwd: string;
  /** System 1's pick is used at or above this confidence; below it the agent is asked. */
  threshold: number;
  /** Who does the teaching: see `Mode`. */
  mode: Mode;
  /** Approve confirmations automatically. Never covers destructive commands. */
  yes: boolean;
  dryRun: boolean;
  /** Capture command output instead of streaming it. */
  capture: boolean;
  /** Skip System 1 and go straight to the agent. */
  agentOnly?: boolean;
  /** Labels to leave out, used when correcting a wrong route. */
  rejected?: string[];
  /** The user's own words about what they expected, passed to the agent. */
  clarification?: string;
  confirm?: (question: string) => Promise<boolean | undefined>;
  /** Ask the user a free-text question. Undefined when nobody is there to answer. */
  ask?: (question: string) => Promise<string | undefined>;
}

export interface Outcome {
  query: string;
  path: Path;
  label: string | null;
  commands: string[];
  confidence: number | null;
  risk: Risk | null;
  ran: boolean;
  exitCode: number | null;
  /** Why nothing ran, when nothing ran. */
  blocked: string | null;
  learned: string[];
  answer: string | null;
  output: string | null;
  msSystem1: number;
  msSystem2: number;
}

const severest = (a: Risk, b: Risk): Risk => (RISKS.indexOf(a) >= RISKS.indexOf(b) ? a : b);

interface Plan {
  skill: Skill;
  path: Path;
  confidence: number | null;
  isNew: boolean;
  provided: Record<string, string>;
  risk: Risk;
}

const draftSkill = (label: string, steps: string[], scenario: string, risk: Risk, slots: Skill["slots"]): Skill => ({
  label,
  steps,
  scenario,
  risk,
  slots,
  examples: [],
  counterExamples: [],
  origin: "agent",
  uses: 0,
  createdAt: new Date().toISOString(),
  lastUsedAt: null,
  createdHost: "",
});

const slug = (text: string): string =>
  text.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 48) || "skill";

/**
 * The whole loop: System 1 decides, System 2 is consulted only when System 1
 * cannot, and whatever gets accepted is written back so System 1 can next time.
 */
export const route = async (brain: Brain, query: string, ui: Ui, options: RouteOptions): Promise<Outcome> => {
  const confirm = options.confirm ?? askUser;
  const context = readContext(options.cwd);
  const rejected = new Set(options.rejected ?? []);
  const allSkills = await brain.skills();
  const skills = () => allSkills.filter((skill) => !rejected.has(skill.label));

  const outcome: Outcome = {
    query,
    path: "none",
    label: null,
    commands: [],
    confidence: null,
    risk: null,
    ran: false,
    exitCode: null,
    blocked: null,
    learned: [],
    answer: null,
    output: null,
    msSystem1: 0,
    msSystem2: 0,
  };
  const finish = async (): Promise<Outcome> => {
    if (!options.dryRun) {
      await brain.logEvent({
        cwd: options.cwd,
        query,
        path: outcome.path,
        label: outcome.label,
        confidence: outcome.confidence,
        command: outcome.commands.join(" && ") || null,
        exitCode: outcome.exitCode,
        accepted: outcome.ran,
        learned: outcome.learned.join("; ") || null,
        msSystem1: outcome.msSystem1 || null,
        msSystem2: outcome.msSystem2 || null,
      });
    }
    return outcome;
  };

  // ── System 1: one fast call against the learned matrix ────────────────────
  let decision: Decision | undefined;
  let plan: Plan | undefined;
  let escalation = options.clarification ? "working from what you said you expected" : "asked to skip the decision model";

  if (!options.agentOnly) {
    decision = await decide(query, context, skills());
    outcome.msSystem1 += decision.ms;
    ui.decision(decision);

    if (decision.skill && decision.confidence >= options.threshold) {
      plan = {
        skill: decision.skill,
        path: "system1",
        confidence: decision.confidence,
        isNew: false,
        provided: {},
        risk: decision.skill.risk,
      };
    } else if (!decision.skill && decision.isCommand < NOT_A_COMMAND_BELOW) {
      ui.note(`Not a command request (${Math.round((1 - decision.isCommand) * 100)}% sure). Nothing to run.`);
      outcome.blocked = "not a command request";
      outcome.confidence = decision.confidence;
      return finish();
    } else if (decision.skill) {
      escalation = `only ${decision.confidence.toFixed(2)} sure about ${decision.skill.label}, below your ${options.threshold} threshold`;
    } else {
      escalation = "no known skill fits";
    }
  }

  // ── System 2: slow, deliberate, and only when needed ──────────────────────
  if (!plan) {
    ui.escalate(escalation, AGENT_MODEL);
    const { proposal, ms } = await propose({
      query,
      context,
      skills: skills(),
      entries: listEntries(options.cwd),
      clarification: options.clarification,
      rejected: allSkills.filter((skill) => rejected.has(skill.label)).map(({ label, steps }) => ({ label, steps })),
    });
    outcome.msSystem2 += ms;
    ui.proposal(proposal, ms);
    const provided = Object.fromEntries(proposal.slotValues.map(({ name, value }) => [name, value]));
    const existing =
      proposal.kind === "existing" ? skills().find((skill) => skill.label === proposal.existingLabel) : undefined;

    if (proposal.kind === "not_a_command" || (!existing && proposal.steps.length === 0)) {
      outcome.path = "system2";
      outcome.answer = proposal.answer;
      outcome.blocked = "not a command request";
      if (proposal.answer) ui.line(proposal.answer);
      return finish();
    }
    if (existing) {
      plan = { skill: existing, path: "system2", confidence: decision?.confidence ?? null, isNew: false, provided, risk: existing.risk };
    } else {
      let label = slug(proposal.label ?? proposal.steps[0] ?? "skill");
      while (allSkills.some((skill) => skill.label === label)) label = `${label}-x`;
      const slots = proposal.slots.map((slot) => ({
        name: slot.name,
        kind: slot.kind,
        description: slot.description,
        ...(slot.default != null ? { default: slot.default } : {}),
      }));
      // A proposal nobody has vetted yet: the request's own risk reading gets a vote too.
      const risk = severest(proposal.risk, decision?.intentRisk ?? "safe");
      plan = {
        skill: draftSkill(label, proposal.steps, proposal.scenario, proposal.risk, slots),
        path: "system2",
        confidence: decision?.confidence ?? null,
        isNew: true,
        provided,
        risk,
      };
    }
  }

  // ── Fill in the blanks ────────────────────────────────────────────────────
  const { skill } = plan;
  outcome.path = plan.path;
  outcome.label = skill.label;
  outcome.confidence = plan.confidence;
  outcome.risk = plan.risk;

  const filled = await fillSlots(query, skill, context, plan.provided);
  outcome.msSystem1 += filled.msSystem1;
  outcome.msSystem2 += filled.msSystem2;
  const unknown = [...filled.missing, ...undeclared(skill.steps, skill.slots)];
  outcome.commands = render(skill.steps, filled.values);
  ui.command(outcome.commands, plan.risk);
  for (const [name, source] of Object.entries(filled.sources)) {
    if (source !== "agent" || !plan.isNew) ui.note(`{${name}} = ${filled.values[name]} (from ${source})`);
  }
  if (unknown.length > 0) {
    outcome.blocked = `could not work out a value for ${unknown.map((name) => `{${name}}`).join(", ")}`;
    ui.warn(`${outcome.blocked}. Say it in the request, in quotes if it is text.`);
    return finish();
  }
  if (options.dryRun) {
    outcome.blocked = "dry run";
    ui.note("Dry run: nothing was executed and nothing was learned.");
    return outcome;
  }

  // ── Corrections: how a "no" or a failed run turns into a lesson ────────────
  const diagnose = async (failure?: { exitCode: number; stderr: string }): Promise<Suggestion | undefined> => {
    try {
      const { suggestion } = await suggest({
        query,
        context,
        entries: listEntries(options.cwd),
        skills: allSkills,
        tried: outcome.commands,
        failure,
      });
      return suggestion;
    } catch (error) {
      ui.note(`Could not get suggestions: ${error instanceof Error ? error.message : error}`);
      return undefined;
    }
  };

  /**
   * Find out what should have run, learn it, and run it. Returns the new outcome,
   * or undefined when the user had nothing to add.
   *   manual and semi: the user types the command.
   *   auto: a fast model offers numbered options; the user picks one or says what they expected.
   */
  const correct = async (given?: Suggestion, unlearn = true): Promise<Outcome | undefined> => {
    const ask = options.ask ?? askText;
    let choice: { steps: string[] } | { clarification: string | undefined } | undefined;

    const suggestion = options.mode === "auto" ? (given ?? (await diagnose())) : undefined;
    if (suggestion) {
      suggestion.options.forEach((option, index) => {
        ui.line(`   ${c.bold(String(index + 1))}  ${option.command}  ${c.dim(option.description)}`);
      });
      const answer = (
        await ask(
          suggestion.options.length > 0
            ? `Pick a number, or say what you expected ${c.dim('· "?" asks the agent · enter skips')}`
            : `Say what you expected ${c.dim('· "?" asks the agent · enter skips')}`,
        )
      )?.trim();
      if (!answer) return undefined;
      const picked = /^\d+$/.test(answer) ? suggestion.options[Number(answer) - 1] : undefined;
      if (picked) choice = { steps: [picked.command] };
      else if (/^\d+$/.test(answer)) {
        ui.note(`There is no option ${answer}.`);
        return undefined;
      }
      else choice = { clarification: answer === "?" ? undefined : answer };
    } else {
      const answer = (
        await ask(`What should it run instead? ${c.dim('a command, "?" to ask the agent, or enter to skip')}`)
      )?.trim();
      if (!answer) return undefined;
      choice = answer === "?" ? { clarification: undefined } : { steps: answer.split(/\s+&&\s+/).filter(Boolean) };
    }

    // Choosing a different command is itself the verdict on the old one. The same command
    // with another slot value is not: that skill was right.
    const sameSkill = "steps" in choice && matchTemplate(skill.steps, choice.steps.join(" && ")) !== undefined;
    const wasWrong = "steps" in choice ? !sameSkill : unlearn;
    if (wasWrong && !plan.isNew) {
      await brain.removeExample(skill.label, query);
      if (await brain.addCounterExample(skill.label, query)) {
        ui.learned(`"${query}" does ${c.bold("not")} mean ${skill.label}`);
      }
    }
    if ("clarification" in choice) {
      return route(brain, query, ui, {
        ...options,
        agentOnly: true,
        clarification: choice.clarification,
        rejected: unlearn ? [...rejected, skill.label] : [...rejected],
      });
    }
    const { steps } = choice;
    const taught = await learnPhrase(
      brain,
      { phrase: query, steps, mode: options.mode, threshold: options.threshold, cwd: options.cwd },
      ui,
    );
    ui.learned(`"${query}" means ${c.bold(taught.label)}${taught.created ? " (new skill)" : ""}`);
    ui.command(steps, (await brain.skill(taught.label))?.risk ?? "modifies");
    const rerun = await run(steps, options.cwd, { capture: options.capture });
    await brain.recordUse(taught.label);
    if (rerun.exitCode !== 0) ui.warn(`exited with code ${rerun.exitCode}`);
    Object.assign(outcome, {
      path: "taught",
      label: taught.label,
      commands: steps,
      ran: true,
      exitCode: rerun.exitCode,
      blocked: null,
      output: rerun.output ?? null,
      learned: [`"${query}" means ${taught.label}`],
    } satisfies Partial<Outcome>);
    return finish();
  };

  // ── Permission: one question, every time, unless --yes ───────────────────
  const destructive = plan.risk === "destructive";
  const approved = options.yes && !destructive ? true : await confirm("Run it?");
  if (approved === undefined) {
    outcome.blocked = destructive ? "destructive commands need an interactive yes" : "needs confirmation";
    ui.warn(
      destructive
        ? "Not run: it is destructive, which always needs an interactive yes."
        : "Not run: nobody to confirm. Re-run with --yes to approve.",
    );
    return finish();
  }
  if (!approved) {
    // A "no" is a chance to learn what the right answer was.
    const corrected = await correct();
    if (corrected) return corrected;
    outcome.blocked = "declined";
    return finish();
  }

  // ── Execute ───────────────────────────────────────────────────────────────
  const result = await run(outcome.commands, options.cwd, { capture: options.capture, watchStderr: options.mode === "auto" });
  outcome.ran = true;
  outcome.exitCode = result.exitCode;
  outcome.output = result.output ?? null;
  if (result.exitCode !== 0) ui.warn(`exited with code ${result.exitCode}`);

  // ── Auto mode: a failure is feedback, but only a diagnosis says what kind ──
  if (options.mode === "auto" && result.exitCode !== 0) {
    const diagnosis = await diagnose({ exitCode: result.exitCode, stderr: result.stderr ?? "" });
    if (diagnosis) {
      const meaning = {
        wrong_command: "this was the wrong command for the request",
        environment: "the command fits the request; the problem is where it ran",
        wrong_value: "right command, wrong value",
        not_applicable: "no verdict",
      }[diagnosis.verdict];
      ui.line(`${c.yellow("◇ diagnosis")} ${c.dim(`· ${meaning}`)}`);
      ui.line(`   ${diagnosis.explanation}`);
      // An environment problem says nothing about the route, so nothing is unlearned or offered.
      if (diagnosis.verdict === "wrong_command" || diagnosis.verdict === "wrong_value") {
        const corrected = await correct(diagnosis, diagnosis.verdict === "wrong_command");
        if (corrected) return corrected;
      }
      if (diagnosis.verdict === "wrong_command") {
        ui.note("Nothing was learned from this run.");
        return finish();
      }
    }
  }

  // ── Write back: this is where the software learns ─────────────────────────
  if (plan.isNew) {
    if (result.exitCode === 0) {
      await brain.addSkill({ ...skill, origin: "agent", examples: [query] });
      await brain.recordUse(skill.label);
      outcome.learned.push(`new skill ${skill.label}`);
      ui.learned(`new skill ${c.bold(skill.label)}. Next time System 1 handles this without the agent.`);
    } else {
      ui.note("The command failed, so the proposal was not learned.");
    }
  } else {
    await brain.recordUse(skill.label);
    // A human yes vouches for the route even if the command itself failed; --yes needs a clean exit.
    const vouched = plan.path === "system2" || !options.yes || result.exitCode === 0;
    const taughtSomething = vouched && (plan.path === "system2" || (plan.confidence ?? 1) < ALREADY_KNOWN_AT);
    if (taughtSomething && (await brain.addExample(skill.label, query))) {
      outcome.learned.push(`"${query}" means ${skill.label}`);
      ui.learned(`"${query}" means ${c.bold(skill.label)}`);
    }
  }
  return finish();
};
