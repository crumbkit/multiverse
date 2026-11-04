//! The `command` check. A [`ProbeExec`] runs the command where the process
//! runs: [`HostExec`] on the host, and the container module's `docker exec`
//! inside a container. [`run_bounded`] runs a prepared command with a
//! timeout, for either of them.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::process::Signal;

use super::{Attempt, Failure};
use crate::clock::{POLL_INTERVAL, now, pause};
use crate::env::AppliedEnvironment;

/// Runs a probe's command where its process runs, with the process's
/// environment.
pub(crate) trait ProbeExec {
    /// Runs `run`, the program and its arguments as separate values that no
    /// shell reads, and returns how it ended within `timeout`. It ends the
    /// command when the timeout passes. It never writes an environment value
    /// to the command's arguments.
    ///
    /// # Errors
    ///
    /// Returns the error that kept the command from starting.
    fn exec(&self, run: &[String], timeout: Duration) -> io::Result<CommandEnd>;
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
    fn exec(&self, run: &[String], timeout: Duration) -> io::Result<CommandEnd> {
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
        run_bounded(command, timeout)
    }
}

/// Runs `command` in a process group of its own, with no input and its
/// output discarded, and waits at most `timeout` for it to exit. When the
/// timeout passes, it kills the command's group and reaps the command.
/// The command gives no event the caller can wait on with a deadline, so
/// the wait polls it every [`POLL_INTERVAL`].
///
/// # Errors
///
/// Returns the error that kept the command from starting.
pub(crate) fn run_bounded(mut command: Command, timeout: Duration) -> io::Result<CommandEnd> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let deadline = now() + timeout;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/probe/command.rs starts probe commands and owns their deadline (PROC-1)"
    )]
    let mut child = command.spawn()?;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(CommandEnd::Exited {
                code: status.code(),
            });
        }
        if now() >= deadline {
            let group = i32::try_from(child.id())
                .ok()
                .and_then(rustix::process::Pid::from_raw);
            if let Some(group) = group {
                let _ = rustix::process::kill_process_group(group, Signal::KILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(CommandEnd::TimedOut);
        }
        pause(POLL_INTERVAL.min(deadline.saturating_duration_since(now())));
    }
}

/// Runs `run` through `exec` within `timeout`, and passes when it exits
/// with status 0.
pub(super) fn attempt(exec: &dyn ProbeExec, run: &[String], timeout: Duration) -> Attempt {
    match exec.exec(run, timeout) {
        Ok(CommandEnd::Exited { code: Some(0) }) => Attempt::Passed,
        Ok(CommandEnd::Exited { code }) => Attempt::Failed(Failure::Exited { code }),
        Ok(CommandEnd::TimedOut) => Attempt::Failed(Failure::TimedOut),
        Err(error) => Attempt::Failed(Failure::Unstarted(error.kind())),
    }
}
