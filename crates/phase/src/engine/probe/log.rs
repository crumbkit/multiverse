//! The `log` check. [`LogWatch`] matches the plan's compiled
//! [`LogPattern`] against the new lines that a [`LogLines`] hands over. The
//! check never opens the log: the host runner's follower reads it and
//! implements [`LogLines`].

use std::io;

use super::{Attempt, Failure};
use crate::plan::probe::LogPattern;

/// A source of a process's log lines that have arrived since the last
/// call, such as the host runner's follower of the process's log file.
pub(crate) trait LogLines {
    /// Returns the complete lines written since the last call, without
    /// their line endings.
    ///
    /// # Errors
    ///
    /// Returns the error that kept the log from being read.
    fn new_lines(&mut self) -> io::Result<Vec<String>>;
}

/// A log pattern and whether a line has matched it since phase started
/// the process.
#[derive(Debug)]
pub(crate) struct LogWatch {
    /// The pattern.
    pattern: LogPattern,
    /// Whether a line has matched.
    matched: bool,
}

impl LogWatch {
    /// Returns a watch that no line has matched yet.
    pub(crate) const fn new(pattern: LogPattern) -> Self {
        Self {
            pattern,
            matched: false,
        }
    }

    /// Takes `lines`, and reports whether any line so far has matched.
    pub(crate) fn observe<'a>(&mut self, lines: impl IntoIterator<Item = &'a str>) -> bool {
        if !self.matched {
            self.matched = lines.into_iter().any(|line| self.pattern.is_match(line));
        }
        self.matched
    }

    /// Reads the new lines from `source`, and passes when any line so far
    /// has matched.
    pub(super) fn attempt(&mut self, source: &mut dyn LogLines) -> Attempt {
        if self.matched {
            return Attempt::Passed;
        }
        match source.new_lines() {
            Ok(lines) if self.observe(lines.iter().map(String::as_str)) => Attempt::Passed,
            Ok(_) => Attempt::Failed(Failure::NoMatch),
            Err(error) => Attempt::Failed(Failure::Unreadable(error.kind())),
        }
    }

    /// Reads the new lines from `source`, and reports whether any line so
    /// far has matched. A read that fails reports the matches so far, and
    /// the next attempt reports the failure.
    pub(super) fn poll(&mut self, source: &mut dyn LogLines) -> bool {
        if self.matched {
            return true;
        }
        source
            .new_lines()
            .is_ok_and(|lines| self.observe(lines.iter().map(String::as_str)))
    }
}
