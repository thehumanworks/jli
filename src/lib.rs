//! jli: say what you want in plain language, it runs the command, and it learns how you say it.
//!
//! System 1 (`system1`) decides on every request against a decision matrix that is a database
//! table (`store`). System 2 (`system2`) is asked only when System 1 has no confident pick. A fast
//! translator (`translator`, `suggest`) turns what is taught, declined or failed into matrix rows.
//! `router` is the loop that ties them together.

pub mod args;
pub mod cli;
pub mod context;
pub mod demo;
pub mod eval;
pub mod exec;
pub mod learn;
pub mod llm;
pub mod router;
pub mod seed;
pub mod shell;
pub mod slots;
pub mod store;
pub mod suggest;
pub mod system1;
pub mod system2;
pub mod translator;
pub mod types;
pub mod ui;

#[cfg(test)]
mod tests;

/// The models a request can reach: System 1, and the language models behind System 2 and the
/// translator. Each is built from the environment on first use, or faked in tests.
#[derive(Default)]
pub struct Models {
    pub system1: system1::System1,
    pub llm: llm::Llm,
}

impl Models {
    pub fn from_env() -> Models {
        Models { system1: system1::System1::from_env(), llm: llm::Llm::from_env() }
    }
}
