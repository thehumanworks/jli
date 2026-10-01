import { TypeSafeClient, choice, noul } from "@typesafe-ai/sdk";

const jev = new TypeSafeClient();

const state = Bun.argv[2];
console.log("State:", state);
const result = await jev.systemOne({
  state: state!,
  questions: {
    "isCommandExecutionRequest": noul(
      [
        "The user has asked explicitly to run a command",
        "The user's request can only be fulfilled if a command is ran",
        "The user has explicitly typed a command"
      ],
      {
        false: {
          intent: "No command execution was requested, explicitly or implicitly",
          examples: [
            "How much is 2 + 2",
            "Help me understand the text I will paste below:"
          ]
        },
        true: {
          intent: "The user's intent can only be fulfilled by running one or more commands, whether by explicit request or implicitly inferred",
          examples: [
            "Edit the file file.txt",
            "What changes are unstaged in this repository?",
            "Run 'ls -la'",
            "Commit my work with a meaningful, yet short, message"
          ]
        }
      }
    ),
    "identifiedCommand": choice(
      "The user's request requires a command to run that is present in the criteria list",
      {
        "git status": {
          "scenario": "the user wants to know the staged and unstaged changes in a repo",
        },
        "git add": {
          "scenario": "the user wants changes staged to git"
        },
        "git commit": {
          "scenario": "the user wants to commit the staged changes"
        },
        "git push": {
          "scenario": "the user wants to push the committed work"
        },
        "ls": {
          "scenario": "the user wants to list files/directories in a directory"
        },
        "pwd": {
          "scenario": "the user wants to know which directory they are in"
        },
      }
    ),
    "gitFlag": choice(
      "The user's command requires a specific flag added to a command running via 'git' CLI tool",
      {
        "-A": {
          "scenario": "the user wants all changes staged"
        },
        "-m": {
          "scenario": "the user specified a commit message between quotation marks"
        },
        "-u origin main": {
          "scenario": "the user wants to push to remote main"
        }
      }
    ),
    "lsFlag": choice(
      "The user's command requires a specific flag added to 'ls'",
      {
        "-ltr": {
          "scenario": "The user wants files listed and ordered in reverse order of last modification date"
        },
        "-lh": {
          "scenario": "The user wants files listed with the size of the file expressed in human readable format"
        },
        "": {
          "scenario": "No flags need to be passed to the ls command"
        }
      }
    )
  }
});

console.log(JSON.stringify(result.answers, null, 2));

if (!result.answers["isCommandExecutionRequest"]) {
  process.exit(0);
}

let command;
if (result.answers.identifiedCommand.confidence === 1) {
  command = result.answers.identifiedCommand.choice;
} else {
  process.exit(0)
}

let flag: string = "";
if (command.startsWith("git")) {
  flag = result.answers.gitFlag.choice;
} else if (command === "ls") {
  flag = result.answers.lsFlag.choice;
}

const args = [...command.split(" "), ...flag.split(" ").filter(Boolean)];

console.log(`\n\nCommand: ${args.join(" ")}`);
console.log(await Bun.$`${args}`.quiet().text());
