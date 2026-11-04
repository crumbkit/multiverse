//! The failures of phase's operations.

use std::fmt;

use crate::env::EnvironmentError;
use crate::name::AppName;

/// A failure of one of [`Phase`](crate::Phase)'s operations.
#[derive(Debug)]
#[non_exhaustive]
pub enum PhaseError {
    /// The operation is declared but phase does not carry it out yet.
    NotBuilt {
        /// The operation.
        operation: Operation,
    },
    /// A resolver could not supply a value of the app's environment, so
    /// phase started nothing.
    Environment {
        /// The app.
        app: AppName,
        /// The resolver's failure.
        source: EnvironmentError,
    },
}

/// One of [`Phase`](crate::Phase)'s operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// [`Phase::up`](crate::Phase::up).
    Up,
    /// [`Phase::down`](crate::Phase::down).
    Down,
    /// [`Phase::status`](crate::Phase::status).
    Status,
    /// [`Phase::logs`](crate::Phase::logs).
    Logs,
    /// [`Phase::sweep`](crate::Phase::sweep).
    Sweep,
    /// [`Phase::exec`](crate::Phase::exec).
    Exec,
    /// [`Phase::restart`](crate::Phase::restart).
    Restart,
}

impl fmt::Display for Operation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Status => "status",
            Self::Logs => "logs",
            Self::Sweep => "sweep",
            Self::Exec => "exec",
            Self::Restart => "restart",
        })
    }
}

impl fmt::Display for PhaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotBuilt { operation } => {
                write!(
                    formatter,
                    "phase cannot {operation} yet, because it is not built"
                )
            }
            Self::Environment { app, .. } => {
                write!(formatter, "the environment of {app} could not be resolved")
            }
        }
    }
}

impl std::error::Error for PhaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Environment { source, .. } => Some(source),
            Self::NotBuilt { .. } => None,
        }
    }
}
