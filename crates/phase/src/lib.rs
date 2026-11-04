//! phase runs the apps of one workspace on the local machine.
//!
//! multiverse builds a [`Plan`] from the workspace record and each app's
//! manifest, and calls [`Phase::up`] with it. A host app's processes run
//! under a keeper each, and a container app runs as a Docker Compose
//! project. An app's environment comes from an [`EnvironmentSource`], whose
//! [`AppliedEnvironment`] applies values to a process and never returns
//! one. phase records what it started in a run record, so that
//! [`Phase::sweep`] can stop it after a crash, and reports state as JSON
//! documents such as [`Status`].
//!
//! The library reads no environment variable and writes to no standard
//! stream. The `mxv` command links it.
//!
//! Every operation of [`Phase`] returns [`PhaseError::NotBuilt`] until its
//! code lands.

mod engine;
mod env;
mod error;
mod logs;
mod name;
mod phase;
mod plan;
mod record;
mod report;
mod select;
mod source;
mod status;

pub use crate::env::{AppliedEnvironment, EnvironmentError, EnvironmentSource};
pub use crate::error::{Operation, PhaseError};
pub use crate::logs::{LineSink, LogLine, LogRequest, LogStream};
pub use crate::name::{
    AppName, BadName, MOST_NAME_BYTES, PortName, ProcessName, VolumeName, WorkspaceId,
    WorkspaceName,
};
pub use crate::phase::{Phase, UpDeadlines};
pub use crate::plan::container::{BindMount, ContainerOptions};
pub use crate::plan::probe::{
    DEFAULT_FAILURE_THRESHOLD, DEFAULT_PROBE_PERIOD, DEFAULT_PROBE_TIMEOUT, Probe, ProbeCheck,
};
pub use crate::plan::restart::{
    Backoff, BadBackoff, DEFAULT_FIRST_BACKOFF, DEFAULT_MOST_BACKOFF, DEFAULT_STOP_GRACE, Resolve,
    Restart, RestartLimits, Stop, StopSignal,
};
pub use crate::plan::{
    AppPlan, ContainerProblem, Permissions, Plan, PlanError, ProcessKind, ProcessPlan,
    ProcessProblem, RunnerKind,
};
pub use crate::record::{BadContainerId, ContainerId, Handle, SweepOutcome, Swept, SweptItem};
pub use crate::report::{AppOutcome, AppReport, Report};
pub use crate::select::{BadTarget, Selection, Target};
pub use crate::source::{
    BadSourceText, BuildSpec, GithubRepository, ImageReference, MOST_IMAGE_BYTES,
    MOST_REPOSITORY_BYTES, MOST_VERSION_BYTES, ReleaseSpec, Source, Verification, Version,
};
pub use crate::status::{
    ProcessState, SourceSummary, Status, StatusRow, StatusSchema, WorkspaceState,
};
