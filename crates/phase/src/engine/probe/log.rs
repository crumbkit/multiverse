//! The `log` check. [`LogPattern`] compiles and validates the probe's
//! regular expression as mxv's `ready = { log = "..." }` check did, and
//! [`LogWatch`] matches it against the new lines that a [`LogLines`]
//! hands over. The check never opens the log: the host runner's follower
//! reads it and implements [`LogLines`].

use std::fmt;
use std::io;

use regex::Regex;

use super::{Attempt, Failure};

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

/// A regular expression that a log probe matches against each new line.
/// A line matches when the expression matches any part of it.
#[derive(Clone, Debug)]
pub(crate) struct LogPattern(Regex);

impl LogPattern {
    /// Compiles `pattern`.
    ///
    /// # Errors
    ///
    /// Returns [`BadLogPattern::Empty`] when `pattern` is empty, and
    /// [`BadLogPattern::Invalid`] with the compiler's one-line reason when
    /// it is not a regular expression.
    pub(crate) fn new(pattern: &str) -> Result<Self, BadLogPattern> {
        if pattern.is_empty() {
            return Err(BadLogPattern::Empty);
        }
        Regex::new(pattern)
            .map(Self)
            .map_err(|error| BadLogPattern::Invalid(regex_problem(&error)))
    }

    /// Returns the pattern as the plan writes it.
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Reports whether `line` matches the pattern.
    pub(crate) fn is_match(&self, line: &str) -> bool {
        self.0.is_match(line)
    }
}

impl PartialEq for LogPattern {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for LogPattern {}

/// The reason a log probe's pattern cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BadLogPattern {
    /// The pattern is empty, so every line would match it.
    Empty,
    /// The pattern is not a regular expression, for the reason given.
    Invalid(String),
}

impl fmt::Display for BadLogPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the log pattern is empty"),
            Self::Invalid(problem) => {
                write!(
                    formatter,
                    "the log pattern is not a regular expression: {problem}"
                )
            }
        }
    }
}

impl std::error::Error for BadLogPattern {}

/// Returns the one-line reason in `error`, which regex reports over several
/// lines with the pattern and a caret.
fn regex_problem(error: &regex::Error) -> String {
    let text = error.to_string();
    text.lines()
        .find_map(|line| line.trim().strip_prefix("error: "))
        .map_or_else(
            || text.split_whitespace().collect::<Vec<_>>().join(" "),
            str::to_owned,
        )
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
