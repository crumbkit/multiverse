//! The seam through which `up`, `down` and `restart` run container apps.
//! The engine calls [`ContainerRunner`], and the container module's
//! `ComposeRunner` implements it with Docker Compose.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::cancel::Cancel;
use crate::env::EnvironmentSource;
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::AppPlan;
use crate::record::run::{ContainerEntry, NetworkId, PlanHash, Ready, VolumeLifetime, VolumeOwner};
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
    /// calls `before` with [`Before::Container`], which writes the
    /// process's entry back to planned with no id. A failure before that,
    /// such as a pull or an environment that cannot be resolved, leaves
    /// the entries of the running containers as they were. Just before it
    /// changes the owner of a volume, it calls `before` with
    /// [`Before::VolumeOwner`], which records the volume with no owner, so
    /// a run that ends while the owner changes leaves a record that no
    /// longer says which owner the volume has. A failure of `before` ends
    /// the start as [`ContainersStarted::failure`].
    fn start(
        &self,
        start: &ContainerStart<'_>,
        before: &mut dyn FnMut(Before<'_>) -> Result<(), PhaseError>,
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

    /// Writes the secret files of the running container of
    /// `rewrite.entry` again from a fresh environment of its process, which
    /// it resolves through `rewrite.env`, until `rewrite.deadline` or
    /// `rewrite.cancel`. It writes the file of each secret that has a
    /// value and removes the file of each optional secret that has lost
    /// its value, and the container keeps running. The files belong to the
    /// owner that the container was created with. `up` calls it for each
    /// container that [`ContainerRunner::running`] reports, so each `up`
    /// keeps the files of a running container current.
    ///
    /// It inspects the container first, and resolves the environment only
    /// when a write or a replacement may follow:
    ///
    /// It compares the recorded plan hash with the hash of the plan and
    /// the keyed digest of the values of the fresh environment's variables
    /// that are not secrets, so a secret's value never asks for a
    /// replacement.
    ///
    /// - [`Rewritten::NoFiles`] for a process that receives no secret
    ///   files and whose hash is unchanged, and for a container without
    ///   secret files whose fresh environment has no value for any of its
    ///   optional secrets.
    /// - [`Rewritten::Replace`] for a container that is gone, no longer
    ///   runs, or has secret files and was restarted by Docker, as
    ///   [`ContainerRunner::running`] judges it, for a container whose hash
    ///   differs, for a process without secret files whose environment
    ///   cannot be resolved, so that the start reports the failure and
    ///   leaves the container running, and for a container without secret
    ///   files whose fresh environment has a value for one of its secrets.
    ///   Such a container started while every one of its secrets was
    ///   optional and absent, so it has no tmpfs and no wrapper. `up` then
    ///   replaces the container.
    /// - [`Rewritten::Kept`] when the environment cannot be resolved, a
    ///   value cannot be read, or Docker refuses the inspection or the
    ///   write while the container still runs. The container keeps its
    ///   earlier files whole, apart from a lapsed optional secret's file
    ///   whose removal failed, which the reason names.
    /// - [`Rewritten::Written`] otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Docker`] when the caller cancels, with the
    /// cancelled cause, and when Docker itself cannot be reached or does
    /// not answer in time: no `docker` program, a daemon that does not
    /// answer, or a command that missed its deadline.
    /// Returns [`PhaseError::StateUnwritable`] when phase cannot create or
    /// use the key of the digest. `up` then reports the app as errored,
    /// because phase cannot tell what the container holds.
    fn rewrite(&self, rewrite: &ContainerRewrite<'_>) -> Result<Rewritten, PhaseError>;

    /// Stops and removes the containers of `stop.containers`, in reverse
    /// order, until `stop.deadline`, and returns what happened to each, by
    /// process. A failure that kept Docker from acting on one container,
    /// such as [`PhaseError::Docker`], is that container's result, and the
    /// stop goes on to the next.
    fn stop(
        &self,
        stop: &ContainerStop<'_>,
    ) -> Vec<(ProcessName, Result<ContainerStopped, PhaseError>)>;

    /// Returns when Docker last started each of `containers`, containers
    /// of the app `app` of `workspace` that the run record names by id,
    /// exactly as `docker container inspect` printed `State.StartedAt`. A
    /// container that Docker no longer has, or that no longer carries the
    /// app's labels, is left out. Every start of a container changes it,
    /// including one under Docker's restart policy and one that phase did
    /// not ask for.
    ///
    /// # Errors
    ///
    /// Returns the failure that kept Docker from answering, such as
    /// [`PhaseError::Docker`].
    fn started_at(
        &self,
        workspace: &WorkspaceId,
        app: &AppName,
        containers: &[ContainerEntry],
    ) -> Result<Vec<(ProcessName, String)>, PhaseError>;
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

/// A step of [`ContainerRunner::start`] that the run record shows before
/// the step acts.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Before<'a> {
    /// The start is about to remove or create the container of this
    /// process.
    Container(&'a ProcessName),
    /// The start is about to change the owner of this volume, which
    /// exists.
    VolumeOwner(&'a DockerName),
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
    /// The processes of `app` that `up` leaves running. The start checks
    /// the owner of each volume that they mount and that no process of
    /// `processes` mounts.
    pub(crate) running: &'a [ProcessName],
    /// The owner that the run record says phase wanted for each volume,
    /// by the volume's name. The start gives a volume of a running process
    /// that owner, because a tag that has moved to an image with another
    /// `USER` does not change the user of a running container. The record
    /// is no proof of the owner the volume has, so the start checks it.
    pub(crate) owners: &'a BTreeMap<DockerName, VolumeOwner>,
}

/// What [`ContainerRunner::start`] started.
#[derive(Debug, Default)]
pub(crate) struct ContainersStarted {
    /// Each container that the start created.
    pub(crate) containers: Vec<StartedContainer>,
    /// Each network that exists, by name.
    pub(crate) networks: Vec<(DockerName, NetworkId)>,
    /// Each volume that exists, by name, with the owner that the start
    /// gave its root directory or that it kept from the run record for a
    /// volume it had no reason to touch, or `None` when no owner holds.
    pub(crate) volumes: Vec<(DockerName, Option<VolumeOwner>)>,
    /// Why the owner of a volume of a running process could not be
    /// checked, or `None`. It is no failure: the container keeps running
    /// and the next `up` checks again.
    pub(crate) owners_unchecked: Option<String>,
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

impl ContainersStarted {
    /// Records that the container of `process` became ready after
    /// `restarts` restarts by Docker, in the start that Docker printed as
    /// `started_at`. The readiness wait counts a restart by Docker as an
    /// end, so these are the restarts and the start that the container had
    /// when the start inspected it. A start that docker did not print
    /// leaves the readiness without one, which `status` counts as not
    /// ready.
    pub(crate) fn ready(&mut self, process: &ProcessName, restarts: u32, started_at: &str) {
        let container = self
            .containers
            .iter_mut()
            .rfind(|container| &container.process == process);
        debug_assert!(
            container.is_some(),
            "a start observed {process} ready before it recorded its container"
        );
        if let Some(container) = container {
            let ready = Ready::now(restarts);
            container.ready = Some(if started_at.is_empty() {
                ready
            } else {
                ready.with_container_start(started_at.to_owned())
            });
        }
    }
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
    /// When the start observed the container ready, with the restarts
    /// that Docker had counted then, or `None` when it did not.
    pub(crate) ready: Option<Ready>,
}

/// What [`ContainerRunner::rewrite`] rewrites.
pub(crate) struct ContainerRewrite<'a> {
    /// The workspace.
    pub(crate) workspace: &'a WorkspaceId,
    /// The app.
    pub(crate) app: &'a AppPlan,
    /// The run record's entry of the running container, which names it
    /// by id.
    pub(crate) entry: &'a ContainerEntry,
    /// The source of the process's environment.
    pub(crate) env: &'a dyn EnvironmentSource,
    /// When the rewrite gives up.
    pub(crate) deadline: Instant,
    /// The flag that asks the rewrite to stop.
    pub(crate) cancel: &'a Cancel,
}

/// How [`ContainerRunner::rewrite`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Rewritten {
    /// The container has no secret files and needs none, so nothing was
    /// written. phase resolves no environment for a process that receives
    /// no secret files.
    NoFiles,
    /// The container's secret files hold the fresh environment's values.
    Written {
        /// How many files the rewrite wrote.
        files: usize,
        /// How many files of lapsed optional secrets it removed.
        removed: usize,
    },
    /// The container cannot receive the files that the fresh environment
    /// has values for, or no longer runs as planned, so `up` must replace
    /// it.
    Replace,
    /// The rewrite failed and the container still runs with its earlier
    /// files.
    Kept {
        /// Why, which holds no value.
        reason: String,
    },
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
