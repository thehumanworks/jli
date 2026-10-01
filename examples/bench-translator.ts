/**
 * Does a translated row match better than a raw taught phrase?
 *
 * Teaches the same commands to two fresh brains, one in manual mode (phrase stored as given)
 * and one in semi mode (a fast model generalises it, System 1 verifies it), then routes
 * held-out paraphrases through both and compares.
 *
 *   bun examples/bench-translator.ts        about 90 System 1 calls and 12 translator calls
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { readContext } from "../src/context.ts";
import { learnPhrase } from "../src/learn.ts";
import { DEFAULT_THRESHOLD } from "../src/router.ts";
import { fillSlots, render } from "../src/slots.ts";
import { Brain } from "../src/store.ts";
import { decide } from "../src/system1.ts";
import type { Mode } from "../src/types.ts";
import { Ui } from "../src/ui.ts";

interface Case {
  phrase: string;
  command: string;
  /** Held-out requests, each with the command that should run. */
  tests: [request: string, expected: string][];
}

const CASES: Case[] = [
  { phrase: "diff of changes in this project", command: "git diff", tests: [
    ["what exactly did I modify, line by line", "git diff"],
    ["show my uncommitted edits", "git diff"],
    ["let me see the patch of my working tree", "git diff"]] },
  { phrase: "summary of what changed per file", command: "git diff --stat", tests: [
    ["which files changed and by how much", "git diff --stat"],
    ["how many lines did I touch in each file", "git diff --stat"],
    ["short overview of my modifications by file", "git diff --stat"]] },
  { phrase: "show the last 3 commits", command: "git log --oneline -n 3", tests: [
    ["give me the 7 most recent commits", "git log --oneline -n 7"],
    ["what were the latest 5 commits", "git log --oneline -n 5"],
    ["recent history, 12 entries", "git log --oneline -n 12"]] },
  { phrase: "which branch am I on", command: "git branch --show-current", tests: [
    ["what's the current branch", "git branch --show-current"],
    ["name of the branch I have checked out", "git branch --show-current"],
    ["where is HEAD pointing, branch-wise", "git branch --show-current"]] },
  { phrase: "how big is this folder", command: "du -sh .", tests: [
    ["total disk space used by this directory", "du -sh ."],
    ["how much room does everything here take up", "du -sh ."],
    ["size of the current directory overall", "du -sh ."]] },
  { phrase: "count lines in package.json", command: "wc -l package.json", tests: [
    ["how many lines does README.md have", "wc -l README.md"],
    ["how long is tsconfig.json in lines", "wc -l tsconfig.json"],
    ["number of lines in bun.lock", "wc -l bun.lock"]] },
  { phrase: "what's listening on port 3000", command: "lsof -i :3000", tests: [
    ["which process is using port 8080", "lsof -i :8080"],
    ["who has port 5432 open", "lsof -i :5432"],
    ["is anything bound to port 9000", "lsof -i :9000"]] },
  { phrase: "show hidden files too", command: "ls -la", tests: [
    ["list everything including dotfiles", "ls -la"],
    ["all files, even the invisible ones, with details", "ls -la"],
    ["long listing that includes entries starting with a dot", "ls -la"]] },
  { phrase: "who changed this repo the most", command: "git shortlog -sn", tests: [
    ["commit counts per author", "git shortlog -sn"],
    ["rank contributors by number of commits", "git shortlog -sn"],
    ["how many commits has each person made", "git shortlog -sn"]] },
  { phrase: "what's my node version", command: "node --version", tests: [
    ["which version of node is installed", "node --version"],
    ["node release I'm running", "node --version"],
    ["print the nodejs version number", "node --version"]] },
];

const cwd = process.cwd();
const context = readContext(cwd);
const dir = mkdtempSync(join(tmpdir(), "jli-bench-"));
const quiet = new Ui(false);

const teachAll = async (mode: Mode): Promise<{ brain: Brain; translated: number }> => {
  const brain = await Brain.open({ url: `file:${join(dir, `${mode}.db`)}` });
  let translated = 0;
  for (const item of CASES) {
    const learned = await learnPhrase(
      brain,
      { phrase: item.phrase, steps: [item.command], risk: "safe", mode, threshold: DEFAULT_THRESHOLD, cwd },
      quiet,
    );
    if (learned.translated) translated++;
  }
  return { brain, translated };
};

interface Score {
  routed: number;
  correct: number;
  confidence: number;
  total: number;
  misses: string[];
}

const evaluate = async (brain: Brain, cases: Case[]): Promise<Score> => {
  const skills = await brain.skills();
  const score: Score = { routed: 0, correct: 0, confidence: 0, total: 0, misses: [] };
  for (const item of cases) {
    const wanted = skills.find((skill) => skill.examples.includes(item.phrase));
    for (const [request, expected] of item.tests) {
      score.total++;
      const decision = await decide(request, context, skills);
      const routed = decision.skill?.label === wanted?.label && decision.confidence >= DEFAULT_THRESHOLD;
      if (routed) {
        score.routed++;
        score.confidence += decision.confidence;
      }
      let command = "(nothing)";
      if (routed && decision.skill) {
        // Slots are filled without the agent here: the question is what System 1 alone can do.
        const filled = decision.skill.slots.length
          ? await fillSlots(request, { ...decision.skill, slots: decision.skill.slots.map((s) => ({ ...s, default: s.default ?? "?" })) }, context)
          : { values: {} };
        command = render(decision.skill.steps, filled.values).join(" && ");
      }
      if (command === expected) score.correct++;
      else score.misses.push(`  "${request}" → ${command}   (wanted ${expected}; picked ${decision.skill?.label ?? "none"} at ${decision.confidence.toFixed(2)})`);
    }
  }
  return score;
};

const slotted = new Set(["git log --oneline -n 3", "wc -l package.json", "lsof -i :3000"]);
const pct = (n: number, d: number) => `${Math.round((100 * n) / Math.max(1, d))}%`.padStart(4);

try {
  const manual = await teachAll("manual");
  const semi = await teachAll("semi");
  console.log(`Taught ${CASES.length} commands to each brain. Semi mode kept ${semi.translated} translated rows and fell back to the raw phrase for ${CASES.length - semi.translated}.\n`);

  const groups = [
    ["all requests", CASES],
    ["fixed commands", CASES.filter((item) => !slotted.has(item.command))],
    ["commands with a value", CASES.filter((item) => slotted.has(item.command))],
  ] as const;
  console.log("                         manual (raw phrase)            semi (translated)");
  console.log("                         routed  right cmd  conf        routed  right cmd  conf");
  const detail: Record<string, Score> = {};
  for (const [name, cases] of groups) {
    const a = await evaluate(manual.brain, [...cases]);
    const b = await evaluate(semi.brain, [...cases]);
    if (name === "all requests") Object.assign(detail, { manual: a, semi: b });
    const row = (s: Score) => `${pct(s.routed, s.total)}    ${pct(s.correct, s.total)}      ${(s.confidence / Math.max(1, s.routed)).toFixed(2)}`;
    console.log(`${name.padEnd(24)} ${row(a)}        ${row(b)}     (${a.total} requests)`);
  }
  for (const mode of ["manual", "semi"] as const) {
    console.log(`\n${mode} misses:`);
    console.log(detail[mode]!.misses.join("\n") || "  none");
  }
  manual.brain.close();
  semi.brain.close();
} finally {
  rmSync(dir, { recursive: true, force: true });
}
