use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What the models are told about where the request was made.
#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    pub cwd: PathBuf,
    pub directory: String,
    pub inside_git_repository: bool,
    pub platform: &'static str,
}

/// The platform by the names the prompts were tuned on (`darwin`, `linux`, `win32`), not Rust's
/// (`macos`, `windows`).
pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn inside_git(cwd: &Path) -> bool {
    cwd.ancestors().any(|dir| dir.join(".git").exists())
}

pub fn read_context(cwd: &Path) -> Context {
    Context {
        cwd: cwd.to_path_buf(),
        directory: cwd.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
        inside_git_repository: inside_git(cwd),
        platform: platform(),
    }
}

/// Entries of the working directory, for file slots.
pub fn list_entries(cwd: &Path) -> Vec<String> {
    const LIMIT: usize = 200;
    let Ok(dir) = std::fs::read_dir(cwd) else { return vec![] };
    let mut names: Vec<String> = dir
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != ".git")
        .collect();
    names.sort();
    names.truncate(LIMIT);
    names
}

pub fn list_branches(cwd: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect(),
        _ => vec![],
    }
}
