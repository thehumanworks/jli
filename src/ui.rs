use std::fmt::Display;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::LazyLock;

use crate::system1::{Decision, NONE};
use crate::system2::{Proposal, ProposalKind};
use crate::types::Risk;

fn set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

static USE_COLOR: LazyLock<bool> = LazyLock::new(|| !set("NO_COLOR") && (io::stderr().is_terminal() || set("FORCE_COLOR")));

fn paint(code: &str, text: impl Display) -> String {
    if *USE_COLOR {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Colours, off when `NO_COLOR` is set or stderr is not a terminal.
pub mod c {
    use super::paint;
    use std::fmt::Display;

    pub fn dim(text: impl Display) -> String {
        paint("2", text)
    }
    pub fn bold(text: impl Display) -> String {
        paint("1", text)
    }
    pub fn green(text: impl Display) -> String {
        paint("32", text)
    }
    pub fn yellow(text: impl Display) -> String {
        paint("33", text)
    }
    pub fn red(text: impl Display) -> String {
        paint("31", text)
    }
    pub fn cyan(text: impl Display) -> String {
        paint("36", text)
    }
    pub fn magenta(text: impl Display) -> String {
        paint("35", text)
    }
}

/// Fixed-point with halves rounded up, as the trace has always shown them. Rust's own
/// formatting rounds halves to even.
pub fn fixed(value: f64, digits: usize) -> String {
    let scale = 10f64.powi(digits as i32);
    let rounded = (value * scale).round() / scale;
    format!("{rounded:.digits$}")
}

pub fn bar(fraction: f64) -> String {
    const WIDTH: usize = 20;
    let filled = (fraction.clamp(0.0, 1.0) * WIDTH as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), c::dim("░".repeat(WIDTH - filled)))
}

pub fn sparkline(fractions: &[f64]) -> String {
    const SPARKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    fractions
        .iter()
        .map(|fraction| {
            let index = (fraction * SPARKS.len() as f64).floor();
            if index.is_nan() || index < 0.0 {
                ' '
            } else {
                SPARKS[(index as usize).min(SPARKS.len() - 1)]
            }
        })
        .collect()
}

pub fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{} s", fixed(ms as f64 / 1000.0, 1))
    }
}

pub fn risk_tag(risk: Risk) -> String {
    match risk {
        Risk::Safe => c::green(risk),
        Risk::Modifies => c::yellow(risk),
        Risk::Destructive => c::red(risk),
    }
}

/// The decision trace. Everything goes to stderr so stdout stays the command's own output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ui {
    /// System 1's table, the agent's lines, notes and what was learned.
    pub enabled: bool,
    /// Why nothing ran, or that a command failed.
    pub warnings: bool,
}

impl Ui {
    /// The trace and its warnings, both on or both off.
    pub fn new(enabled: bool) -> Ui {
        Ui { enabled, warnings: enabled }
    }

    /// As the flags ask: `--quiet` hides the trace but keeps the warnings, so a run that does nothing
    /// still says why; `--json` hides both, since its outcome says the same.
    pub fn of(quiet: bool, json: bool) -> Ui {
        Ui { enabled: !quiet && !json, warnings: !json }
    }

    pub fn line(&self, text: impl Display) {
        if self.enabled {
            eprintln!("{text}");
        }
    }

    pub fn decision(&self, decision: &Decision) {
        self.line(format!(
            "{} {}",
            c::cyan("⚡ system 1"),
            c::dim(format!(
                "· {} · {} · {} skills in the matrix",
                decision.model,
                duration(decision.ms),
                decision.matrix_size
            ))
        ));
        let shown: Vec<_> = decision.candidates.iter().filter(|candidate| candidate.probability > 0.0).take(3).collect();
        let width = shown.iter().map(|candidate| candidate.label.chars().count()).max().unwrap_or(0);
        for candidate in shown {
            let padded = format!("{:<width$}", candidate.label);
            let label = if candidate.label == NONE { c::dim(padded) } else { padded };
            self.line(format!("   {label}  {}  {}", bar(candidate.probability), fixed(candidate.probability, 2)));
        }
    }

    pub fn escalate(&self, reason: &str, model: &str) {
        self.line(format!("{} {}", c::magenta("◆ system 2"), c::dim(format!("· {model} · {reason}"))));
    }

    pub fn proposal(&self, proposal: &Proposal, ms: u64) {
        let what = match proposal.kind {
            ProposalKind::Existing => {
                format!("matched known skill {}", c::bold(proposal.existing_label.as_deref().unwrap_or("?")))
            }
            ProposalKind::New => format!("proposes new skill {}", c::bold(proposal.label.as_deref().unwrap_or("?"))),
            ProposalKind::NotACommand => "says this is not a command".to_string(),
        };
        self.line(format!("   {what} {}", c::dim(format!("· {}", duration(ms)))));
        if !proposal.explanation.is_empty() {
            self.line(format!("   {}", c::dim(&proposal.explanation)));
        }
    }

    pub fn command(&self, commands: &[String], risk: Risk) {
        for (index, command) in commands.iter().enumerate() {
            let tag = if index == 0 { format!("  {} {}", c::dim("risk:"), risk_tag(risk)) } else { String::new() };
            self.line(format!("{} {}{tag}", c::bold("→"), c::bold(command)));
        }
    }

    pub fn learned(&self, text: impl Display) {
        self.line(format!("{} {text}", c::green("✚ learned")));
    }

    pub fn note(&self, text: impl Display) {
        self.line(c::dim(format!("  {text}")));
    }

    pub fn warn(&self, text: impl Display) {
        if self.warnings {
            eprintln!("{} {text}", c::yellow("!"));
        }
    }
}

/// Everything that reads from the person at the keyboard. One owner of stdin: the session
/// prompt and the confirmation prompt read through the same handle, so they cannot collide.
pub trait Prompt {
    /// One line of input, or None at end of input.
    fn read_line(&mut self) -> Option<String>;
    /// Ask a yes/no question. None when nobody is there to answer.
    fn confirm(&mut self, question: &str) -> Option<bool>;
    /// Ask a free-text question. None when nobody is there to answer.
    fn ask(&mut self, question: &str) -> Option<String>;
}

/// The real terminal.
pub struct Terminal {
    stdin: io::Stdin,
    interactive: bool,
}

impl Terminal {
    pub fn new() -> Terminal {
        let stdin = io::stdin();
        let interactive = stdin.is_terminal();
        Terminal { stdin, interactive }
    }

    pub fn interactive(&self) -> bool {
        self.interactive
    }

    /// Nobody at the keyboard, whatever stdin the tests were started with.
    #[cfg(test)]
    pub fn not_a_terminal() -> Terminal {
        Terminal { stdin: io::stdin(), interactive: false }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Terminal::new()
    }
}

fn prompt(text: &str) {
    let mut stderr = io::stderr();
    let _ = write!(stderr, "{text}");
    let _ = stderr.flush();
}

impl Prompt for Terminal {
    fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                let trimmed = line.strip_suffix('\n').unwrap_or(&line);
                Some(trimmed.strip_suffix('\r').unwrap_or(trimmed).to_string())
            }
        }
    }

    fn confirm(&mut self, question: &str) -> Option<bool> {
        if !self.interactive {
            return None;
        }
        prompt(&format!("{} {question} {} ", c::yellow("?"), c::dim("[Y/n]")));
        let answer = self.read_line()?.trim().to_lowercase();
        Some(answer.is_empty() || answer == "y" || answer == "yes")
    }

    fn ask(&mut self, question: &str) -> Option<String> {
        if !self.interactive {
            return None;
        }
        prompt(&format!("{} {question}\n{} ", c::yellow("?"), c::yellow("›")));
        self.read_line()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_fixed_rounds_halves_up() {
        assert_eq!(fixed(0.98, 2), "0.98");
        assert_eq!(fixed(1.25, 1), "1.3");
        assert_eq!(fixed(1.0, 2), "1.00");
        assert_eq!(duration(289), "289 ms");
        assert_eq!(duration(16_400), "16.4 s");
    }

    #[test]
    fn quiet_keeps_the_warnings_and_json_hides_them() {
        let ui = |quiet, json| {
            let ui = Ui::of(quiet, json);
            (ui.enabled, ui.warnings)
        };
        assert_eq!(ui(false, false), (true, true), "no flag: the trace and its warnings");
        assert_eq!(ui(true, false), (false, true), "--quiet: the warnings only");
        assert_eq!(ui(false, true), (false, false), "--json: the outcome says it");
        assert_eq!(ui(true, true), (false, false));
        assert_eq!(Ui::new(true), Ui::of(false, false));
        assert_eq!(Ui::new(false), Ui::of(false, true));
    }

    #[test]
    fn sparklines_cover_the_range() {
        assert_eq!(sparkline(&[0.0, 0.5, 1.0]), "▁▅█");
    }
}
