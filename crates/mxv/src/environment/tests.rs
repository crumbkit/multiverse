use std::ffi::OsString;
use std::path::PathBuf;

use super::{Environment, VARIABLES};
use crate::failures::MxvFailure;

fn environment(pairs: &[(&str, &str)]) -> Environment {
    Environment::read(|name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| OsString::from(value))
    })
}

#[test]
fn the_snapshot_reads_exactly_the_variables_in_the_catalog() {
    let mut read = Vec::new();
    let _ = Environment::read(|name| {
        read.push(name.to_owned());
        None
    });
    let catalog: Vec<&str> = VARIABLES.iter().map(|variable| variable.name).collect();
    assert_eq!(read, catalog);
}

#[test]
fn the_root_and_the_records_default_to_the_home_directory() {
    let environment = environment(&[("HOME", "/home/agent")]);
    assert_eq!(
        environment.root().unwrap(),
        PathBuf::from("/home/agent/mxv")
    );
    assert_eq!(
        environment.records().unwrap(),
        PathBuf::from("/home/agent/.local/state/mxv/workspaces")
    );
}

#[test]
fn mxv_root_and_xdg_state_home_override_the_home_directory() {
    let environment = environment(&[
        ("HOME", "/home/agent"),
        ("MXV_ROOT", "/work"),
        ("XDG_STATE_HOME", "/state"),
    ]);
    assert_eq!(environment.root().unwrap(), PathBuf::from("/work"));
    assert_eq!(
        environment.records().unwrap(),
        PathBuf::from("/state/mxv/workspaces")
    );
}

#[test]
fn a_relative_mxv_root_is_refused() {
    let environment = environment(&[("HOME", "/home/agent"), ("MXV_ROOT", "work")]);
    assert!(matches!(
        environment.root(),
        Err(MxvFailure::NoDirectory {
            variable: "MXV_ROOT"
        })
    ));
}

#[test]
fn without_a_home_directory_the_root_and_the_records_are_refused() {
    let environment = environment(&[]);
    assert!(environment.root().is_err());
    assert!(environment.records().is_err());
}
