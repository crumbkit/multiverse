//! The container runner of `up`, `down` and `restart`, which runs each
//! container app of a plan as one compose project through the `docker`
//! program and its `compose` plugin.
//!
//! [`super::app`] names the project's objects and writes its compose
//! file, which `docker compose up` reads from standard input, so the file
//! never reaches the disk. `start` checks that docker and its compose
//! plugin answer, creates the workspace's volumes that the app mounts, and
//! then starts the selected processes one at a time, in dependency order.
//! For each process it removes a container that holds the process's name
//! and carries the app's labels, checks that the process's published host
//! ports are free, resolves the process's environment, and runs `docker
//! compose up` with the values of the process's variables on that child
//! alone, each under its private reference. It then waits for the
//! container to be ready through the probe runner before it starts the
//! next process.
//!
//! - An `http` probe asks the published port on 127.0.0.1.
//! - A `tcp` probe counts the published port as answering only when a
//!   connection stays open or the container sends data, because Docker's
//!   port proxy accepts a connection before anything in the container
//!   listens.
//! - A `command` probe runs inside the container through `docker exec`.
//! - A `log` probe reads the container's output through `docker logs`, at
//!   most once each [`LOG_INTERVAL`].
//! - A process without a probe is watched for the start grace, and a
//!   container that exits meanwhile fails to start.
//!
//! While it waits, `start` reads the container's state at most once each
//! [`STATE_INTERVAL`], and a container that docker no longer has or that
//! exited has ended. A process that does not become ready within its ready
//! timeout or the caller's deadline fails as not ready.
//!
//! When a docker command fails after some containers started, `start`
//! stops and removes those containers before it returns the failure, so
//! no container that the caller does not learn of keeps running.
//!
//! `stop` finds each container by its recorded id, or by its name when the
//! record holds no id yet, and acts on it only when its labels name the
//! workspace, app and process. It stops the container with the stop
//! signal that the compose file set, waits the container's stop grace or
//! until the deadline, whichever comes first, removes it, and checks that
//! docker no longer has it. Volumes stay.

use std::cell::Cell;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::clock::now;
use crate::engine::container::ContainerError;
use crate::engine::container::app::{self, reference};
use crate::engine::container::command::{
    DockerExec, Referenced, check, check_compose, message, quick,
};
use crate::engine::container::compose::{Up, app_up};
use crate::engine::container::names::{self, Identity, Scope};
use crate::engine::container::objects::{
    ContainerState, Inspected, ensure_volume, inspect_id, inspect_named, reinspect, remove, stop,
};
use crate::engine::containers::{
    ContainerObjects, ContainerRunner, ContainerStart, ContainerStop, ContainerStopped,
    ContainersStarted,
};
use crate::engine::probe::{self, Check, Goal, LogLines, LogWatch, Outcome};
use crate::engine::run::watch;
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::probe::{LogPattern, ProbeCheck};
use crate::plan::restart::DEFAULT_STOP_GRACE;
use crate::plan::{AppPlan, ProcessPlan};
use crate::record::docker::DockerError;
use crate::record::store::RunStore;
use crate::record::{ContainerId, DockerName};
use crate::report::StartProblem;

#[cfg(test)]
mod tests;

/// The shortest time between two reads of a waiting container's state.
pub(crate) const STATE_INTERVAL: Duration = Duration::from_millis(250);

/// The shortest time between two reads of a waiting container's output for
/// a log probe.
pub(crate) const LOG_INTERVAL: Duration = Duration::from_millis(100);

/// The most lines of a container's output that a report holds when the
/// container exits before it is ready.
const TAIL_LINES: &str = "20";

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
    /// The container failed to become ready.
    Failed(StartProblem),
}

impl ComposeRunner {
    /// Starts the processes of `start` into `started`, as the module docs
    /// say, and returns the failure that kept docker from acting.
    fn start_into(
        &self,
        start: &ContainerStart<'_>,
        started: &mut ContainersStarted,
    ) -> Result<(), PhaseError> {
        let app = start.app;
        let scope = self.scope(start.workspace, &app.name);
        check().map_err(|error| docker_error(&error))?;
        check_compose().map_err(|error| docker_error(&error))?;
        let file = app::compose_file(&scope, app)?;
        let dir = self.project_dir()?;
        for key in app::volume_keys(app) {
            ensure_volume(&scope, &key).map_err(|error| docker_error(&error))?;
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
                return Ok(());
            }
            let Some(process) = app.processes.iter().find(|process| &process.name == name) else {
                continue;
            };
            let service = app::service(app, name)?;
            if let Some(existing) =
                inspect_named(&scope, &service).map_err(|error| docker_error(&error))?
            {
                let grace = existing
                    .stop_timeout
                    .map_or(DEFAULT_STOP_GRACE, Duration::from_secs);
                discard(&existing, grace)?;
            }
            if let Some(port) = taken_port(app, process) {
                started.problem = Some((name.clone(), StartProblem::port_taken(port, Vec::new())));
                return Ok(());
            }
            let environment =
                start
                    .env
                    .resolve(&app.name, name, start.deadline)
                    .map_err(|source| PhaseError::Environment {
                        app: app.name.clone(),
                        source,
                    })?;
            let refer = |variable: &str| reference(app, name, variable);
            let up = app_up(
                &scope,
                &dir,
                &file,
                &service,
                Referenced {
                    environment: environment.as_ref(),
                    reference: &refer,
                },
            );
            drop(environment);
            // `compose up` may have created the container before it failed,
            // so the container is reported, and stopped, whatever it said.
            let inspected =
                inspect_named(&scope, &service).map_err(|error| docker_error(&error))?;
            if let Some(inspected) = &inspected {
                started
                    .containers
                    .push((name.clone(), record_id(inspected)?));
            }
            match up {
                Ok(Up::Started) => {}
                Ok(Up::PortAllocated(number)) => {
                    if let Some(port) = port_named(app, process, number) {
                        started.problem =
                            Some((name.clone(), StartProblem::port_taken(port, Vec::new())));
                        return Ok(());
                    }
                    return Err(docker_error(&ContainerError::failed(
                        "compose up",
                        Some(service.as_str()),
                        "docker reported a port that the process does not publish as allocated",
                    )));
                }
                Err(error) => return Err(docker_error(&error)),
            }
            let Some(inspected) = inspected else {
                started.problem =
                    Some((name.clone(), StartProblem::exited(None, None, Vec::new())));
                return Ok(());
            };
            match self.ready(app, process, &inspected, start) {
                Readiness::Ready => {}
                Readiness::Cancelled => return Ok(()),
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
        let deadline = (began + process.ready_timeout).min(start.deadline);
        let id = inspected.container.id().clone();
        let watcher = Watcher::new(inspected);
        let mut ended = || watcher.ended();
        let outcome = match &process.ready {
            None => watch(
                (began + self.start_grace).min(deadline),
                &mut ended,
                start.cancel,
            ),
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
            Outcome::NotReady { waited, .. } => Readiness::Failed(StartProblem::not_ready(waited)),
            Outcome::Failing { .. } => Readiness::Failed(StartProblem::not_ready(now() - began)),
        }
    }
}

/// Stops and removes the container `inspected`, which waits `grace` after
/// its stop signal.
fn discard(inspected: &Inspected, grace: Duration) -> Result<(), PhaseError> {
    stop(&inspected.container, grace).map_err(|error| docker_error(&error))?;
    remove(inspected.container.clone()).map_err(|error| docker_error(&error))
}

/// Stops and removes each container of `started`, after a failure ended
/// the start. A failure to stop one is left for `sweep`, which finds the
/// container by its name.
fn clean_up(scope: &Scope, app: &AppPlan, started: &ContainersStarted) {
    for (process, id) in &started.containers {
        let (Ok(service), Ok(id)) = (
            app::service(app, process),
            names::ContainerId::new(id.as_str()),
        ) else {
            continue;
        };
        if let Ok(Some(inspected)) = inspect_id(scope, &service, &id) {
            let _ = discard(&inspected, Duration::ZERO);
        }
    }
}

impl ContainerRunner for ComposeRunner {
    fn objects(
        &self,
        workspace: &WorkspaceId,
        app: &AppPlan,
    ) -> Result<ContainerObjects, PhaseError> {
        app::objects(&self.scope(workspace, &app.name), app)
    }

    fn start(&self, start: &ContainerStart<'_>) -> Result<ContainersStarted, PhaseError> {
        let mut started = ContainersStarted::default();
        match self.start_into(start, &mut started) {
            Ok(()) => Ok(started),
            Err(error) => {
                clean_up(
                    &self.scope(start.workspace, &start.app.name),
                    start.app,
                    &started,
                );
                Err(error)
            }
        }
    }

    fn stop(
        &self,
        stop: &ContainerStop<'_>,
    ) -> Result<Vec<(ProcessName, ContainerStopped)>, PhaseError> {
        let scope = self.scope(stop.workspace, stop.app);
        let mut stopped = Vec::new();
        for entry in stop.containers {
            let how = stop_one(&scope, entry, stop.deadline)?;
            stopped.push((entry.process.clone(), how));
        }
        Ok(stopped)
    }
}

/// Stops and removes the container of `entry` in `scope`, as the module
/// docs say, within `deadline`.
fn stop_one(
    scope: &Scope,
    entry: &crate::record::run::ContainerEntry,
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
    let own = inspected
        .stop_timeout
        .map_or(DEFAULT_STOP_GRACE, Duration::from_secs);
    let left = deadline.saturating_duration_since(now());
    discard(&inspected, own.min(left))?;
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

    /// Reports whether docker no longer has the container or the container
    /// exited. A read that fails reports the container as running, and a
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

/// Returns the last [`TAIL_LINES`] lines of the container `id`'s output, or
/// none when docker cannot give them.
fn tail(id: &names::ContainerId) -> Vec<String> {
    quick(["logs", "--tail", TAIL_LINES, "--", id.as_str()], "logs")
        .ok()
        .filter(|finished| finished.status.success())
        .map(|finished| {
            finished
                .stdout
                .lines()
                .chain(finished.stderr.lines())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The output of a container, read with `docker logs` at most once each
/// [`LOG_INTERVAL`], for a log probe. It counts the lines of each stream
/// that it handed over, and hands over only the lines after them. `docker`
/// keeps at most 1 MiB of each stream, so a container that writes more
/// before it is ready may hide its ready line.
struct DockerLogs {
    /// The container.
    id: names::ContainerId,
    /// The lines of standard output handed over.
    out: usize,
    /// The lines of standard error handed over.
    err: usize,
    /// When the output was last read.
    last: Option<Instant>,
}

impl DockerLogs {
    /// Returns the reader of the output of the container `id`.
    const fn new(id: names::ContainerId) -> Self {
        Self {
            id,
            out: 0,
            err: 0,
            last: None,
        }
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
        let finished = quick(["logs", "--", self.id.as_str()], "logs")
            .map_err(|error| io::Error::other(error.to_string()))?;
        if !finished.status.success() {
            return Err(io::Error::other(message(&finished.stderr)));
        }
        let mut lines: Vec<String> = finished
            .stdout
            .lines()
            .skip(self.out)
            .map(str::to_owned)
            .collect();
        self.out += lines.len();
        let errors: Vec<String> = finished
            .stderr
            .lines()
            .skip(self.err)
            .map(str::to_owned)
            .collect();
        self.err += errors.len();
        lines.extend(errors);
        Ok(lines)
    }
}
