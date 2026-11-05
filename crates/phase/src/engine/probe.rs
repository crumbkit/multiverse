//! The probe runner, which carries out a process's
//! [`Probe`](crate::Probe): HTTP, TCP, command and log checks with a
//! period, a timeout and success and failure thresholds.
//!
//! The module has two layers.
//!
//! - [`Check`] is one attempt of a probe against one process. The caller
//!   builds it with the process's host ports already looked up, the
//!   [`ProbeExec`] that runs a command where the process runs, and the
//!   [`LogLines`] that hands over the process's new log lines. An attempt
//!   holds no clock and no thread, and ends within the probe's timeout.
//! - [`wait`] runs a check under the probe's thresholds until the process
//!   is ready, or for liveness until it fails, or until the deadline
//!   passes, the process ends or the caller cancels. A readiness wait
//!   attempts the check every [`READY_INTERVAL`](runner::READY_INTERVAL), or every period when the
//!   period is shorter, so `up` learns of readiness as soon as mxv's
//!   20 ms polling did, within one interval. A liveness wait attempts it
//!   once each period. Every pause goes through [`crate::clock`].
//!
//! # The checks
//!
//! - `http` sends `GET <path>` as HTTP/1.1 to 127.0.0.1 on the process's
//!   host port, and passes on a 2xx status. It follows no redirect, so a
//!   3xx status fails. It reads at most [`MOST_RESPONSE_BYTES`] bytes of
//!   the response and only needs the status line.
//! - `tcp` passes when a connection to 127.0.0.1 on the port succeeds.
//!   The connection blocks in `connect_timeout`, so a TCP check, and the
//!   connect step of an HTTP check, see a cancel only once the connection
//!   succeeds or fails, at most the probe's timeout later.
//! - `command` passes when the command exits with status 0 within the
//!   timeout. A [`ProbeExec`] runs it with the process's environment:
//!   [`HostExec`] for a host process, and `docker exec` in the container
//!   module for a container process. The command's values are separate
//!   arguments that no shell reads, and no environment value is ever an
//!   argument. When a `docker exec` probe times out, phase kills the local
//!   `docker` client, and the command inside the container may keep
//!   running.
//! - `log` passes once the process has written a line, after phase
//!   started it, that the probe's regular expression matches. A match
//!   counts for every later attempt, because the line stays written, so a
//!   readiness wait reaches the success threshold with attempts in a row
//!   that do not wait an interval. The wait checks the log for a match
//!   before it checks whether the process ended, so a process that writes
//!   its ready line and exits within one poll counts as ready.

use std::fmt;
use std::io;
use std::time::Duration;

use crate::cancel::Cancel;

mod command;
mod http;
mod log;
mod runner;

#[cfg(test)]
mod tests;

pub(crate) use command::{CommandEnd, HostExec, ProbeExec, exited, run_bounded};
pub(crate) use log::{LogLines, LogWatch};
pub(crate) use runner::{Goal, Outcome, wait};
#[cfg(test)]
pub(crate) use runner::{READY_INTERVAL, Tally, Verdict};

/// The most bytes of an HTTP response that a probe reads while it looks
/// for the status line. A response whose status line is longer fails the
/// attempt.
pub(crate) const MOST_RESPONSE_BYTES: usize = 4096;

/// One probe's check of one process, ready to attempt.
pub(crate) enum Check<'a> {
    /// `GET path` on 127.0.0.1 at `port` returns a 2xx status.
    Http {
        /// The process's host port.
        port: u16,
        /// The request path, which starts with `/`.
        path: &'a str,
    },
    /// A TCP connection to 127.0.0.1 at `port` succeeds.
    Tcp {
        /// The host port that the probe's port name stands for.
        port: u16,
    },
    /// Something inside a container answers on the published host port
    /// `port`. Docker's port proxy accepts a connection to a published port
    /// before anything in the container listens, so a TCP probe of a
    /// container process uses this check, which
    /// [`answers`](crate::engine::container::answers) describes.
    Published {
        /// The published host port that the probe's port name stands for.
        port: u16,
    },
    /// The command `run` exits with status 0.
    Command {
        /// The program and its arguments.
        run: &'a [String],
        /// What runs the command where the process runs.
        exec: &'a dyn ProbeExec,
    },
    /// The process has written a line that the pattern matches.
    Log {
        /// The pattern and whether a line has matched it.
        watch: LogWatch,
        /// The process's new log lines.
        lines: &'a mut dyn LogLines,
    },
}

impl fmt::Debug for Check<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http { port, path } => formatter
                .debug_struct("Http")
                .field("port", port)
                .field("path", path)
                .finish(),
            Self::Tcp { port } => formatter.debug_struct("Tcp").field("port", port).finish(),
            Self::Published { port } => formatter
                .debug_struct("Published")
                .field("port", port)
                .finish(),
            Self::Command { run, .. } => {
                formatter.debug_struct("Command").field("run", run).finish()
            }
            Self::Log { watch, .. } => formatter.debug_struct("Log").field("watch", watch).finish(),
        }
    }
}

impl Check<'_> {
    /// Makes one attempt of the check, which ends within `timeout` or soon
    /// after `cancel` is set, and returns whether it passed or why it
    /// failed.
    pub(crate) fn attempt(&mut self, timeout: Duration, cancel: &Cancel) -> Attempt {
        match self {
            Self::Http { port, path } => http::attempt(*port, path, timeout, cancel),
            Self::Tcp { port } => http::connect(*port, timeout).map(drop).into(),
            Self::Published { port } => {
                if crate::engine::container::answers_within(*port, timeout, cancel) {
                    Attempt::Passed
                } else {
                    Attempt::Failed(Failure::Connect(io::ErrorKind::ConnectionRefused))
                }
            }
            Self::Command { run, exec } => command::attempt(*exec, run, timeout, cancel),
            Self::Log { watch, lines } => watch.attempt(*lines),
        }
    }

    /// Reads what arrived since the last attempt without making one, and
    /// reports whether an attempt now would pass. Only a log check learns
    /// anything between attempts, so the runner attempts it at once when a
    /// matching line has arrived.
    pub(crate) fn poll(&mut self) -> bool {
        match self {
            Self::Log { watch, lines } => watch.poll(*lines),
            Self::Http { .. }
            | Self::Tcp { .. }
            | Self::Published { .. }
            | Self::Command { .. } => false,
        }
    }
}

/// The result of one attempt of a check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// The check passed.
    Passed,
    /// The check failed for the reason given.
    Failed(Failure),
}

impl From<Result<(), Failure>> for Attempt {
    fn from(result: Result<(), Failure>) -> Self {
        match result {
            Ok(()) => Self::Passed,
            Err(failure) => Self::Failed(failure),
        }
    }
}

/// Why an attempt of a check failed. It holds no environment value and no
/// response body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The attempt did not finish within the probe's timeout.
    TimedOut,
    /// The connection to the port failed.
    Connect(io::ErrorKind),
    /// Sending the request or reading the response failed.
    Exchange(io::ErrorKind),
    /// The response did not start with an HTTP/1 status line within
    /// [`MOST_RESPONSE_BYTES`] bytes.
    BadResponse,
    /// The response's status was not 2xx.
    Status(u16),
    /// The request path holds a space, a control character, a character
    /// outside ASCII or `#`, so phase does not send it.
    BadPath,
    /// The command could not be started.
    Unstarted(io::ErrorKind),
    /// Waiting for the command failed, so its end is unknown. Its group was
    /// killed.
    WaitFailed(io::ErrorKind),
    /// The caller cancelled the attempt.
    Cancelled,
    /// The command exited with a status other than 0, or a signal ended it
    /// and `code` is `None`.
    Exited {
        /// The command's exit status.
        code: Option<i32>,
    },
    /// The process has written no line that the pattern matches.
    NoMatch,
    /// The process's log could not be read.
    Unreadable(io::ErrorKind),
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => formatter.write_str("the attempt timed out"),
            Self::Connect(kind) => write!(formatter, "the connection failed: {kind}"),
            Self::Exchange(kind) => write!(formatter, "the HTTP exchange failed: {kind}"),
            Self::BadResponse => formatter.write_str("the response had no HTTP/1 status line"),
            Self::Status(status) => write!(formatter, "the response status was {status}"),
            Self::BadPath => formatter.write_str(
                "the request path holds a space, a control character, a character outside ASCII or `#`",
            ),
            Self::Unstarted(kind) => write!(formatter, "the command did not start: {kind}"),
            Self::WaitFailed(kind) => {
                write!(formatter, "the wait for the command failed: {kind}")
            }
            Self::Cancelled => formatter.write_str("the attempt was cancelled"),
            Self::Exited { code: Some(code) } => {
                write!(formatter, "the command exited with status {code}")
            }
            Self::Exited { code: None } => formatter.write_str("a signal ended the command"),
            Self::NoMatch => formatter.write_str("no log line matched the pattern"),
            Self::Unreadable(kind) => write!(formatter, "the log could not be read: {kind}"),
        }
    }
}
