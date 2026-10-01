import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DEFAULT_THRESHOLD, route, type Outcome } from "./router.ts";
import { Brain } from "./store.ts";
import { DEFAULT_MODE } from "./types.ts";
import { c, duration, Ui } from "./ui.ts";

/**
 * Each lesson is asked twice, in different words. The first time the brain has
 * never seen the task. The second time it has, and the phrasing is new, so a
 * fast answer is generalisation and not a cache hit.
 */
const LESSONS: { task: string; first: string; again: string }[] = [
  { task: "already known", first: "what files are here", again: "which files do I have in this folder" },
  { task: "new skill with a number", first: "show the last 3 commits", again: "give me the 2 most recent commits" },
  { task: "new skill with a file", first: "how many lines are in notes.txt", again: "count the lines of todo.md" },
  { task: "new skill", first: "which branch am I on", again: "what's the current branch?" },
  { task: "seeded skill", first: "what's changed in the repo?", again: "anything changed in this repo?" },
];

const git = async (cwd: string, ...args: string[]) => {
  await Bun.$`git -c user.name=jli -c user.email=jli@example.invalid ${args}`.cwd(cwd).quiet();
};

const makeSandbox = async (): Promise<string> => {
  const dir = mkdtempSync(join(tmpdir(), "jli-demo-"));
  const repo = join(dir, "repo");
  await Bun.$`mkdir -p ${repo}`.quiet();
  await git(repo, "init", "-q", "-b", "main");
  const files: [string, string][] = [
    ["notes.txt", "one\ntwo\nthree\nfour\n"],
    ["todo.md", "- learn\n- adapt\n"],
    ["app.ts", "console.log('hello');\n"],
  ];
  for (const [name, body] of files) {
    writeFileSync(join(repo, name), body);
    await git(repo, "add", name);
    await git(repo, "commit", "-q", "-m", `add ${name}`);
  }
  writeFileSync(join(repo, "todo.md"), "- learn\n- adapt\n- ship\n");
  return dir;
};

const describe = (outcome: Outcome): string => {
  const ms = duration(outcome.msSystem1 + outcome.msSystem2).padStart(7);
  if (outcome.path === "system1") return `${c.cyan("⚡ system 1")} ${ms}`;
  if (outcome.path === "system2") return `${c.magenta("◆ system 2")} ${ms}`;
  return `${c.dim("· nothing ")} ${ms}`;
};

/** Teach a fresh brain in a throwaway git repository, then ask again in other words. */
export const runDemo = async (): Promise<number> => {
  const dir = await makeSandbox();
  const cwd = join(dir, "repo");
  const brain = await Brain.open({ url: `file:${join(dir, "brain.db")}` });
  const ui = new Ui(true);
  const options = { cwd, threshold: DEFAULT_THRESHOLD, mode: DEFAULT_MODE, yes: true, dryRun: false, capture: false };
  const before: Outcome[] = [];
  const after: Outcome[] = [];

  try {
    ui.line(c.bold("\njli demo: a fresh brain, a throwaway git repository"));
    ui.line(c.dim(`sandbox ${cwd}\n`));

    ui.line(c.bold("━━ Pass 1: first time it hears each request ━━"));
    for (const lesson of LESSONS) {
      ui.line(`\n${c.cyan("›")} ${c.bold(lesson.first)}`);
      before.push(await route(brain, lesson.first, ui, options));
    }

    ui.line(c.bold("\n━━ Pass 2: the same tasks, in words it has never seen ━━"));
    for (const lesson of LESSONS) {
      ui.line(`\n${c.cyan("›")} ${c.bold(lesson.again)}`);
      after.push(await route(brain, lesson.again, ui, options));
    }

    ui.line(c.bold("\n━━ What changed ━━"));
    LESSONS.forEach((lesson, index) => {
      const a = before[index]!;
      const b = after[index]!;
      ui.line(`  ${lesson.task.padEnd(28)} ${describe(a)}  ${c.dim("then")}  ${describe(b)}  ${c.dim(b.commands.join(" && "))}`);
    });
    const total = (outcomes: Outcome[]) => outcomes.reduce((sum, o) => sum + o.msSystem1 + o.msSystem2, 0);
    const instant = (outcomes: Outcome[]) => outcomes.filter((o) => o.path === "system1").length;
    ui.line(
      `\n  answered instantly: ${instant(before)} of ${LESSONS.length} ${c.dim("then")} ${c.green(`${instant(after)} of ${LESSONS.length}`)}` +
        `   time spent deciding: ${duration(total(before))} ${c.dim("then")} ${c.green(duration(total(after)))}`,
    );
    const learned = (await brain.skills()).filter((skill) => skill.origin === "agent");
    ui.line(`  skills the agent taught the decision model: ${learned.map((skill) => c.bold(skill.label)).join(", ") || "none"}\n`);
    return instant(after) > instant(before) ? 0 : 1;
  } finally {
    brain.close();
    rmSync(dir, { recursive: true, force: true });
  }
};
