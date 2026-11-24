use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::environment::Environment;

use super::{MISE_JOBS, MOST_COMMAND_BYTES, cut, job_variables, socket_path};

fn variables(pairs: &[(&str, &str)]) -> BTreeMap<String, OsString> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
        .collect()
}

#[test]
fn a_command_longer_than_the_limit_is_cut_on_a_character_boundary() {
    let command = "é".repeat(MOST_COMMAND_BYTES);
    let kept = cut(&command);
    assert_eq!(kept.len(), MOST_COMMAND_BYTES);
    assert!(kept.chars().all(|character| character == 'é'));
}

#[test]
fn a_command_within_the_limit_is_kept_whole() {
    assert_eq!(cut("cargo build"), "cargo build");
}

#[test]
fn the_job_variables_are_deadbands_and_mise_runs_one_task_at_a_time() {
    let variables = job_variables(&[("CARGO_BUILD_JOBS", "3".to_owned())]);
    let shown: Vec<(&str, &str)> = variables
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert_eq!(shown, [("CARGO_BUILD_JOBS", "3"), (MISE_JOBS, "1")]);
}

#[test]
fn the_socket_is_in_the_user_directory_and_not_in_tmpdir() {
    let variables = variables(&[("TMPDIR", "/shell/tmp")]);
    let path = socket_path(&variables, || Some(PathBuf::from("/user/tmp")));
    assert_eq!(path, Path::new("/user/tmp/deadband.sock"));
}

#[test]
fn the_socket_is_in_the_user_directory_when_tmpdir_is_unset() {
    let path = socket_path(&variables(&[]), || Some(PathBuf::from("/user/tmp")));
    assert_eq!(path, Path::new("/user/tmp/deadband.sock"));
}

#[test]
fn the_socket_falls_back_to_tmpdir_when_the_system_has_no_user_directory() {
    let variables = variables(&[("TMPDIR", "/shell/tmp")]);
    let path = socket_path(&variables, || None);
    assert_eq!(path, Path::new("/shell/tmp/deadband.sock"));
}

#[test]
fn deadband_socket_wins_over_the_user_directory() {
    let variables = variables(&[("DEADBAND_SOCKET", "/set/d.sock"), ("TMPDIR", "/shell/tmp")]);
    let path = socket_path(&variables, || Some(PathBuf::from("/user/tmp")));
    assert_eq!(path, Path::new("/set/d.sock"));
}

#[test]
fn deadband_socket_dir_wins_over_the_user_directory() {
    let variables = variables(&[("DEADBAND_SOCKET_DIR", "/set/dir")]);
    let path = socket_path(&variables, || Some(PathBuf::from("/user/tmp")));
    assert_eq!(path, Path::new("/set/dir/deadband.sock"));
}

fn read_environment(pairs: &[(&str, &str)]) -> Environment {
    let set = variables(pairs);
    Environment::read(|name| set.get(name).cloned(), None)
}

#[test]
fn deadband_socket_dir_read_from_the_environment_wins_over_the_user_directory() {
    let environment = read_environment(&[("DEADBAND_SOCKET_DIR", "/set/dir")]);
    let path = socket_path(environment.deadband_variables(), || {
        Some(PathBuf::from("/user/tmp"))
    });
    assert_eq!(path, Path::new("/set/dir/deadband.sock"));
}

#[test]
fn the_user_directory_is_used_when_the_environment_has_no_deadband_socket_dir() {
    let environment = read_environment(&[]);
    let path = socket_path(environment.deadband_variables(), || {
        Some(PathBuf::from("/user/tmp"))
    });
    assert_eq!(path, Path::new("/user/tmp/deadband.sock"));
}

#[test]
fn the_socket_is_under_xdg_runtime_dir_read_from_the_environment_when_nothing_else_is_set() {
    let environment = read_environment(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
    let path = socket_path(environment.deadband_variables(), || None);
    assert_eq!(path, Path::new("/run/user/1000/deadband/deadband.sock"));
}

#[test]
fn tmpdir_read_from_the_environment_wins_over_xdg_runtime_dir() {
    let environment = read_environment(&[
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ("TMPDIR", "/shell/tmp"),
    ]);
    let path = socket_path(environment.deadband_variables(), || None);
    assert_eq!(path, Path::new("/shell/tmp/deadband.sock"));
}
