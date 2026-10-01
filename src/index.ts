#!/usr/bin/env bun
import { runDemo } from "./demo.ts";
import { DEFAULT_THRESHOLD, route, type Outcome } from "./router.ts";
import { Brain } from "./store.ts";
import { learnPhrase } from "./learn.ts";
import type { Mode, Risk } from "./types.ts";
import { DEFAULT_MODE, MODES, RISKS } from "./types.ts";
import { bar, c, duration, readLine, riskTag, sparkline, Ui } from "./ui.ts";

const HELP = `${c.bold("jli")}  say what you want, it runs the command, and it learns how you say it

${c.bold("Usage")}
  jli <request in plain language>      route, run, learn
  jli                                  interactive session

${c.bold("Options")}
  -n, --dry-run    show what would run, execute nothing, learn nothing
  -y, --yes        run without asking (never approves destructive commands)
      --confidence=N   use System 1's pick at or above N for this run (0 to 1)
      --mode=M         learning mode for this run: manual, semi or auto
  -q, --quiet      hide the decision trace
      --json       print the outcome as JSON (for agents); command output is captured
      --agent      skip the decision model and ask the reasoning agent directly

${c.bold("The brain")}
  jli skills                           what it knows, and the phrasings it learned
  jli stats                            how much it has learned
  jli history                          recent requests and how they were routed
  jli teach "<phrase>" -- <command>    link a phrase to a command (repeat to add more phrasings)
  jli config                           show settings
  jli config confidence <0..1>         confidence System 1 needs before its pick is used (default 0.5)
  jli config mode <manual|semi|auto>   who does the teaching (default semi)
  jli fix                              the last command was wrong: unlearn it and try again
  jli forget <label>                   delete a skill
  jli sync                             sync the local replica with the cloud brain
  jli demo                             watch it learn in a sandbox (about 12 API calls)

${c.bold("Storage")}
  JLI_DB_URL     file:/path/brain.db (default ~/.jli/brain.db) or libsql://… for a cloud brain
  JLI_SYNC_URL   libsql://… keeps a local replica in sync with a cloud brain
  JLI_DB_TOKEN   auth token for the cloud brain
`;

interface Flags {
  dryRun: boolean;
  yes: boolean;
  quiet: boolean;
  json: boolean;
  agent: boolean;
  risk?: Risk;
  /** From --confidence; resolved against env, the brain's setting and the default in main. */
  threshold?: number;
  /** From --mode. */
  mode?: Mode;
}

const parseThreshold = (text: string): number => {
  const value = Number(text);
  if (text.trim() === "" || !Number.isFinite(value) || value < 0 || value > 1) {
    throw new Error(`confidence must be a number from 0 to 1, got "${text}"`);
  }
  return value;
};

const parseMode = (text: string): Mode => {
  const mode = MODES.find((candidate) => candidate === text.trim());
  if (!mode) throw new Error(`mode must be one of ${MODES.join(", ")}, got "${text}"`);
  return mode;
};

/** Flag, then JLI_MODE, then the brain's stored setting, then the default. */
const resolveMode = async (brain: Brain, flags: Flags): Promise<Mode> => {
  if (flags.mode) return flags.mode;
  const fromEnv = process.env.JLI_MODE?.trim();
  if (fromEnv) return parseMode(fromEnv);
  const stored = await brain.setting("mode");
  return stored ? parseMode(stored) : DEFAULT_MODE;
};

/** Flag, then JLI_CONFIDENCE, then the brain's stored setting, then the default. */
const resolveThreshold = async (brain: Brain, flags: Flags): Promise<number> => {
  if (flags.threshold !== undefined) return flags.threshold;
  const fromEnv = process.env.JLI_CONFIDENCE?.trim();
  if (fromEnv) return parseThreshold(fromEnv);
  const stored = await brain.setting("confidence");
  return stored ? parseThreshold(stored) : DEFAULT_THRESHOLD;
};

const parse = (argv: string[]): { flags: Flags; words: string[]; rest: string[] } => {
  const flags: Flags = { dryRun: false, yes: false, quiet: false, json: false, agent: false };
  const words: string[] = [];
  const separator = argv.indexOf("--");
  const head = separator === -1 ? argv : argv.slice(0, separator);
  const rest = separator === -1 ? [] : argv.slice(separator + 1);
  for (const arg of head) {
    if (arg === "-n" || arg === "--dry-run") flags.dryRun = true;
    else if (arg === "-y" || arg === "--yes") flags.yes = true;
    else if (arg === "-q" || arg === "--quiet") flags.quiet = true;
    else if (arg === "--json") flags.json = true;
    else if (arg === "--agent") flags.agent = true;
    else if (arg.startsWith("--confidence=")) flags.threshold = parseThreshold(arg.slice(13));
    else if (arg.startsWith("--mode=")) flags.mode = parseMode(arg.slice(7));
    else if (arg.startsWith("--risk=") && (RISKS as readonly string[]).includes(arg.slice(7))) flags.risk = arg.slice(7) as Risk;
    else words.push(arg);
  }
  return { flags, words, rest };
};

const say = (text = "") => console.log(text);

const ask = async (brain: Brain, query: string, flags: Flags, rejected: string[] = []): Promise<Outcome> => {
  const ui = new Ui(!flags.quiet && !flags.json);
  const outcome = await route(brain, query, ui, {
    cwd: process.cwd(),
    threshold: await resolveThreshold(brain, flags),
    mode: await resolveMode(brain, flags),
    yes: flags.yes,
    dryRun: flags.dryRun,
    capture: flags.json,
    agentOnly: flags.agent,
    rejected,
  });
  if (flags.json) say(JSON.stringify(outcome, null, 2));
  return outcome;
};

const exitCodeFor = (outcome: Outcome): number => {
  if (outcome.ran) return outcome.exitCode ?? 0;
  if (outcome.blocked === "dry run" || outcome.blocked === "not a command request") return 0;
  return 3;
};

const showSkills = async (brain: Brain): Promise<void> => {
  const skills = await brain.skills();
  say(`${c.bold(`${skills.length} skills`)} ${c.dim(`in ${brain.location}`)}\n`);
  for (const skill of skills) {
    const origin = skill.origin === "seed" ? c.dim("seed") : skill.origin === "agent" ? c.magenta("learned from agent") : c.cyan("taught");
    say(`${c.bold(skill.label)}  ${riskTag(skill.risk)}  ${origin}  ${c.dim(`used ${skill.uses}×`)}`);
    for (const step of skill.steps) say(`  ${c.green("$")} ${step}`);
    say(`  ${c.dim(skill.scenario)}`);
    for (const example of skill.examples) say(`  ${c.green("+")} "${example}"`);
    for (const example of skill.counterExamples) say(`  ${c.red("−")} "${example}"`);
    say();
  }
};

const PATH_TAG = {
  system1: c.cyan("⚡ system 1"),
  "system1-confirmed": c.yellow("⚡ confirmed"),
  system2: c.magenta("◆ system 2"),
  taught: c.green("✎ taught  "),
  none: c.dim("· nothing "),
} as const;

const showHistory = async (brain: Brain): Promise<void> => {
  const events = (await brain.events(25)).reverse();
  if (events.length === 0) return say("No requests yet.");
  for (const event of events) {
    const ms = (event.msSystem1 ?? 0) + (event.msSystem2 ?? 0);
    say(
      `${c.dim(event.ts.slice(0, 16).replace("T", " "))}  ${PATH_TAG[event.path]}  ${duration(ms).padStart(7)}  ` +
        `"${event.query}" ${c.dim("→")} ${event.command ?? c.dim("—")}` +
        (event.learned ? `  ${c.green(`✚ ${event.learned}`)}` : ""),
    );
  }
};

const mean = (values: number[]): number => (values.length ? values.reduce((a, b) => a + b, 0) / values.length : 0);

const showStats = async (brain: Brain): Promise<void> => {
  const [skills, newestFirst] = await Promise.all([brain.skills(), brain.events(500)]);
  const events = newestFirst.reverse().filter((event) => event.label !== null && event.learned !== "corrected");
  const learned = skills.filter((skill) => skill.origin !== "seed");
  const phrasings = skills.reduce((sum, skill) => sum + skill.examples.length, 0);
  say(`${c.bold("Brain")} ${c.dim(brain.location)} ${brain.isShared ? c.green("shared") : c.dim("local only")}`);
  say(`  ${skills.length} skills (${learned.length} learned, ${skills.length - learned.length} seeded), ${phrasings} learned phrasings`);
  if (events.length === 0) return say("\nNo routed requests yet. Try: jli show me the newest files");

  const fast = events.filter((event) => event.path === "system1");
  const slow = events.filter((event) => event.path === "system2");
  say(`\n${c.bold("Routing")} over ${events.length} requests`);
  for (const [name, group] of [["system 1, instant", fast], ["taught by you", events.filter((e) => e.path === "taught" || e.path === "system1-confirmed")], ["system 2, agent", slow]] as const) {
    say(`  ${name.padEnd(22)} ${bar(group.length / events.length)} ${String(group.length).padStart(4)}`);
  }
  say(`\n${c.bold("Latency")}`);
  say(`  system 1  ${duration(Math.round(mean(fast.map((e) => (e.msSystem1 ?? 0) + (e.msSystem2 ?? 0)))))} on average`);
  if (slow.length) say(`  system 2  ${duration(Math.round(mean(slow.map((e) => (e.msSystem1 ?? 0) + (e.msSystem2 ?? 0)))))} on average`);

  const buckets = Math.min(12, events.length);
  const size = events.length / buckets;
  const curve = Array.from({ length: buckets }, (_, i) => {
    const chunk = events.slice(Math.floor(i * size), Math.floor((i + 1) * size));
    return chunk.filter((event) => event.path === "system1").length / Math.max(1, chunk.length);
  });
  say(`\n${c.bold("Learning curve")} ${c.dim("share of requests answered instantly, oldest to newest")}`);
  say(`  ${c.cyan(sparkline(curve))}  ${Math.round((curve.at(-1) ?? 0) * 100)}% now`);

  const hosts = new Set([...skills.map((skill) => skill.createdHost), ...events.map((event) => event.host)]);
  say(`\n${c.bold("Hosts")} ${[...hosts].filter(Boolean).join(", ")}`);
};

const teach = async (brain: Brain, phrase: string, command: string[], flags: Flags): Promise<void> => {
  const steps = command.join(" ").split(/\s+&&\s+/).filter(Boolean);
  const { label, created } = await learnPhrase(
    brain,
    {
      phrase,
      steps,
      risk: flags.risk,
      mode: await resolveMode(brain, flags),
      threshold: await resolveThreshold(brain, flags),
      cwd: process.cwd(),
    },
    new Ui(!flags.quiet),
  );
  const skill = await brain.skill(label);
  say(`${c.green("✚ learned")} "${phrase}" means ${c.bold(label)} ${c.dim(`(${skill?.steps.join(" && ")})`)}`);
  say(c.dim(created ? "  new skill" : `  added to an existing skill, which now has ${skill?.examples.length ?? 1} phrasings`));
};

const fix = async (brain: Brain, flags: Flags): Promise<number> => {
  const last = await brain.lastExecuted();
  if (!last?.label) {
    say("Nothing to fix: no command has been run yet.");
    return 1;
  }
  if (flags.dryRun) {
    console.error(c.dim(`  Dry run: would unlearn that "${last.query}" means ${last.label}.\n`));
  } else {
    await brain.removeExample(last.label, last.query);
    await brain.addCounterExample(last.label, last.query);
    await brain.markRejected(last.id);
    console.error(`${c.green("✚ learned")} "${last.query}" does ${c.bold("not")} mean ${last.label}. Trying again.\n`);
  }
  return exitCodeFor(await ask(brain, last.query, { ...flags, agent: true }, [last.label]));
};

const repl = async (brain: Brain, flags: Flags): Promise<void> => {
  console.error(`${c.bold("jli")} ${c.dim(`· brain: ${brain.location} · confident at ${await resolveThreshold(brain, flags)} · ctrl-d to leave`)}`);
  for (;;) {
    process.stderr.write(c.cyan("\n› "));
    const line = await readLine();
    if (line === undefined) break;
    const query = line.trim();
    if (query === "exit" || query === "quit") break;
    if (!query) continue;
    try {
      await ask(brain, query, flags);
    } catch (error) {
      console.error(c.red(error instanceof Error ? error.message : String(error)));
    }
  }
  process.stderr.write("\n");
};

const main = async (): Promise<number> => {
  const { flags, words, rest } = parse(Bun.argv.slice(2));
  const [first, ...others] = words;

  if (first === "help" || first === "--help" || first === "-h") {
    say(HELP);
    return 0;
  }
  if (first === "demo" && others.length === 0) return runDemo();

  const brain = await Brain.open();
  try {
    if (first === undefined) {
      if (!process.stdin.isTTY) {
        say(HELP);
        return 0;
      }
      await repl(brain, flags);
      return 0;
    }
    if (others.length === 0 && rest.length === 0) {
      if (first === "skills") return (await showSkills(brain), 0);
      if (first === "stats") return (await showStats(brain), 0);
      if (first === "history") return (await showHistory(brain), 0);
      if (first === "fix") return await fix(brain, flags);
      if (first === "sync") {
        say((await brain.sync()) ? `Synced with ${brain.config.syncUrl}` : "No JLI_SYNC_URL set: this brain is not a replica.");
        return 0;
      }
    }
    if (first === "config" && rest.length === 0 && others.length <= 2) {
      const [key, value] = others;
      if (key === "confidence" && value !== undefined) {
        await brain.setSetting("confidence", String(parseThreshold(value)));
        say(`Confidence threshold saved in the brain: ${value}`);
        return 0;
      }
      if (key === "mode" && value !== undefined) {
        await brain.setSetting("mode", parseMode(value));
        say(`Learning mode saved in the brain: ${value}`);
        return 0;
      }
      if (key === undefined || key === "confidence" || key === "mode") {
        say(`confidence  ${await resolveThreshold(brain, flags)}  ${c.dim("System 1's pick is used at or above this; below it the agent is asked")}`);
        say(`mode        ${await resolveMode(brain, flags)}  ${c.dim("manual: stored as taught · semi: a fast model generalises what you teach · auto: also learns from a no or a failed command")}`);
        say(c.dim("\nSet with: jli config confidence 0.7 · jli config mode auto\nOr per shell with JLI_CONFIDENCE / JLI_MODE, or per run with --confidence= / --mode="));
        return 0;
      }
    }
    if (first === "forget" && others.length === 1) {
      const label = others[0]!;
      say((await brain.forget(label)) ? `Forgot ${label}.` : `No skill called ${label}.`);
      return 0;
    }
    if (first === "teach") {
      if (others.length === 0 || rest.length === 0) {
        say('Usage: jli teach "<phrase>" [--risk=safe|modifies|destructive] -- <command>');
        return 1;
      }
      await teach(brain, others.join(" "), rest, flags);
      return 0;
    }
    return exitCodeFor(await ask(brain, [...words, ...rest].join(" "), flags));
  } finally {
    await brain.sync().catch(() => {});
    brain.close();
  }
};

try {
  process.exit(await main());
} catch (error) {
  console.error(`${c.red("jli:")} ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
}
