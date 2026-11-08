//! The `mxv` binary, which runs the command that the mxv library holds.

use std::process::ExitCode;

/// Starts the usage log, reads the environment once and runs the command.
/// Returning the exit code lets destructors run before the process ends
/// (BASE-7).
fn main() -> ExitCode {
    // BASE-6: main reads the whole environment once, before anything else
    // does. phase's keeper receives it, the process verbs pass PATH, HOME
    // and the variables of `multiverse::PASSED_THROUGH` to the host
    // processes that phase starts, and phase's docker commands receive
    // PATH, HOME and each DOCKER_* variable. The decision record
    // crates/mxv/docs/decisions/host-process-environment.md says why.
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/main.rs reads the environment once, into a snapshot"
    )]
    let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // phase starts its keeper as `mxv phase keep`, which writes no usage
    // line and reads nothing else.
    if mxv::is_keeper(&args) {
        return mxv::keep(args, vars);
    }
    // phase starts the app `mxv-provision` as `mxv phase provision`, which
    // writes no usage line and reads only its argument.
    if mxv::is_provisioner(&args) {
        return mxv::provision(&args);
    }
    // The guard writes this run's line to the usage log when `main`
    // returns, after the output is flushed (CLI-7).
    let _log = crumbkit_usage::start();
    // BASE-6: main is the one place that reads the environment, and it
    // reads only the variables of `mxv::VARIABLES`: MXV_ROOT here, and HOME
    // and the XDG base directories through etcetera.
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/main.rs reads the environment once, into a snapshot"
    )]
    let environment = mxv::Environment::read(|name| std::env::var_os(name), mxv::Bases::find());
    // The current directory names the workspace that a process verb works
    // in when the command line names none.
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/main.rs reads the current directory once, into the snapshot"
    )]
    let current_dir = std::env::current_dir().ok();
    // phase starts each keeper with this program, so a keeper runs the
    // same mxv as the command that started it.
    let program = std::env::current_exe().ok();
    let environment = environment
        .with_current_dir(current_dir)
        .with_caller(multiverse::CallerEnvironment::from_vars(vars), program);
    mxv::run(args, &environment)
}
