use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context as _, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunResult {
    pub exit_code: i32,
    /// Only set when output was captured instead of streamed.
    pub output: Option<String>,
    /// The end of stderr, when it was watched or captured.
    pub stderr: String,
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

/// Run shell steps in order with `bash -c`, stopping at the first failure.
pub fn run(commands: &[String], cwd: &Path, options: RunOptions) -> Result<RunResult> {
    let mut output = String::new();
    let mut stderr = String::new();
    for command in commands {
        let mut shell = Command::new("bash");
        shell.arg("-c").arg(command).current_dir(cwd);
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
            return Ok(RunResult { exit_code, output: options.capture.then_some(output), stderr });
        }
    }
    Ok(RunResult { exit_code: 0, output: options.capture.then_some(output), stderr })
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
    }

    #[test]
    fn watched_stderr_is_forwarded_and_its_tail_kept() {
        let result = run(
            &steps(&["echo watched >&2; exit 2"]),
            Path::new("/"),
            RunOptions { capture: false, watch_stderr: true },
        )
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
    fn a_signal_is_reported_like_a_shell_would() {
        let result = run(&steps(&["kill -9 $$"]), Path::new("/"), RunOptions::default()).unwrap();
        assert_eq!(result.exit_code, 137);
    }
}
