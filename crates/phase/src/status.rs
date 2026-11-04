//! The status document `phase/status/v1`, which `mxv phase status --json`
//! prints for people, agents and other tools. The code that builds it from
//! the run record lands here.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, ProcessName, WorkspaceName};
use crate::plan::RunnerKind;
use crate::record::Handle;
use crate::source::{ImageReference, Source, Version};

#[cfg(test)]
mod tests;

/// The status of a workspace's processes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Status {
    /// The document's version. A reader refuses any other.
    pub schema: StatusSchema,
    /// The workspace.
    pub workspace: WorkspaceName,
    /// The state of the workspace as a whole.
    pub state: WorkspaceState,
    /// One row per process.
    pub rows: Vec<StatusRow>,
}

impl Status {
    /// Returns the status of `workspace` in `state`, with no rows.
    #[must_use]
    pub const fn new(workspace: WorkspaceName, state: WorkspaceState) -> Self {
        Self {
            schema: StatusSchema::V1,
            workspace,
            state,
            rows: Vec::new(),
        }
    }
}

/// The version of the status document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusSchema {
    /// `phase/status/v1`.
    #[serde(rename = "phase/status/v1")]
    V1,
}

/// The state of a workspace as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum WorkspaceState {
    /// An `up` is in progress.
    Starting,
    /// Every planned process is running or has completed.
    Running,
    /// Some planned processes are running and others are not.
    PartlyRunning,
    /// No process is running.
    Stopped,
}

// Only phase builds a status row, so it has no constructor. It is
// non-exhaustive because LIB-9 says adding a field to an exhaustive public
// type breaks every dependant that builds or matches it.
/// The status of one process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StatusRow {
    /// The app.
    pub app: AppName,
    /// The process.
    pub process: ProcessName,
    /// How the process runs.
    pub runner: RunnerKind,
    /// Where its program came from.
    pub source: SourceSummary,
    /// What the process is doing.
    pub state: ProcessState,
    /// Its pid or container id, or `None` when it is not running.
    pub handle: Option<Handle>,
    /// The address it listens on, or `None`.
    pub address: Option<SocketAddr>,
    /// When it started, in milliseconds since the Unix epoch, or `None`.
    pub started_at_ms: Option<u64>,
    /// When the earliest of its environment values expires, in milliseconds
    /// since the Unix epoch, or `None` when none expires. A time in the past
    /// means the process's credentials have expired.
    pub expires_at_ms: Option<u64>,
    /// How many times phase has restarted it.
    pub restarts: u32,
    /// The path of its log file, or `None` for a container, whose log comes
    /// from `docker logs`.
    pub log: Option<PathBuf>,
}

/// Where a running process's program came from, without the details of its
/// build or verification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceSummary {
    /// A published release of this version.
    Release {
        /// The version.
        version: Version,
    },
    /// The checkout in this directory.
    Checkout {
        /// The checkout's directory.
        dir: PathBuf,
    },
    /// This container image.
    Image {
        /// The image.
        image: ImageReference,
    },
}

impl From<&Source> for SourceSummary {
    fn from(source: &Source) -> Self {
        match source {
            Source::Release { version, .. } => Self::Release {
                version: version.clone(),
            },
            Source::Checkout { dir, .. } => Self::Checkout { dir: dir.clone() },
            Source::Image { image } => Self::Image {
                image: image.clone(),
            },
        }
    }
}

/// What a process is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ProcessState {
    /// The process has started and has not passed its probe yet.
    Starting,
    /// The process passed its probe, or has no probe.
    Ready,
    /// The process exited and phase is waiting to restart it.
    Restarting,
    /// phase sent the stop signal and the process has not exited yet.
    Stopping,
    /// The process is not running and phase did not see it fail.
    Stopped,
    /// A oneshot process exited with status 0.
    Completed,
    /// The process exited and phase will not restart it.
    Exited {
        /// The exit status, or `None` when a signal ended the process.
        code: Option<i32>,
    },
}
