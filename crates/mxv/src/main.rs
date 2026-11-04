//! The `mxv` binary, which runs the command that the mxv library holds.

use std::process::ExitCode;

/// Starts the usage log, reads the environment once and runs the command.
/// Returning the exit code lets destructors run before the process ends
/// (BASE-7).
fn main() -> ExitCode {
    // The guard writes this run's line to the usage log when `main`
    // returns, after the output is flushed (CLI-7).
    let _log = crumbkit_usage::start();
    // BASE-6: main is the one place that reads the environment, and it
    // reads only the variables of `mxv::VARIABLES`.
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/main.rs reads the environment once, into a snapshot"
    )]
    let environment = mxv::Environment::read(|name| std::env::var_os(name));
    mxv::run(std::env::args_os(), &environment)
}
