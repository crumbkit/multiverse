//! The failures of phase's operations.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crate::env::EnvironmentError;
use crate::name::{AppName, WorkspaceId};
use crate::record::DockerError;

/// A failure of one of [`Phase`](crate::Phase)'s operations.
#[derive(Debug)]
#[non_exhaustive]
pub enum PhaseError {
    /// The operation is declared but phase does not carry it out yet.
    #[non_exhaustive]
    NotBuilt {
        /// The operation.
        operation: Operation,
    },
    /// A resolver could not supply a value of the app's environment, so
    /// phase started nothing.
    #[non_exhaustive]
    Environment {
        /// The app.
        app: AppName,
        /// The resolver's failure.
        source: EnvironmentError,
    },
    /// Another `up`, `down`, `restart` or `sweep` of the workspace held its
    /// lifecycle lock for all of the lock wait, so this operation did
    /// nothing.
    #[non_exhaustive]
    Busy {
        /// The workspace's stable id.
        workspace: WorkspaceId,
        /// How long the operation waited for the lock.
        waited: Duration,
    },
    /// The workspace's run record cannot be read, so phase does nothing to
    /// that workspace. Other workspaces are unaffected.
    #[non_exhaustive]
    RecordUnreadable {
        /// The workspace's stable id.
        workspace: WorkspaceId,
        /// The record's file.
        path: PathBuf,
        /// Why it cannot be read.
        reason: String,
    },
    /// A newer phase wrote the workspace's run record, so this phase does
    /// nothing to that workspace until it is upgraded.
    #[non_exhaustive]
    NewerRecord {
        /// The workspace's stable id.
        workspace: WorkspaceId,
        /// The record's file.
        path: PathBuf,
        /// The record's schema, such as `phase/run/v2`.
        schema: String,
    },
    /// phase cannot write a file of its state directory.
    #[non_exhaustive]
    StateUnwritable {
        /// The file or directory.
        path: PathBuf,
        /// The failure.
        source: io::Error,
    },
    /// phase cannot read the process table or signal a process.
    #[non_exhaustive]
    ProcessTable {
        /// What failed.
        message: String,
    },
    /// A Docker call failed: Docker did not answer, so phase cannot tell
    /// what is running, or it answered and refused.
    #[non_exhaustive]
    Docker {
        /// Docker's failure.
        source: DockerError,
    },
    /// phase tried to record a change to an object that the run record does
    /// not name, which is a fault in phase.
    #[non_exhaustive]
    Unplanned {
        /// The workspace's stable id.
        workspace: WorkspaceId,
        /// The object.
        object: String,
    },
}

impl PhaseError {
    /// Returns the stable code of the failure, such as
    /// `phase::record::unreadable`, which a program can match on.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotBuilt { .. } => "phase::not_built",
            Self::Environment { .. } => "phase::environment",
            Self::Busy { .. } => "phase::lock::busy",
            Self::RecordUnreadable { .. } => "phase::record::unreadable",
            Self::NewerRecord { .. } => "phase::record::newer",
            Self::StateUnwritable { .. } => "phase::state::unwritable",
            Self::ProcessTable { .. } => "phase::process_table",
            Self::Docker {
                source: DockerError::Refused { .. },
            } => "phase::docker::refused",
            Self::Docker { .. } => "phase::docker::unavailable",
            Self::Unplanned { .. } => "phase::record::unplanned",
        }
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
            Self::Busy { workspace, waited } => write!(
                formatter,
                "another operation held the lifecycle lock of the workspace {workspace} for {} ms",
                waited.as_millis()
            ),
            Self::RecordUnreadable {
                workspace,
                path,
                reason,
            } => write!(
                formatter,
                "the run record of the workspace {workspace} at {} cannot be read: {reason}",
                path.display()
            ),
            Self::NewerRecord {
                workspace, schema, ..
            } => write!(
                formatter,
                "a newer phase wrote the run record of the workspace {workspace} as {schema}; upgrade phase to manage it"
            ),
            Self::StateUnwritable { path, .. } => {
                write!(formatter, "phase cannot write {}", path.display())
            }
            Self::ProcessTable { message } => {
                write!(
                    formatter,
                    "phase cannot read or signal processes: {message}"
                )
            }
            Self::Docker {
                source: DockerError::Refused { .. },
            } => formatter.write_str("Docker refused to remove an object of the run"),
            Self::Docker { .. } => {
                formatter.write_str("Docker did not answer, so phase cannot tell what is running")
            }
            Self::Unplanned { workspace, object } => write!(
                formatter,
                "the run record of the workspace {workspace} does not name {object}"
            ),
        }
    }
}

impl std::error::Error for PhaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Environment { source, .. } => Some(source),
            Self::StateUnwritable { source, .. } => Some(source),
            Self::Docker { source } => Some(source),
            Self::NotBuilt { .. }
            | Self::Busy { .. }
            | Self::RecordUnreadable { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessTable { .. }
            | Self::Unplanned { .. } => None,
        }
    }
}
