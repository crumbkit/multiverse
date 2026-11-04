//! End-to-end tests of the workspace verbs that are declared before they are
//! built. Each test runs the built `mxv` binary in an empty directory.

#![cfg(test)]

use serde_json::Value;
use snapbox::cmd::Command;

/// Each root verb with the fewest words that its arguments accept.
const VERBS: &[&[&str]] = &[&["run"], &["archive"], &["restore"]];

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
fn every_unbuilt_verb_fails_with_the_unbuilt_code_and_names_itself() {
    for verb in VERBS {
        let (exit, document) = run_json(verb);
        assert_eq!(exit, 1, "{verb:?}");
        assert_eq!(document["code"], "mxv::command::unbuilt", "{verb:?}");
        assert_eq!(document["fields"]["command"], verb[0], "{verb:?}");
    }
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
