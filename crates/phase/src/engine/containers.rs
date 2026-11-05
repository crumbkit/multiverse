//! The seam through which `up`, `down` and `restart` run container apps.
//! The engine calls [`ContainerRunner`], and the container module's
//! `ComposeRunner` implements it with Docker Compose.

use std::time::Instant;

use crate::cancel::Cancel;
use crate::env::EnvironmentSource;
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::AppPlan;
use crate::record::run::{ContainerEntry, NetworkId, PlanHash, VolumeLifetime};
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
    /// It reports every container that it created, even when a failure
    /// ended the start. [`ContainersStarted::failure`] holds the failure
    /// that kept Docker from acting, such as [`PhaseError::Docker`], and
    /// [`ContainersStarted::removed`] the containers that the start removed
    /// after it. A process that started and did not become ready is no
    /// failure, and [`ContainersStarted::problem`] names it.
    ///
    /// Just before it removes or creates the container of a process, it
    /// calls `launching` with the process, which writes the process's
    /// entry back to planned with no id. A failure before that, such as a
    /// pull or an environment that cannot be resolved, leaves the entries
    /// of the running containers as they were. A failure of `launching`
    /// ends the start as [`ContainersStarted::failure`].
    fn start(
        &self,
        start: &ContainerStart<'_>,
        launching: &mut dyn FnMut(&ProcessName) -> Result<(), PhaseError>,
    ) -> ContainersStarted;

    /// Returns the processes of `containers`, containers of `app` of
    /// `workspace` that the run record names by id, whose container still
    /// runs as `app` plans it: Docker has the recorded id, the container's
    /// labels name the workspace, app and process, it is running, and the
    /// record's plan hash is the hash of its part of `app`. `up` leaves
    /// those containers alone and reports them as already running, and
    /// replaces the others.
    ///
    /// # Errors
    ///
    /// Returns the failure that kept Docker from answering, such as
    /// [`PhaseError::Docker`].
    fn running(
        &self,
        workspace: &WorkspaceId,
        app: &AppPlan,
        containers: &[ContainerEntry],
    ) -> Result<Vec<ProcessName>, PhaseError>;

    /// Stops and removes the containers of `stop.containers`, in reverse
    /// order, until `stop.deadline`, and returns what happened to each, by
    /// process. A failure that kept Docker from acting on one container,
    /// such as [`PhaseError::Docker`], is that container's result, and the
    /// stop goes on to the next.
    fn stop(
        &self,
        stop: &ContainerStop<'_>,
    ) -> Vec<(ProcessName, Result<ContainerStopped, PhaseError>)>;
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
pub(crate) struct ContainerStart<'a> {
    /// The workspace.
    pub(crate) workspace: &'a WorkspaceId,
    /// The app.
    pub(crate) app: &'a AppPlan,
    /// The processes to start, in dependency order.
    pub(crate) processes: &'a [ProcessName],
    /// The source of each process's environment.
    pub(crate) env: &'a dyn EnvironmentSource,
    /// When pulling the app's image must have finished.
    pub(crate) build: Instant,
    /// When every process must be ready.
    pub(crate) deadline: Instant,
    /// The flag that asks the start to stop.
    pub(crate) cancel: &'a Cancel,
}

/// What [`ContainerRunner::start`] started.
#[derive(Debug, Default)]
pub(crate) struct ContainersStarted {
    /// Each container that the start created.
    pub(crate) containers: Vec<StartedContainer>,
    /// Each network that exists, by name.
    pub(crate) networks: Vec<(DockerName, NetworkId)>,
    /// Each volume that exists, by name.
    pub(crate) volumes: Vec<DockerName>,
    /// The process that failed to become ready, and why, or `None` when
    /// every process is ready.
    pub(crate) problem: Option<(ProcessName, StartProblem)>,
    /// The process whose wait a deadline cut short, so phase does not know
    /// whether it is ready, or `None`.
    pub(crate) uncertain: Option<ProcessName>,
    /// The failure that kept Docker from acting, which ended the start, or
    /// `None`.
    pub(crate) failure: Option<PhaseError>,
    /// The processes of [`Self::containers`] whose container the start
    /// removed after [`Self::failure`], and which Docker no longer has.
    pub(crate) removed: Vec<ProcessName>,
}

/// A container that [`ContainerRunner::start`] created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartedContainer {
    /// The process.
    pub(crate) process: ProcessName,
    /// The container's id.
    pub(crate) id: ContainerId,
    /// The hash of the process's part of the plan, which the run record
    /// keeps for [`ContainerRunner::running`].
    pub(crate) plan_hash: PlanHash,
}

/// What [`ContainerRunner::stop`] stops.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContainerStopped {
    /// Docker removed it, and phase checked that it is gone.
    Stopped,
    /// It was already gone.
    AlreadyGone,
    /// It was still there after Docker's removal.
    Unstoppable,
}
