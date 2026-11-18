//! The check that `mxv.toml` at the repository root is a manifest that mxv
//! accepts, and that it declares the tasks and the want that the file's
//! comments describe.

use std::path::Path;

use multiverse::Manifest;

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
fn mxv_toml_wants_the_deadband_daemon_as_an_optional_process() {
    let table: toml::Table = toml::from_str(&declaration().unwrap()).unwrap();
    let wants = table["wants"].as_table().unwrap();
    assert_eq!(wants.keys().collect::<Vec<_>>(), ["deadband"]);
    let want = wants["deadband"].as_table().unwrap();
    assert_eq!(want["repo"].as_str(), Some("github.com/crumbkit/deadband"));
    assert_eq!(want["provides"].as_str(), Some("deadband"));
    assert_eq!(want["as"].as_str(), Some("DEADBAND_SOCKET"));
    assert_eq!(want["optional"].as_bool(), Some(true));
}
