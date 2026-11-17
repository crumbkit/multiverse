//! The path of the `listen` example, a small TCP server that the
//! end-to-end tests run as an app's process. Cargo builds examples for a
//! test run that selects no target. A run with `--test` builds only that
//! test, so it needs `cargo build -p mxv --example listen` first.

use std::path::Path;

/// Returns the path of the `listen` example that cargo built beside the
/// `mxv` binary.
pub(crate) fn listen() -> String {
    let mxv = Path::new(env!("CARGO_BIN_EXE_mxv"));
    let mut path = mxv.parent().unwrap().join("examples").join("listen");
    if !path.exists() {
        path = mxv.parent().unwrap().join("examples").join("listen.exe");
    }
    assert!(
        path.exists(),
        "cargo did not build {}, because `--test` limits a run to that test's target; \
         run `cargo build -p mxv --example listen` first",
        path.display()
    );
    path.display().to_string()
}
