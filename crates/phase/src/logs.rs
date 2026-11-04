//! Reading a process's log. A host process's keeper writes its log a line
//! at a time to a file, and a container's log comes from `docker logs`.
//! The code that reads and follows logs lands here.

use std::ops::ControlFlow;
use std::time::Duration;

use crate::name::{AppName, ProcessName};

/// Which log `logs` reads, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogRequest {
    /// The app.
    pub app: AppName,
    /// The app's process.
    pub process: ProcessName,
    /// Whether `logs` keeps passing new lines after the existing ones, until
    /// the sink breaks.
    pub follow: bool,
    /// How far back the log starts, or `None` for the whole log.
    pub since: Option<Duration>,
}

impl LogRequest {
    /// Returns the request for the whole log of `process` of `app`, without
    /// following it.
    #[must_use]
    pub const fn new(app: AppName, process: ProcessName) -> Self {
        Self {
            app,
            process,
            follow: false,
            since: None,
        }
    }
}

/// Receives the lines of a log, one at a time.
pub trait LineSink {
    /// Receives one line. Returning [`ControlFlow::Break`] ends the read,
    /// which is how a caller stops following a log.
    fn line(&mut self, line: &LogLine<'_>) -> ControlFlow<()>;
}

/// One line of a process's log, without its line ending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogLine<'a> {
    /// The stream the process wrote the line to.
    pub stream: LogStream,
    /// The line's text.
    pub text: &'a str,
}

impl<'a> LogLine<'a> {
    /// Returns the line `text` that the process wrote to `stream`.
    #[must_use]
    pub const fn new(stream: LogStream, text: &'a str) -> Self {
        Self { stream, text }
    }
}

/// The stream a process wrote a log line to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}
