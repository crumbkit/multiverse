//! The restart policy and stop handling of a process, in the vocabulary of
//! process-compose. The engine's restart module carries them out.

use std::fmt;
use std::num::NonZeroU32;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// The first wait before a restart, unless the plan states another.
pub const DEFAULT_FIRST_BACKOFF: Duration = Duration::from_secs(1);

/// The longest wait before a restart, unless the plan states another.
pub const DEFAULT_MOST_BACKOFF: Duration = Duration::from_secs(30);

/// The time a process has to stop after its stop signal, unless the plan
/// states another. phase sends `KILL` to the process group when it passes.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(10);

/// When phase restarts a process that exits. phase restarts a container
/// process by recreating its container.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "kebab-case")]
pub enum Restart {
    /// phase never restarts the process.
    #[default]
    No,
    /// phase restarts the process when it exits with a status other than 0
    /// or is ended by a signal that phase did not send.
    OnFailure(RestartLimits),
    /// phase restarts the process whenever it exits, until phase stops it.
    Always(RestartLimits),
}

/// How often and how soon phase restarts a process, and whether it resolves
/// the environment again first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartLimits {
    /// The most restarts after which phase leaves the process stopped, or
    /// `None` when phase restarts it without limit.
    pub max: Option<NonZeroU32>,
    /// The waits before each restart.
    pub backoff: Backoff,
    /// Whether a restart after a crash resolves the app's environment again.
    #[serde(default)]
    pub resolve: Resolve,
}

/// Whether a restart after a crash resolves the app's environment again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resolve {
    /// phase resolves the environment again when one of its values has
    /// expired, as [`AppliedEnvironment::expires_at`] reports, and reuses it
    /// otherwise.
    ///
    /// [`AppliedEnvironment::expires_at`]: crate::AppliedEnvironment::expires_at
    #[default]
    WhenExpired,
    /// phase resolves the environment again before every restart.
    Always,
    /// phase reuses the environment the process first started with.
    Never,
}

/// The waits before successive restarts, which double from `first` up to
/// `most`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "BackoffFile", into = "BackoffFile")]
pub struct Backoff {
    /// The wait before the first restart, which is greater than zero.
    first: Duration,
    /// The longest wait, which is at least `first`.
    most: Duration,
}

impl Backoff {
    /// Returns the backoff that waits `first` before the first restart and
    /// doubles each wait up to `most`.
    ///
    /// # Errors
    ///
    /// Returns [`BadBackoff::ZeroFirst`] when `first` is zero, and
    /// [`BadBackoff::FirstAboveMost`] when `first` is longer than `most`.
    pub fn new(first: Duration, most: Duration) -> Result<Self, BadBackoff> {
        if first.is_zero() {
            return Err(BadBackoff::ZeroFirst);
        }
        if first > most {
            return Err(BadBackoff::FirstAboveMost);
        }
        Ok(Self { first, most })
    }

    /// Returns the wait before the first restart.
    #[must_use]
    pub const fn first(&self) -> Duration {
        self.first
    }

    /// Returns the longest wait.
    #[must_use]
    pub const fn most(&self) -> Duration {
        self.most
    }
}

impl Default for Backoff {
    /// Returns the backoff from [`DEFAULT_FIRST_BACKOFF`] to
    /// [`DEFAULT_MOST_BACKOFF`].
    fn default() -> Self {
        Self {
            first: DEFAULT_FIRST_BACKOFF,
            most: DEFAULT_MOST_BACKOFF,
        }
    }
}

/// The form of a [`Backoff`] in JSON.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackoffFile {
    /// The wait before the first restart.
    first: Duration,
    /// The longest wait.
    most: Duration,
}

impl TryFrom<BackoffFile> for Backoff {
    type Error = BadBackoff;

    fn try_from(file: BackoffFile) -> Result<Self, BadBackoff> {
        Self::new(file.first, file.most)
    }
}

impl From<Backoff> for BackoffFile {
    fn from(backoff: Backoff) -> Self {
        Self {
            first: backoff.first,
            most: backoff.most,
        }
    }
}

/// The reason that two waits are not a valid [`Backoff`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadBackoff {
    /// The first wait is zero, so restarts would not back off.
    ZeroFirst,
    /// The first wait is longer than the longest wait.
    FirstAboveMost,
}

impl fmt::Display for BadBackoff {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroFirst => formatter.write_str("the first backoff is zero"),
            Self::FirstAboveMost => {
                formatter.write_str("the first backoff is longer than the longest backoff")
            }
        }
    }
}

impl std::error::Error for BadBackoff {}

/// How phase stops a process: it sends `signal` to the process group, and
/// sends `KILL` when the process has not exited after `grace`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    /// The signal that asks the process to stop.
    pub signal: StopSignal,
    /// How long phase waits after the signal before it sends `KILL`.
    pub grace: Duration,
}

impl Default for Stop {
    /// Returns `TERM` with a grace of [`DEFAULT_STOP_GRACE`].
    fn default() -> Self {
        Self {
            signal: StopSignal::Term,
            grace: DEFAULT_STOP_GRACE,
        }
    }
}

/// A signal that asks a process to stop. `KILL` is not one, because phase
/// sends it only after the grace period.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum StopSignal {
    /// `SIGTERM`.
    Term,
    /// `SIGINT`.
    Int,
    /// `SIGQUIT`.
    Quit,
    /// `SIGHUP`.
    Hup,
    /// `SIGUSR1`.
    Usr1,
    /// `SIGUSR2`.
    Usr2,
}
