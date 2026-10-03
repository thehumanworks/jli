//! Steps that only have an effect in the shell that runs them, such as `cd` and `export`, and the
//! shell function that lets jli hand them to the shell it was called from (ADR 14).

use std::fmt;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, Context as _, Result};

/// The environment variable that carries the hand-off file's path, set by the shell function.
pub const HANDOFF_VAR: &str = "JLI_HANDOFF";
/// The environment variable that names the shell the function was written for.
pub const HANDOFF_SHELL_VAR: &str = "JLI_HANDOFF_SHELL";

/// A shell jli can write its function for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    pub const ALL: [Shell; 3] = [Shell::Zsh, Shell::Bash, Shell::Fish];

    pub fn as_str(self) -> &'static str {
        match self {
            Shell::Zsh => "zsh",
            Shell::Bash => "bash",
            Shell::Fish => "fish",
        }
    }

    /// A shell by its name or by the path of its program, as `$SHELL` gives it: `/bin/zsh`, `-zsh`.
    pub fn parse(text: &str) -> Option<Shell> {
        let name = text.trim().rsplit('/').next().unwrap_or_default().trim_start_matches('-');
        Shell::ALL.into_iter().find(|shell| shell.as_str() == name)
    }

    /// The user's login shell, from `$SHELL`.
    pub fn of_user() -> Option<Shell> {
        std::env::var("SHELL").ok().as_deref().and_then(Shell::parse)
    }

    /// The shell jli is running under, which may not be the login shell: a bash started from zsh
    /// is bash. The shell the function names, else the nearest shell among jli's ancestors, else
    /// `$SHELL` (ADR 23).
    pub fn running(lookup: impl Fn(&str) -> Option<String>, processes: impl FnOnce() -> Option<String>) -> Option<Shell> {
        let parse = |value: Option<String>| value.as_deref().and_then(Shell::parse);
        parse(lookup(HANDOFF_SHELL_VAR))
            .or_else(|| processes().and_then(|table| nearest_shell(&table, std::process::id())))
            .or_else(|| parse(lookup("SHELL")))
    }

    /// The line that installs the function, and where it goes.
    pub fn install_hint(self) -> (&'static str, &'static str) {
        match self {
            Shell::Zsh => ("eval \"$(jli shell-init zsh)\"", "~/.zshrc"),
            Shell::Bash => ("eval \"$(jli shell-init bash)\"", "~/.bashrc"),
            Shell::Fish => ("jli shell-init fish | source", "~/.config/fish/config.fish"),
        }
    }
}

/// How far up the process tree to look for a shell: past a shim, `env`, `sudo` or `time`, not
/// round a loop in a table that changed while it was read.
const ANCESTORS_SEEN: usize = 8;

/// The nearest shell above `pid` in a process table of `pid ppid program` lines, as
/// `ps -A -o pid=,ppid=,comm=` prints them. A program's path may contain spaces.
fn nearest_shell(table: &str, pid: u32) -> Option<Shell> {
    let rows: Vec<(u32, u32, &str)> = table
        .lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let (parent, program) = rest.trim_start().split_once(char::is_whitespace)?;
            Some((pid.parse().ok()?, parent.parse().ok()?, program.trim()))
        })
        .collect();
    let parent_of = |pid: u32| rows.iter().find(|row| row.0 == pid).map(|row| row.1);
    let mut at = parent_of(pid)?;
    for _ in 0..ANCESTORS_SEEN {
        let (_, parent, program) = rows.iter().find(|row| row.0 == at)?;
        if let Some(shell) = Shell::parse(program) {
            return Some(shell);
        }
        at = *parent;
    }
    None
}

impl fmt::Display for Shell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The shell function `jli shell-init` prints. It gives jli a file to write to, runs jli with its
/// arguments untouched, and afterwards sources what jli wrote, in the calling shell. jli's own
/// exit code is kept unless something was handed over, and then it is the status of what ran.
///
/// A ctrl-c while jli runs must not leave the file behind (ADR 23). zsh stops a function when its
/// command is interrupted, unless an INT trap is set, so each function sets one for as long as it
/// runs, and the function then goes on to its own clean-up: jli's status is 130, nothing is
/// sourced, and the file is removed. bash keeps the user's own INT trap and puts it back; zsh does
/// that itself with `local_traps`, which is set only while the trap is (`local_options` would also
/// undo a `setopt` that was handed over). fish runs no signal handler for a ctrl-c that reaches only the
/// command in the foreground, so its file is also removed once the command line is done.
pub fn init_script(shell: Shell) -> String {
    match shell {
        Shell::Bash => format!(
            r#"# jli: commands such as cd and export are handed back to this shell to run (ADR 14).
jli() {{
  local __jli_handoff __jli_status __jli_trap
  __jli_handoff="$(command mktemp "${{TMPDIR:-/tmp}}/jli-handoff.XXXXXX")" || return 1
  __jli_trap="$(trap -p INT)"
  trap 'command rm -f -- "$__jli_handoff"' INT
  {HANDOFF_VAR}="$__jli_handoff" {HANDOFF_SHELL_VAR}=bash command jli "$@"
  __jli_status=$?
  if [ "$__jli_status" -eq 0 ] && [ -s "$__jli_handoff" ]; then
    . "$__jli_handoff"
    __jli_status=$?
  fi
  command rm -f -- "$__jli_handoff"
  trap - INT
  eval "$__jli_trap"
  return "$__jli_status"
}}
"#
        ),
        Shell::Zsh => format!(
            r#"# jli: commands such as cd and export are handed back to this shell to run (ADR 14).
jli() {{
  local __jli_handoff __jli_status __jli_local_traps
  __jli_handoff="$(command mktemp "${{TMPDIR:-/tmp}}/jli-handoff.XXXXXX")" || return 1
  if [[ -o local_traps ]]; then __jli_local_traps=1; else setopt local_traps; fi
  trap 'command rm -f -- "$__jli_handoff"' INT
  [[ -n $__jli_local_traps ]] || unsetopt local_traps
  {HANDOFF_VAR}="$__jli_handoff" {HANDOFF_SHELL_VAR}=zsh command jli "$@"
  __jli_status=$?
  if [ "$__jli_status" -eq 0 ] && [ -s "$__jli_handoff" ]; then
    . "$__jli_handoff"
    __jli_status=$?
  fi
  command rm -f -- "$__jli_handoff"
  return "$__jli_status"
}}
"#
        ),
        Shell::Fish => format!(
            r#"# jli: commands such as cd and export are handed back to this shell to run (ADR 14).
function jli --wraps jli --description 'jli, able to change this shell'
    set -l __jli_handoff (command mktemp)
    or return 1
    set -g __jli_handoffs $__jli_handoffs $__jli_handoff
    command env {HANDOFF_VAR}=$__jli_handoff {HANDOFF_SHELL_VAR}=fish jli $argv
    set -l __jli_status $status
    if test $__jli_status -eq 0; and test -s $__jli_handoff
        source $__jli_handoff
        set __jli_status $status
    end
    command rm -f -- $__jli_handoff
    return $__jli_status
end
# A ctrl-c stops the function before its own rm; this removes the file once the line is done.
function __jli_remove_handoffs --on-event fish_postexec
    set -q __jli_handoffs; or return
    command rm -f -- $__jli_handoffs
    set -e __jli_handoffs
end
"#
        ),
    }
}

/// Where jli hands steps to the calling shell: the file the shell function made for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandOff {
    pub path: PathBuf,
    pub shell: Shell,
}

impl HandOff {
    /// The hand-off the shell function set up, if jli was started by it.
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Option<HandOff> {
        let path = lookup(HANDOFF_VAR).filter(|path| !path.trim().is_empty())?;
        let shell = lookup(HANDOFF_SHELL_VAR).as_deref().and_then(Shell::parse).unwrap_or(Shell::Bash);
        Some(HandOff { path: PathBuf::from(path), shell })
    }

    /// Append the steps for the calling shell to run, in order, stopping at the first failure.
    /// The file must already exist: jli never creates a file the shell function did not make.
    pub fn write(&self, steps: &[String]) -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .with_context(|| format!("the shell function's hand-off file {} cannot be written", self.path.display()))?;
        writeln!(file, "{}", hand_off_text(steps, self.shell)).map_err(|error| anyhow!("{error}"))
    }
}

/// The text the calling shell evaluates. One step is exactly that step. Several are each kept
/// verbatim inside a group, so a `;`, `||` or `# comment` in one cannot reach the next, and each
/// runs only if the one before succeeded, as in jli's own shell.
pub fn hand_off_text(steps: &[String], shell: Shell) -> String {
    match steps {
        [one] => one.clone(),
        _ => {
            let groups: Vec<String> = match shell {
                Shell::Zsh | Shell::Bash => steps.iter().map(|step| format!("{{ {step}\n}}")).collect(),
                Shell::Fish => steps.iter().map(|step| format!("begin\n{step}\nend")).collect(),
            };
            groups.join(if shell == Shell::Fish { "\nand " } else { " && " })
        }
    }
}

/// Commands whose whole effect is on the shell that runs them. In jli's own shell they would
/// exit 0 and change nothing the user can see.
const CALLER_SHELL: [&str; 16] = [
    "cd", "chdir", "pushd", "popd", "export", "unset", "source", ".", "alias", "unalias", "set", "setopt", "unsetopt",
    "shopt", "umask", "ulimit",
];

/// Of those, the ones that touch nothing outside the shell and mean the same in bash and zsh, so
/// jli can try them in a shell of its own first and see whether they fail.
const SAFE_TO_TRY: [&str; 7] = ["cd", "pushd", "export", "unset", "alias", "unalias", "umask"];

/// Never handed over: in the calling shell these would end it or replace it.
const NEVER_HANDED: [&str; 3] = ["exit", "logout", "exec"];

/// Words that may stand before a command without changing which command it is.
const PREFIXES: [&str; 4] = ["{", "}", "!", "builtin"];

fn is_assignment(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && word.split_once('=').is_some_and(|(name, _)| name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_'))
}

/// The commands a step runs at its top level, each as its words. Text in quotes, in `( … )` or
/// `$( … )`, and after a `#` is not split: `(cd a && make)` is one word and runs in a subshell.
fn commands_of(step: &str) -> Vec<Vec<String>> {
    let mut commands: Vec<Vec<String>> = vec![];
    let mut words: Vec<String> = vec![];
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    let mut chars = step.chars().peekable();
    let end_word = |word: &mut String, words: &mut Vec<String>| {
        if !word.is_empty() {
            words.push(std::mem::take(word));
        }
    };
    while let Some(ch) = chars.next() {
        if let Some(open) = quote {
            word.push(ch);
            if ch == '\\' && open != '\'' {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            } else if ch == open {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => {
                quote = Some(ch);
                word.push(ch);
            }
            '\\' => {
                word.push(ch);
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            '(' => {
                depth += 1;
                word.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                word.push(ch);
            }
            _ if depth > 0 => word.push(ch),
            '#' if word.is_empty() => {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            ';' | '&' | '|' | '\n' => {
                end_word(&mut word, &mut words);
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            _ if ch.is_whitespace() => end_word(&mut word, &mut words),
            _ => word.push(ch),
        }
    }
    end_word(&mut word, &mut words);
    if !words.is_empty() {
        commands.push(words);
    }
    commands
}

/// The command a list of words runs, past any `{`, `!` or `builtin` before it.
fn head(words: &[String]) -> Option<&str> {
    words.iter().map(String::as_str).find(|word| !PREFIXES.contains(word))
}

/// The words that matter: a command, or None for a run of variable assignments with no command.
fn command_name(words: &[String]) -> Option<Option<&str>> {
    let rest: Vec<&String> = words.iter().filter(|word| !PREFIXES.contains(&word.as_str())).collect();
    if rest.is_empty() {
        return None;
    }
    if rest.iter().all(|word| is_assignment(word)) {
        return Some(None);
    }
    Some(head(words))
}

/// Whether a step has to run in the calling shell to do anything: one of its top-level commands
/// is a builtin such as `cd` or `export`, or a bare `NAME=value`. A step that would also run
/// `exit` or `exec` is never handed over.
pub fn needs_caller(step: &str) -> bool {
    let commands = commands_of(step);
    let names: Vec<Option<&str>> = commands.iter().filter_map(|words| command_name(words)).collect();
    let never = names.iter().flatten().any(|name| NEVER_HANDED.contains(name));
    !never && names.iter().any(|name| name.is_none_or(|name| CALLER_SHELL.contains(&name)))
}

/// Whether jli can try the steps in a shell of its own before handing them over: every command
/// in them is one that changes nothing outside that shell.
/// A `$( … )`, `<( … )` or backtick would run its command once here and again in the calling shell.
/// `cd -`, `popd` and a bare `pushd` read state that only the calling shell has: `OLDPWD` and the
/// directory stack.
pub fn safe_to_try(steps: &[String]) -> bool {
    let tryable = |words: &[String]| {
        let Some(name) = command_name(words) else { return true };
        let Some(name) = name else { return true };
        let args: Vec<&String> = words.iter().skip_while(|word| word.as_str() != name).skip(1).collect();
        let plain = args.iter().all(|arg| !arg.starts_with(['-', '+']));
        match name {
            "cd" => plain && args.len() <= 1,
            "pushd" => plain && args.len() == 1,
            _ => SAFE_TO_TRY.contains(&name),
        }
    };
    steps.iter().all(|step| {
        !step.contains("$(")
            && !step.contains("<(")
            && !step.contains('`')
            && commands_of(step).iter().all(|words| tryable(words))
    })
}

/// The first step that needs the calling shell. It and every step after it are handed over, so
/// the later steps see what it changed.
pub fn first_caller_step(steps: &[String]) -> Option<usize> {
    steps.iter().position(|step| needs_caller(step))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn builtins_that_change_the_shell_are_recognised() {
        for step in [
            "cd src",
            "cd",
            "  cd '/tmp/a b'",
            "pushd ..",
            "popd",
            "export EDITOR=vim",
            "unset EDITOR",
            "source .venv/bin/activate",
            ". ./env.sh",
            "alias gs='git status'",
            "EDITOR=vim",
            "A=1 B='two words'",
            "git clone x && cd x",
            "make; cd build",
            "{ cd src; }",
            "builtin cd src",
            "test -d src || cd ..",
        ] {
            assert!(needs_caller(step), "{step}");
        }
        for step in [
            "ls -ltr",
            "EDITOR=vim git commit",
            "(cd src && make)",
            "echo $(cd src; pwd)",
            "echo 'cd src'",
            "echo \"a; cd b\"",
            "git commit -m 'x; cd /'",
            "echo cd",
            "ls # ; cd src",
            "find . -name cd",
            "cd src && exit",
            "exec zsh",
            "",
        ] {
            assert!(!needs_caller(step), "{step}");
        }
    }

    #[test]
    fn a_quoted_value_cannot_make_a_step_a_builtin() {
        // Values are rendered quoted, so `; cd /` inside one stays a word of the command.
        let rendered = crate::slots::render(
            &steps(&["git commit -m {message}"]),
            &[("message".to_string(), "x'; cd / ; echo '".to_string())].into_iter().collect(),
        );
        assert!(!needs_caller(&rendered[0]), "{}", rendered[0]);
    }

    #[test]
    fn everything_from_the_first_builtin_on_is_handed_over() {
        assert_eq!(first_caller_step(&steps(&["git clone u", "cd repo", "make"])), Some(1));
        assert_eq!(first_caller_step(&steps(&["ls", "pwd"])), None);
        assert!(safe_to_try(&steps(&["cd repo", "export A=1 && alias x=y", "B=2"])));
        assert!(!safe_to_try(&steps(&["cd repo", "make"])));
        assert!(!safe_to_try(&steps(&["source ~/.zshrc"])), "a sourced file is a script of its own");
        assert!(!safe_to_try(&steps(&["setopt autocd"])), "zsh only: bash would refuse it");
        assert!(safe_to_try(&steps(&["cd", "cd '/tmp/a b'", "pushd /tmp"])));
        for step in ["cd -", "popd", "pushd", "pushd +1", "cd -P x"] {
            assert!(!safe_to_try(&steps(&[step])), "{step} reads the calling shell's own state");
        }
        for step in ["export STAMP=$(date)", "cd \"$(git rev-parse --show-toplevel)\"", "cd `pwd`", "source <(x)"] {
            assert!(!safe_to_try(&steps(&[step])), "{step} would run its command twice");
        }
    }

    #[test]
    fn the_hand_off_is_the_rendered_steps() {
        assert_eq!(hand_off_text(&steps(&["cd '/tmp/a b'"]), Shell::Zsh), "cd '/tmp/a b'");
        assert_eq!(
            hand_off_text(&steps(&["cd x # go", "make || true"]), Shell::Bash),
            "{ cd x # go\n} && { make || true\n}"
        );
        assert_eq!(hand_off_text(&steps(&["cd x", "make"]), Shell::Fish), "begin\ncd x\nend\nand begin\nmake\nend");
    }

    #[test]
    fn shells_are_named_or_found_from_their_path() {
        assert_eq!(Shell::parse("/bin/zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::parse("-bash"), Some(Shell::Bash));
        assert_eq!(Shell::parse("/opt/homebrew/bin/fish"), Some(Shell::Fish));
        assert_eq!(Shell::parse("tcsh"), None);
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
        };
        assert_eq!(HandOff::from_env(env(&[])), None);
        assert_eq!(HandOff::from_env(env(&[(HANDOFF_VAR, " ")])), None);
        assert_eq!(
            HandOff::from_env(env(&[(HANDOFF_VAR, "/tmp/h"), (HANDOFF_SHELL_VAR, "fish")])),
            Some(HandOff { path: "/tmp/h".into(), shell: Shell::Fish })
        );
    }

    #[test]
    fn the_hand_off_file_is_only_written_when_the_function_made_it() {
        let dir = std::env::temp_dir().join(format!("jli-handoff-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = HandOff { path: dir.join("missing"), shell: Shell::Zsh };
        assert!(missing.write(&steps(&["cd x"])).is_err());
        assert!(!missing.path.exists(), "no file is created");
        let made = HandOff { path: dir.join("made"), shell: Shell::Zsh };
        std::fs::write(&made.path, "").unwrap();
        made.write(&steps(&["cd x"])).unwrap();
        made.write(&steps(&["export A=1"])).unwrap();
        assert_eq!(std::fs::read_to_string(&made.path).unwrap(), "cd x\nexport A=1\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_function_passes_arguments_through_and_keeps_the_exit_code() {
        for shell in [Shell::Zsh, Shell::Bash] {
            let script = init_script(shell);
            assert!(script.contains("command jli \"$@\""), "{script}");
            assert!(script.contains(&format!("{HANDOFF_SHELL_VAR}={shell}")));
            assert!(script.contains("return \"$__jli_status\""));
        }
        let fish = init_script(Shell::Fish);
        assert!(fish.contains("jli $argv") && fish.contains("source $__jli_handoff"));
    }

    #[test]
    fn an_interrupted_function_still_removes_its_file() {
        // Issue #22: a ctrl-c stopped zsh's function before its rm. The trap is set before jli
        // runs, and bash and zsh put back the user's own INT trap afterwards.
        let remove = "trap 'command rm -f -- \"$__jli_handoff\"' INT";
        let bash = init_script(Shell::Bash);
        let order = ["__jli_trap=\"$(trap -p INT)\"", remove, "command jli", "trap - INT", "eval \"$__jli_trap\""];
        let at: Vec<usize> = order.iter().map(|part| bash.find(part).unwrap_or_else(|| panic!("{part}\n{bash}"))).collect();
        assert!(at.windows(2).all(|pair| pair[0] < pair[1]), "{bash}");
        assert!(!bash.contains("return 130"), "the function runs on to its own clean-up");

        let zsh = init_script(Shell::Zsh);
        let order = ["setopt local_traps", remove, "unsetopt local_traps", "command jli"];
        let at: Vec<usize> = order.iter().map(|part| zsh.find(part).unwrap_or_else(|| panic!("{part}\n{zsh}"))).collect();
        assert!(at.windows(2).all(|pair| pair[0] < pair[1]), "{zsh}");
        assert!(!zsh.contains("local_options"), "a handed-over setopt must outlast the function");

        let fish = init_script(Shell::Fish);
        assert!(fish.contains("set -g __jli_handoffs $__jli_handoffs $__jli_handoff"), "{fish}");
        assert!(fish.contains("--on-event fish_postexec") && fish.contains("command rm -f -- $__jli_handoffs"), "{fish}");
    }

    #[test]
    fn the_running_shell_is_the_function_s_then_the_nearest_ancestor_then_the_login_shell() {
        // Issue #22: in a bash started from zsh, the hint named ~/.zshrc.
        let me = std::process::id();
        let table = format!(
            "    1     0 /sbin/launchd\n  500     1 /Applications/Some Terminal.app/Contents/MacOS/term\n  \
             600   500 -zsh\n  700   600 /bin/bash\n  800   700 /opt/homebrew/bin/mise\n {me}   800 jli\n"
        );
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
        };
        let login_zsh: &[(&str, &str)] = &[("SHELL", "/bin/zsh")];
        assert_eq!(Shell::running(env(login_zsh), || Some(table.clone())), Some(Shell::Bash), "past the shim to bash");
        assert_eq!(
            Shell::running(env(&[("SHELL", "/bin/zsh"), (HANDOFF_SHELL_VAR, "fish")]), || Some(table.clone())),
            Some(Shell::Fish)
        );
        assert_eq!(Shell::running(env(login_zsh), || None), Some(Shell::Zsh), "no ps: the login shell");
        let no_shell = format!("  1 0 launchd\n 42 1 node\n {me} 42 jli\n");
        assert_eq!(Shell::running(env(login_zsh), || Some(no_shell)), Some(Shell::Zsh));
        assert_eq!(Shell::running(env(&[]), || None), None);
        assert_eq!(nearest_shell(&table, 600), None, "the shell's own parent is a terminal");
        assert_eq!(nearest_shell(&table, 700), Some(Shell::Zsh), "a login shell shows as -zsh");
        let looped = format!("  5 6 env\n  6 5 env\n {me} 5 jli\n");
        assert_eq!(nearest_shell(&looped, me), None, "a loop ends");
    }
}
