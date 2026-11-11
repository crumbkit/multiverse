//! Grants the permissions of every workspace's units, as a person at a
//! terminal would by answering yes to `mxv up`.
//!
//! The end-to-end tests run mxv without a terminal, where `mxv up` refuses
//! a unit whose permissions no person granted. A test that starts host
//! units calls [`grant_all`] first, which writes each repository's grant
//! through the library, so no flag or variable of mxv approves a grant.

use std::path::Path;

use multiverse::{Granting, Runs, Store};

/// Grants what every workspace asks for, as [`grant_all`] does, when `args`
/// are those of a verb that starts or runs units: `up`, `restart` or `exec`,
/// after any flags such as `--json`.
pub(crate) fn before(state: &Path, args: &[&str]) {
    let verb = args.iter().find(|arg| !arg.starts_with('-'));
    if matches!(verb, Some(&("up" | "restart" | "exec"))) {
        grant_all(state);
    }
}

/// Grants what each workspace under mxv's state directory `state`, such as
/// `<root>/state/mxv`, asks for: every unit of its manifest, and the units
/// they depend on. A workspace whose manifest cannot be read or run asks
/// for nothing here, and `mxv up` reports its failure.
pub(crate) fn grant_all(state: &Path) {
    let store = Store::new(state.join("workspaces"));
    let runs = Runs::new(state);
    let Ok(records) = store.list() else {
        return;
    };
    for workspace in records.workspaces {
        let Ok(Some(manifest)) = workspace.manifest() else {
            continue;
        };
        if let Ok(Granting::Asks(ask)) = runs.granting(&store, &workspace, &manifest, &[]) {
            runs.grant(&ask).unwrap();
        }
    }
}
