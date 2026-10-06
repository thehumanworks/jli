//! jli's own help (ADR 25): every command, the options they all take and the environment, written
//! once in the tables below, so that `jli --help` and `jli <command> --help` say the same thing. And
//! which help, if any, a command line asks for.

use crate::ui::c;

/// One line of the help: what is typed, and what it does. A `\n` in what it does goes on below it.
type Line = (&'static str, &'static str);

/// One of jli's own commands: the word that names it, its lines, and the options that matter to it.
struct Command {
    name: &'static str,
    lines: &'static [Line],
    options: &'static [Line],
}

/// How wide the first column is: what a command, an option or a variable does begins after it.
const WIDTH: usize = 36;
const OPTION_WIDTH: usize = 21;
const ENVIRONMENT_WIDTH: usize = 15;

const USAGE: &[Line] = &[
    ("jli <request in plain language>", "route, run, learn"),
    ("jli", "interactive session"),
    ("jli <command> --help", "what one command takes, and the options that matter to it"),
];

const DRY_RUN: Line =
    ("-n, --dry-run", "show what would run or change, and do neither; a request is kept as a plan for jli run");
const YES: Line = ("-y, --yes", "run without asking (never approves destructive commands)");
const YOLO: Line = ("-Y, --yolo", "run without asking, destructive commands included");
const CONFIDENCE: Line = ("    --confidence=N", "use System 1's pick at or above N for this run (0 to 1)");
const MODE: Line = ("    --mode=M", "learning mode for this run: manual, semi or auto");
const QUIET: Line = ("-q, --quiet", "hide the decision trace; a question still shows what it asks about");
const JSON: Line = ("    --json", "print the outcome as JSON (for agents); command output is captured");
const AGENT: Line = ("    --agent", "skip the decision model and ask the reasoning agent directly");
const USER_ID: Line = ("    --user-id=ID", "whose skills, history and settings to use (default \"default\")");
const PROJECT: Line = (
    "    --project[=<id>]",
    "act in project <id> as if run in one of its directories (also --project <id>);\nbare, the project of this directory's .jlirc",
);
const HELP: Line = ("-h, --help", "this help, or one command's after its name; after -- it belongs to the command");

const OPTIONS: &[Line] = &[DRY_RUN, YES, YOLO, CONFIDENCE, MODE, QUIET, JSON, AGENT, USER_ID, PROJECT, HELP];

const ARGUMENTS: &[Line] = &[
    ("jli commit message=\"fix the parser\"", "name=value goes straight to the command's {message} slot"),
    ("jli commit with message: fix it", "so does the slot's name with a colon, or any \"quoted text\""),
    ("jli go to dir=src", "another name for a slot is worked out once by the agent, then kept"),
];
const STATED: &str = "Stated values are passed as written, by no model, and never influence which command is picked.";

/// In the order the full help lists them.
const COMMANDS: &[Command] = &[
    Command {
        name: "skills",
        lines: &[(
            "jli skills [--project[=<id>]]",
            "what it knows, and the phrasings it learned; with --project, only\nthat project's own skills, not the global ones",
        )],
        options: &[PROJECT],
    },
    Command { name: "stats", lines: &[("jli stats", "how much it has learned")], options: &[] },
    Command { name: "history", lines: &[("jli history", "recent requests and how they were routed")], options: &[] },
    Command {
        name: "teach",
        lines: &[
            (
                "jli teach \"<phrase>\" -- <command>",
                "link a phrase to the command as you wrote it; a fast model writes only\nwhen to run it (repeat to add more phrasings)",
            ),
            ("jli teach --global \"<phrase>\" -- …", "the same, for every project, from inside one"),
            (
                "    --name=<name>",
                "what to call the skill; refused if a skill of that name runs another\ncommand, or another skill already runs this one",
            ),
            ("    --description=<words>", "when and why to run it, in your words: the model keeps to them"),
            ("    --risk=R", "the command's risk, safe, modifies or destructive, instead of jli's guess"),
            (
                "    --mode=M",
                "semi or auto: the model may also turn the command's values into slots;\nmanual: no model at all. jli config mode does not apply here",
            ),
        ],
        options: &[],
    },
    Command {
        name: "config",
        lines: &[
            ("jli config", "show settings"),
            ("jli config confidence <0..1>", "confidence System 1 needs before its pick is used (default 0.5)"),
            (
                "jli config mode <manual|semi|auto>",
                "how a no, a failure and a correction are learned (default semi);\njli teach follows only its own --mode",
            ),
            ("jli config model <slug>", "the model the agent and the translator run on your ChatGPT plan"),
        ],
        options: &[],
    },
    Command {
        name: "login",
        lines: &[
            ("jli login", "continue with ChatGPT: the agent and the translator run on your\nChatGPT plan instead of an API key"),
            ("jli login --new", "the same with another ChatGPT account than the one saved here"),
            (
                "jli login --device-auth",
                "sign in with a code entered on another device, over SSH or wherever\nno browser opens (with --new, as another account)",
            ),
            ("jli login --codex", "use the ChatGPT account the Codex CLI is signed in with"),
        ],
        options: &[QUIET, JSON],
    },
    Command {
        name: "logout",
        lines: &[("jli logout", "stop using your ChatGPT plan, and end jli's session with it")],
        options: &[JSON],
    },
    Command {
        name: "models",
        lines: &[("jli models", "the models your ChatGPT plan has; jli config model picks one")],
        options: &[JSON],
    },
    Command {
        name: "fix",
        lines: &[
            ("jli fix", "the last command run here was wrong: unlearn it and try again"),
            ("jli fix <eventId>", "the same for that event (eventId in --json), from anywhere"),
            ("jli fix <eventId> -- <command>", "that was wrong, this is right: learn it, and return a plan of it"),
            (
                "jli fix <eventId> --expected <words>",
                "ask the agent again with what you expected, and return a plan\n(add --run to either to run the correction instead)",
            ),
        ],
        options: &[DRY_RUN, JSON],
    },
    Command {
        name: "run",
        lines: &[(
            "jli run <planId> [<planId> …]",
            "run plans (planId in --json) exactly as shown, in order, with no\nmodel call; stops at the first that fails. name=value fills a gap",
        )],
        options: &[YOLO, JSON],
    },
    Command {
        name: "forget",
        lines: &[("jli forget <label>", "delete a skill (inside a project, one of the project's: --global for the user's)")],
        options: &[],
    },
    Command {
        name: "forget-phrase",
        lines: &[(
            "jli forget-phrase <label> <phrase>",
            "take one phrasing back from a skill; it is kept as a counter-example",
        )],
        options: &[DRY_RUN, JSON],
    },
    Command {
        name: "alias",
        lines: &[("jli alias <label> <slot> <name>", "give a slot another name for name=value (--remove takes one away)")],
        options: &[DRY_RUN, JSON],
    },
    Command {
        name: "slot",
        lines: &[
            ("jli slot <label> <slot> <setting>…", "how a slot is filled and written; each setting is written with ="),
            ("    --default=<value>", "the value it takes when the request names none (--default= for none)"),
            ("    --optional | --required", "whether it, and its --flag=<flag>, may be left out of the command"),
            (
                "    --choice=<value>[=<text>] …",
                "named values. A choice puts its own name in the command, or <text>\nwhen one is given: --choice=release=--release. --choice=debug= puts\nnothing; --no-choices takes them away. A slot whose choices write a\nflag takes only them; any other takes a value you state as well",
            ),
            ("    --kind=file|branch|number|text", "what kind of value it takes, and so where one is looked for"),
        ],
        options: &[DRY_RUN, JSON],
    },
    Command {
        name: "eval",
        lines: &[
            (
                "jli eval [<label>] [--limit N]",
                "route every stored example with itself held out, and replay accepted\nrequests: how often each skill comes home (System 1 calls, priced first)",
            ),
            (
                "jli eval --optimise",
                "the agent rewrites the weakest rows; a rewrite is kept only if the\nmeasurement improves (--max-skills=3, --max-calls=400)",
            ),
            (
                "jli eval -n [--optimise]",
                "measure, and show the rewrites and their effect, but write nothing:\nno summary is stored and no rewrite is applied",
            ),
            ("jli eval undo [<label>]", "put back what the last optimisation changed"),
        ],
        options: &[JSON],
    },
    Command {
        name: "share",
        lines: &[("jli share <label> --to <user>", "give another user a copy of one of your skills")],
        options: &[USER_ID],
    },
    Command {
        name: "project",
        lines: &[
            ("jli project", "the project this directory is in, from the nearest .jlirc"),
            ("jli project init [<id>]", "make this directory a project: what is learned here stays here"),
        ],
        options: &[],
    },
    Command {
        name: "generalize",
        lines: &[
            ("jli generalize [<label>]", "propose making project skills global; each is written only once you approve"),
            ("    --all-projects", "every project's skills"),
            ("    --approve-all", "write what passes the checks unasked"),
            ("    -n", "only show"),
        ],
        options: &[],
    },
    Command { name: "sync", lines: &[("jli sync", "sync the local replica with the cloud brain")], options: &[] },
    Command {
        name: "shell-init",
        lines: &[(
            "jli shell-init [zsh|bash|fish]",
            "the shell function that lets jli cd and export in your shell:\neval \"$(jli shell-init zsh)\" in ~/.zshrc",
        )],
        options: &[],
    },
    Command { name: "demo", lines: &[("jli demo", "watch it learn in a sandbox (about 12 API calls)")], options: &[] },
    Command {
        name: "help",
        lines: &[("jli help [<command>]", "this help, or one command's: the same as jli <command> --help")],
        options: &[],
    },
];

const ENVIRONMENT: &[Line] = &[
    ("JLI_DB_URL", "file:/path/brain.db (default ~/.jli/brain.db) or libsql://… for a cloud brain"),
    ("JLI_SYNC_URL", "libsql://… keeps a local replica in sync with a cloud brain"),
    ("JLI_DB_TOKEN", "auth token for the cloud brain"),
    ("JLI_USER_ID", "the same as --user-id= (the flag wins)"),
    ("JLI_YES", "1 or true: the same as --yes"),
    ("JLI_YOLO", "1 or true: the same as --yolo"),
    ("JLI_QUIET", "1 or true: the same as --quiet"),
    ("JLI_THRESHOLD", "0 to 1: the same as --confidence= (JLI_CONFIDENCE also works)"),
    ("JLI_MODE", "manual, semi or auto: the same as --mode= (the flag wins), but not for\njli teach"),
    ("JLI_PROVIDER", "chatgpt, openrouter, gateway or custom: where language model requests go\n(default: a ChatGPT sign-in, then OPENROUTER_API_KEY, then AI_GATEWAY_API_KEY)"),
    ("JLI_CHATGPT_DIR", "where this host keeps its ChatGPT sign-in (default ~/.jli/chatgpt)"),
    ("JLI_AGENT_MODEL", "the agent's model, over jli config model; JLI_TRANSLATOR_MODEL the translator's"),
];

/// Lines in two columns, what is typed padded to `width`.
fn columns(lines: &[Line], width: usize) -> String {
    let rows: Vec<String> = lines
        .iter()
        .flat_map(|(typed, does)| {
            does.split('\n').enumerate().map(move |(at, part)| {
                let typed = if at == 0 { *typed } else { "" };
                format!("  {typed:<width$} {part}")
            })
        })
        .collect();
    rows.join("\n")
}

fn section(title: &str, body: String) -> String {
    format!("{}\n{body}", c::bold(title))
}

fn command(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.name == name)
}

/// Everything: what jli is, its usage, options, arguments, commands and environment.
pub fn full() -> String {
    let commands: Vec<Line> = COMMANDS.iter().flat_map(|command| command.lines.iter().copied()).collect();
    [
        format!("{}  say what you want, it runs the command, and it learns how you say it", c::bold("jli")),
        section("Usage", columns(USAGE, WIDTH)),
        section("Options", columns(OPTIONS, OPTION_WIDTH)),
        section("Arguments", format!("{}\n  {STATED}", columns(ARGUMENTS, WIDTH))),
        section("Commands", columns(&commands, WIDTH)),
        section("Environment", columns(ENVIRONMENT, ENVIRONMENT_WIDTH)),
    ]
    .join("\n\n")
}

/// One command's lines, the options that matter to it, and where to find the rest.
fn of(command: &Command) -> String {
    let mut parts = vec![section("Usage", columns(command.lines, WIDTH))];
    if !command.options.is_empty() {
        parts.push(section("Options", columns(command.options, OPTION_WIDTH)));
    }
    parts.push("jli --help lists every command, the options they all take, and the environment.".to_string());
    parts.join("\n\n")
}

/// The words of a command line without jli's flags. The word after a bare `--project` is the
/// project's id, unless it names a command.
fn words(head: &[String]) -> Vec<&str> {
    let mut words = Vec::new();
    let mut previous = "";
    for arg in head {
        let id = previous == "--project" && command(arg).is_none();
        if !arg.starts_with('-') && !id {
            words.push(arg.as_str());
        }
        previous = arg;
    }
    words
}

/// The help a command line asks for, if it asks: `help` as its command, or `--help` or `-h`
/// anywhere before `--` (after it they belong to the command). When the first word names a command
/// that command's help is given, and otherwise, for a request or for nothing, the full help.
pub fn asked(args: &[String]) -> Option<String> {
    let head = args.split(|arg| arg == "--").next().unwrap_or_default();
    let words = words(head);
    let named = match words.as_slice() {
        ["help", after @ ..] => after.first(),
        _ if head.iter().any(|arg| arg == "--help" || arg == "-h") => words.first(),
        _ => return None,
    };
    Some(named.and_then(|name| command(name)).map_or_else(full, of))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn help(list: &[&str]) -> Option<String> {
        let args: Vec<String> = list.iter().map(|arg| arg.to_string()).collect();
        asked(&args)
    }

    fn of_command(name: &str) -> Option<String> {
        command(name).map(of)
    }

    #[test]
    fn every_command_has_its_help() {
        let names = [
            "skills",
            "stats",
            "history",
            "teach",
            "config",
            "login",
            "logout",
            "models",
            "fix",
            "run",
            "forget",
            "forget-phrase",
            "alias",
            "slot",
            "eval",
            "share",
            "project",
            "generalize",
            "sync",
            "shell-init",
            "demo",
            "help",
        ];
        assert_eq!(COMMANDS.iter().map(|command| command.name).collect::<Vec<_>>(), names);
        let full = full();
        for command in COMMANDS {
            let lines = columns(command.lines, WIDTH);
            assert!(full.contains(&lines), "the full help holds {}'s lines as they are", command.name);
            assert!(of(command).contains(&lines), "{}", command.name);
            assert!(lines.trim_start().starts_with(&format!("jli {}", command.name)), "{lines}");
        }
    }

    #[test]
    fn a_command_named_first_gets_its_own_help() {
        for (args, name) in [
            (&["skills", "--help"][..], "skills"),
            (&["slot", "--help"][..], "slot"),
            (&["teach", "--help"][..], "teach"),
            (&["--json", "skills", "-h"][..], "skills"),
            (&["help", "skills"][..], "skills"),
            (&["help", "eval", "--help"][..], "eval"),
            (&["-n", "slot", "build-app", "profile", "-h"][..], "slot"),
            (&["--project", "shop", "skills", "--help"][..], "skills"),
            (&["--project=shop", "skills", "--help"][..], "skills"),
            (&["teach", "--help", "--", "ls"][..], "teach"),
        ] {
            assert_eq!(help(args), of_command(name), "{args:?}");
        }
        let slot = help(&["slot", "--help"]).unwrap_or_default();
        assert!(slot.contains("--choice=<value>[=<text>]") && slot.contains("--dry-run"), "{slot}");
        assert!(!slot.contains("jli alias"), "only the slot command's lines: {slot}");
    }

    #[test]
    fn anything_else_that_asks_gets_the_full_help() {
        for args in [
            &["--help"][..],
            &["-h"],
            &["help"],
            &["help", "--help"],
            &["help", "me", "find", "big", "files"],
            &["deploy", "the", "app", "--help"],
            &["show", "me", "the", "skills", "-h"],
            &["--mode=wild", "--help"],
        ] {
            assert_eq!(help(args), Some(full()), "{args:?}");
        }
    }

    #[test]
    fn help_after_the_separator_belongs_to_the_command() {
        assert_eq!(help(&["teach", "x", "--", "ls", "--help"]), None);
        assert_eq!(help(&["fix", "7", "--", "du", "-h"]), None);
        assert_eq!(help(&["show", "disk", "usage", "--", "-h"]), None);
        assert_eq!(help(&["skills"]), None);
        assert_eq!(help(&["show", "the", "help", "page"]), None, "help is a command only as the first word");
        assert_eq!(help(&["what", "does", "--helper", "do"]), None);
        assert_eq!(help(&[]), None);
    }
}
