import { existsSync, readdirSync } from "node:fs";
import { basename, dirname, join } from "node:path";

/** What the models are told about where the request was made. */
export interface Context {
  cwd: string;
  directory: string;
  insideGitRepository: boolean;
  platform: NodeJS.Platform;
}

const insideGit = (cwd: string): boolean => {
  for (let dir = cwd; ; dir = dirname(dir)) {
    if (existsSync(join(dir, ".git"))) return true;
    if (dirname(dir) === dir) return false;
  }
};

export const readContext = (cwd: string = process.cwd()): Context => ({
  cwd,
  directory: basename(cwd),
  insideGitRepository: insideGit(cwd),
  platform: process.platform,
});

/** Entries of the working directory, for file slots. */
export const listEntries = (cwd: string, limit = 200): string[] => {
  try {
    return readdirSync(cwd).filter((name) => name !== ".git").slice(0, limit);
  } catch {
    return [];
  }
};

export const listBranches = async (cwd: string): Promise<string[]> => {
  const result = await Bun.$`git branch --format=%(refname:short)`.cwd(cwd).quiet().nothrow();
  if (result.exitCode !== 0) return [];
  return result.text().split("\n").map((line) => line.trim()).filter(Boolean);
};
