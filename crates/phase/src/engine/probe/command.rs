//! The `command` check. A [`ProbeExec`] runs the command where the process
//! runs: [`HostExec`] on the host, and the container module's `docker exec`
//! inside a container. [`run_bounded`] runs a prepared command with a
//! timeout and a [`Cancel`], for either of them.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crumbkit_process::{Outcome, RunError, RunOptions, Scope, Status};

use super::{Attempt, Failure};
use crate::cancel::Cancel;
use crate::clock::deadline_in;
use crate::env::AppliedEnvironment;

/// Runs a probe's command where its process runs, with the process's
/// environment.
pub(crate) trait ProbeExec {
    /// Runs `run`, the program and its arguments as separate values that no
    /// shell reads, and returns how it ended within `timeout`. It ends the
    /// command when the timeout passes or `cancel` is set. It never writes
    /// an environment value to the command's arguments.
    ///
    /// # Errors
    ///
    /// Returns the error that kept the command from starting.
    fn exec(&self, run: &[String], timeout: Duration, cancel: &Cancel) -> io::Result<CommandEnd>;
}

/// How a probe's command ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandEnd {
    /// The command exited with `code`, or a signal ended it and `code` is
    /// `None`.
    Exited {
        /// The exit status.
        code: Option<i32>,
    },
    /// The command was still running when the timeout passed, and was
    /// killed.
    TimedOut,
    /// The caller cancelled the command, which was killed.
    Cancelled,
    /// Waiting for the command failed with `kind`, so its end is unknown.
    /// The command's group was killed.
    WaitFailed {
        /// The kind of the wait's error.
        kind: io::ErrorKind,
    },
}

/// Runs a host process's probe commands on the host, in the process's
/// directory, with only `PATH`, `HOME` and the process's resolved
/// environment, as the keeper starts the process itself.
pub(crate) struct HostExec<'a> {
    /// The process's resolved environment.
    pub(crate) environment: &'a dyn AppliedEnvironment,
    /// The value of `PATH`, on which the program is found.
    pub(crate) path: OsString,
    /// The value of `HOME`.
    pub(crate) home: OsString,
    /// The directory the command starts in.
    pub(crate) dir: PathBuf,
}

impl std::fmt::Debug for HostExec<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostExec")
            .field("environment", &self.environment.names())
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl ProbeExec for HostExec<'_> {
    fn exec(&self, run: &[String], timeout: Duration, cancel: &Cancel) -> io::Result<CommandEnd> {
        let Some((program, args)) = run.split_first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the probe's command is empty",
            ));
        };
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/phase/src/engine/probe/command.rs starts a host process's probe command (PROC-1)"
        )]
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&self.dir)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", &self.home);
        self.environment.apply(&mut command);
        run_bounded(command, timeout, cancel)
    }
}

/// Runs `command` in a process group of its own, with no input and its
/// output discarded, and waits at most `timeout` for it to exit, or until
/// `cancel` is set.
///
/// crumbkit-process runs it with `Scope::Group`: however the command ends,
/// the run kills what is left of its group before it reaps the command, so
/// the group's id cannot name another process in between. A command that
/// leaves a process running in the background therefore never outlives its
/// probe, unless that process started a group of its own. The run wakes on
/// the command's exit, the timeout or the cancel, without polling. A cancel
/// that was set before the run starts no command.
///
/// # Errors
///
/// Returns the error that kept the command from starting.
pub(crate) fn run_bounded(
    command: Command,
    timeout: Duration,
    cancel: &Cancel,
) -> io::Result<CommandEnd> {
    let options = RunOptions::new()
        .scope(Scope::Group)
        .deadline(deadline_in(timeout))
        .cancel(cancel.runs());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/probe/command.rs starts probe commands and owns their deadline (PROC-1)"
    )]
    let ran = crumbkit_process::run(command, &options);
    Ok(match ran {
        Ok(finished) => match finished.outcome() {
            Outcome::Ended(Status::Code(code)) => CommandEnd::Exited { code: Some(code) },
            Outcome::Ended(_) => CommandEnd::Exited { code: None },
            Outcome::TimedOut => CommandEnd::TimedOut,
            Outcome::Stopped(_) => CommandEnd::Cancelled,
            _ => CommandEnd::WaitFailed {
                kind: io::ErrorKind::Other,
            },
        },
        Err(RunError::Start(error)) => return Err(error),
        Err(RunError::Run(error)) => CommandEnd::WaitFailed { kind: error.kind() },
        Err(_) => CommandEnd::WaitFailed {
            kind: io::ErrorKind::Other,
        },
    })
}

/// Runs `run` through `exec` within `timeout`, and passes when it exits
/// with status 0.
pub(super) fn attempt(
    exec: &dyn ProbeExec,
    run: &[String],
    timeout: Duration,
    cancel: &Cancel,
) -> Attempt {
    match exec.exec(run, timeout, cancel) {
        Ok(CommandEnd::Exited { code: Some(0) }) => Attempt::Passed,
        Ok(CommandEnd::Exited { code }) => Attempt::Failed(Failure::Exited { code }),
        Ok(CommandEnd::TimedOut) => Attempt::Failed(Failure::TimedOut),
        Ok(CommandEnd::Cancelled) => Attempt::Failed(Failure::Cancelled),
        Ok(CommandEnd::WaitFailed { kind }) => Attempt::Failed(Failure::WaitFailed(kind)),
        Err(error) => Attempt::Failed(Failure::Unstarted(error.kind())),
    }
}
