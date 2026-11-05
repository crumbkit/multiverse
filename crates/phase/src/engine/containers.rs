//! The seam through which `up`, `down` and `restart` run container apps.
//! The engine calls [`ContainerRunner`], and the container module supplies
//! the implementation that runs Docker Compose when it lands. Until then
//! [`Unbuilt`] refuses every container app with
//! [`PhaseError::NotRunnable`], and the engine reports that app as errored.

use std::time::Instant;

use crate::cancel::Cancel;
use crate::env::EnvironmentSource;
use crate::error::{NotRunnable, PhaseError};
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::AppPlan;
use crate::record::run::{ContainerEntry, NetworkId, VolumeLifetime};
use crate::record::{ContainerId, DockerName};
use crate::report::StartProblem;

/// What the engine asks of Docker to run container apps.
///
/// When the caller cancels an operation, or a failure ends it, the engine
/// calls [`ContainerRunner::stop`] for the containers that this operation
/// started and recorded, as it stops the host processes it started. A
/// container app whose start reports a problem has its started containers
/// stopped in the same way. The engine leaves networks and volumes to
/// `sweep`.
pub(crate) trait ContainerRunner {
    /// Returns the Docker objects that starting `app` of `workspace`
    /// creates, which `up` records as planned before it starts anything.
    ///
    /// # Errors
    ///
    /// Returns the failure that keeps phase from running `app`, such as
    /// [`PhaseError::NotRunnable`].
    fn objects(
        &self,
        workspace: &WorkspaceId,
        app: &AppPlan,
    ) -> Result<ContainerObjects, PhaseError>;

    /// Starts the processes of `start.app` that `start.processes` names,
    /// resolving each one's environment through `start.env`, and waits for
    /// each to be ready, until `start.deadline` or `start.cancel`.
    ///
    /// # Errors
    ///
    /// Returns the failure that kept Docker from acting, such as
    /// [`PhaseError::Docker`]. A process that started and did not become
    /// ready is no error, and [`ContainersStarted::problem`] names it.
    fn start(&self, start: &ContainerStart<'_>) -> Result<ContainersStarted, PhaseError>;

    /// Stops and removes the containers of `stop.containers`, until
    /// `stop.deadline`, and returns what happened to each, by process.
    ///
    /// # Errors
    ///
    /// Returns the failure that kept Docker from acting, such as
    /// [`PhaseError::Docker`].
    fn stop(
        &self,
        stop: &ContainerStop<'_>,
    ) -> Result<Vec<(ProcessName, ContainerStopped)>, PhaseError>;
}

/// The Docker objects that a container app creates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ContainerObjects {
    /// Each process's container, by the name phase gives it.
    pub(crate) containers: Vec<(ProcessName, DockerName)>,
    /// The networks.
    pub(crate) networks: Vec<DockerName>,
    /// The volumes, with their lifetimes.
    pub(crate) volumes: Vec<(DockerName, VolumeLifetime)>,
}

/// What [`ContainerRunner::start`] starts.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the container module reads this when it lands, and until then only the tests' runner does"
    )
)]
pub(crate) struct ContainerStart<'a> {
    /// The workspace.
    pub(crate) workspace: &'a WorkspaceId,
    /// The app.
    pub(crate) app: &'a AppPlan,
    /// The processes to start, in dependency order.
    pub(crate) processes: &'a [ProcessName],
    /// The source of each process's environment.
    pub(crate) env: &'a dyn EnvironmentSource,
    /// When every process must be ready.
    pub(crate) deadline: Instant,
    /// The flag that asks the start to stop.
    pub(crate) cancel: &'a Cancel,
}

/// What [`ContainerRunner::start`] started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ContainersStarted {
    /// Each container that started, by process.
    pub(crate) containers: Vec<(ProcessName, ContainerId)>,
    /// Each network that exists, by name.
    pub(crate) networks: Vec<(DockerName, NetworkId)>,
    /// Each volume that exists, by name.
    pub(crate) volumes: Vec<DockerName>,
    /// The process that failed to become ready, and why, or `None` when
    /// every process is ready.
    pub(crate) problem: Option<(ProcessName, StartProblem)>,
}

/// What [`ContainerRunner::stop`] stops.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the container module reads this when it lands, and until then only the tests' runner does"
    )
)]
pub(crate) struct ContainerStop<'a> {
    /// The workspace.
    pub(crate) workspace: &'a WorkspaceId,
    /// The app.
    pub(crate) app: &'a AppName,
    /// The containers of the app that the run record names.
    pub(crate) containers: &'a [ContainerEntry],
    /// When the stop gives up waiting.
    pub(crate) deadline: Instant,
}

/// How a container's stop ended.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the container module reports these when it lands, and until then only the tests' runner does"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContainerStopped {
    /// Docker removed it, and phase checked that it is gone.
    Stopped,
    /// It was already gone.
    AlreadyGone,
    /// It was still there after Docker's removal.
    Unstoppable,
}

/// The container runner until the container module lands. It refuses
/// every container app.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Unbuilt;

impl ContainerRunner for Unbuilt {
    fn objects(
        &self,
        _workspace: &WorkspaceId,
        app: &AppPlan,
    ) -> Result<ContainerObjects, PhaseError> {
        Err(not_runnable(&app.name))
    }

    fn start(&self, start: &ContainerStart<'_>) -> Result<ContainersStarted, PhaseError> {
        Err(not_runnable(&start.app.name))
    }

    fn stop(
        &self,
        stop: &ContainerStop<'_>,
    ) -> Result<Vec<(ProcessName, ContainerStopped)>, PhaseError> {
        Err(not_runnable(stop.app))
    }
}

/// Returns the failure of running the container app `app`.
fn not_runnable(app: &AppName) -> PhaseError {
    PhaseError::NotRunnable {
        app: app.clone(),
        reason: NotRunnable::Containers,
    }
}
