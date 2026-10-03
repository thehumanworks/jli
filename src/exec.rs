use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context as _, Result};

use crate::shell::{HANDOFF_SHELL_VAR, HANDOFF_VAR};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunResult {
    pub exit_code: i32,
    /// Only set when output was captured instead of streamed.
    pub output: Option<String>,
    /// The end of stderr, when it was watched or captured.
    pub stderr: String,
    /// stdout alone, and all of stderr, only when output was captured: `output` holds both, one after the other.
    pub captured_stdout: Option<String>,
    pub captured_stderr: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// Capture all output instead of streaming it.
    pub capture: bool,
    /// Stream stderr through and keep a copy, so a failure can be diagnosed.
    pub watch_stderr: bool,
}

const STDERR_KEPT: usize = 4000;

fn last_chars(text: &str, count: usize) -> String {
    let total = text.chars().count();
    text.chars().skip(total.saturating_sub(count)).collect()
}

/// Forward everything from `pipe` as it arrives, and return the end of it.
fn tee(mut pipe: impl Read, forward: &mut dyn Write) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let _ = forward.write_all(&chunk[..n]);
                let _ = forward.flush();
                kept.extend_from_slice(&chunk[..n]);
                // Four bytes per character at most, so this always holds the last STDERR_KEPT.
                if kept.len() > 8 * STDERR_KEPT {
                    kept.drain(..kept.len() - 4 * STDERR_KEPT);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    last_chars(&String::from_utf8_lossy(&kept), STDERR_KEPT)
}

fn code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// A `bash -c` for one step. The shell function's hand-off is jli's alone: a step that itself
/// runs `jli` must not write into the calling shell's file (ADR 14).
fn bash(command: &str, cwd: &Path) -> Command {
    let mut shell = Command::new("bash");
    shell.arg("-c").arg(command).current_dir(cwd).env_remove(HANDOFF_VAR).env_remove(HANDOFF_SHELL_VAR);
    shell
}

/// Try a script of shell builtins in a shell of jli's own, to see whether it fails before it is
/// handed to the calling shell. Its output is not wanted, since the calling shell will print it;
/// stderr is kept, and shown or captured as for any step.
pub fn try_builtins(script: &str, cwd: &Path, capture: bool) -> Result<RunResult> {
    let mut shell = bash(script, cwd);
    shell.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let (status, stderr) = if capture {
        let done = shell.output().context("could not start bash")?;
        (done.status, last_chars(&String::from_utf8_lossy(&done.stderr), STDERR_KEPT))
    } else {
        let mut child = shell.spawn().context("could not start bash")?;
        let pipe = child.stderr.take().expect("stderr is piped");
        let kept = tee(pipe, &mut std::io::stderr());
        (child.wait().context("bash did not finish")?, kept)
    };
    Ok(RunResult {
        exit_code: code(status),
        output: capture.then(|| stderr.clone()),
        captured_stdout: capture.then(String::new),
        captured_stderr: capture.then(|| stderr.clone()),
        stderr,
    })
}

/// Every process's id, parent's id and program, one per line, as `ps` prints them, so the
/// shell jli was started from can be found (ADR 23). None when `ps` cannot be run.
pub fn process_table() -> Option<String> {
    let done = Command::new("ps").args(["-A", "-o", "pid=,ppid=,comm="]).stdin(Stdio::null()).output().ok()?;
    done.status.success().then(|| String::from_utf8_lossy(&done.stdout).into_owned())
}

/// Run shell steps in order with `bash -c`, stopping at the first failure.
pub fn run(commands: &[String], cwd: &Path, options: RunOptions) -> Result<RunResult> {
    let mut output = String::new();
    let mut stderr = String::new();
    let (mut all_stdout, mut all_stderr) = (String::new(), String::new());
    for command in commands {
        let mut shell = bash(command, cwd);
        let status = if options.capture {
            let done = shell
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .context("could not start bash")?;
            let err = String::from_utf8_lossy(&done.stderr);
            output.push_str(&String::from_utf8_lossy(&done.stdout));
            output.push_str(&err);
            all_stdout.push_str(&String::from_utf8_lossy(&done.stdout));
            all_stderr.push_str(&err);
            stderr = last_chars(&(stderr + &err), STDERR_KEPT);
            done.status
        } else if options.watch_stderr {
            let mut child = shell.stderr(Stdio::piped()).spawn().context("could not start bash")?;
            let pipe = child.stderr.take().expect("stderr is piped");
            let kept = tee(pipe, &mut std::io::stderr());
            stderr = last_chars(&(stderr + &kept), STDERR_KEPT);
            child.wait().context("bash did not finish")?
        } else {
            shell.status().context("could not start bash")?
        };
        let exit_code = code(status);
        if exit_code != 0 {
            return Ok(RunResult {
                exit_code,
                output: options.capture.then_some(output),
                stderr,
                captured_stdout: options.capture.then_some(all_stdout),
                captured_stderr: options.capture.then_some(all_stderr),
            });
        }
    }
    Ok(RunResult {
        exit_code: 0,
        output: options.capture.then_some(output),
        stderr,
        captured_stdout: options.capture.then_some(all_stdout),
        captured_stderr: options.capture.then_some(all_stderr),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stops_at_the_first_failure_and_captures_both_streams() {
        let result = run(
            &steps(&["echo one; echo two >&2", "exit 7", "echo never"]),
            Path::new("/"),
            RunOptions { capture: true, watch_stderr: false },
        )
        .unwrap();
        assert_eq!(result.exit_code, 7);
        assert_eq!(result.output.as_deref(), Some("one\ntwo\n"));
        assert_eq!(result.stderr, "two\n");
        assert_eq!((result.captured_stdout.as_deref(), result.captured_stderr.as_deref()), (Some("one\n"), Some("two\n")));
    }

    #[test]
    fn watched_stderr_is_forwarded_and_its_tail_kept() {
        let result =
            run(&steps(&["echo watched >&2; exit 2"]), Path::new("/"), RunOptions { capture: false, watch_stderr: true })
                .unwrap();
        assert_eq!((result.exit_code, result.output, result.stderr.as_str()), (2, None, "watched\n"));

        let long = format!("{}é{}", "a".repeat(50_000), "b".repeat(STDERR_KEPT - 1));
        let mut forwarded = Vec::new();
        let kept = tee(long.as_bytes(), &mut forwarded);
        assert_eq!(forwarded, long.as_bytes(), "everything is forwarded");
        assert_eq!(kept.chars().count(), STDERR_KEPT);
        assert!(kept.starts_with('é'), "the tail is whole characters");
    }

    #[test]
    fn builtins_are_tried_without_their_output() {
        let tried = try_builtins("cd /nowhere-at-all", Path::new("/"), true).unwrap();
        assert_ne!(tried.exit_code, 0);
        assert!(tried.stderr.contains("nowhere-at-all"), "{}", tried.stderr);
        let tried = try_builtins("pushd /tmp && export A=1", Path::new("/"), true).unwrap();
        assert_eq!((tried.exit_code, tried.output.as_deref()), (0, Some("")), "pushd prints the stack, which is dropped");
    }

    #[test]
    fn a_step_cannot_see_the_hand_off() {
        // Removed from every step's environment, whether or not jli itself was given them.
        let shell = bash("jli go to src", Path::new("/"));
        let removed: Vec<_> = shell.get_envs().filter(|(_, value)| value.is_none()).map(|(name, _)| name).collect();
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&std::ffi::OsStr::new(HANDOFF_VAR)));
        assert!(removed.contains(&std::ffi::OsStr::new(HANDOFF_SHELL_VAR)));
    }

    #[test]
    fn a_signal_is_reported_like_a_shell_would() {
        let result = run(&steps(&["kill -9 $$"]), Path::new("/"), RunOptions::default()).unwrap();
        assert_eq!(result.exit_code, 137);
    }
}
