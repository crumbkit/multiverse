//! The check that `starmap.toml` at the repository root declares the
//! environment variables that mxv reads, no more and no fewer.

use std::collections::BTreeSet;

/// The text of the repository's `starmap.toml`.
fn declaration() -> std::io::Result<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../starmap.toml");
    std::fs::read_to_string(path)
}

/// The variables that mxv reads outside [`mxv::VARIABLES`]. `PATH` comes
/// from the caller's environment that `main` snapshots, and mxv searches its
/// directories for the programs of a workspace's units.
const ALSO_READ: &[&str] = &["PATH"];

#[test]
fn starmap_toml_parses_with_the_starmap_crate() {
    let parsed = starmap::Spec::parse(&declaration().unwrap());
    assert!(parsed.is_ok(), "starmap.toml does not parse: {parsed:?}");
}

#[test]
fn starmap_toml_declares_exactly_the_variables_that_mxv_reads() {
    let spec = starmap::Spec::parse(&declaration().unwrap()).unwrap();
    let declared: BTreeSet<String> = spec
        .variables()
        .map(|(name, _)| name.as_str().to_owned())
        .collect();
    let read: BTreeSet<String> = mxv::VARIABLES
        .iter()
        .map(|variable| variable.name)
        .chain(ALSO_READ.iter().copied())
        .map(str::to_owned)
        .collect();
    assert_eq!(declared, read);
}

#[test]
fn starmap_toml_declares_no_secret_because_mxv_reads_none() {
    let spec = starmap::Spec::parse(&declaration().unwrap()).unwrap();
    let withheld: Vec<&str> = spec
        .variables()
        .filter(|(_, variable)| matches!(variable.show(), starmap::Show::Withheld))
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(withheld.is_empty(), "withheld variables: {withheld:?}");
}
