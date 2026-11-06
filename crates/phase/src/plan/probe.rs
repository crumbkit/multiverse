//! The readiness probe of a process, in the vocabulary of process-compose.
//! The engine's probe module runs it.

use std::fmt;
use std::num::NonZeroU32;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::name::PortName;

/// The time between two attempts of a probe, unless the plan states another.
pub const DEFAULT_PROBE_PERIOD: Duration = Duration::from_secs(10);

/// How long one attempt of a probe may take, unless the plan states another.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// The shortest timeout of a container process's `tcp` probe. Docker's
/// port proxy accepts a connection before anything in the container
/// listens, so one attempt allows up to 300 ms to connect and then holds
/// the connection open for 300 ms. One second leaves room for both.
pub const LEAST_PUBLISHED_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// The consecutive failures that make a probe fail, unless the plan states
/// another number.
pub const DEFAULT_FAILURE_THRESHOLD: NonZeroU32 = match NonZeroU32::new(3) {
    Some(threshold) => threshold,
    None => NonZeroU32::MIN,
};

/// Returns [`DEFAULT_PROBE_PERIOD`], for a probe read from JSON that omits
/// its period.
const fn default_period() -> Duration {
    DEFAULT_PROBE_PERIOD
}

/// Returns [`DEFAULT_PROBE_TIMEOUT`], for a probe read from JSON that omits
/// its timeout.
const fn default_timeout() -> Duration {
    DEFAULT_PROBE_TIMEOUT
}

/// Returns a success threshold of 1, for a probe read from JSON that omits
/// it.
const fn default_success_threshold() -> NonZeroU32 {
    NonZeroU32::MIN
}

/// Returns [`DEFAULT_FAILURE_THRESHOLD`], for a probe read from JSON that
/// omits it.
const fn default_failure_threshold() -> NonZeroU32 {
    DEFAULT_FAILURE_THRESHOLD
}

/// The check that tells phase a process is ready. A process with no probe
/// is ready as soon as it starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Probe {
    /// What one attempt checks.
    pub check: ProbeCheck,
    /// The time between the start of one attempt and the start of the next.
    /// It is greater than zero.
    #[serde(
        default = "default_period",
        rename = "period_ms",
        with = "crate::millis"
    )]
    pub period: Duration,
    /// How long one attempt may take before it counts as a failure. It is
    /// greater than zero.
    #[serde(
        default = "default_timeout",
        rename = "timeout_ms",
        with = "crate::millis"
    )]
    pub timeout: Duration,
    /// The consecutive successful attempts that make the process ready.
    #[serde(default = "default_success_threshold")]
    pub success_threshold: NonZeroU32,
    /// The consecutive failed attempts that make the probe fail.
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: NonZeroU32,
}

impl Probe {
    /// Returns a probe that runs `check` every [`DEFAULT_PROBE_PERIOD`],
    /// with a timeout of [`DEFAULT_PROBE_TIMEOUT`], a success threshold of 1
    /// and a failure threshold of [`DEFAULT_FAILURE_THRESHOLD`].
    #[must_use]
    pub const fn new(check: ProbeCheck) -> Self {
        Self {
            check,
            period: DEFAULT_PROBE_PERIOD,
            timeout: DEFAULT_PROBE_TIMEOUT,
            success_threshold: NonZeroU32::MIN,
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
        }
    }
}

/// What one attempt of a probe checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
#[non_exhaustive]
pub enum ProbeCheck {
    /// An HTTP `GET` of `path` on the process's own port returns a 2xx
    /// status. The probe connects to 127.0.0.1, and to ::1 when 127.0.0.1
    /// refuses or fails, so a server that listens only on ::1 passes. It
    /// sends the request to the address that accepted, with the `Host`
    /// header `localhost:<port>`. The process has a port.
    #[non_exhaustive]
    Http {
        /// The request path, which starts with `/` and holds only printable
        /// ASCII characters other than space and `#`.
        path: String,
    },
    /// A TCP connection to the named port of the app succeeds, on 127.0.0.1
    /// or, when 127.0.0.1 refuses or fails, on ::1.
    #[non_exhaustive]
    Tcp {
        /// One of the app's ports.
        port: PortName,
    },
    /// The process writes a log line that matches a regular expression,
    /// after phase started it. A process that listens on no port, such as a
    /// worker that reads a queue, uses it. The plan holds the pattern's
    /// text, and refuses a pattern that is not a regular expression.
    #[non_exhaustive]
    Log {
        /// The regular expression. It is not empty, and it compiles.
        pattern: String,
    },
    /// A command exits with status 0. The command runs with the app's
    /// environment, as separate values that no shell reads. For a host
    /// process it runs on the host, and for a container process it runs
    /// inside the process's container through `docker exec`.
    #[non_exhaustive]
    Command {
        /// The program and its arguments. It is not empty.
        run: Vec<String>,
    },
}

impl ProbeCheck {
    /// Returns the check that `GET path` on the process's port returns a
    /// 2xx status.
    #[must_use]
    pub const fn http(path: String) -> Self {
        Self::Http { path }
    }

    /// Returns the check that a TCP connection to the app's `port` succeeds.
    #[must_use]
    pub const fn tcp(port: PortName) -> Self {
        Self::Tcp { port }
    }

    /// Returns the check that the process writes a log line matching
    /// `pattern`.
    #[must_use]
    pub const fn log(pattern: String) -> Self {
        Self::Log { pattern }
    }

    /// Returns the check that the command `run` exits with status 0.
    #[must_use]
    pub const fn command(run: Vec<String>) -> Self {
        Self::Command { run }
    }
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

/// Reports whether `path` can be sent as written in an HTTP request line:
/// it holds only ASCII characters, and no space, control character or `#`.
pub(crate) fn is_sendable_path(path: &str) -> bool {
    path.chars()
        .all(|character| character.is_ascii_graphic() && character != '#')
}
