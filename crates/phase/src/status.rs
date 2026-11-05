//! The status document `phase/status/v1`, which `mxv phase status --json`
//! prints for people, agents and other tools, and [`status`], which builds
//! it from the run record.
//!
//! `status` opens the record read-only and takes no lock. It never trusts
//! a stored state alone. A keeper that an outside `TERM` stops while
//! another operation holds the lock drops its record notes, so the record
//! can name a process as running after it has gone. `status` therefore
//! checks each live host process's identity in the process table, and each
//! live container's state with Docker. A process that phase cannot prove
//! is its own, such as one that phase may not read or an entry with a
//! group of 1, which `sweep` leaves until someone removes it, is reported
//! as unknown, never hidden.

mod build;

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, ProcessName, WorkspaceId, WorkspaceName};
use crate::plan::RunnerKind;
use crate::record::Handle;
use crate::source::{ImageReference, Source, Version};
pub(crate) use crate::status::build::{Sources, status};

#[cfg(test)]
mod tests;

/// The status of a workspace's processes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Status {
    /// The document's version. A reader refuses any other.
    pub schema: StatusSchema,
    /// The workspace's stable id, which keys its run record.
    pub workspace: WorkspaceId,
    /// The workspace's name, or `None` when `status` had no plan.
    pub name: Option<WorkspaceName>,
    /// The state of the workspace as a whole.
    pub state: WorkspaceState,
    /// One row per process.
    pub rows: Vec<StatusRow>,
}

impl Status {
    /// Returns the status of the workspace whose stable id is `workspace`
    /// in `state`, with no name and no rows.
    #[must_use]
    pub const fn new(workspace: WorkspaceId, state: WorkspaceState) -> Self {
        Self {
            schema: StatusSchema::V1,
            workspace,
            name: None,
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
    /// A process is starting.
    Starting,
    /// Every process is running or has completed.
    Running,
    /// Some planned processes are running and others are not.
    PartlyRunning,
    /// No process is running.
    Stopped,
    /// phase cannot tell what the processes it cannot read are doing, and
    /// they decide the workspace's state: every other process is running,
    /// or none of the others runs.
    Unknown,
}

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
    /// Where its program came from, or `None` when `status` had no plan
    /// or the plan lacks the app.
    pub source: Option<SourceSummary>,
    /// What the process is doing. In JSON its fields sit beside the row's.
    #[serde(flatten)]
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
    /// How a host process last ended by itself, which no stop request
    /// caused, or `None` when it has not since phase last started it. A
    /// process that its keeper restarted keeps the end of its last crash
    /// here. A stop that phase begins, such as a `down` while the keeper
    /// waits out a backoff, replaces the crash with a requested end, so
    /// this is then `None`. A `down` or a sweep that leaves everything
    /// stopped removes the run record, and with it every recorded end,
    /// unless the `down` did not select a process that holds such an end.
    /// In JSON it is left out when it is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<LastExit>,
    /// The path of its log file, or `None` for a container, whose log comes
    /// from `docker logs`.
    pub log: Option<PathBuf>,
}

/// How a process ended by itself: its exit status, or the signal that
/// ended it. One of the two is `None`, and both are `None` when phase
/// could not read how it ended. In JSON a `None` is left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LastExit {
    /// The exit status, or `None` when a signal ended the process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// The signal that ended the process, or `None` when it exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

impl LastExit {
    /// Returns the end of a process that exited with `code` or that
    /// `signal` ended.
    #[must_use]
    pub const fn new(code: Option<i32>, signal: Option<i32>) -> Self {
        Self { code, signal }
    }
}

impl StatusRow {
    /// Returns the row of `process` of `app`, which runs with `runner` from
    /// `source` and is in `state`, with no handle, address, times, restarts
    /// or log.
    #[must_use]
    pub const fn new(
        app: AppName,
        process: ProcessName,
        runner: RunnerKind,
        source: Option<SourceSummary>,
        state: ProcessState,
    ) -> Self {
        Self {
            app,
            process,
            runner,
            source,
            state,
            handle: None,
            address: None,
            started_at_ms: None,
            expires_at_ms: None,
            restarts: 0,
            last_exit: None,
            log: None,
        }
    }
}

/// Where a running process's program came from, without the details of its
/// build or verification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum SourceSummary {
    /// A published release of this version.
    #[non_exhaustive]
    Release {
        /// The version.
        version: Version,
    },
    /// The checkout in this directory.
    #[non_exhaustive]
    Checkout {
        /// The checkout's directory.
        dir: PathBuf,
    },
    /// This container image.
    #[non_exhaustive]
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
    /// The process's keeper runs and has not started it yet, or the run
    /// record plans a container that Docker has not started.
    Starting,
    /// The process runs, and phase observed it ready since its last start
    /// or restart: its readiness probe passed, or it has no probe and
    /// outlasted its start grace after `up` started it or its keeper
    /// restarted it. A container also needs Docker to report the same
    /// start that phase observed ready, so any later start, such as one
    /// under Docker's restart policy or a `docker restart`, makes it not
    /// ready.
    Ready,
    /// The process runs, and phase has not observed it ready since its
    /// last start or restart. Its readiness probe has not passed yet, or
    /// its keeper or Docker restarted it after the probe passed, or an
    /// older phase wrote the run record.
    Running,
    /// The process exited and phase is waiting to restart it, or Docker is
    /// restarting the container under its restart policy.
    Restarting,
    /// The container is paused: its processes are frozen and keep their
    /// resources. phase never pauses a container, so something outside
    /// phase paused it.
    Paused,
    /// phase sent the stop signal and the process has not exited yet.
    Stopping,
    /// The process is not running and phase did not see it fail.
    Stopped,
    /// A oneshot process exited with status 0.
    Completed,
    /// The process exited by itself, which no stop request caused, and
    /// phase will not restart it.
    #[non_exhaustive]
    Exited {
        /// The exit status, or `None` when a signal ended the process or
        /// phase could not read the status.
        code: Option<i32>,
        /// The signal that ended a host process, or `None`. In JSON it is
        /// left out when it is `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
        /// Why phase knows neither the code nor the signal, or `None`. In
        /// JSON it is left out when it is `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<ExitReason>,
    },
    /// phase cannot tell what the process is doing. The run record keeps
    /// its entry, which `down` and `sweep` leave alone.
    #[non_exhaustive]
    Unknown {
        /// Why phase cannot tell.
        reason: UnknownReason,
    },
}

/// Why `status` knows neither the code nor the signal of a process that
/// exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ExitReason {
    /// Nothing recorded how the process ended. Its keeper ended before it
    /// recorded the end, as when something killed the keeper or the
    /// machine restarted, or the keeper could not read the status. A stop
    /// that the keeper could not record shows this way too: a `TERM` from
    /// outside phase while another operation holds the lifecycle lock makes
    /// the keeper stop its app and drop its record notes.
    Unrecorded,
}

/// Why `status` cannot tell what a process is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum UnknownReason {
    /// phase may not read the process that has the recorded pid, as
    /// inside a sandbox, or the record holds no boot id on a system that
    /// has one.
    Unreadable,
    /// The record names a process or group that phase never signals, such
    /// as group 1, which `kill(2)` reads as every process of the user.
    Unsignallable,
    /// Docker did not answer, so phase cannot tell whether the container
    /// runs.
    DockerUnavailable,
    /// Docker reported a container status that phase does not know, such
    /// as `removing`.
    UnrecognisedState,
    /// The recorded process has ended and left members in its process
    /// group, and nothing proves that the group is still phase's own: the
    /// group may have emptied and its id passed to another program's
    /// group. A keeper that phase proved its own vouches for its app's
    /// group, and then the process is not unknown.
    LeaderGone,
}

impl ProcessState {
    /// Returns the state of a process that exited with `code`, or with
    /// `None` when phase could not read it, and that phase will not
    /// restart.
    #[must_use]
    pub const fn exited(code: Option<i32>) -> Self {
        Self::Exited {
            code,
            signal: None,
            reason: None,
        }
    }

    /// Returns the state of a process that ended while nothing recorded
    /// how, as [`ExitReason::Unrecorded`] says.
    #[must_use]
    pub const fn unrecorded() -> Self {
        Self::Exited {
            code: None,
            signal: None,
            reason: Some(ExitReason::Unrecorded),
        }
    }

    /// Returns the state of a process that `signal` ended, and that phase
    /// will not restart.
    #[must_use]
    pub const fn killed(signal: i32) -> Self {
        Self::Exited {
            code: None,
            signal: Some(signal),
            reason: None,
        }
    }

    /// Returns the state of a process that phase cannot tell about, for
    /// `reason`.
    #[must_use]
    pub const fn unknown(reason: UnknownReason) -> Self {
        Self::Unknown { reason }
    }

    /// Reports whether a process in this state may be running, which
    /// includes a process whose state phase cannot tell.
    #[must_use]
    pub const fn may_run(self) -> bool {
        !matches!(self, Self::Stopped | Self::Completed | Self::Exited { .. })
    }
}
