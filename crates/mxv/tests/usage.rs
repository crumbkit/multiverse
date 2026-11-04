//! End-to-end tests of the `mxv` command's usage log (CLI-7). Each test
//! runs the built binary with a state directory of its own and reads the
//! lines the runs appended to `mxv/usage.jsonl` there.

#![cfg(test)]

use std::fs;

use serde_json::Value;
use snapbox::cmd::Command;

#[test]
fn each_run_appends_one_line_that_names_its_command_and_exit_code() {
    let state = tempfile::tempdir().unwrap();
    for args in [&["commands", "list"][..], &["new"][..]] {
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(state.path())
            .env_clear()
            .env("XDG_STATE_HOME", state.path())
            .output()
            .unwrap();
    }
    let log = fs::read_to_string(state.path().join("mxv").join("usage.jsonl")).unwrap();
    let commands: Vec<(String, u64)> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .map(|line| {
            (
                line["command"].as_str().unwrap().to_owned(),
                line["exit"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        commands,
        [("commands list".to_owned(), 0), ("new".to_owned(), 1)]
    );
}
