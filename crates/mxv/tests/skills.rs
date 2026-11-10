//! End-to-end tests of the `mxv skills` commands. Each test runs the built
//! binary with `HOME` set to a temporary directory of its own, so no test
//! installs into the real home directory.

#![cfg(test)]

use std::fs;
use std::path::Path;

use serde_json::Value;
use snapbox::cmd::{Command, OutputAssert};
use tempfile::TempDir;

/// Returns a temporary home with an empty `.agents/skills` directory.
fn home() -> TempDir {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".agents").join("skills")).unwrap();
    home
}

/// Runs the built `mxv` binary with `args`, a cleared environment, `HOME`
/// set to `home` and the XDG directories under it, and returns its output
/// for assertions.
fn mxv(home: &Path, args: &[&str]) -> OutputAssert {
    Command::new(env!("CARGO_BIN_EXE_mxv"))
        .args(args)
        .current_dir(home)
        .env_clear()
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .assert()
}

/// Returns the standard output of a run as a JSON value.
fn json(output: &OutputAssert) -> Value {
    serde_json::from_slice(&output.get_output().stdout).unwrap()
}

#[test]
fn skills_install_writes_the_skill_with_metadata_naming_mxv() {
    let home = home();

    let output = mxv(home.path(), &["skills", "install", "--json"]).success();

    assert_eq!(json(&output)["data"][0]["state"], "written");
    let text = fs::read_to_string(home.path().join(".agents/skills/mxv/SKILL.md")).unwrap();
    assert!(text.starts_with("---\nname: mxv\n"), "{text}");
    assert!(
        text.contains("metadata:\n  tool: \"mxv\"\n  version: \""),
        "{text}"
    );
}

#[test]
fn skills_check_exits_with_1_and_names_a_missing_copy() {
    let home = home();

    let output = mxv(home.path(), &["skills", "check"]).code(1);

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("missing"), "{stdout}");
    assert!(stdout.contains(".agents/skills"), "{stdout}");
}

#[test]
fn skills_check_exits_with_0_and_prints_no_row_after_install() {
    let home = home();
    mxv(home.path(), &["skills", "install"]).success();

    let output = mxv(home.path(), &["skills", "check"]).success();

    assert_eq!(output.get_output().stdout, [] as [u8; 0]);
}

#[test]
fn skills_check_exits_with_1_when_the_copy_was_edited() {
    let home = home();
    mxv(home.path(), &["skills", "install"]).success();
    let file = home.path().join(".agents/skills/mxv/SKILL.md");
    let edited = fs::read_to_string(&file).unwrap() + "An edit.\n";
    #[expect(
        clippy::disallowed_methods,
        reason = "the test edits the installed copy, as a person might"
    )]
    fs::write(&file, edited).unwrap();

    let output = mxv(home.path(), &["skills", "check", "--json"]).code(1);

    assert_eq!(json(&output)["data"][0]["state"], "differs");
}

#[test]
fn skills_print_prints_the_embedded_skill() {
    let home = home();

    mxv(home.path(), &["skills", "print"])
        .success()
        .stdout_eq(include_str!("../skill/SKILL.md"));
}
