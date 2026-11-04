//! The keeper, which the hidden `phase keep` subcommand runs. A keeper is
//! the parent of exactly one app process. It holds only that process's
//! environment, gives the process a log file descriptor, restarts it under
//! its policy, records each start and stop in the run record, and exits
//! when it is stopped. The sandbox is a later release's work, and the
//! `sandbox` module's `apply_sandbox` is the hook where the keeper will
//! apply it to the app's command.
//!
//! The program that links phase, such as mxv, calls [`run`] from its
//! hidden `phase keep` subcommand, with the arguments that follow the
//! subcommand parsed by [`KeeperArgs::parse`], its standard input and its
//! standard output. The engine starts a keeper with the `spawn` module's
//! `spawn`, which is the only code that builds the keeper's command line.
//!
//! # Starting the keeper (PROC-2)
//!
//! - Program: the program that links phase, at the path that its host
//!   gives the engine, followed by the words of its hidden subcommand, such
//!   as `phase keep`. The host supplies the program's bytes.
//! - Arguments: `--workspace <id> --app <app> --process <process>
//!   --state-dir <dir>`, each a separate value. They hold no environment
//!   value, and the workspace's stable id lets a later `sweep` find a
//!   keeper that the run record does not name.
//! - Environment: the keeper's environment is cleared, and then receives
//!   only `PATH` and `HOME` from the caller and the process's resolved
//!   environment through [`AppliedEnvironment::apply`]. Standard input
//!   carries the keeper's specification, a `phase/keeper/v1` JSON document
//!   of at most [`MOST_SPEC_BYTES`] bytes that names the command, its
//!   directory, its log, its restart policy and its stop handling, and
//!   holds no environment value.
//! - Streams and deadline: the keeper writes one handshake line to standard
//!   output once its app has started or failed to start, and standard error
//!   is discarded. The engine reads at most [`MOST_HANDSHAKE_BYTES`] bytes
//!   before the deadline it is given, and kills the keeper's group when the
//!   deadline passes.
//! - End: the keeper runs in a process group of its own, so it outlives the
//!   engine that started it and a Ctrl-C at the engine's terminal does not
//!   reach it. It ends when it receives `TERM` or `INT`, or when its app
//!   ends and the policy does not restart it. The engine reaps it when it
//!   ends before the handshake. Otherwise the engine leaves it running, and
//!   the operating system reaps it once the engine exits.
//!
//! # Starting the app process (PROC-2)
//!
//! - Program: the first word of the specification's command, found on the
//!   `PATH` of the keeper's environment. The plan's author supplies it,
//!   and the engine has replaced `{program}` with the app's program.
//! - Arguments: the rest of the command, each a separate value.
//! - Environment: the keeper's own environment, unchanged.
//! - Streams and deadline: standard input is empty. Standard output and
//!   standard error are the app's log file, opened with `O_APPEND`, which
//!   the app holds as its own descriptors. The keeper never reads the
//!   app's output, so a slow log reader never blocks supervision, and the
//!   app keeps logging when the keeper dies. The app has no deadline.
//! - End: the app runs in a process group of its own, so killing the
//!   keeper leaves it running. The keeper reaps it. To stop it, the keeper
//!   sends the plan's stop signal to its group, waits the plan's grace,
//!   then sends `KILL` to the group and waits the kill wait. When the app
//!   ends by itself, the keeper stops what is left of its group the same
//!   way before it restarts the app or exits. A descendant that starts a
//!   group of its own is not reached.
//!
//! # Restarts
//!
//! The keeper restarts its app under the plan's [`Restart`] policy, waiting
//! the backoff before each restart. When the policy's [`Resolve`] says the
//! environment must be resolved again, the keeper cannot do that itself,
//! because it holds only the values it was given. After the backoff it
//! exits with [`KeeperExit::NeedsEnvironment`], and the engine starts a
//! new keeper with a fresh environment.
//!
//! # The run record
//!
//! The keeper records the app's pid, start time, group and boot id as
//! `running` each time it starts the app, `stopping` before it stops the
//! app's group, and `stopped` once the group is gone. A thread of its own
//! writes these through the run record's journal, which takes the
//! workspace's lifecycle lock, so an `up` or `down` that holds the lock
//! never blocks supervision. The engine also records the first start from
//! the handshake, while it holds the lock. When the keeper exits, it waits
//! at most [`EXIT_RECORD_WAIT`] for its last writes, because the operation
//! that stops it holds the lock and records the stop itself.
//!
//! [`AppliedEnvironment::apply`]: crate::AppliedEnvironment::apply
//! [`Restart`]: crate::Restart
//! [`Resolve`]: crate::Resolve

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

mod args;
mod policy;
mod recorder;
mod sandbox;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the host runner starts keepers through `spawn` when `up` lands"
    )
)]
mod spawn;
mod spec;
mod supervise;

#[cfg(test)]
mod tests;

pub use args::{BadKeeperArgs, KeeperArgs};
#[cfg_attr(
    not(test),
    expect(
        unused_imports,
        reason = "the host runner starts keepers through `spawn` when `up` lands"
    )
)]
pub(crate) use spawn::{KeeperCommand, Kept, Launch, spawn};
#[cfg_attr(
    not(test),
    expect(
        unused_imports,
        reason = "the host runner builds a keeper's specification when `up` lands"
    )
)]
pub(crate) use spec::KeeperSpec;

/// The most bytes of the specification that a keeper reads from its
/// standard input. A longer specification is refused.
pub const MOST_SPEC_BYTES: u64 = 1024 * 1024;

/// The most bytes of a keeper's standard output that the engine reads
/// while it looks for the handshake line. The engine stops reading there
/// and fails the start.
pub const MOST_HANDSHAKE_BYTES: u64 = 64 * 1024;

/// How long a keeper that is exiting waits for its last writes to the run
/// record. An operation that holds the lifecycle lock for longer, such as
/// the `down` that stopped the keeper, records the stop itself.
pub const EXIT_RECORD_WAIT: Duration = Duration::from_secs(2);

/// How a keeper ended, which its exit status reports.
///
/// | Exit | Status |
/// | --- | --- |
/// | [`Stopped`](KeeperExit::Stopped) | 0 |
/// | [`Completed`](KeeperExit::Completed) | 0 |
/// | [`Failed`](KeeperExit::Failed) | 1 |
/// | [`Unstoppable`](KeeperExit::Unstoppable) | 2 |
/// | [`BadInput`](KeeperExit::BadInput) | 64 |
/// | [`Unstartable`](KeeperExit::Unstartable) | 69 |
/// | [`NeedsEnvironment`](KeeperExit::NeedsEnvironment) | 75 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeeperExit {
    /// The keeper received `TERM` or `INT` and stopped its app.
    Stopped,
    /// The app exited with status 0 and the policy did not restart it.
    Completed,
    /// The app failed, by a status other than 0 or a signal, and the
    /// policy did not restart it, or it had reached its most restarts.
    Failed,
    /// The app's process group was still there after `KILL` and the kill
    /// wait. The run record keeps the app as `stopping`.
    Unstoppable,
    /// The keeper's arguments or specification could not be read.
    BadInput,
    /// The keeper could not start the app, or could not open its log.
    Unstartable,
    /// The app ended, the policy restarts it, and the restart needs its
    /// environment resolved again, which only the engine can do.
    NeedsEnvironment,
}

impl KeeperExit {
    /// Returns the keeper's exit status, as the table above gives it.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Stopped | Self::Completed => 0,
            Self::Failed => 1,
            Self::Unstoppable => 2,
            Self::BadInput => 64,
            Self::Unstartable => 69,
            Self::NeedsEnvironment => 75,
        }
    }
}

impl From<KeeperExit> for ExitCode {
    fn from(exit: KeeperExit) -> Self {
        Self::from(exit.code())
    }
}

/// Runs the keeper that `args` names: reads its specification from
/// `input`, starts the app, writes the handshake line to `output`, and
/// supervises the app until it is stopped or its policy leaves it ended.
/// The program that links phase passes its standard input and standard
/// output, and returns the result from `main` as its exit status.
#[must_use]
pub fn run(args: &KeeperArgs, input: impl Read, output: impl Write) -> KeeperExit {
    supervise::run(args, input, output)
}
