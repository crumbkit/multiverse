//! The process table that mxv's `clean` and `reclaim` read, as phase reads
//! it, and the runner of the programs other than git that the library
//! starts: the check commands of a manifest's secrets, `gh` and `board`
//! (PROC-1).

use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use crumbkit_process::{
    Capture, ChildFinished, Outcome, OverLimit, RunError, RunOptions, Scope, Status,
};
use phase::host::ProcessError;

use crate::clock;
use crate::error::Error;

/// Returns every process that has not ended, with its parent, its group,
/// its working directory and its resident memory, as phase reads them.
///
/// # Errors
///
/// Returns [`Error::ProcessTable`] when the process table cannot be read,
/// and [`Error::UnsupportedOs`] when this system's structs differ from
/// those phase reads.
pub fn process_table() -> Result<Vec<phase::host::TableEntry>, Error> {
    phase::host::process_table().map_err(process_error)
}

/// Returns mxv's error for `error`, a failure of phase's process table.
pub(crate) fn process_error(error: ProcessError) -> Error {
    match error {
        ProcessError::Unsupported {
            call,
            pid,
            written,
            expected,
            ..
        } => Error::UnsupportedOs {
            message: format!(
                "{call} for the process {pid} wrote {written} bytes, and mxv reads a struct of {expected}"
            ),
        },
        ProcessError::Table { .. } => Error::ProcessTable {
            message: error.to_string(),
        },
        _ => Error::ProcessTable {
            message: error.to_string(),
        },
    }
}

/// What a command of [`run_command`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ran {
    /// The command ended before its deadline.
    Exited {
        /// Whether it exited with 0.
        success: bool,
        /// What it wrote to standard output, which is empty unless the
        /// caller asked to keep it.
        stdout: Vec<u8>,
    },
    /// The program could not be started, such as one that is not installed.
    Unstartable,
    /// The command was still running at its deadline, and mxv killed it.
    TimedOut,
}

/// The most bytes of a command's standard output that [`run_command`] keeps.
const MOST_KEPT_BYTES: usize = 64 * 1024;

/// Runs `words`, a program and its arguments that no shell reads, in `dir`
/// with an empty standard input, and waits at most `deadline`. Its standard
/// error is discarded, and its standard output is discarded unless
/// `keep_stdout` is set, in which case the first 64 KiB are returned and
/// the rest is read into nothing.
///
/// The command stays in mxv's process group, so a Ctrl-C at the terminal
/// reaches it. When it misses its deadline, crumbkit-process kills it alone
/// and reaps it: a check such as `sh -c` that forked a helper leaves the
/// helper running, and a process group kill would stop it too. A helper
/// that holds standard output open holds mxv for at most
/// [`crumbkit_process::DRAIN_GRACE`] after the command exits.
///
/// This is the module that starts the programs other than git: the check
/// commands of a manifest's secrets, `gh` and `board` (PROC-1).
pub(crate) fn run_command(
    words: &[String],
    dir: &Path,
    deadline: Duration,
    keep_stdout: bool,
) -> Ran {
    let Some((program, arguments)) = words.split_first() else {
        return Ran::Unstartable;
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/process.rs starts the programs other than git (PROC-1)"
    )]
    let mut command = Command::new(program);
    command.args(arguments).current_dir(dir);
    let stdout = if keep_stdout {
        Capture::from(Capture::whole(MOST_KEPT_BYTES).over(OverLimit::Sink))
    } else {
        Capture::discard()
    };
    let options = RunOptions::new()
        .stdout(stdout)
        .scope(Scope::Child)
        .deadline(deadline_in(deadline));
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/process.rs starts the programs other than git (PROC-1)"
    )]
    let ran = crumbkit_process::run(command, &options);
    match ran {
        Ok(finished) => match ran_to_its_end(&finished) {
            Some(status) => Ran::Exited {
                success: status.success(),
                stdout: finished.into_output().0.into_bytes(),
            },
            None => Ran::TimedOut,
        },
        Err(RunError::Start(_)) => Ran::Unstartable,
        // crumbkit-process could not wait for the command or read it, and
        // has killed and reaped it, as a failed wait did before.
        Err(_) => Ran::TimedOut,
    }
}

/// Returns the exit status of `finished` when its child ran to its own end,
/// or `None` when the run stopped it at its deadline or on a cancel.
pub(crate) fn ran_to_its_end(finished: &ChildFinished) -> Option<ExitStatus> {
    match finished.outcome() {
        Outcome::Ended(status) => Some(exit_status(status)),
        Outcome::TimedOut | Outcome::Stopped(_) => None,
        _ => None,
    }
}

/// Returns the exit status that `status`, as crumbkit-process reaped it,
/// stands for, so the library's readers of a status keep the standard
/// library's type.
pub(crate) fn exit_status(status: Status) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    match status {
        Status::Signal(signal) => ExitStatus::from_raw(signal & 0x7f),
        Status::Code(code) => ExitStatus::from_raw((code & 0xff) << 8),
        // A status that a later crumbkit-process adds reads as a failure.
        _ => ExitStatus::from_raw(1 << 8),
    }
}

/// Returns the deadline `wait` from now, or a century from now when `wait`
/// is too long for an [`Instant`] to hold.
pub(crate) fn deadline_in(wait: Duration) -> Instant {
    let start = clock::now();
    start
        .checked_add(wait)
        .or_else(|| start.checked_add(Duration::from_secs(100 * 365 * 24 * 60 * 60)))
        .unwrap_or(start)
}

#[cfg(test)]
mod tests;
