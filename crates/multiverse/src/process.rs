//! The process table that mxv's `clean` and `reclaim` read, as phase reads
//! it, and the runner of the programs other than git that the library
//! starts: the check commands of a manifest's secrets, `gh` and `board`
//! (PROC-1).

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use phase::host::ProcessError;
use wait_timeout::ChildExt as _;

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
        other => Error::ProcessTable {
            message: other.to_string(),
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
const MOST_KEPT_BYTES: u64 = 64 * 1024;

/// Runs `words`, a program and its arguments that no shell reads, in `dir`
/// with an empty standard input, and waits at most `deadline`. Its standard
/// error is discarded, and its standard output is discarded unless
/// `keep_stdout` is set, in which case the first 64 KiB are returned.
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
    command
        .args(arguments)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(if keep_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/process.rs starts the programs other than git (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/process.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let Ok(mut child): Result<std::process::Child, _> = spawned else {
        return Ran::Unstartable;
    };
    let reader = child.stdout.take().map(|stream| {
        thread::spawn(move || {
            let mut kept = Vec::new();
            let _ = stream.take(MOST_KEPT_BYTES).read_to_end(&mut kept);
            kept
        })
    });
    match child.wait_timeout(deadline) {
        Ok(Some(status)) => Ran::Exited {
            success: status.success(),
            stdout: reader
                .and_then(|reader| reader.join().ok())
                .unwrap_or_default(),
        },
        Ok(None) | Err(_) => {
            // The child is killed and reaped, so no zombie remains. A kill of
            // a child that already exited fails harmlessly. This stops only
            // the direct child: a check such as `sh -c` that forked a helper
            // leaves the helper running, and a process group kill would
            // stop it too.
            let _ = child.kill();
            let _ = child.wait();
            Ran::TimedOut
        }
    }
}
