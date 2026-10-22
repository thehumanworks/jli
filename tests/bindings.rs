//! Separate CLI processes use only a disposable brain and fixture commands, with no model credentials.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("jli-binding-{name}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_jli"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("JLI_DB_URL", format!("file:{}", self.0.join("brain.db").display()))
            .env("JLI_CHATGPT_DIR", self.0.join("unused-chatgpt"))
            .current_dir(&self.0)
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        output
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.run(args).stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn correction_bindings_survive_fifty_cli_lessons_and_process_reopen() {
    let fixture = Fixture::new("reopen");
    fixture.run(&["--mode=manual", "--risk=safe", "teach", "my phrase", "--", "printf correct"]);
    for n in 1..=50 {
        fixture.run(&["--mode=manual", "teach", &format!("later lesson {n}"), "--", "printf correct"]);
        if [1, 10, 50].contains(&n) {
            let result = fixture.json(&["--json", "-n", "my phrase"]);
            assert_eq!(result["path"], "binding");
            assert_eq!(result["commands"], serde_json::json!(["printf correct"]));
            let plan = result["planId"].as_i64().unwrap().to_string();
            let run = fixture.json(&["--json", "run", &plan]);
            assert!(run.to_string().contains("correct"));
        }
    }
    assert!(!fixture.0.join("unused-chatgpt").exists(), "no runtime credential directory was opened");
    let rules = fixture.json(&["--json", "bindings"]);
    assert_eq!(rules.as_array().unwrap().len(), 51);
    let id = rules.as_array().unwrap().iter().find(|rule| rule["phrase"] == "my phrase").unwrap()["id"].to_string();
    fixture.run(&["bindings", "remove", &id]);
    let remaining = fixture.json(&["--json", "bindings"]);
    assert!(!remaining.as_array().unwrap().iter().any(|rule| rule["phrase"] == "my phrase"));
}

#[test]
fn concurrent_cli_writers_keep_separate_rules_and_one_complete_choice() {
    let fixture = Fixture::new("concurrent");
    fixture.run(&["--mode=manual", "--risk=safe", "teach", "initial", "--", "printf first"]);
    // Both processes edit one phrase; the last committed choice wins as a whole.
    let one = fixture.command(&["--mode=manual", "teach", "contended", "--", "printf first"]).spawn().unwrap();
    let two = fixture.command(&["--mode=manual", "teach", "contended", "--", "printf second"]).spawn().unwrap();
    assert!(one.wait_with_output().unwrap().status.success());
    assert!(two.wait_with_output().unwrap().status.success());
    let bindings = fixture.json(&["--json", "bindings"]);
    assert_eq!(bindings.as_array().unwrap().iter().filter(|rule| rule["phrase"] == "contended").count(), 1);
    let routed = fixture.json(&["--json", "-n", "contended"]);
    assert_eq!(routed["path"], "binding");
    assert!(["printf first", "printf second"].contains(&routed["commands"][0].as_str().unwrap()));
    let one = fixture.command(&["--mode=manual", "teach", "one", "--", "printf first"]).spawn().unwrap();
    let two = fixture.command(&["--mode=manual", "teach", "two", "--", "printf first"]).spawn().unwrap();
    assert!(one.wait_with_output().unwrap().status.success());
    assert!(two.wait_with_output().unwrap().status.success());
    let bindings = fixture.json(&["--json", "bindings"]);
    assert_eq!(bindings.as_array().unwrap().len(), 4);
}
