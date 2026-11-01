//! The `mxv` binary, which runs the command that the mxv library holds.

use std::process::ExitCode;

/// Starts the usage log and runs the command. Returning the exit code lets
/// destructors run before the process ends (BASE-7).
fn main() -> ExitCode {
    // The guard writes this run's line to the usage log when `main`
    // returns, after the output is flushed (CLI-7).
    let _log = crumbkit_usage::start();
    mxv::run(std::env::args_os())
}
