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
//! order. For each process it resolves the process's environment, removes
//! a container that holds the process's name and carries the app's labels,
//! checks that the process's published host ports are free, and runs
//! `docker compose up` with the values of the process's variables on that
//! child alone, each under its private reference. The pull and the
//! resolution come before any removal, so a failure in either leaves the
//! old containers running. It then waits for the container to be ready
//! through the probe runner before it starts the next process. A pull,
//! `docker compose up` and a removal stop when the caller cancels.
//!
//! - An `http` probe asks the published port on 127.0.0.1.
//! - A `tcp` probe counts the published port as answering only when a
//!   connection stays open or the container sends data, because Docker's
//!   port proxy accepts a connection before anything in the container
//!   listens.
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
//! plan. `up` leaves those alone and asks `start` only for the others.
//!
//! When a docker command fails after some containers started, `start`
//! removes those containers, and reports each container that it created
//! and each that Docker no longer has, so the run record names every
//! container that may still run.
//!
//! `stop` takes the containers in reverse of the record's order, which is
//! dependency order. It finds each container by its recorded id, or by its
//! name when the record holds no id yet, and acts on it only when its
//! labels name the workspace, app and process. It stops a running
//! container with the stop signal that the compose file set, waits the
//! container's stop grace or until the deadline, whichever comes first,
//! removes it, and checks that docker no longer has it. It removes a
//! container that does not run at once. A failure on one container is that
//! container's result, and `stop` goes on to the next. Volumes stay.

use std::cell::Cell;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::cancel::Cancel;
use crate::clock::now;
use crate::engine::container::ContainerError;
use crate::engine::container::app::{self, reference};
use crate::engine::container::command::{
    DockerExec, MOST_OUTPUT_BYTES, Referenced, check, check_compose, message, quick,
};
use crate::engine::container::compose::{Up, app_up};
use crate::engine::container::names::{self, Identity, Scope};
use crate::engine::container::objects::{
    self, ContainerState, Inspected, Status, ensure_image, ensure_volume, inspect_id,
    inspect_named, reinspect, remove, stop, stop_until,
};
use crate::engine::containers::{
    ContainerObjects, ContainerRunner, ContainerStart, ContainerStop, ContainerStopped,
    ContainersStarted, StartedContainer,
};
use crate::engine::probe::{self, Check, Goal, LogLines, LogWatch, Outcome};
use crate::engine::run::watch;
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
        started: &mut ContainersStarted,
    ) -> Result<(), Halt> {
        let app = start.app;
        let scope = self.scope(start.workspace, &app.name);
        check().map_err(|error| halt(&error))?;
        check_compose().map_err(|error| halt(&error))?;
        let file = app::compose_file(&scope, app)?;
        let cleared = app::references(app);
        let dir = self.project_dir()?;
        // The image is pulled before any container is removed, so a pull
        // that fails leaves the old containers running.
        if let Source::Image { image } = &app.source {
            ensure_image(image.as_str(), left(start.build), start.cancel)
                .map_err(|error| halt(&error))?;
        }
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
            drop(environment);
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

/// Stops and removes the container `inspected`, which waits `grace` after
/// its stop signal. A container that does not run, such as one that Docker
/// is restarting, is removed at once, because `docker stop` waits for
/// Docker's restart delay first.
fn discard(inspected: &Inspected, grace: Duration) -> Result<(), ContainerError> {
    if inspected.status == Status::Running {
        stop(&inspected.container, grace)?;
    }
    remove(inspected.container.clone())
}

/// Stops and removes the container `inspected` as [`discard`] does, and
/// stops waiting for docker once `cancel` is set.
fn discard_until(inspected: &Inspected, grace: Duration, cancel: &Cancel) -> Result<(), Halt> {
    if inspected.status == Status::Running {
        stop_until(&inspected.container, grace, cancel).map_err(|error| halt(&error))?;
    }
    remove(inspected.container.clone()).map_err(|error| halt(&error))
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

    fn start(&self, start: &ContainerStart<'_>) -> ContainersStarted {
        let mut started = ContainersStarted::default();
        match self.start_into(start, &mut started) {
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
            match inspect_id(&scope, &service, &id) {
                Ok(Some(inspected)) if inspected.status == Status::Running => {
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
                } else if inspected.status == Status::Restarting || inspected.restarts > 0 {
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
    let lines = stamped(&finished.stdout, &finished.stderr);
    let skip = lines.len().saturating_sub(TAIL_LINES);
    lines
        .into_iter()
        .skip(skip)
        .map(|(_, text)| text.to_owned())
        .collect()
}

/// Returns the lines that `docker logs --timestamps` printed on `stdout`
/// and `stderr`, each as its timestamp and its text, in the order of their
/// timestamps. Docker prints each timestamp in a fixed width, so text
/// order is time order. A stream that filled the bytes that a docker
/// command keeps loses its last line, which the cut may have split.
fn stamped<'a>(stdout: &'a str, stderr: &'a str) -> Vec<(&'a str, &'a str)> {
    let whole = |stream: &'a str| {
        let mut lines: Vec<&str> = stream.lines().collect();
        if u64::try_from(stream.len()).unwrap_or(u64::MAX) >= MOST_OUTPUT_BYTES {
            lines.pop();
        }
        lines
    };
    let mut lines: Vec<(&str, &str)> = whole(stdout)
        .into_iter()
        .chain(whole(stderr))
        .map(|line| line.split_once(' ').unwrap_or((line, "")))
        .collect();
    lines.sort_by(|left, right| left.0.cmp(right.0));
    lines
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
}

impl DockerLogs {
    /// Returns the reader of the output of the container `id`.
    const fn new(id: names::ContainerId) -> Self {
        Self {
            id,
            since: None,
            last: None,
        }
    }

    /// Returns the lines of `stdout` and `stderr`, which `docker logs
    /// --timestamps` printed, that come after the lines already handed
    /// over, and counts them as handed over.
    fn after(&mut self, stdout: &str, stderr: &str) -> Vec<String> {
        let (since, mut repeated) = self.since.clone().unwrap_or_default();
        let mut handed = Vec::new();
        for (stamp, text) in stamped(stdout, stderr) {
            if !since.is_empty() && stamp < since.as_str() {
                continue;
            }
            if stamp == since && repeated > 0 {
                repeated -= 1;
                continue;
            }
            match &mut self.since {
                Some((last, count)) if last == stamp => *count += 1,
                _ => self.since = Some((stamp.to_owned(), 1)),
            }
            handed.push(text.to_owned());
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
