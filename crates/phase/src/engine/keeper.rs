//! The keeper, which the hidden `phase keep` subcommand runs. A keeper is
//! the parent of exactly one app process. It holds only that process's
//! environment, gives the process a log file descriptor, restarts it under
//! its policy, records each start and stop in the run record, and exits
//! when it is stopped. When its specification names a sandbox, it starts
//! the app through the launcher, which applies the sandbox and becomes
//! the app, so each restart runs under the sandbox too.
//!
//! The program that links phase, such as mxv, calls [`run`] from its
//! hidden `phase keep` subcommand, with the arguments that follow the
//! subcommand parsed by [`KeeperArgs::parse`], a [`KeeperEnvironment`]
//! that `main` built from its own environment, its standard input and its
//! standard output. `main` stays the only reader of the environment. The engine starts a keeper with the `spawn` module's
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
//!   directory, its log, its restart policy, its stop handling, the
//!   values of `PATH` and `HOME`, and holds no other environment value.
//! - Streams and deadline: the keeper writes one handshake line to standard
//!   output once its app has started or failed to start. Once that first
//!   start of the app ends, the keeper writes one end line, which says how
//!   the app ended, and writes nothing more. `up` reads the end line when
//!   it sees the app gone during its readiness wait, and closes its end of
//!   the pipe otherwise, so the keeper's write then fails and is ignored.
//!   Its standard error
//!   is appended to its diagnostic log, `<log dir>/<app>.<process>.keeper.log`,
//!   which the `logfile` module describes. The engine reads at most
//!   [`MOST_HANDSHAKE_BYTES`] bytes before the deadline it is given. The
//!   handshake names the app's process and the keeper's group. When the
//!   deadline passes or the handshake cannot be read, the keeper may
//!   already have started the app, so the engine sends the keeper `TERM`,
//!   which makes it stop the app, waits the app's grace and kill wait, and
//!   then kills the keeper and its group. A window remains. When the
//!   keeper's start of the app takes longer than that wait, the engine
//!   kills the keeper before it can stop the app. The app then keeps
//!   running in its own group with no keeper, and since the engine never
//!   received its pid, the run record does not name it and phase does not
//!   stop it.
//! - End: the keeper's first action is `setsid`, so it leads a session and
//!   a group of its own. A keeper that cannot start a session refuses its
//!   specification and exits with [`KeeperExit::BadInput`]. It outlives
//!   the engine that started it, and a hangup or a Ctrl-C at the engine's
//!   terminal does not reach it. It ends when it receives `TERM` or `INT`,
//!   or when its app ends and the policy does not restart it. Only the
//!   first stop request counts, and a later one while the keeper stops the
//!   app changes nothing. A stop request during a backoff or while the
//!   keeper resolves an environment ends the keeper without a restart. The
//!   engine reaps the keeper when it ends before the handshake. Otherwise
//!   the engine leaves it running, and the operating system reaps it once
//!   the engine exits.
//!
//! # Starting the app process (PROC-2)
//!
//! - Program: the first word of the specification's command, found on the
//!   `PATH` of the keeper's environment. The plan's author supplies it,
//!   and the engine has replaced `{program}` with the app's program.
//! - Arguments: the rest of the command, each a separate value.
//! - Environment: only `PATH`, `HOME` and the resolved environment. The
//!   keeper clears the app's environment on every start and sets `PATH`
//!   and `HOME` from the specification. It then sets either the fresh
//!   environment it resolved for a restart, or the values of the names
//!   that the specification lists as resolved, which the keeper holds in
//!   the [`KeeperEnvironment`] that `main` passed it. The specification
//!   refuses any inherited variable other than `PATH` and `HOME`, and any
//!   resolved name that is empty, holds `=` or NUL, or is `PATH` or
//!   `HOME`. A sandboxed app also receives `TMPDIR`, its own temporary
//!   directory, which replaces any value the environment gave.
//! - Sandbox: when the specification names one, the program is the
//!   launcher, `<keeper program> <keeper words> --sandboxed <sandbox> --`
//!   followed by the app's command. The sandbox holds paths and ports and
//!   no environment value. The launcher applies it and replaces itself
//!   with the app, so the app keeps the launcher's pid, environment and
//!   streams. A launcher that cannot apply the sandbox or start the app
//!   writes why to the app's log and exits with [`SANDBOX_REFUSED`].
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
//! the backoff before each restart. The backoff doubles with each restart
//! in a row, and starts again from its first wait after a run longer than
//! its longest wait or a minute, whichever is longer. When the policy's
//! [`Resolve`] says the environment must be resolved again, the keeper
//! resolves it through the [`EnvironmentSource`] that the program which
//! links phase passes to [`run`], within [`RESOLVE_WAIT`], and applies it
//! to the restarted app. The keeper holds only applied environments, and
//! never writes a value to its arguments, a file or a log. When resolving
//! fails, the keeper notes the failure in its diagnostic log, counts it as
//! a failed run, waits the next backoff and tries again. Once the policy's
//! most restarts are used up, it exits with
//! [`KeeperExit::EnvironmentUnresolved`].
//!
//! # The run record
//!
//! The keeper records the app's pid, start time, group and boot id as
//! `running` each time it starts the app, with the count of restarts so
//! far after a restart, `stopping` before it stops the
//! app's group, and `stopped` once the group is gone. It records how the
//! app ended, its exit status or signal and whether a stop request caused
//! the end, with `stopping` when the app ended by itself and with
//! `stopped` either way. A restart keeps that end, so the record shows the
//! last crash of an app that restarts. A thread of its own
//! writes these through the run record's journal, which takes the
//! workspace's lifecycle lock, so an `up` or `down` that holds the lock
//! never blocks supervision. The engine also records the first start from
//! the handshake, while it holds the lock. A change applies only while the
//! record's entry names the keeper's own app, so a process that another
//! operation recorded meanwhile keeps its entry, and the keeper notes the
//! skipped change in its diagnostic log. When the keeper exits after a stop
//! request (`TERM` or `INT`), it tries the lock once more without waiting.
//! When the lock is free, it writes the changes still queued. When another
//! operation holds the lock, as the `down` that stopped it usually does,
//! it drops them and notes each one in its diagnostic log, because that
//! operation records the stop itself. After any other end, nothing else
//! records it, so the keeper waits as long as the writer waits for the
//! lock, unless a stop request comes meanwhile, which ends the wait in the
//! same way.
//!
//! A restart that cannot start the app counts as a failed run under the
//! backoff. Only a failure of the first start makes the keeper exit with
//! [`KeeperExit::Unstartable`].
//!
//! [`AppliedEnvironment::apply`]: crate::AppliedEnvironment::apply
//! [`Restart`]: crate::Restart
//! [`Resolve`]: crate::Resolve

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use crate::engine::restart::RunEnd;
use crate::env::EnvironmentSource;

mod args;
mod environment;
mod logfile;
mod recorder;
mod sandbox;
mod spawn;
mod spec;
mod supervise;

#[cfg(test)]
mod tests;

pub use crate::sandbox::{SANDBOX_REFUSED, launch};
pub use args::{BadKeeperArgs, KeeperArgs};
pub use environment::KeeperEnvironment;
pub(crate) use spawn::{AppEnd, KeeperCommand, Kept, Launch, spawn};
pub(crate) use spec::KeeperSpec;

/// The most bytes of the specification that a keeper reads from its
/// standard input. A longer specification is refused.
pub const MOST_SPEC_BYTES: u64 = 1024 * 1024;

/// The most bytes of a keeper's standard output that the engine reads
/// while it looks for the handshake line. The engine stops reading there
/// and fails the start.
pub const MOST_HANDSHAKE_BYTES: u64 = 64 * 1024;

/// How long a keeper gives its environment source to resolve a fresh
/// environment before a restart. A source that has not answered by then
/// fails, and the keeper tries again after the next backoff.
pub const RESOLVE_WAIT: Duration = Duration::from_secs(60);

/// How a keeper ended, which its exit status reports.
///
/// | Exit | Status |
/// | --- | --- |
/// | [`Stopped`](KeeperExit::Stopped) | 0 |
/// | [`Completed`](KeeperExit::Completed) | 0 |
/// | [`Failed`](KeeperExit::Failed) | 1 |
/// | [`Unstoppable`](KeeperExit::Unstoppable) | 2 |
/// | [`EnvironmentUnresolved`](KeeperExit::EnvironmentUnresolved) | 3 |
/// | [`BadInput`](KeeperExit::BadInput) | 64 |
/// | [`Unstartable`](KeeperExit::Unstartable) | 69 |
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
    /// The policy restarts the app with a fresh environment, and the
    /// environment source failed to resolve one on each try until the
    /// policy's most restarts.
    EnvironmentUnresolved,
}

impl KeeperExit {
    /// Returns the keeper's exit status, as the table above gives it.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Stopped | Self::Completed => 0,
            Self::Failed => 1,
            Self::Unstoppable => 2,
            Self::EnvironmentUnresolved => 3,
            Self::BadInput => 64,
            Self::Unstartable => 69,
        }
    }
}

impl From<RunEnd> for KeeperExit {
    fn from(ended: RunEnd) -> Self {
        match ended {
            RunEnd::Completed => Self::Completed,
            RunEnd::Failed => Self::Failed,
        }
    }
}

impl From<KeeperExit> for ExitCode {
    fn from(exit: KeeperExit) -> Self {
        Self::from(exit.code())
    }
}

/// Runs the keeper that `args` names: starts a session of its own, reads
/// its specification from `input`, starts the app, writes the handshake
/// line to `output`, and supervises the app until it is stopped or its
/// policy leaves it ended. Once the app's first start ends, the keeper
/// writes one end line to `output`, which says how the app ended. The
/// engine reads it only while it waits for the app to be ready, and closes
/// its end of the pipe afterwards, so `run` handles `SIGPIPE` itself and
/// ignores a write that fails. The caller writes nothing to its standard
/// output after `run` returns, because the engine may have closed it. The app's first start receives the resolved
/// values that `environment` holds. When a restart needs a fresh
/// environment, the keeper resolves it through `source`. The program that
/// links phase passes the environment it read in `main`, its own
/// environment source, its standard input and its standard output, and
/// returns the result from `main` as its exit status.
#[must_use]
pub fn run(
    args: &KeeperArgs,
    environment: KeeperEnvironment,
    source: &dyn EnvironmentSource,
    input: impl Read,
    output: impl Write,
) -> KeeperExit {
    supervise::run(args, environment, source, input, output)
}
