//! End-to-end tests of the command's spelling. Each test runs the built `mxv`
//! binary in an empty directory.

#![cfg(test)]

use serde_json::Value;
use snapbox::cmd::Command;

/// Runs `mxv` with `args` and `--json` in an empty directory, and returns
/// the exit code and the JSON document that it printed.
fn run_json(args: &[&str]) -> (i32, Value) {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mxv"))
        .args(args)
        .arg("--json")
        .current_dir(dir.path())
        .env_clear()
        .env("XDG_STATE_HOME", dir.path())
        .output()
        .unwrap();
    let document = serde_json::from_slice(&output.stdout).unwrap();
    (output.status.code().unwrap(), document)
}

#[test]
fn rm_is_a_hidden_alias_of_remove() {
    let (_, document) = run_json(&["rm", "reading-list", "--yes"]);
    assert_eq!(document["code"], "mxv::workspace::not_found");
    let (_, commands) = run_json(&["commands", "list"]);
    let listed = commands.to_string();
    assert!(listed.contains("\"mxv remove\""));
    assert!(!listed.contains("\"mxv rm\""));
}
