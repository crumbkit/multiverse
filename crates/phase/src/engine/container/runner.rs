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
//!   container that exits meanwhile fails to start. It is ready only once
//!   a read of its state that began after the grace found it running,
//!   because a slow Docker can answer a read after the grace with a state
//!   it took before the container exited. When the caller's deadline
//!   passes before such a read, the start is uncertain.
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
//! `rewrite` writes the secret files of a container that `running`
//! reported, from a fresh environment of its process, as [`write_secrets`]
//! does, and removes the file of each optional secret that lost its value,
//! as [`lost_secrets`] names them. It inspects the container first and
//! resolves nothing for a process without secret files, or for a
//! container that no longer runs as planned, which `up` replaces. The
//! files keep the owner that the container was created with, read from
//! its own user, so a tag that has moved to another image changes
//! nothing. A container whose labels lack the secrets label started while
//! every one of its secrets was optional and absent, so it has no tmpfs and
//! no wrapper. When its fresh environment has a value for a secret,
//! `rewrite` reports that the container must be replaced, and `up` asks
//! `start` to replace it. A rewrite that fails while the container still
//! runs reports why, and the container keeps its earlier files.
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
//! container's result, and `stop` goes on to the next.
//!
//! Every removal of a container, in `stop`, in the replacement of a
//! container by `start` and in the clean-up after a failed start, removes
//! the container's anonymous volumes with it. docker makes one for each
//! `VOLUME` path of the image, and nothing records it, so it would
//! otherwise stay behind. The workspace's named volumes stay.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::cancel::Cancel;
use crate::clock::{POLL_INTERVAL, now, pause};
use crate::engine::container::ContainerError;
use crate::engine::container::app::{self, reference};
use crate::engine::container::command::{
    DockerExec, MOST_OUTPUT_BYTES, Referenced, check, check_compose, message, quick, write_secret,
};
use crate::engine::container::compose::{Up, app_up};
use crate::engine::container::names::{self, Identity, Scope};
use crate::engine::container::objects::{
    self, AnonymousVolumes, ContainerState, Inspected, ensure_image, ensure_volume, inspect_id,
    inspect_named, no_restart, reinspect, remove, stop, stop_until,
};
use crate::engine::container::secrets::{self, ImageConfig, SecretBytes};
use crate::engine::containers::{
    ContainerObjects, ContainerRewrite, ContainerRunner, ContainerStart, ContainerStop,
    ContainerStopped, ContainersStarted, Rewritten, StartedContainer,
};
use crate::engine::probe::{self, Check, Goal, LogLines, LogWatch, Outcome};
use crate::env::AppliedEnvironment;
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::probe::{LogPattern, ProbeCheck};
use crate::plan::restart::DEFAULT_STOP_GRACE;
use crate::plan::{AppPlan, ProcessPlan};
use crate::record::docker::DockerError;
use crate::record::run::ContainerEntry;
use crate::record::store::RunStore;
use crate::record::{ContainerId, DockerCause, DockerName};
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
    /// The only environment of the `docker exec` that writes a secret, or
    /// `None` for the host program's whole environment.
    docker_environment: Option<BTreeMap<String, String>>,
}

/// The values of the file secrets of a process, by secret name.
type NamedSecrets = Vec<(String, SecretBytes)>;

/// Returns the owner of the file secrets of `process` and their values,
/// or `None` when the process has no file secret with a value in
/// `environment`.
///
/// # Errors
///
/// Returns [`PhaseError::SecretOwnerUnknown`] when the owner would be
/// root, and [`PhaseError::ProcessUnstartable`] when `environment` cannot
/// write a value.
fn delivered(
    app: &AppPlan,
    process: &ProcessPlan,
    image_config: Option<&ImageConfig>,
    environment: &dyn AppliedEnvironment,
) -> Result<Option<(String, NamedSecrets)>, PhaseError> {
    let Some(image) = image_config else {
        return Ok(None);
    };
    // A process whose every secret file is optional and absent starts as
    // one without secret files, so it needs no owner.
    let present = environment.names();
    if app::present_file_secrets(app, process, Some(&present))
        .next()
        .is_none()
    {
        return Ok(None);
    }
    let owner =
        secrets::owner(app, process, image).ok_or_else(|| PhaseError::SecretOwnerUnknown {
            app: app.name.clone(),
            process: process.name.clone(),
        })?;
    let values = secret_values(app, process, environment).map_err(|reason| {
        PhaseError::ProcessUnstartable {
            app: app.name.clone(),
            process: process.name.clone(),
            reason,
        }
    })?;
    Ok(Some((owner, values)))
}

impl ComposeRunner {
    /// Returns the runner of the phase whose state is in `state_dir` and
    /// which watches a process without a probe for `start_grace`.
    pub(crate) const fn new(state_dir: PathBuf, start_grace: Duration) -> Self {
        Self {
            state_dir,
            start_grace,
            docker_environment: None,
        }
    }

    /// Returns this runner with `vars` as the only environment of the
    /// `docker exec` that writes a secret, or with the host program's
    /// whole environment when `vars` is `None`.
    pub(crate) fn with_docker_environment(
        mut self,
        vars: Option<BTreeMap<String, String>>,
    ) -> Self {
        self.docker_environment = vars;
        self
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
/// answer, and it refused otherwise. It keeps the step's cause. The
/// message holds no value of a variable, because no value is ever an
/// argument, a compose file entry or a label.
pub(crate) fn docker_error(error: &ContainerError) -> PhaseError {
    let source = match error {
        ContainerError::Missing { .. } | ContainerError::Unavailable { .. } => {
            DockerError::unavailable(error.to_string())
        }
        _ => DockerError::refused(error.to_string()),
    };
    PhaseError::Docker {
        source: error.describe(source.with_cause(error.cause())),
    }
}

/// Passes the log of the container `id` that the run record of
/// `workspace` names for the process that `request` names to `line`, as
/// [`objects::logs`] does with the request's lines and follow, after it
/// inspects the container with the app's scope. It returns the note that
/// says why no lines passed, or `None` when it read the log. A process
/// name that is not a service name, or an `id` that is not a container
/// id, gives [`crate::LogNote::EntryUnusable`], and a container that docker no
/// longer has gives [`crate::LogNote::ContainerGone`]. One whose labels do not
/// name the app's scope fails with the foreign cause, so docker's own
/// errors never reach `line` as lines of the app.
///
/// # Errors
///
/// Returns [`PhaseError::Docker`] when the inspection or `docker logs`
/// fails.
pub(crate) fn recorded_logs(
    state_dir: &std::path::Path,
    workspace: &WorkspaceId,
    id: &ContainerId,
    request: &crate::logs::LogRequest,
    line: &mut dyn FnMut(bool, &str) -> std::ops::ControlFlow<()>,
) -> Result<Option<crate::LogNote>, PhaseError> {
    use crate::LogNote;
    let scope = Scope::app(
        Identity {
            id: workspace.clone(),
            run_record: RunStore::new(state_dir).path(workspace),
        },
        &request.app,
    );
    let (Ok(service), Ok(docker_id)) = (
        names::ServiceName::new(request.process.as_str()),
        names::ContainerId::new(id.as_str()),
    ) else {
        return Ok(Some(LogNote::EntryUnusable));
    };
    match inspect_id(&scope, &service, &docker_id) {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(Some(LogNote::ContainerGone)),
        Err(error) => return Err(docker_error(&error)),
    }
    objects::logs(id, request.lines, request.follow, line).map_err(|error| docker_error(&error))?;
    Ok(None)
}

/// Returns the record's id of the container that `inspected` names.
fn record_id(inspected: &Inspected) -> Result<ContainerId, PhaseError> {
    ContainerId::try_from(inspected.container.id().as_str().to_owned()).map_err(|_| {
        docker_error(&ContainerError::unexpected(
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
        let mut image_config = None;
        if let Source::Image { image } = &app.source {
            ensure_image(image.as_str(), left(start.build), start.cancel)
                .map_err(|error| halt(&error))?;
            let has_files = app
                .processes
                .iter()
                .any(|process| app::file_secrets(app, process).next().is_some());
            if has_files {
                image_config =
                    Some(objects::image_config(image.as_str()).map_err(|error| halt(&error))?);
            }
        }
        // The file is checked before any volume is created. Each process's
        // child reads its own form of it, made once its environment is
        // resolved.
        app::compose_file(&scope, app, image_config.as_ref())?;
        for key in app::volume_keys(app) {
            ensure_volume(&scope, &key).map_err(|error| halt(&error))?;
            let name: DockerName = scope.volume_name(&key).as_str().parse().map_err(|_| {
                docker_error(&ContainerError::unexpected(
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
                crate::env::resolve_required(start.env, &app.name, process, start.deadline)
                    .map_err(|source| PhaseError::Environment {
                        app: app.name.clone(),
                        source,
                    })?;
            // The owner and the values of the secret files are settled
            // before the old container is removed, so a refusal, or an
            // environment that cannot write a value, leaves it running.
            let (owner, values) =
                match delivered(app, process, image_config.as_ref(), environment.as_ref())? {
                    Some((owner, values)) => (Some(owner), values),
                    None => (None, Vec::new()),
                };
            // An optional variable that the environment lacks is left
            // out of this child's compose file, so the container does
            // not receive it.
            let file = app::for_environment(
                &scope,
                app,
                image_config.as_ref(),
                name,
                &environment.names(),
            )?;
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
                    ready: None,
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
                    return Err(Halt::Failed(docker_error(&ContainerError::unexpected(
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
                Err(error) => return Err(halt(&no_shell_hint(error, owner.is_some()))),
            }
            let Some(inspected) = inspected else {
                started.problem =
                    Some((name.clone(), StartProblem::exited(None, None, Vec::new())));
                return Ok(());
            };
            // The secrets are written before the readiness wait, and a
            // write that fails ends the start, which removes the container.
            if let Some(owner) = &owner {
                write_secrets(
                    &inspected,
                    Delivery {
                        values,
                        // A new container has no old file to remove.
                        removed: Vec::new(),
                        owner,
                        docker_environment: self.docker_environment.as_ref(),
                        starting: true,
                    },
                    (start.deadline, start.cancel),
                )
                .map_err(|error| halt(&error))?;
            }
            drop(environment);
            match self.ready(app, process, &inspected, start) {
                Readiness::Ready => started.ready(name, inspected.restarts, &inspected.started),
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
            None => match watch_grace(
                began + self.start_grace,
                start.deadline,
                &mut ended,
                &|| watcher.running_since(),
                start.cancel,
            ) {
                // The operation's deadline passed before a read of the
                // container's state that began after its start grace found
                // it running, so the container may have ended within it.
                Outcome::NotReady { .. } => return Readiness::Uncertain,
                outcome => outcome,
            },
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

/// Watches a container without a probe through its start grace, which
/// ends at `grace_end`. The container is ready once a read of its state
/// that began at or after `grace_end` found it running, as
/// `running_since` tells. A read that a slow Docker answers late can
/// report a container running that has since exited, so the wall clock
/// passing `grace_end` proves nothing by itself. The watch ends early when
/// `ended` reports the container ended or `cancel` is set, and returns
/// [`Outcome::NotReady`] once `deadline` passes with no such read.
pub(crate) fn watch_grace(
    grace_end: Instant,
    deadline: Instant,
    ended: &mut dyn FnMut() -> bool,
    running_since: &dyn Fn() -> Option<Instant>,
    cancel: &Cancel,
) -> Outcome {
    let began = now();
    loop {
        if cancel.is_cancelled() {
            return Outcome::Cancelled;
        }
        if ended() {
            return Outcome::Ended;
        }
        if running_since().is_some_and(|since| since >= grace_end) {
            return Outcome::Ready;
        }
        let at = now();
        if at >= deadline {
            return Outcome::NotReady {
                waited: at - began,
                last: None,
            };
        }
        pause(POLL_INTERVAL.min(deadline - at));
    }
}

/// The hint that ends the failure of a container that receives secret
/// files and whose image cannot run the wrapper.
const NO_SHELL_HINT: &str = "the image has no /bin/sh, which phase's wrapper for secret files needs, so set secret_delivery = \"env\" for this process";

/// Returns `error`, the failure of `docker compose up`, with
/// [`NO_SHELL_HINT`] added when the process receives secret files, as
/// `files` tells, and Docker could not find `/bin/sh` to run the wrapper.
fn no_shell_hint(error: ContainerError, files: bool) -> ContainerError {
    match error {
        ContainerError::Failed {
            operation,
            service,
            message,
            cause,
        } if files && message.contains("/bin/sh") => ContainerError::Failed {
            operation,
            service,
            message: format!("{message}; {NO_SHELL_HINT}"),
            cause,
        },
        other => other,
    }
}

/// Returns the value of each secret of `process` of `app` that it
/// receives as a file, read from `environment` in name order. `up` reads
/// them before it removes the process's old container, so an environment
/// that cannot write a value leaves that container running. An optional
/// secret that the environment's names lack is absent: it has no value
/// here and gets no file, and [`lost_secrets`] names it so that a rewrite
/// removes its old file.
///
/// # Errors
///
/// Returns why the environment could not write a value, which names the
/// secret and holds no value.
pub(crate) fn secret_values(
    app: &AppPlan,
    process: &ProcessPlan,
    environment: &dyn AppliedEnvironment,
) -> Result<Vec<(String, SecretBytes)>, String> {
    let present = environment.names();
    app::present_file_secrets(app, process, Some(&present))
        .map(|name| {
            SecretBytes::of(environment, name)
                .map(|bytes| (name.clone(), bytes))
                .map_err(|error| match error.kind() {
                    io::ErrorKind::NotFound => {
                        format!("the environment source supplied no value for the secret {name}")
                    }
                    io::ErrorKind::Unsupported => format!(
                        "the environment source cannot write the value of the secret {name} to a file, so set secret_delivery = \"env\" for this process"
                    ),
                    kind => format!(
                        "the environment source failed to write the value of the secret {name}: {kind}"
                    ),
                })
        })
        .collect()
}

/// Returns the optional secrets of `process` of `app` that it receives as
/// files and that `environment` has no value for, in name order. A rewrite
/// of a running container's files, which each `up` makes, removes their
/// files, so a secret that lost its value keeps no old one. Only a rewrite
/// uses it: a start makes a new container, which has no old file.
pub(crate) fn lost_secrets(
    app: &AppPlan,
    process: &ProcessPlan,
    environment: &dyn AppliedEnvironment,
) -> Vec<String> {
    let present = environment.names();
    app::file_secrets(app, process)
        .filter(|name| process.optional_env.contains(*name) && !present.contains(*name))
        .cloned()
        .collect()
}

/// What [`write_secrets`] writes: the values of a process's secrets, from
/// [`secret_values`], the optional secrets whose files it removes, from
/// [`lost_secrets`], and the owner its files get.
pub(crate) struct Delivery<'a> {
    /// Each secret's name and value.
    pub(crate) values: Vec<(String, SecretBytes)>,
    /// The secrets whose files the commit removes, from [`lost_secrets`].
    /// Only a rewrite of a running container's files removes any; a start
    /// passes none, because a new container has no old file.
    pub(crate) removed: Vec<String>,
    /// The owner of the files, in the form that `chown` reads. Only the
    /// staging of a value uses it, so a delivery without values may leave
    /// it empty.
    pub(crate) owner: &'a str,
    /// The only environment of the `docker exec` that writes each secret,
    /// or `None` for the host program's whole environment.
    pub(crate) docker_environment: Option<&'a BTreeMap<String, String>>,
    /// Whether the write belongs to a start, whose wrapper waits for the
    /// files. A start's failed write explains itself with the wrapper's own
    /// line when the container has exited. A rewrite of a running
    /// container's files never reads that line, because the wrapper of a
    /// running container has long since started the app, and the app's own
    /// output could hold a line that looks like the wrapper's.
    pub(crate) starting: bool,
}

/// Writes the secret files of `delivery` into the container `inspected`,
/// until the deadline or the cancel flag of `until`.
///
/// It stages every secret first and renames the staged files over the
/// old ones only once all of them are staged, as the secrets module says.
/// A staging or a rename that fails removes the staged files, so the
/// container keeps its old set of files whole, apart from the files that
/// a failed rename already replaced. Writing again with fresh values
/// therefore rotates the secrets of a running container without
/// restarting it.
///
/// # Errors
///
/// Returns [`ContainerError::Failed`] when a staging or the renames fail,
/// or the container ended before its files were written,
/// [`ContainerError::Cancelled`], and the errors of checking that docker
/// answers. No error holds a value.
pub(crate) fn write_secrets(
    inspected: &Inspected,
    delivery: Delivery<'_>,
    until: (Instant, &Cancel),
) -> Result<(), ContainerError> {
    let id = inspected.container.id().as_str();
    let service = inspected.container.service().as_str();
    let names: Vec<String> = delivery
        .values
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let (deadline, cancel) = until;
    let delivery_starting = delivery.starting;
    let mut staged = Ok(());
    for (name, bytes) in delivery.values {
        staged = write_secret(
            &secrets::stage(id, &name, delivery.owner),
            bytes,
            delivery.docker_environment,
            (left(deadline), cancel),
        )
        .and_then(|finished| finished.checked("secret write", Some(service)).map(drop));
        if staged.is_err() {
            break;
        }
    }
    let committed = staged.and_then(|()| {
        quick(
            secrets::commit(id, names.iter(), delivery.removed.iter()),
            "secret commit",
        )?
        .checked("secret commit", Some(service))
        .map(drop)
    });
    committed.map_err(|error| {
        let _ = quick(secrets::discard(id), "secret discard");
        if delivery_starting {
            ended_early(inspected).unwrap_or(error)
        } else {
            error
        }
    })
}

/// Returns the failure of a container `inspected` that ended before its
/// secret files were written, which explains a start's write that failed:
/// the wrapper's own line when it gave up, and [`NO_SHELL_HINT`] when
/// Docker could not run it. It returns `None` when the container still
/// runs. Only a start calls it, as [`Delivery::starting`] says.
fn ended_early(inspected: &Inspected) -> Option<ContainerError> {
    let fresh = reinspect(&inspected.container).ok()??;
    let ContainerState::Exited(code) = fresh.state else {
        return None;
    };
    let service = inspected.container.service().as_str();
    let message = match code {
        secrets::NO_SHELL => format!("the container exited with status {code}; {NO_SHELL_HINT}"),
        secrets::WRAPPER_FAILED => {
            let said = tail(inspected.container.id())
                .into_iter()
                .rev()
                .find(|line| line.starts_with("phase:"))
                .unwrap_or_default();
            format!(
                "the container exited with status {code} before its secrets were written: {said}"
            )
        }
        code => format!("the container exited with status {code} before its secrets were written"),
    };
    Some(ContainerError::failed(
        "secret write",
        Some(service),
        message,
    ))
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
/// restart delay. docker removes the container's anonymous volumes with
/// it, the ones it made for its image's `VOLUME` paths, which nothing else
/// records. Its named volumes stay.
fn discard(inspected: &Inspected, grace: Duration) -> Result<(), ContainerError> {
    let Some(fresh) = settle(&inspected.container)? else {
        return Ok(());
    };
    if fresh.state.runs() {
        stop(&fresh.container, grace)?;
    }
    remove(fresh.container, AnonymousVolumes::Remove)
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
    remove(fresh.container, AnonymousVolumes::Remove).map_err(|error| halt(&error))
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

/// Reports whether `error` says that Docker itself could not be reached
/// or did not answer in time: no `docker` program, a daemon that does not
/// answer, or a command that missed its deadline. A rewrite reports such a
/// failure as a failure of `up`, because phase cannot tell what the
/// container holds, and keeps a refusal as a note.
fn unreachable(error: &ContainerError) -> bool {
    matches!(
        error.cause(),
        DockerCause::DockerMissing | DockerCause::DaemonUnavailable | DockerCause::TimedOut
    )
}

/// Reports whether the container `inspected` still runs as it was
/// started: it is running, and Docker has not restarted it when it has
/// secret files, because the restart emptied its tmpfs. `running` judges a
/// container the same way.
fn runs_as_planned(inspected: &Inspected) -> bool {
    inspected.state == ContainerState::Running && !(inspected.files && inspected.restarts > 0)
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

    fn rewrite(&self, rewrite: &ContainerRewrite<'_>) -> Result<Rewritten, PhaseError> {
        let app = rewrite.app;
        let Some(process) = app
            .processes
            .iter()
            .find(|process| process.name == rewrite.entry.process)
        else {
            return Ok(Rewritten::NoFiles);
        };
        // A start writes secret files only into a container of an image.
        if !matches!(app.source, Source::Image { .. })
            || app::file_secrets(app, process).next().is_none()
        {
            return Ok(Rewritten::NoFiles);
        }
        let scope = self.scope(rewrite.workspace, &app.name);
        let (Ok(service), Some(Ok(id))) = (
            names::ServiceName::new(process.name.as_str()),
            rewrite
                .entry
                .id
                .as_ref()
                .map(|id| names::ContainerId::new(id.as_str())),
        ) else {
            return Ok(Rewritten::Replace);
        };
        let inspected = match inspect_id(&scope, &service, &id) {
            Ok(Some(inspected)) => inspected,
            // A container that is gone, or no longer carries the app's
            // labels, is not the one the record names, so `up` replaces it.
            Ok(None) | Err(ContainerError::Foreign { .. }) => return Ok(Rewritten::Replace),
            Err(error) if unreachable(&error) => return Err(docker_error(&error)),
            Err(error) => {
                return Ok(Rewritten::Kept {
                    reason: error.to_string(),
                });
            }
        };
        if !runs_as_planned(&inspected) {
            return Ok(Rewritten::Replace);
        }
        let environment =
            match crate::env::resolve_required(rewrite.env, &app.name, process, rewrite.deadline) {
                Ok(environment) => environment,
                Err(source) => {
                    return Ok(Rewritten::Kept {
                        reason: source.to_string(),
                    });
                }
            };
        let present = environment.names();
        let has_values = app::present_file_secrets(app, process, Some(&present))
            .next()
            .is_some();
        // A container that started while every secret was optional and
        // absent has no tmpfs and no wrapper, so it can receive a file
        // only as a new container.
        if !inspected.files {
            return Ok(if has_values {
                Rewritten::Replace
            } else {
                Rewritten::NoFiles
            });
        }
        let values = match secret_values(app, process, environment.as_ref()) {
            Ok(values) => values,
            Err(reason) => return Ok(Rewritten::Kept { reason }),
        };
        // The files keep the owner that the container was created with,
        // whatever image its tag names now.
        let owner = if values.is_empty() {
            String::new()
        } else {
            match secrets::container_owner(process, &inspected.user) {
                Some(owner) => owner,
                None => {
                    return Ok(Rewritten::Kept {
                        reason: "the container runs as root and the plan names no secret_owner, so the files would belong to root".to_owned(),
                    });
                }
            }
        };
        let removed = lost_secrets(app, process, environment.as_ref());
        drop(environment);
        let written = Rewritten::Written {
            files: values.len(),
            removed: removed.len(),
        };
        if values.is_empty() && removed.is_empty() {
            return Ok(written);
        }
        match write_secrets(
            &inspected,
            Delivery {
                values,
                removed,
                owner: &owner,
                docker_environment: self.docker_environment.as_ref(),
                starting: false,
            },
            (rewrite.deadline, rewrite.cancel),
        ) {
            Ok(()) => Ok(written),
            Err(error @ ContainerError::Cancelled { .. }) => Err(docker_error(&error)),
            Err(error) if unreachable(&error) => Err(docker_error(&error)),
            // A container that ended during the write must be replaced.
            Err(error) => match reinspect(&inspected.container) {
                Ok(None) => Ok(Rewritten::Replace),
                Ok(Some(fresh)) if !runs_as_planned(&fresh) => Ok(Rewritten::Replace),
                Err(fresh) if unreachable(&fresh) => Err(docker_error(&fresh)),
                Ok(Some(_)) | Err(_) => Ok(Rewritten::Kept {
                    reason: error.to_string(),
                }),
            },
        }
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
    /// When the last read that found the container running began.
    running_since: Cell<Option<Instant>>,
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
            running_since: Cell::new(None),
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
                } else {
                    self.running_since.set(Some(at));
                }
            }
            Err(_) => {}
        }
        self.ended.get()
    }

    /// Returns when the last read that found the container running began.
    /// Docker takes the state it reports after the read begins, so the
    /// container ran at least until then.
    fn running_since(&self) -> Option<Instant> {
        self.running_since.get()
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
