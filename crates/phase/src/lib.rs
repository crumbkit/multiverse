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
//! [`Phase::status`], [`Phase::logs`] and [`Phase::exec`] return
//! [`PhaseError::NotBuilt`] until their code lands.

mod cancel;
mod clock;
mod engine;
mod env;
mod error;
mod logs;
mod millis;
mod name;
mod phase;
mod plan;
pub mod random;
mod record;
mod report;
mod select;
mod source;
mod status;

pub use crate::cancel::Cancel;
pub use crate::engine::container;
pub use crate::engine::host;
pub use crate::engine::keeper;
pub use crate::env::{AppliedEnvironment, EnvironmentError, EnvironmentSource};
pub use crate::error::{NotRunnable, Operation, PhaseError};
pub use crate::logs::{LineSink, LogLine, LogRequest, LogStream, MOST_TAIL_BYTES, tail};
pub use crate::name::{
    AppName, BadName, MOST_NAME_BYTES, PortName, ProcessName, VolumeName, WorkspaceId,
    WorkspaceName, is_unit_name,
};
pub use crate::phase::{
    DEFAULT_KILL_WAIT, DEFAULT_LOCK_WAIT, DEFAULT_START_GRACE, Phase, UpDeadlines,
};
pub use crate::plan::container::{BindMount, ContainerOptions};
pub use crate::plan::probe::{
    DEFAULT_FAILURE_THRESHOLD, DEFAULT_PROBE_PERIOD, DEFAULT_PROBE_TIMEOUT,
    LEAST_PUBLISHED_PROBE_TIMEOUT, Probe, ProbeCheck,
};
pub use crate::plan::restart::{
    Backoff, BadBackoff, DEFAULT_FIRST_BACKOFF, DEFAULT_MOST_BACKOFF, DEFAULT_STOP_GRACE,
    LimitsWithoutRestart, Resolve, Restart, RestartLimits, Stop, StopSignal,
};
pub use crate::plan::{
    AppPlan, ContainerProblem, DEFAULT_READY_TIMEOUT, MOST_READY_TIMEOUT, MOST_STOP_GRACE,
    OnceWithoutOneshot, Permissions, Plan, PlanError, ProcessKind, ProcessPlan, ProcessProblem,
    RunnerKind, SecretDelivery, SourceProblem,
};
pub use crate::record::{
    BadContainerId, BadDockerName, ContainerId, DockerCause, DockerError, DockerName, Handle,
    MOST_DOCKER_MESSAGE_BYTES, MOST_DOCKER_NAME_BYTES, ObjectKind, Role, SweepOutcome, Swept,
    SweptItem, SweptObject,
};
pub use crate::report::{AppOutcome, AppReport, Note, Report, StartProblem};
pub use crate::select::{BadTarget, Selection, Target};
pub use crate::source::{
    BadSourceText, BuildSpec, GithubRepository, ImageReference, MOST_IMAGE_BYTES,
    MOST_REPOSITORY_BYTES, MOST_VERSION_BYTES, ReleaseSpec, Source, Verification, Version,
};
pub use crate::status::{
    LastExit, ProcessState, SourceSummary, Status, StatusRow, StatusSchema, UnknownReason,
    WorkspaceState,
};
