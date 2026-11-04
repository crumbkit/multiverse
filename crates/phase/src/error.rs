//! The failures of phase's operations.

use std::fmt;

use crate::env::EnvironmentError;
use crate::name::AppName;
use crate::plan::Permissions;

/// A failure of one of [`Phase`](crate::Phase)'s operations.
#[derive(Debug)]
#[non_exhaustive]
pub enum PhaseError {
    /// The operation is declared but phase does not carry it out yet.
    NotBuilt {
        /// The operation.
        operation: Operation,
    },
    /// The permissions of these apps grew beyond what was granted, so `up`
    /// started nothing. `mxv phase grants approve APP` grants them.
    PermissionsGrew {
        /// Each app whose permissions grew, with what it asks for beyond
        /// its grant.
        grown: Vec<Growth>,
    },
    /// A resolver could not supply a value of the app's environment, so
    /// `up` started nothing.
    Environment {
        /// The app.
        app: AppName,
        /// The resolver's failure.
        source: EnvironmentError,
    },
}

/// The permissions an app asks for beyond what was granted to it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Growth {
    /// The app.
    pub app: AppName,
    /// The permissions it asks for that were not granted.
    pub asked: Permissions,
}

impl Growth {
    /// Returns the growth of `app`, which asks for `asked` beyond its grant.
    #[must_use]
    pub const fn new(app: AppName, asked: Permissions) -> Self {
        Self { app, asked }
    }
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
            Self::PermissionsGrew { grown } => {
                formatter.write_str("the permissions of ")?;
                for (index, growth) in grown.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{}", growth.app)?;
                }
                formatter.write_str(" grew and need approval")
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
            Self::NotBuilt { .. } | Self::PermissionsGrew { .. } => None,
        }
    }
}
