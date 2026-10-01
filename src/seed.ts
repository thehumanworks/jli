import type { NewSkill } from "./store.ts";

/**
 * The decision matrix that used to be hard-coded in the proof of concept.
 * It now only seeds an empty brain; everything after this is learned.
 */
export const SEED_SKILLS: NewSkill[] = [
  {
    label: "git-status",
    steps: ["git status"],
    scenario: "the user wants to know the staged and unstaged changes in a repo",
    examples: ["What changes are unstaged in this repository?"],
    risk: "safe",
    origin: "seed",
  },
  {
    label: "git-stage-all",
    steps: ["git add -A"],
    scenario: "the user wants all changes staged to git",
    risk: "modifies",
    origin: "seed",
  },
  {
    label: "git-commit",
    steps: ["git commit -m {message}"],
    scenario: "the user wants to commit the staged changes with a commit message",
    slots: [{ name: "message", kind: "text", description: "the commit message" }],
    risk: "modifies",
    origin: "seed",
  },
  {
    label: "git-push",
    steps: ["git push"],
    scenario: "the user wants to push the committed work to the default remote",
    risk: "modifies",
    origin: "seed",
  },
  {
    label: "git-push-main",
    steps: ["git push -u origin main"],
    scenario: "the user wants to push to remote main and set it as upstream",
    risk: "modifies",
    origin: "seed",
  },
  {
    label: "list-files",
    steps: ["ls"],
    scenario: "the user wants a plain list of files/directories in the current directory, no particular order or detail",
    risk: "safe",
    origin: "seed",
  },
  {
    label: "list-files-by-time",
    steps: ["ls -ltr"],
    scenario: "the user wants files listed and ordered in reverse order of last modification date",
    risk: "safe",
    origin: "seed",
  },
  {
    label: "list-files-with-sizes",
    steps: ["ls -lh"],
    scenario: "the user wants files listed with the size of each file in human readable format",
    risk: "safe",
    origin: "seed",
  },
  {
    label: "print-working-directory",
    steps: ["pwd"],
    scenario: "the user wants to know which directory they are in",
    risk: "safe",
    origin: "seed",
  },
];
