//! The container runner of `up`, `down` and `restart`, which runs each
//! container app of a plan as one compose project through the `docker`
//! program and its `compose` plugin.
//!
//! [`super::app`] names the project's objects and writes its compose
//! file, which `docker compose up` reads from standard input, so the file
//! never reaches the disk. `start` checks that docker and its compose
//! plugin answer, pulls the app's image unless docker has it, within the
//! build deadline, creates the workspace's volumes that the app mounts,
//! and then starts the selected processes one at a time, in dependency
//! order. For each process it resolves the process's environment, has the
//! caller write the process's entry back to planned, removes a container
//! that holds the process's name and carries the app's labels,
//! checks that the process's published host ports are free, and runs
//! `docker compose up` with the values of the process's variables on that
//! child alone, each under its private reference. The pull and the
//! resolution come before any removal, so a failure in either leaves the
//! old containers running. A process that receives secrets as files then
//! has each one written into its container's tmpfs through `docker exec`
//! with the value on standard input, as [`super::secrets`] says, and a
//! write that fails ends the start. It then waits for the container to be
//! ready through the probe runner before it starts the next process. A pull,
//! `docker compose up` and a removal stop when the caller cancels.
//!
//! - An `http` probe asks the published port on 127.0.0.1.
//! - A `tcp` probe counts the published port as answering only when a
//!   connection stays open for 300 ms or the container sends data, because
//!   Docker's port proxy accepts a connection before anything in the
//!   container listens.
//! - A `command` probe runs inside the container through `docker exec`.
//! - A `log` probe reads the container's output through `docker logs
//!   --timestamps`, at most once each [`LOG_INTERVAL`], and each read asks
//!   only for the lines since the last line it handed over.
//! - A process without a probe is watched for the start grace, and a
//!   container that exits meanwhile fails to start.
//!
//! While it waits, `start` reads the container's state at most once each
//! [`STATE_INTERVAL`]. A container that docker no longer has, that exited,
//! that Docker is restarting, or that Docker has restarted since it was
//! created has ended, so a container that crashes in a loop under its
//! restart policy fails to start. A process that does not become ready
//! within its ready timeout fails as not ready. When the caller's deadline
//! cuts the wait short first, the start is uncertain.
//!
//! `running` tells `up` which recorded containers still run as the plan
//! asks: Docker has the recorded id, the container's labels name the
//! workspace, app and process, Docker's status is exactly `running`, and
//! the recorded plan hash is the hash of the process's service in the
//! plan. A container with secret files that Docker has restarted does not
//! count, because the restart emptied its tmpfs. `up` leaves those alone
//! and asks `start` only for the others.
//!
//! When a docker command fails after some containers started, `start`
//! removes those containers, and reports each container that it created
//! and each that Docker no longer has, so the run record names every
//! container that may still run.
//!
//! `stop` takes the containers in reverse of the record's order, which is
//! dependency order. It finds each container by its recorded id, or by its
//! name when the record holds no id yet, and acts on it only when its
//! labels name the workspace, app and process. It turns off the
//! container's restart policy and reads its status again. It stops a
//! running or paused container with the stop signal that the compose file
//! set, waits the container's stop grace or until the deadline, whichever
//! comes first, removes it, and checks that docker no longer has it. It
//! removes any other container at once. A failure on one container is that
//! container's result, and `stop` goes on to the next. Volumes stay.

use std::cell::Cell;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::cancel::Cancel;
use crate::clock::now;
use crate::engine::container::ContainerError;
use crate::engine::container::app::{self, reference};
use crate::engine::container::command::{
    DockerExec, MOST_OUTPUT_BYTES, Referenced, check, check_compose, message, quick, write_secret,
};
use crate::engine::container::compose::{Up, app_up};
use crate::engine::container::names::{self, Identity, Scope};
use crate::engine::container::objects::{
    self, ContainerState, Inspected, ensure_image, ensure_volume, inspect_id, inspect_named,
    no_restart, reinspect, remove, stop, stop_until,
};
use crate::engine::container::secrets;
use crate::engine::containers::{
    ContainerObjects, ContainerRunner, ContainerStart, ContainerStop, ContainerStopped,
    ContainersStarted, StartedContainer,
};
use crate::engine::probe::{self, Check, Goal, LogLines, LogWatch, Outcome};
use crate::engine::run::watch;
use crate::env::AppliedEnvironment;
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::probe::{LogPattern, ProbeCheck};
use crate::plan::restart::DEFAULT_STOP_GRACE;
use crate::plan::{AppPlan, ProcessPlan};
use crate::record::docker::DockerError;
use crate::record::run::ContainerEntry;
use crate::record::store::RunStore;
use crate::record::{ContainerId, DockerName};
use crate::report::StartProblem;
use crate::source::Source;

#[cfg(test)]
mod tests;

/// The shortest time between two reads of a waiting container's state.
pub(crate) const STATE_INTERVAL: Duration = Duration::from_millis(250);

/// The shortest time between two reads of a waiting container's output for
/// a log probe.
pub(crate) const LOG_INTERVAL: Duration = Duration::from_millis(100);

/// The most lines of a container's output that a report holds when the
/// container exits before it is ready.
const TAIL_LINES: usize = 20;

/// The container runner that runs the `docker` program found on `PATH`.
#[derive(Clone, Debug)]
pub(crate) struct ComposeRunner {
    /// The directory of phase's state, which holds the run records whose
    /// paths label the containers, and the empty project directory of
    /// every compose project.
    state_dir: PathBuf,
    /// How long `start` watches a process without a readiness probe.
    start_grace: Duration,
}

impl ComposeRunner {
    /// Returns the runner of the phase whose state is in `state_dir` and
    /// which watches a process without a probe for `start_grace`.
    pub(crate) const fn new(state_dir: PathBuf, start_grace: Duration) -> Self {
        Self {
            state_dir,
            start_grace,
        }
    }

    /// Returns the scope of `app` in `workspace`.
    fn scope(&self, workspace: &WorkspaceId, app: &AppName) -> Scope {
        Scope::app(
            Identity {
                id: workspace.clone(),
                run_record: RunStore::new(&self.state_dir).path(workspace),
            },
            app,
        )
    }

    /// Returns the project directory of every compose project, which holds
    /// no file, after creating it.
    fn project_dir(&self) -> Result<PathBuf, PhaseError> {
        let dir = self.state_dir.join("compose");
        std::fs::create_dir_all(&dir).map_err(|source| PhaseError::StateUnwritable {
            path: dir.clone(),
            source,
        })?;
        Ok(dir)
    }
}

/// Returns the failure of a container step as phase reports it: Docker
/// did not answer when its program is missing or its daemon does not
/// answer, and it refused otherwise. The message holds no value of a
/// variable, because no value is ever an argument, a compose file entry or
/// a label.
pub(crate) fn docker_error(error: &ContainerError) -> PhaseError {
    let source = match error {
        ContainerError::Missing { .. } | ContainerError::Unavailable { .. } => {
            DockerError::unavailable(error.to_string())
        }
        _ => DockerError::refused(error.to_string()),
    };
    PhaseError::Docker { source }
}

/// Returns the record's id of the container that `inspected` names.
fn record_id(inspected: &Inspected) -> Result<ContainerId, PhaseError> {
    ContainerId::try_from(inspected.container.id().as_str().to_owned()).map_err(|_| {
        docker_error(&ContainerError::failed(
            "container inspect",
            Some(inspected.container.service().as_str()),
            "docker printed a container id that is not 64 hex digits",
        ))
    })
}

/// Returns the first of the published host ports of `process` that
/// something on 127.0.0.1 already holds, with its name.
fn taken_port(app: &AppPlan, process: &ProcessPlan) -> Option<crate::name::PortName> {
    process.publish.keys().find_map(|port| {
        let host = app.ports.get(port)?.get();
        let free = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, host))).is_ok();
        (!free).then(|| port.clone())
    })
}

/// Returns the name of the published port of `process` whose host port is
/// `number`, or its first published port when docker named none.
fn port_named(
    app: &AppPlan,
    process: &ProcessPlan,
    number: Option<u16>,
) -> Option<crate::name::PortName> {
    process
        .publish
        .keys()
        .find(|port| {
            number.is_some_and(|number| app.ports.get(*port).map(|host| host.get()) == Some(number))
        })
        .or_else(|| process.publish.keys().next())
        .cloned()
}

/// How the wait for one container ended.
enum Readiness {
    /// The container is ready.
    Ready,
    /// The caller cancelled the wait.
    Cancelled,
    /// A deadline of the operation cut the wait short, so phase does not
    /// know whether the container would have become ready.
    Uncertain,
    /// The container failed to become ready.
    Failed(StartProblem),
}

/// What ended a start before every process was ready.
enum Halt {
    /// The caller cancelled the start.
    Cancelled,
    /// Docker, or a step around it, failed.
    Failed(PhaseError),
}

impl From<PhaseError> for Halt {
    fn from(error: PhaseError) -> Self {
        Self::Failed(error)
    }
}

/// Returns how a container step that failed with `error` ends the start.
fn halt(error: &ContainerError) -> Halt {
    match error {
        ContainerError::Cancelled { .. } => Halt::Cancelled,
        _ => Halt::Failed(docker_error(error)),
    }
}

/// Returns the time from now until `deadline`, or zero once it has passed.
fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(now())
}

/// Returns the stop grace of `container`, as it was created with, or
/// Docker's default.
fn own_grace(container: &crate::engine::container::OwnedContainer) -> Duration {
    objects::stop_timeout(container).map_or(DEFAULT_STOP_GRACE, Duration::from_secs)
}

impl ComposeRunner {
    /// Starts the processes of `start` into `started`, as the module docs
    /// say, and returns what ended the start early.
    fn start_into(
        &self,
        start: &ContainerStart<'_>,
        launching: &mut dyn FnMut(&ProcessName) -> Result<(), PhaseError>,
        started: &mut ContainersStarted,
    ) -> Result<(), Halt> {
        let app = start.app;
        let scope = self.scope(start.workspace, &app.name);
        check().map_err(|error| halt(&error))?;
        check_compose().map_err(|error| halt(&error))?;
        let cleared = app::references(app);
        let dir = self.project_dir()?;
        // The image is pulled before any container is removed, so a pull
        // that fails leaves the old containers running.
        let mut image_command = None;
        if let Source::Image { image } = &app.source {
            ensure_image(image.as_str(), left(start.build), start.cancel)
                .map_err(|error| halt(&error))?;
            let has_files = app
                .processes
                .iter()
                .any(|process| app::file_secrets(app, process).next().is_some());
            if has_files {
                image_command =
                    Some(objects::image_command(image.as_str()).map_err(|error| halt(&error))?);
            }
        }
        let file = app::compose_file(&scope, app, image_command.as_ref())?;
        for key in app::volume_keys(app) {
            ensure_volume(&scope, &key).map_err(|error| halt(&error))?;
            let name: DockerName = scope.volume_name(&key).as_str().parse().map_err(|_| {
                docker_error(&ContainerError::failed(
                    "volume create",
                    None,
                    format!("{} is not a Docker name", scope.volume_name(&key)),
                ))
            })?;
            started.volumes.push(name);
        }
        for name in start.processes {
            if start.cancel.is_cancelled() {
                return Err(Halt::Cancelled);
            }
            let Some(process) = app.processes.iter().find(|process| &process.name == name) else {
                continue;
            };
            let service = app::service(app, name)?;
            let plan_hash = app::plan_hash(&scope, app, name)?;
            // The environment is resolved before the old container is
            // removed, so a resolver that fails leaves it running.
            let environment =
                start
                    .env
                    .resolve(&app.name, name, start.deadline)
                    .map_err(|source| PhaseError::Environment {
                        app: app.name.clone(),
                        source,
                    })?;
            launching(name)?;
            if let Some(existing) = inspect_named(&scope, &service).map_err(|error| halt(&error))? {
                let grace = own_grace(&existing.container).min(left(start.deadline));
                discard_until(&existing, grace, start.cancel)?;
            }
            if let Some(port) = taken_port(app, process) {
                started.problem = Some((name.clone(), StartProblem::port_taken(port, Vec::new())));
                return Ok(());
            }
            let remaining = left(start.deadline);
            if remaining.is_zero() {
                started.uncertain = Some(name.clone());
                return Ok(());
            }
            let refer = |variable: &str| reference(app, name, variable);
            let up = app_up(
                &scope,
                &dir,
                &file,
                &service,
                Referenced {
                    environment: environment.as_ref(),
                    reference: &refer,
                    cleared: &cleared,
                },
                (remaining, start.cancel),
            );
            // `compose up` may have created the container before it failed,
            // so the container is reported, and stopped, whatever it said.
            let inspected = inspect_named(&scope, &service).map_err(|error| halt(&error))?;
            if let Some(inspected) = &inspected {
                started.containers.push(StartedContainer {
                    process: name.clone(),
                    id: record_id(inspected)?,
                    plan_hash,
                });
            }
            match up {
                Ok(Up::Started) => {}
                Ok(Up::PortAllocated(number)) => {
                    if let Some(port) = port_named(app, process, number) {
                        started.problem =
                            Some((name.clone(), StartProblem::port_taken(port, Vec::new())));
                        return Ok(());
                    }
                    return Err(Halt::Failed(docker_error(&ContainerError::failed(
                        "compose up",
                        Some(service.as_str()),
                        "docker reported a port that the process does not publish as allocated",
                    ))));
                }
                Err(error @ ContainerError::Cancelled { .. }) => return Err(halt(&error)),
                // The ready deadline stopped `compose up`, so phase does
                // not know whether the container would have started.
                Err(_) if now() >= start.deadline => {
                    started.uncertain = Some(name.clone());
                    return Ok(());
                }
                Err(error) => return Err(halt(&error)),
            }
            let Some(inspected) = inspected else {
                started.problem =
                    Some((name.clone(), StartProblem::exited(None, None, Vec::new())));
                return Ok(());
            };
            // The secrets are written before the readiness wait, and a
            // write that fails ends the start, which removes the container.
            write_secrets(
                app,
                process,
                &inspected,
                environment.as_ref(),
                (start.deadline, start.cancel),
            )
            .map_err(|error| halt(&error))?;
            drop(environment);
            match self.ready(app, process, &inspected, start) {
                Readiness::Ready => {}
                Readiness::Cancelled => return Err(Halt::Cancelled),
                Readiness::Uncertain => {
                    started.uncertain = Some(name.clone());
                    return Ok(());
                }
                Readiness::Failed(problem) => {
                    started.problem = Some((name.clone(), problem));
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Waits until the container `inspected` of `process` is ready, as the
    /// module docs say.
    fn ready(
        &self,
        app: &AppPlan,
        process: &ProcessPlan,
        inspected: &Inspected,
        start: &ContainerStart<'_>,
    ) -> Readiness {
        let began = now();
        let own_deadline = began + process.ready_timeout;
        let deadline = own_deadline.min(start.deadline);
        let id = inspected.container.id().clone();
        let watcher = Watcher::new(inspected);
        let mut ended = || watcher.ended();
        let outcome = match &process.ready {
            None => {
                let grace_end = began + self.start_grace;
                match watch(grace_end.min(deadline), &mut ended, start.cancel) {
                    // The ready deadline cut the watch short, so the
                    // container may still end within its start grace.
                    Outcome::Ready if start.deadline < grace_end => return Readiness::Uncertain,
                    outcome => outcome,
                }
            }
            Some(probe) => {
                let exec = DockerExec { id: id.clone() };
                let mut logs = DockerLogs::new(id.clone());
                let port = |port: Option<&crate::name::PortName>| {
                    port.and_then(|port| app.ports.get(port))
                        .map(|port| port.get())
                };
                let mut check = match &probe.check {
                    ProbeCheck::Http { path } => match port(process.port.as_ref()) {
                        Some(port) => Check::Http { port, path },
                        None => return Readiness::Failed(StartProblem::not_ready(Duration::ZERO)),
                    },
                    ProbeCheck::Tcp { port: name } => match port(Some(name)) {
                        Some(port) => Check::Published { port },
                        None => return Readiness::Failed(StartProblem::not_ready(Duration::ZERO)),
                    },
                    ProbeCheck::Command { run } => Check::Command { run, exec: &exec },
                    ProbeCheck::Log { pattern } => match LogPattern::new(pattern) {
                        Ok(pattern) => Check::Log {
                            watch: LogWatch::new(pattern),
                            lines: &mut logs,
                        },
                        Err(_) => {
                            return Readiness::Failed(StartProblem::not_ready(Duration::ZERO));
                        }
                    },
                };
                probe::wait(
                    probe,
                    &mut check,
                    Goal::Ready,
                    deadline,
                    &mut ended,
                    start.cancel,
                )
            }
        };
        match outcome {
            Outcome::Ready => Readiness::Ready,
            Outcome::Cancelled => Readiness::Cancelled,
            Outcome::Ended => {
                Readiness::Failed(StartProblem::exited(watcher.code(), None, tail(&id)))
            }
            // The ready deadline of the operation, which comes before the
            // process's own ready timeout, cut the wait short.
            Outcome::NotReady { .. } | Outcome::Failing { .. } if own_deadline > deadline => {
                Readiness::Uncertain
            }
            Outcome::NotReady { waited, .. } => Readiness::Failed(StartProblem::not_ready(waited)),
            Outcome::Failing { .. } => Readiness::Failed(StartProblem::not_ready(now() - began)),
        }
    }
}

/// Writes each secret of `process` of `app` that it receives as a file
/// into its container `inspected`, from `environment`, until the deadline
/// or the cancel flag of `until`. Each write replaces the file whole, so
/// writing again with a fresh environment rotates the secrets of a
/// running container without restarting it.
///
/// # Errors
///
/// Returns [`ContainerError::Failed`] when the environment holds no value
/// of a secret or a write fails, [`ContainerError::Cancelled`], and the
/// errors of checking that docker answers. No error holds a value.
pub(crate) fn write_secrets(
    app: &AppPlan,
    process: &ProcessPlan,
    inspected: &Inspected,
    environment: &dyn AppliedEnvironment,
    until: (Instant, &Cancel),
) -> Result<(), ContainerError> {
    let (deadline, cancel) = until;
    let service = inspected.container.service().as_str();
    let names = environment.names();
    for name in app::file_secrets(app, process) {
        if !names.contains(name) {
            return Err(ContainerError::failed(
                "secret write",
                Some(service),
                format!("the environment source supplied no value for the secret {name}"),
            ));
        }
        let args = secrets::write_args(inspected.container.id().as_str(), name);
        write_secret(&args, environment, name, left(deadline), cancel)?
            .checked("secret write", Some(service))?;
    }
    Ok(())
}

/// Turns off Docker's restart policy of `container` and returns the
/// container as docker then has it, or `None` when docker no longer has it.
/// Docker restarts no container after this, so a container that crashes in
/// a loop holds still for its stop, and its status is current.
fn settle(
    container: &crate::engine::container::OwnedContainer,
) -> Result<Option<Inspected>, ContainerError> {
    let updated = no_restart(container);
    settled(updated, reinspect(container)?, |fresh| fresh.state.runs())
}

/// Returns `fresh`, the container as docker has it after `updated`, the
/// update of its restart policy. Docker refuses to update a container that
/// has stopped, such as one between two restarts, and `docker rm --force`
/// removes that container anyway, so a refused update fails only while the
/// container runs, as `runs` tells.
fn settled<T>(
    updated: Result<(), ContainerError>,
    fresh: Option<T>,
    runs: impl Fn(&T) -> bool,
) -> Result<Option<T>, ContainerError> {
    match (updated, fresh) {
        (Err(error), Some(fresh)) if runs(&fresh) => Err(error),
        (_, fresh) => Ok(fresh),
    }
}

/// Stops and removes the container `inspected`, which waits `grace` after
/// its stop signal. phase first turns off its restart policy and reads its
/// status again. A running or paused container gets its stop signal and
/// its grace. Any other container, such as one that Docker was restarting,
/// is removed at once, because `docker stop` would first wait out Docker's
/// restart delay.
fn discard(inspected: &Inspected, grace: Duration) -> Result<(), ContainerError> {
    let Some(fresh) = settle(&inspected.container)? else {
        return Ok(());
    };
    if fresh.state.runs() {
        stop(&fresh.container, grace)?;
    }
    remove(fresh.container)
}

/// Stops and removes the container `inspected` as [`discard`] does, and
/// stops waiting for docker once `cancel` is set.
fn discard_until(inspected: &Inspected, grace: Duration, cancel: &Cancel) -> Result<(), Halt> {
    let Some(fresh) = settle(&inspected.container).map_err(|error| halt(&error))? else {
        return Ok(());
    };
    if fresh.state.runs() {
        stop_until(&fresh.container, grace, cancel).map_err(|error| halt(&error))?;
    }
    remove(fresh.container).map_err(|error| halt(&error))
}

/// Stops and removes each container of `started`, after a failure ended
/// the start, and returns the processes whose container docker no longer
/// has. A container that is still there is left for `down` and `sweep`,
/// which find it by its recorded id or its name.
fn clean_up(scope: &Scope, app: &AppPlan, started: &ContainersStarted) -> Vec<ProcessName> {
    let mut removed = Vec::new();
    for container in &started.containers {
        let (Ok(service), Ok(id)) = (
            app::service(app, &container.process),
            names::ContainerId::new(container.id.as_str()),
        ) else {
            continue;
        };
        let gone = match inspect_id(scope, &service, &id) {
            Ok(None) => true,
            Ok(Some(inspected)) => {
                discard(&inspected, Duration::ZERO).is_ok()
                    && matches!(reinspect(&inspected.container), Ok(None))
            }
            Err(_) => false,
        };
        if gone {
            removed.push(container.process.clone());
        }
    }
    removed
}

impl ContainerRunner for ComposeRunner {
    fn objects(
        &self,
        workspace: &WorkspaceId,
        app: &AppPlan,
    ) -> Result<ContainerObjects, PhaseError> {
        app::objects(&self.scope(workspace, &app.name), app)
    }

    fn start(
        &self,
        start: &ContainerStart<'_>,
        launching: &mut dyn FnMut(&ProcessName) -> Result<(), PhaseError>,
    ) -> ContainersStarted {
        let mut started = ContainersStarted::default();
        match self.start_into(start, launching, &mut started) {
            Ok(()) | Err(Halt::Cancelled) => {}
            Err(Halt::Failed(error)) => {
                let scope = self.scope(start.workspace, &start.app.name);
                started.removed = clean_up(&scope, start.app, &started);
                started.failure = Some(error);
            }
        }
        started
    }

    fn running(
        &self,
        workspace: &WorkspaceId,
        app: &AppPlan,
        containers: &[ContainerEntry],
    ) -> Result<Vec<ProcessName>, PhaseError> {
        let scope = self.scope(workspace, &app.name);
        let mut running = Vec::new();
        for entry in containers {
            let (Ok(service), Some(Ok(id))) = (
                names::ServiceName::new(entry.process.as_str()),
                entry
                    .id
                    .as_ref()
                    .map(|id| names::ContainerId::new(id.as_str())),
            ) else {
                continue;
            };
            // A container that phase started from another plan of its
            // process is not the one that the plan asks for, so `start`
            // replaces it.
            let planned = app::plan_hash(&scope, app, &entry.process).ok();
            if planned.is_none() || entry.plan_hash != planned {
                continue;
            }
            // Docker's restart of a container empties its tmpfs, so a
            // container that waits for its secrets' files after a restart
            // is not running as planned, and `start` replaces it.
            let files = app
                .processes
                .iter()
                .find(|process| process.name == entry.process)
                .is_some_and(|process| app::file_secrets(app, process).next().is_some());
            match inspect_id(&scope, &service, &id) {
                Ok(Some(inspected)) if files && inspected.restarts > 0 => {}
                Ok(Some(inspected)) if inspected.state == ContainerState::Running => {
                    running.push(entry.process.clone());
                }
                // A container that is gone, has stopped, is restarting, or
                // no longer carries the app's labels is not running as the
                // record says, so `start` replaces it.
                Ok(_) | Err(ContainerError::Foreign { .. }) => {}
                Err(error) => return Err(docker_error(&error)),
            }
        }
        Ok(running)
    }

    fn stop(
        &self,
        stop: &ContainerStop<'_>,
    ) -> Vec<(ProcessName, Result<ContainerStopped, PhaseError>)> {
        let scope = self.scope(stop.workspace, stop.app);
        stop.containers
            .iter()
            .rev()
            .map(|entry| {
                (
                    entry.process.clone(),
                    stop_one(&scope, entry, stop.deadline),
                )
            })
            .collect()
    }
}

/// Stops and removes the container of `entry` in `scope`, as the module
/// docs say, within `deadline`.
fn stop_one(
    scope: &Scope,
    entry: &ContainerEntry,
    deadline: Instant,
) -> Result<ContainerStopped, PhaseError> {
    let Ok(service) = names::ServiceName::new(entry.process.as_str()) else {
        return Ok(ContainerStopped::Unstoppable);
    };
    let found = match &entry.id {
        Some(id) => match names::ContainerId::new(id.as_str()) {
            Ok(id) => inspect_id(scope, &service, &id),
            Err(_) => return Ok(ContainerStopped::Unstoppable),
        },
        None => inspect_named(scope, &service),
    };
    let inspected = match found {
        Ok(Some(inspected)) => inspected,
        Ok(None) => return Ok(ContainerStopped::AlreadyGone),
        // A container that holds the name without the scope's labels is
        // another program's, so the one that the record names is gone. A
        // container that the record names by id and that lacks the labels
        // is left alone, and stays recorded.
        Err(ContainerError::Foreign { .. }) if entry.id.is_none() => {
            return Ok(ContainerStopped::AlreadyGone);
        }
        Err(ContainerError::Foreign { .. }) => return Ok(ContainerStopped::Unstoppable),
        Err(error) => return Err(docker_error(&error)),
    };
    discard(
        &inspected,
        own_grace(&inspected.container).min(left(deadline)),
    )
    .map_err(|error| docker_error(&error))?;
    match reinspect(&inspected.container) {
        Ok(None) => Ok(ContainerStopped::Stopped),
        Ok(Some(_)) | Err(ContainerError::Foreign { .. }) => Ok(ContainerStopped::Unstoppable),
        Err(error) => Err(docker_error(&error)),
    }
}

/// Reads the state of a waiting container at most once each
/// [`STATE_INTERVAL`], and remembers its exit code.
struct Watcher {
    /// The container.
    container: crate::engine::container::OwnedContainer,
    /// When the state was last read.
    last: Cell<Option<Instant>>,
    /// Whether the container has ended.
    ended: Cell<bool>,
    /// The exit code that docker reported, once the container exited.
    code: Cell<Option<i32>>,
}

impl Watcher {
    /// Returns the watcher of the container `inspected`.
    fn new(inspected: &Inspected) -> Self {
        Self {
            container: inspected.container.clone(),
            last: Cell::new(None),
            ended: Cell::new(false),
            code: Cell::new(None),
        }
    }

    /// Reports whether docker no longer has the container, or the container
    /// exited, is restarting, or has restarted since it was created. A read that fails reports the container as running, and a
    /// later read reports it again.
    fn ended(&self) -> bool {
        if self.ended.get() {
            return true;
        }
        let at = now();
        if self
            .last
            .get()
            .is_some_and(|last| at.saturating_duration_since(last) < STATE_INTERVAL)
        {
            return false;
        }
        self.last.set(Some(at));
        match reinspect(&self.container) {
            Ok(None) => self.ended.set(true),
            Ok(Some(inspected)) => {
                if let ContainerState::Exited(code) = inspected.state {
                    self.ended.set(true);
                    self.code.set(Some(code));
                } else if inspected.state == ContainerState::Restarting || inspected.restarts > 0 {
                    // Docker restarts a container that exits under its
                    // restart policy, so a container that crashes in a loop
                    // never shows as exited. Its first exit ends the wait.
                    self.ended.set(true);
                }
            }
            Err(_) => {}
        }
        self.ended.get()
    }

    /// Returns the exit code of a container that exited.
    fn code(&self) -> Option<i32> {
        self.code.get()
    }
}

/// Returns the last [`TAIL_LINES`] lines of the container `id`'s output,
/// from both of its streams in the order the container wrote them, or none
/// when docker cannot give them. The lines are what the container wrote,
/// which may hold values that it printed itself.
fn tail(id: &names::ContainerId) -> Vec<String> {
    let count = TAIL_LINES.to_string();
    let Some(finished) = quick(
        ["logs", "--timestamps", "--tail", &count, "--", id.as_str()],
        "logs",
    )
    .ok()
    .filter(|finished| finished.status.success()) else {
        return Vec::new();
    };
    let lines = stamped(&finished.stdout, &finished.stderr, MOST_OUTPUT_BYTES);
    let skip = lines.len().saturating_sub(TAIL_LINES);
    lines.into_iter().skip(skip).map(|(_, text)| text).collect()
}

/// The text that ends a line longer than a docker command keeps, which
/// phase hands over cut short.
const CUT_LINE: &str = " [cut by phase]";

/// Returns the lines that `docker logs --timestamps` printed on `stdout`
/// and `stderr`, each as its timestamp and its text, in the order of their
/// timestamps. Docker prints each timestamp in a fixed width, so text
/// order is time order.
///
/// A stream that filled the `cap` bytes that a docker command keeps lost
/// its later lines, and its last kept line may be cut. That line is left
/// out, and so is every line of either stream stamped after the last whole
/// line of a full stream, so a later read that starts from the last line
/// handed over misses nothing. A full stream that holds no whole line
/// holds one line longer than `cap`, which is handed over cut short and
/// ended with [`CUT_LINE`], so the reads move past it.
fn stamped<'a>(stdout: &'a str, stderr: &'a str, cap: u64) -> Vec<(&'a str, String)> {
    let split = |line: &'a str| line.split_once(' ').unwrap_or((line, ""));
    let mut lines: Vec<(&str, String)> = Vec::new();
    let mut until: Option<&str> = None;
    for stream in [stdout, stderr] {
        let mut kept: Vec<&str> = stream.lines().collect();
        if u64::try_from(stream.len()).unwrap_or(u64::MAX) >= cap {
            let cut = kept.pop();
            let last = match (kept.last(), cut) {
                (Some(last), _) => split(last).0,
                (None, Some(cut)) => {
                    let (stamp, text) = split(cut);
                    lines.push((stamp, format!("{text}{CUT_LINE}")));
                    stamp
                }
                (None, None) => continue,
            };
            until = Some(until.map_or(last, |until| until.min(last)));
        }
        lines.extend(kept.into_iter().map(|line| {
            let (stamp, text) = split(line);
            (stamp, text.to_owned())
        }));
    }
    if let Some(until) = until {
        lines.retain(|(stamp, _)| *stamp <= until);
    }
    lines.sort_by(|left, right| left.0.cmp(right.0));
    lines
}

/// Returns the timestamp one nanosecond after `stamp`, which docker
/// printed as `2026-11-05T10:00:00.123456789Z`, with nine digits of
/// fraction. The nanosecond carries into the seconds and on into the date.
/// A stamp that is not in that form comes back as it is.
fn just_after(stamp: &str) -> String {
    Stamp::parse(stamp).map_or_else(|| stamp.to_owned(), |parsed| parsed.next().to_string())
}

/// A UTC time as docker prints it in `docker logs --timestamps`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    /// Days since 1970-01-01.
    days: i64,
    /// Seconds since midnight.
    seconds: u32,
    /// Nanoseconds since the second.
    nanos: u32,
}

impl Stamp {
    /// Seconds in a day.
    const DAY: u32 = 86_400;
    /// Nanoseconds in a second.
    const SECOND: u32 = 1_000_000_000;

    /// Reads `YYYY-MM-DDTHH:MM:SS.fZ`, where the fraction has one to nine
    /// digits.
    fn parse(stamp: &str) -> Option<Self> {
        let (date, rest) = stamp.split_once('T')?;
        let (time, fraction) = rest.strip_suffix('Z')?.split_once('.')?;
        let mut date = date.splitn(3, '-');
        let year: i64 = number(date.next()?)?;
        let month: u32 = number(date.next()?)?;
        let day: u32 = number(date.next()?)?;
        let mut time = time.splitn(3, ':');
        let hours: u32 = number(time.next()?)?;
        let minutes: u32 = number(time.next()?)?;
        let seconds: u32 = number(time.next()?)?;
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hours > 23
            || minutes > 59
            || seconds > 59
            || fraction.is_empty()
            || fraction.len() > 9
        {
            return None;
        }
        let nanos: u32 = number(&format!("{fraction:0<9}"))?;
        Some(Self {
            days: days_from_civil(year, month, day),
            seconds: hours * 3600 + minutes * 60 + seconds,
            nanos,
        })
    }

    /// Returns the time one nanosecond later.
    const fn next(self) -> Self {
        let mut next = self;
        next.nanos += 1;
        if next.nanos == Self::SECOND {
            next.nanos = 0;
            next.seconds += 1;
            if next.seconds == Self::DAY {
                next.seconds = 0;
                next.days += 1;
            }
        }
        next
    }
}

impl fmt::Display for Stamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = civil_from_days(self.days);
        write!(
            formatter,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:09}Z",
            self.seconds / 3600,
            self.seconds / 60 % 60,
            self.seconds % 60,
            self.nanos,
        )
    }
}

/// Reads `text`, which holds only ASCII digits, as a number.
fn number<T: std::str::FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Returns the days from 1970-01-01 to the proleptic Gregorian date
/// `year`-`month`-`day`, with Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_from_march = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Returns the proleptic Gregorian date `days` after 1970-01-01, with
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The output of a container, read with `docker logs --timestamps` at most
/// once each [`LOG_INTERVAL`], for a log probe. Each read asks only for
/// the lines since the last line that it handed over, so a container that
/// writes more than a docker command keeps still shows its later lines,
/// and a line that a read cut short comes back whole in the next read.
struct DockerLogs {
    /// The container.
    id: names::ContainerId,
    /// The timestamp of the last line handed over, and how many lines with
    /// that timestamp were handed over, or `None` before the first line.
    since: Option<(String, usize)>,
    /// When the output was last read.
    last: Option<Instant>,
    /// The bytes of each stream that a read keeps.
    cap: u64,
}

impl DockerLogs {
    /// Returns the reader of the output of the container `id`.
    const fn new(id: names::ContainerId) -> Self {
        Self {
            id,
            since: None,
            last: None,
            cap: MOST_OUTPUT_BYTES,
        }
    }

    /// Returns the lines of `stdout` and `stderr`, which `docker logs
    /// --timestamps` printed, that come after the lines already handed
    /// over, and counts them as handed over.
    fn after(&mut self, stdout: &str, stderr: &str) -> Vec<String> {
        let (since, already) = self.since.clone().unwrap_or_default();
        let mut repeated = already;
        let mut handed = Vec::new();
        // The latest stamp handed over, how many lines carry it, and
        // whether a read cut one of them short.
        let mut latest: Option<(&str, usize, bool)> = None;
        for (stamp, text) in stamped(stdout, stderr, self.cap) {
            if !since.is_empty() && stamp < since.as_str() {
                continue;
            }
            if stamp == since && repeated > 0 {
                repeated -= 1;
                continue;
            }
            let cut = text.ends_with(CUT_LINE);
            latest = match latest {
                Some((last, count, was_cut)) if last == stamp => {
                    Some((last, count + 1, was_cut || cut))
                }
                _ => Some((stamp, 1, cut)),
            };
            handed.push(text);
        }
        if let Some((stamp, count, cut)) = latest {
            self.since = Some(if cut {
                // `--since` includes its own time, so the next read starts
                // just after the cut line, which would fill it again.
                (just_after(stamp), 0)
            } else if stamp == since {
                (since.clone(), already + count)
            } else {
                (stamp.to_owned(), count)
            });
        }
        handed
    }
}

impl LogLines for DockerLogs {
    fn new_lines(&mut self) -> io::Result<Vec<String>> {
        let at = now();
        if self
            .last
            .is_some_and(|last| at.saturating_duration_since(last) < LOG_INTERVAL)
        {
            return Ok(Vec::new());
        }
        self.last = Some(at);
        let mut args = vec!["logs", "--timestamps"];
        if let Some((since, _)) = &self.since {
            args.extend(["--since", since.as_str()]);
        }
        args.extend(["--", self.id.as_str()]);
        let finished = quick(args, "logs").map_err(|error| io::Error::other(error.to_string()))?;
        if !finished.status.success() {
            return Err(io::Error::other(message(&finished.stderr)));
        }
        Ok(self.after(&finished.stdout, &finished.stderr))
    }
}
