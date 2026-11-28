//! The check that `mxv.toml` at the repository root is a manifest that mxv
//! accepts, and that it declares the tasks and the need that the file's
//! comments describe, by its provider alone.

#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the files and directories that they and the code under test write"
)]

use std::path::Path;

use multiverse::Manifest;
use multiverse::needs::{Holder, Provider};

/// The text of the repository's `mxv.toml`.
fn declaration() -> std::io::Result<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mxv.toml");
    std::fs::read_to_string(path)
}

#[test]
fn mxv_toml_is_a_manifest_that_mxv_accepts() {
    let parsed = Manifest::parse(&declaration().unwrap(), Path::new("mxv.toml"));
    assert!(parsed.is_ok(), "mxv.toml is refused: {parsed:?}");
    let warnings = parsed.map(|manifest| manifest.warnings).unwrap_or_default();
    assert!(warnings.is_empty(), "mxv.toml has warnings: {warnings:?}");
}

#[test]
fn mxv_toml_declares_the_fmt_clippy_and_test_tasks_and_no_unit() {
    let manifest = Manifest::parse(&declaration().unwrap(), Path::new("mxv.toml")).unwrap();
    let tasks: Vec<String> = manifest
        .tasks
        .keys()
        .map(|name| name.as_str().to_owned())
        .collect();
    assert_eq!(tasks, ["clippy", "fmt", "test"]);
    assert!(manifest.processes.is_empty());
    assert!(manifest.containers.is_empty());
}

#[test]
fn mxv_toml_needs_the_deadband_socket_by_its_provider_alone() {
    let manifest = Manifest::parse(&declaration().unwrap(), Path::new("mxv.toml")).unwrap();
    let declared = &manifest.needs[&Holder::Workspace];
    let needs: Vec<(&str, &str, &str)> = declared
        .needs
        .iter()
        .map(|need| {
            let Provider::Repo(origin) = &need.provider else {
                panic!("the need names no repository: {need:?}");
            };
            (need.name.as_str(), origin.as_str(), need.from.as_str())
        })
        .collect();
    assert_eq!(
        needs,
        [(
            "deadband",
            "github.com/crumbkit/deadband",
            "deadband-socket"
        )]
    );
}
