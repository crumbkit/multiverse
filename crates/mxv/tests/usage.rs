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
    for args in [&["commands", "list"][..], &["archive"][..]] {
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
        [("commands list".to_owned(), 0), ("archive".to_owned(), 1)]
    );
}

#[test]
fn self_runs_list_prints_each_run_in_the_usage_log_grouped_by_session() {
    let state = tempfile::tempdir().unwrap();
    let mxv = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(state.path())
            .env_clear()
            .env("XDG_STATE_HOME", state.path())
            .output()
            .unwrap()
    };
    mxv(&["commands", "list"]);
    mxv(&["archive"]);

    let output = mxv(&["self", "runs", "list", "--json"]);

    assert!(output.status.success(), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["schema"], "mxv/self-runs-list/v1");
    let runs: Vec<(u64, String, u64)> = document["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| {
            (
                run["session"].as_u64().unwrap(),
                run["command"].as_str().unwrap().to_owned(),
                run["exit"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        runs,
        [
            (1, "commands list".to_owned(), 0),
            (1, "archive".to_owned(), 1)
        ]
    );
}
