//! End-to-end tests of the `mxv completion` commands and of the completion
//! request that a shell makes. Each test runs the built binary with `HOME`
//! and the XDG directories set to a temporary directory of its own, so no
//! test writes into the real home directory.

// clippy allows `unwrap` only in code under `cfg(test)`, and an integration
// test's helper functions are not under it without this attribute.
#![cfg(test)]

use std::fs;
use std::path::Path;

use serde_json::Value;
use snapbox::cmd::{Command, OutputAssert};
use tempfile::TempDir;

/// Runs the built `mxv` binary with `args`, a cleared environment, `HOME`
/// set to `home` and the XDG directories under it, and returns its output
/// for assertions. `extra` adds environment variables to the run.
fn mxv(home: &Path, args: &[&str], extra: &[(&str, &str)]) -> OutputAssert {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mxv"))
        .args(args)
        .current_dir(home)
        .env_clear()
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"));
    for (name, value) in extra {
        command = command.env(name, value);
    }
    command.assert()
}

/// Returns the standard output of a run as a JSON value.
fn json(output: &OutputAssert) -> Value {
    serde_json::from_slice(&output.get_output().stdout).unwrap()
}

/// Returns the standard output of a run as text.
fn text(output: &OutputAssert) -> String {
    String::from_utf8(output.get_output().stdout.clone()).unwrap()
}

/// Returns an empty temporary home.
fn home() -> TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn completion_install_writes_fish_completion_naming_mxv_under_the_config_directory() {
    let home = home();

    let output = mxv(
        home.path(),
        &["completion", "install", "--shell", "fish", "--json"],
        &[],
    )
    .success();

    assert_eq!(json(&output)["data"][0]["state"], "written");
    let file = home.path().join("config/fish/completions/mxv.fish");
    let script = fs::read_to_string(file).unwrap();
    assert!(script.contains("mxv"), "{script}");
}

#[test]
fn completion_check_exits_with_1_and_names_a_missing_file() {
    let home = home();

    let output = mxv(
        home.path(),
        &["completion", "check", "--shell", "fish"],
        &[],
    )
    .code(1);

    assert!(text(&output).contains("missing"), "{}", text(&output));
}

#[test]
fn completion_check_exits_with_0_after_install() {
    let home = home();
    mxv(
        home.path(),
        &["completion", "install", "--shell", "fish"],
        &[],
    )
    .success();

    let output = mxv(
        home.path(),
        &["completion", "check", "--shell", "fish"],
        &[],
    )
    .success();

    assert_eq!(output.get_output().stdout, [] as [u8; 0]);
}

#[test]
fn completion_print_writes_a_fish_registration_that_runs_mxv() {
    let home = home();

    let output = mxv(
        home.path(),
        &["completion", "print", "--shell", "fish"],
        &[],
    )
    .success();

    let script = text(&output);
    assert!(script.contains("COMPLETE=fish"), "{script}");
    assert!(script.contains("mxv"), "{script}");
    assert!(!home.path().join("config/fish").exists());
}

#[test]
fn completion_install_reads_the_shell_from_the_shell_variable() {
    let home = home();

    mxv(
        home.path(),
        &["completion", "install"],
        &[("SHELL", "/usr/bin/fish")],
    )
    .success();

    assert!(
        home.path()
            .join("config/fish/completions/mxv.fish")
            .exists()
    );
}

#[test]
fn the_complete_variable_makes_mxv_print_a_fish_registration() {
    let home = home();

    let output = mxv(home.path(), &[], &[("COMPLETE", "fish")]).success();

    let script = text(&output);
    assert!(script.contains("complete"), "{script}");
    assert!(script.contains("COMPLETE=fish"), "{script}");
}

#[test]
fn a_completion_request_writes_no_usage_line() {
    let home = home();

    mxv(home.path(), &[], &[("COMPLETE", "fish")]).success();

    assert!(!home.path().join("state").exists());
}
